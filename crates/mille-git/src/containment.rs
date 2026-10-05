//! Path and revision guards. Rust re-implements them rather than trusting
//! napi arguments (SPEC mille-git §15.2, §20.1): the TypeScript adapters run
//! the same checks first so renderer strings fail in JS with the same
//! messages, but the engine never assumes they did.

use std::path::{Path, PathBuf};

use crate::error::{GitError, Result};
use crate::paths;

/// `isSafeWorkspaceRelativePath` from `mille-ui/src/diagnostics/provider.ts`:
/// non-empty, no NUL, no backslash, not absolute (POSIX, `~`, or a drive
/// letter), and no empty / `.` / `..` segment.
pub fn is_safe_workspace_relative_path(path: &str) -> bool {
    if path.is_empty() || path.contains('\0') || path.contains('\\') {
        return false;
    }
    if path.starts_with('/') || path.starts_with('~') || has_drive_prefix(path) {
        return false;
    }
    path.split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// `assertPathUnderRoot` (`shell-history.ts`): validate a workspace-relative
/// POSIX path and prove it cannot leave `root` lexically. Returns the path
/// unchanged (it is already normalized, or it would have been rejected).
///
/// This is lexical. A symlink inside the workspace that points outside it is
/// caught by [`assert_real_path_under_root`], which callers that *follow*
/// links (working-tree reads) must also run.
pub fn assert_path_under_root<'a>(root: &Path, relative: &'a str) -> Result<&'a str> {
    if relative.is_empty() {
        return Err(GitError::invalid(
            "path must be a non-empty workspace-relative path",
        ));
    }
    // Absolute / home / drive / backslash / NUL first, before anything could
    // re-read `/etc/passwd` as the relative `etc/passwd`.
    if relative.starts_with('/')
        || relative.starts_with('~')
        || has_drive_prefix(relative)
        || relative.contains('\\')
        || relative.contains('\0')
    {
        return Err(escape(relative));
    }
    if !is_safe_workspace_relative_path(relative) {
        return Err(escape(relative));
    }
    let joined = paths::normalize_lexical(&paths::join_posix(root, relative));
    let root = paths::normalize_lexical(root);
    if paths::relative_posix(&joined, &root).is_none() {
        return Err(escape(relative));
    }
    Ok(relative)
}

/// `assertRealPathUnderRoot`: both arguments are already realpaths; the
/// target must be strictly inside the root.
pub fn assert_real_path_under_root(
    real_root: &Path,
    real_target: &Path,
    reported: &str,
) -> Result<()> {
    if paths::relative_posix(real_target, real_root).is_none() {
        return Err(escape(reported));
    }
    Ok(())
}

/// `assertSafeRevision`: a revision is interpolated positionally
/// (`git show <rev>:<path>` cannot take `--`), so one starting with `-`
/// reaches git as an *option* — `--output=<file>` alone is an arbitrary file
/// write. The charset admits `HEAD~3`, `HEAD@{2}`, `v1.2.0^{}`,
/// `refs/heads/x` and hex ids, and rejects `:` (a second `rev:path`
/// separator) along with every shell / control byte.
pub fn assert_safe_revision(rev: &str) -> Result<&str> {
    if rev.is_empty() {
        return Err(GitError::invalid("revision must be a non-empty string"));
    }
    if rev.starts_with('-') {
        return Err(GitError::invalid(format!(
            "invalid revision (looks like a git option): {rev}"
        )));
    }
    let ok = rev.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'.' | b'_' | b'/' | b'^' | b'~' | b'@' | b'{' | b'}' | b'+' | b'-'
            )
    });
    if !ok {
        return Err(GitError::invalid(format!("invalid revision: {rev}")));
    }
    Ok(rev)
}

fn escape(path: &str) -> GitError {
    GitError::invalid(format!("path escapes workspace root: {path}")).with_path(path)
}

/// The engine's `allowedRoots` (SPEC §9.1, §20.2): the fail-closed gate on
/// the `open()` caller path and on every mutation / working-tree read. It
/// does **not** constrain where `.git` is discovered.
#[derive(Clone, Debug, Default)]
pub(crate) struct AllowList {
    roots: Vec<PathBuf>,
}

impl AllowList {
    pub(crate) fn new(roots: &[PathBuf]) -> Self {
        // Construction does no I/O (§9.1); realpaths are taken per check.
        Self {
            roots: roots
                .iter()
                .map(|root| paths::normalize_lexical(root))
                .collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Is `path` (which may be a root itself) inside some allowed root?
    /// Both ends are realpath'd — a symlinked caller path that resolves
    /// outside every root is refused even when its spelling looks inside.
    pub(crate) fn admits(&self, path: &Path) -> bool {
        let real = paths::canonicalize_lenient(path);
        self.roots
            .iter()
            .any(|root| paths::starts_with(&real, &paths::canonicalize_lenient(root)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T-PATH
    #[test]
    fn path_guard_rejects_every_escape_form() {
        let root = Path::new("/repo");
        for bad in [
            "",
            "../x",
            "a/../../x",
            "..",
            "/etc/passwd",
            "C:\\x",
            "c:/x",
            "a\\b",
            "a\0b",
            "~",
            "~/x",
            "a//b",
            "./a",
            "a/./b",
            "a/..",
            "a/",
        ] {
            let err = assert_path_under_root(root, bad).expect_err(bad);
            assert_eq!(err.code(), "EINVAL", "{bad:?}");
        }
        for good in [
            "a",
            "a/b.ts",
            "dir with space/f",
            ".gitignore",
            "a..b",
            "...",
            "x/.hidden",
        ] {
            assert_eq!(assert_path_under_root(root, good).unwrap(), good);
        }
    }

    // T-REV
    #[test]
    fn revision_guard_matches_shell_history() {
        for bad in [
            "",
            "-o",
            "--output=/tmp/x",
            "HEAD:path",
            "a b",
            "HEAD;rm",
            "x\n",
            "$(x)",
            "a`b`",
        ] {
            assert_eq!(
                assert_safe_revision(bad).unwrap_err().code(),
                "EINVAL",
                "{bad:?}"
            );
        }
        for good in [
            "HEAD",
            "HEAD~3",
            "HEAD@{2}",
            "v1.2.0^{}",
            "refs/heads/my-branch",
            "0123abcdef",
            "a+b",
        ] {
            assert_eq!(assert_safe_revision(good).unwrap(), good);
        }
    }

    #[test]
    fn real_path_guard_requires_a_strict_descendant() {
        assert!(assert_real_path_under_root(Path::new("/r"), Path::new("/r/a"), "a").is_ok());
        assert!(assert_real_path_under_root(Path::new("/r"), Path::new("/r"), ".").is_err());
        assert!(
            assert_real_path_under_root(Path::new("/r"), Path::new("/etc/passwd"), "x").is_err()
        );
    }

    #[test]
    fn allow_list_fails_closed_and_sees_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("ws");
        let outside = dir.path().join("secret");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        assert!(!AllowList::new(&[]).admits(&inside));
        let allow = AllowList::new(std::slice::from_ref(&inside));
        assert!(allow.admits(&inside));
        assert!(allow.admits(&inside.join("not-yet-created")));
        assert!(!allow.admits(&outside));
        #[cfg(unix)]
        {
            let link = inside.join("escape");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(
                !allow.admits(&link),
                "a symlink out of the root must not pass"
            );
        }
    }
}
