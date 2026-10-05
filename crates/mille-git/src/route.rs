//! Explorer `'event'` OS paths → per-session worktree-relative hints
//! (SPEC mille-git §11.2.1).
//!
//! One `'event'` carries one absolute OS path. The engine may hold several
//! sessions (two explorer roots, linked worktrees side by side), so the path
//! must reach exactly the checkout it is in — not every session, not none —
//! and it must arrive as a worktree-relative POSIX pathspec, because
//! `git status -- /abs/path` does not match porcelain's `src/a.ts`.
//!
//! The match is on realpaths: the event path's (or, for a deleted path, its
//! deepest surviving ancestor's, with the missing tail re-appended) against
//! each session's checkout realpath taken at `open`. The explorer's own
//! `normalize_path` is deliberately NOT applied: it maps FSEvents' reports
//! into the configured explorer-root spelling (`/var/folders/…`), while the
//! checkout realpath is `/private/var/folders/…`, and re-applying it would
//! undo exactly the prefix match this needs (T-HINT-ROUTE).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::paths;
use crate::session::Session;

/// Paths that are never worktree content: anything inside a `.git`
/// directory (the gitdir watch owns those), lock files, watchman cookies.
pub(crate) fn is_noise(path: &Path) -> bool {
    if path.components().any(|c| c.as_os_str() == ".git") {
        return true;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    name.ends_with(".lock") || name.starts_with(".watchman-cookie-")
}

/// Group `paths` by the session whose checkout is their longest realpath
/// prefix, as worktree-relative POSIX paths. Unmatched paths are dropped.
pub(crate) fn route(
    sessions: &[Arc<Session>],
    os_paths: &[PathBuf],
) -> Vec<(Arc<Session>, Vec<String>)> {
    let mut grouped: HashMap<PathBuf, (Arc<Session>, Vec<String>)> = HashMap::new();
    for os_path in os_paths {
        if is_noise(os_path) {
            continue;
        }
        let real = paths::canonicalize_lenient(os_path);
        let Some(owner) = sessions
            .iter()
            .filter(|s| paths::starts_with(&real, &s.real_root) && real != s.real_root)
            .max_by_key(|s| s.real_root.components().count())
        else {
            continue;
        };
        let Some(relative) = paths::relative_posix(&real, &owner.real_root) else {
            continue;
        };
        grouped
            .entry(owner.real_root.clone())
            .or_insert_with(|| (owner.clone(), Vec::new()))
            .1
            .push(relative);
    }
    grouped.into_values().collect()
}

/// Sessions that must full-scan for a coarse / overflow path: those whose
/// checkout root is a prefix of (or equal to) it. The inverse — a checkout
/// *under* the coarse path — matches nothing (§11.2 rule 4).
pub(crate) fn coarse_owners(sessions: &[Arc<Session>], os_path: &Path) -> Vec<Arc<Session>> {
    let real = paths::canonicalize_lenient(os_path);
    sessions
        .iter()
        .filter(|s| paths::starts_with(&real, &s.real_root))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitdir_and_lock_noise_is_filtered_before_routing() {
        assert!(is_noise(Path::new("/r/.git/index")));
        assert!(is_noise(Path::new("/r/sub/.git")));
        assert!(is_noise(Path::new("/r/src/a.ts.lock")));
        assert!(is_noise(Path::new("/r/.watchman-cookie-host-1")));
        assert!(!is_noise(Path::new("/r/.gitignore")));
        assert!(!is_noise(Path::new("/r/src/locked.ts")));
    }
}
