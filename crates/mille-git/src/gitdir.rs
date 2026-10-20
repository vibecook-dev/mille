//! `.git` entry discovery and `gitdir:` redirect following.
//!
//! Ported from `packages/mille-ui/src/git/watch-dotgit.ts` (`resolveGitDir`)
//! and `shell-client.ts` (`findDotGit`). SPEC mille-git §9.3 makes the port
//! normative, including the comment below.

use std::path::{Path, PathBuf};

use crate::paths;

/// How far [`find_dot_git`] climbs before giving up, so a pathological
/// symlink chain cannot lock the caller up. Matches `findDotGit`.
pub const MAX_DOT_GIT_WALK: usize = 32;

/// Resolve a `.git` entry to the directory that actually holds `HEAD` and
/// `index`.
///
/// In an ordinary repository `.git` *is* that directory. In a linked worktree
/// (`git worktree add`) or a submodule it is a **file** whose contents are
/// `gitdir: <path>`, and both `HEAD` and `index` live at the far end of that
/// redirect — per worktree, not in the common dir.
///
/// Following it is load-bearing, not a nicety. Watching the `.git` file
/// itself attaches an inode watch whose events report the basename `.git`,
/// which the HEAD/index filter rejects; watching its containing directory
/// (what the TypeScript watcher used to fall back to) attaches to the
/// worktree root, where neither `HEAD` nor `index` will ever appear. Both
/// spell the same failure: the watcher goes permanently silent and status
/// freezes at whatever it showed on first paint, with no error anywhere.
///
/// The redirect is resolved against the `.git` entry's own directory. Git
/// writes an absolute path today, but the format permits a relative one and
/// submodules in older repositories carry them.
///
/// Returns `None` when the entry cannot be stat'd, read, or parsed — callers
/// skip the watcher entirely rather than attach a dead one (status still asks
/// gix or the CLI, which follow the redirect themselves).
pub fn resolve_git_dir(dot_git: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let contents = std::fs::read_to_string(dot_git).ok()?;
    // Anchored per line: the file is a single `gitdir: …` line today, but an
    // unanchored search would happily match the word inside some future
    // trailing key.
    let target = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    if target.is_empty() {
        return None;
    }
    let base = dot_git.parent().unwrap_or_else(|| Path::new(""));
    Some(paths::normalize_lexical(&base.join(target)))
}

/// Walk up from `start` looking for a `.git` entry (directory or `gitdir:`
/// file), at most [`MAX_DOT_GIT_WALK`] levels, stopping at the filesystem
/// root.
///
/// This walk MAY leave the engine's `allowedRoots` (SPEC §9.2 step 3): a
/// package workspace `/repo/packages/app` has its `.git` at `/repo/.git`, and
/// that is still a repository.
pub fn find_dot_git(start: &Path) -> Option<PathBuf> {
    let mut cursor = paths::normalize_lexical(start);
    for _ in 0..MAX_DOT_GIT_WALK {
        let candidate = cursor.join(".git");
        if candidate.exists() {
            return Some(candidate);
        }
        if !cursor.pop() {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // T-GITDIR
    #[test]
    fn a_directory_is_its_own_gitdir() {
        let dir = tempfile::tempdir().unwrap();
        let dot_git = dir.path().join(".git");
        std::fs::create_dir(&dot_git).unwrap();
        assert_eq!(resolve_git_dir(&dot_git), Some(dot_git));
    }

    #[test]
    fn an_absolute_redirect_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let dot_git = dir.path().join(".git");
        let target = dir.path().join("elsewhere").join("worktrees").join("wt");
        std::fs::write(&dot_git, format!("gitdir: {}\n", target.display())).unwrap();
        assert_eq!(resolve_git_dir(&dot_git), Some(target));
    }

    #[test]
    fn a_relative_redirect_resolves_against_the_entry_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let dot_git = sub.join(".git");
        std::fs::write(&dot_git, "gitdir: ../.git/modules/sub\r\n").unwrap();
        assert_eq!(
            resolve_git_dir(&dot_git),
            Some(dir.path().join(".git").join("modules").join("sub"))
        );
    }

    #[test]
    fn unreadable_or_unparseable_entries_are_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_git_dir(&dir.path().join("missing")), None);
        let garbage = dir.path().join("garbage");
        std::fs::write(&garbage, "not a redirect\n").unwrap();
        assert_eq!(resolve_git_dir(&garbage), None);
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "gitdir:   \n").unwrap();
        assert_eq!(resolve_git_dir(&empty), None);
        let binary = dir.path().join("binary");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(resolve_git_dir(&binary), None);
    }

    #[test]
    fn the_walk_climbs_out_of_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let deep = dir.path().join("packages").join("app").join("src");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_dot_git(&deep), Some(dir.path().join(".git")));
    }
}
