//! Path spelling helpers shared by identity, containment and hint routing.
//!
//! Three spellings of one directory meet in this crate: the caller's (the
//! explorer root, e.g. `/var/folders/…/repo` on macOS), the realpath
//! (`/private/var/folders/…/repo`, which is what `git rev-parse
//! --show-toplevel` prints), and git's own worktree-relative POSIX form
//! (`src/a.ts`). Every comparison below happens on realpaths; every path
//! handed back to a caller keeps the caller's spelling.

use std::path::{Component, Path, PathBuf};

/// `path.resolve` without touching the disk: drop `.`, fold `..` into the
/// parent. A `..` above the root stays at the root, as Node does.
pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `pop` refuses to remove a root or prefix, which is exactly
                // the clamp `path.resolve('/..')` applies.
                if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_)) | None
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Realpath of the deepest existing ancestor, with the missing tail
/// re-appended. A deleted file under `/var/folders/…` therefore still lands
/// under `/private/var/folders/…`, which a plain "realpath if it exists"
/// would miss.
pub(crate) fn canonicalize_lenient(path: &Path) -> PathBuf {
    let path = normalize_lexical(path);
    if let Ok(real) = std::fs::canonicalize(&path) {
        return real;
    }
    let mut tail = Vec::new();
    let mut cursor = path.as_path();
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(real) = std::fs::canonicalize(parent) {
            let mut out = real;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        cursor = parent;
    }
    path
}

/// Like [`canonicalize_lenient`] but never follows the final component.
///
/// Git mutations act on a symlink itself (staging a changed link, restoring
/// it), so containment for them must not resolve the link's target — that
/// would refuse to stage a tracked link that points outside the repo.
pub(crate) fn canonicalize_parent_lenient(path: &Path) -> PathBuf {
    let path = normalize_lexical(path);
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonicalize_lenient(parent).join(name),
        _ => canonicalize_lenient(&path),
    }
}

/// Component-wise `starts_with`, case-insensitive on Windows (NTFS and the
/// default `core.ignorecase=true` agree that `C:\Repo` and `c:\repo` are
/// one directory).
pub(crate) fn starts_with(path: &Path, base: &Path) -> bool {
    let mut path_components = path.components();
    for base_component in base.components() {
        match path_components.next() {
            Some(component) if component_eq(component, base_component) => {}
            _ => return false,
        }
    }
    true
}

fn component_eq(a: Component<'_>, b: Component<'_>) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// The POSIX relative path of `path` under `base` (no leading slash), or
/// `None` when `path` is not strictly under `base`.
pub(crate) fn relative_posix(path: &Path, base: &Path) -> Option<String> {
    if !starts_with(path, base) {
        return None;
    }
    let base_len = base.components().count();
    let parts: Vec<String> = path
        .components()
        .skip(base_len)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Join a worktree-relative POSIX path onto an OS directory.
pub(crate) fn join_posix(base: &Path, relative: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for segment in relative.split('/') {
        out.push(segment);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_dots() {
        assert_eq!(
            normalize_lexical(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(normalize_lexical(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../../..")),
            PathBuf::from("/")
        );
    }

    #[test]
    fn relative_posix_requires_a_strict_descendant() {
        assert_eq!(
            relative_posix(Path::new("/r/a/b.ts"), Path::new("/r")).as_deref(),
            Some("a/b.ts")
        );
        assert_eq!(relative_posix(Path::new("/r"), Path::new("/r")), None);
        assert_eq!(relative_posix(Path::new("/rr/a"), Path::new("/r")), None);
    }

    #[test]
    fn lenient_canonicalize_reaches_through_a_deleted_tail() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let missing = dir.path().join("gone").join("file.ts");
        assert_eq!(
            canonicalize_lenient(&missing),
            real.join("gone").join("file.ts")
        );
    }
}
