//! Watches on the resolved gitdir and the common dir (SPEC mille-git §11.3,
//! §11.4).
//!
//! The explorer already watches the worktree, and its walker skips `.git`.
//! So the engine watches exactly what the explorer cannot see — `HEAD`,
//! `index`, refs, `packed-refs`, `<common>/worktrees/` — and never attaches
//! a second recursive watch to the worktree root (§11.1).
//!
//! Every watch is a **directory** watch filtered by name. git rewrites
//! `index` (and `HEAD`, and loose refs) by `rename(index.lock, index)`; an
//! inode watch on the old `index` goes silent on Linux after the first
//! rename, which is the `watch-dotgit.ts` QA fix this ports. A directory
//! watch tracks the directory's inode and reports the new `index` by name.
//! It also sees the first `index` appear in an empty repository.
//!
//! `*.lock` is ignored as a trigger: git is mid-mutation while it exists, and
//! the rename that ends the mutation arrives as `index` / `HEAD` anyway.
//!
//! The watcher's callback holds only weak references and only queues work
//! (`Session::hint`): it runs on notify's thread and must never compute.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecursiveMode, Watcher};

use crate::paths;
use crate::queue::{Hints, Urgency};
use crate::session::Session;
use crate::warning::WarningCode;
use crate::Inner;

/// What a gitdir-side event means for the session.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitdirChange {
    /// HEAD / index / refs moved: run a full status.
    Status,
    /// A linked worktree was added or removed: refresh the worktree list.
    Worktrees,
    /// Lock files, editor droppings, objects, other worktrees' internals.
    Ignore,
}

/// Classify one event path. `gitdir` and `common` are realpaths (the
/// directories the watches were attached to), so FSEvents' realpath'd
/// reports and inotify's watch-relative ones both strip cleanly.
pub(crate) fn classify(path: &Path, gitdir: &Path, common: &Path) -> GitdirChange {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.ends_with(".lock")
        || name.starts_with(".watchman-cookie-")
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name == "4913"
    {
        return GitdirChange::Ignore;
    }
    // A coalesced "something changed in here" report for a watched dir.
    if path == gitdir || path == common {
        return GitdirChange::Status;
    }
    for base in [gitdir, common] {
        let Some(rel) = paths::relative_posix(path, base) else {
            continue;
        };
        let mut parts = rel.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("HEAD" | "index" | "packed-refs" | "commondir"), None, _) => {
                return GitdirChange::Status
            }
            (Some("refs" | "reftable"), _, _) => return GitdirChange::Status,
            (Some("worktrees"), None, _) | (Some("worktrees"), Some(_), None) if base == common => {
                return GitdirChange::Worktrees;
            }
            _ => {}
        }
    }
    GitdirChange::Ignore
}

/// The watches for one session. Dropping this stops notify's thread and
/// joins it (§11.3: "`close` / `Drop` MUST join the notify thread").
pub(crate) struct GitdirWatch {
    watcher: notify::RecommendedWatcher,
    watched: Vec<PathBuf>,
}

impl GitdirWatch {
    /// Watch `dir` non-recursively if it is not watched yet.
    pub(crate) fn ensure(&mut self, dir: &Path) {
        let Ok(dir) = std::fs::canonicalize(dir) else {
            return;
        };
        if self.watched.contains(&dir) {
            return;
        }
        if self
            .watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .is_ok()
        {
            self.watched.push(dir);
        }
    }
}

/// Attach the gitdir + common-dir watches for a `kind: worktree` session.
pub(crate) fn attach(session: &Arc<Session>, inner: &Arc<Inner>) -> Option<GitdirWatch> {
    let identity = session.identity();
    let gitdir = std::fs::canonicalize(identity.gitdir.as_ref()?).ok()?;
    let common = identity
        .common_dir
        .as_ref()
        .and_then(|c| std::fs::canonicalize(c).ok())
        .unwrap_or(gitdir.clone());
    let root = identity.worktree_path.clone();

    let weak_session: Weak<Session> = Arc::downgrade(session);
    let weak_inner: Weak<Inner> = Arc::downgrade(inner);
    let (event_gitdir, event_common) = (gitdir.clone(), common.clone());
    let handler = move |result: notify::Result<notify::Event>| {
        let (Some(session), Some(inner)) = (weak_session.upgrade(), weak_inner.upgrade()) else {
            return;
        };
        let event = match result {
            Ok(event) => event,
            Err(err) => {
                inner.warn(
                    WarningCode::Watch,
                    format!("gitdir watch error: {err}"),
                    &session.identity().worktree_path,
                );
                return;
            }
        };
        // Reads (open/close-without-write) are not changes; our own status
        // reads `index` and must not trigger itself.
        if matches!(event.kind, EventKind::Access(kind) if kind != AccessKind::Close(AccessMode::Write))
        {
            return;
        }
        let mut status = event.need_rescan();
        let mut worktrees = false;
        for path in &event.paths {
            match classify(path, &event_gitdir, &event_common) {
                GitdirChange::Status => status = true,
                GitdirChange::Worktrees => worktrees = true,
                GitdirChange::Ignore => {}
            }
        }
        if status {
            session.hint(&inner, Hints::Full, Urgency::Debounced);
        }
        if worktrees {
            session.worktrees_changed(&inner);
        }
    };

    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(err) => {
            inner.warn(
                WarningCode::Watch,
                format!("cannot create a gitdir watcher: {err}"),
                &root,
            );
            return None;
        }
    };
    let mut targets: Vec<(PathBuf, RecursiveMode)> =
        vec![(gitdir.clone(), RecursiveMode::NonRecursive)];
    if common != gitdir {
        targets.push((common.clone(), RecursiveMode::NonRecursive));
    }
    for (dir, mode) in [
        ("refs", RecursiveMode::Recursive),
        ("reftable", RecursiveMode::Recursive),
    ] {
        let path = common.join(dir);
        if path.is_dir() {
            targets.push((path, mode));
        }
    }
    let worktrees = common.join("worktrees");
    if worktrees.is_dir() {
        targets.push((worktrees, RecursiveMode::NonRecursive));
    }
    let mut watched = Vec::new();
    for (path, mode) in targets {
        match watcher.watch(&path, mode) {
            Ok(()) => watched.push(path),
            Err(err) => inner.warn(
                WarningCode::Watch,
                format!("cannot watch {}: {err}", path.display()),
                &root,
            ),
        }
    }
    Some(GitdirWatch { watcher, watched })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_follows_the_gitdir_layout() {
        let common = Path::new("/r/.git");
        let linked = Path::new("/r/.git/worktrees/wt");
        let c = |p: &str, gitdir: &Path| classify(Path::new(p), gitdir, common);
        assert_eq!(c("/r/.git/HEAD", common), GitdirChange::Status);
        assert_eq!(c("/r/.git/index", common), GitdirChange::Status);
        assert_eq!(c("/r/.git/packed-refs", common), GitdirChange::Status);
        assert_eq!(c("/r/.git/refs/heads/main", common), GitdirChange::Status);
        assert_eq!(c("/r/.git", common), GitdirChange::Status);
        assert_eq!(c("/r/.git/index.lock", common), GitdirChange::Ignore);
        assert_eq!(
            c("/r/.git/refs/heads/main.lock", common),
            GitdirChange::Ignore
        );
        assert_eq!(
            c("/r/.git/.watchman-cookie-x", common),
            GitdirChange::Ignore
        );
        assert_eq!(c("/r/.git/objects/ab/cdef", common), GitdirChange::Ignore);
        assert_eq!(c("/r/.git/ORIG_HEAD", common), GitdirChange::Ignore);
        assert_eq!(c("/r/.git/worktrees", common), GitdirChange::Worktrees);
        assert_eq!(c("/r/.git/worktrees/wt", common), GitdirChange::Worktrees);
        assert_eq!(
            c("/r/.git/worktrees/wt/HEAD", common),
            GitdirChange::Ignore,
            "another worktree's HEAD"
        );
        // A linked worktree's own HEAD/index live under common/worktrees/wt.
        assert_eq!(c("/r/.git/worktrees/wt/HEAD", linked), GitdirChange::Status);
        assert_eq!(
            c("/r/.git/worktrees/wt/index", linked),
            GitdirChange::Status
        );
        assert_eq!(
            c("/r/.git/refs/heads/feature", linked),
            GitdirChange::Status
        );
    }
}
