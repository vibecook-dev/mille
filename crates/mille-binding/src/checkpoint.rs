//! The saved index supplies the first view in full mode. Root population
//! reconciles that view with disk before signaling readiness.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::events::{clone_event, EventBus};
use crate::types::WarningPayloadJs;
use mille_core::{store::SnapshotBaseline, EntryKind, EntryStore, ErrorCode, FsChangeEvent};

#[derive(Default)]
pub(crate) struct Checkpoint {
    previous: Option<SnapshotBaseline>,
    opened_version: u64,
    seeded_version: Option<u64>,
    saved_version: Option<u64>,
    warning: Option<String>,
    populated: bool,
}

impl Checkpoint {
    pub(crate) fn open(
        path: Option<&Path>,
        roots: &[PathBuf],
        store: &mut EntryStore,
        warm: bool,
        policy: &mille_core::resume::CheckpointPolicy,
    ) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match store.prepare_resume(path, roots, warm, policy) {
            Ok(previous) => Self {
                previous: Some(previous),
                opened_version: store.tree_version(),
                ..Self::default()
            },
            Err(error) if error.code() == ErrorCode::ENOENT => Self::default(),
            Err(error) => Self {
                warning: Some(error.to_string()),
                ..Self::default()
            },
        }
    }

    pub(crate) fn has_baseline(&self) -> bool {
        self.previous.is_some()
    }

    pub(crate) fn seeded(&mut self, version: u64, events: &EventBus) {
        self.warn_if_stale(events);
        self.seeded_version.get_or_insert(version);
    }

    pub(crate) fn should_save(&self, store: &EntryStore) -> bool {
        let version = store.tree_version();
        self.saved_version != Some(version)
            && (self.populated
                || (version != self.opened_version
                    && (self.previous.is_none() || self.seeded_version != Some(version))))
    }

    pub(crate) fn saved(&mut self, version: u64) {
        self.saved_version = Some(version);
    }

    fn warn_if_stale(&mut self, events: &EventBus) {
        if let Some(detail) = self.warning.take() {
            events.emit_warning(WarningPayloadJs {
                code: "SNAPSHOT_STALE".into(),
                detail: Some(detail),
            });
        }
    }

    pub(crate) fn complete(
        &mut self,
        store: &EntryStore,
        events: &EventBus,
        complete: &HashSet<PathBuf>,
        observed: &HashSet<PathBuf>,
        checkpoint_path: &Path,
    ) {
        self.populated = true;
        self.saved(store.tree_version());
        self.warn_if_stale(events);
        let Some(previous) = self.previous.take() else {
            return;
        };
        let mut changes = Vec::new();
        let snapshot = store.snapshot();
        for (id, entry) in snapshot.entries_iter() {
            let Some(path) = store.path_for_id(id) else {
                continue;
            };
            if !observed.contains(&path) {
                continue;
            }
            let change = match previous.entries.get(&path) {
                None => {
                    // A row newly loaded from the lazy tail may have always existed.
                    let prior_parent = path
                        .ancestors()
                        .skip(1)
                        .find_map(|p| previous.entries.get(p));
                    if prior_parent.is_some_and(|e| {
                        previous
                            .complete_directories
                            .as_ref()
                            .is_some_and(|dirs| !dirs.contains(&e.id))
                    }) {
                        continue;
                    }
                    FsChangeEvent::Created { path }
                }
                Some(old)
                    if old.kind != entry.kind
                        || (Some(path.as_path()) != checkpoint_path.parent()
                            && (old.mtime_ms != entry.mtime_ms || old.size != entry.size))
                        || old.is_readonly != entry.is_readonly =>
                {
                    FsChangeEvent::Modified { path }
                }
                _ => continue,
            };
            changes.push(crate::watch_runtime::to_public_event(store, &change, None));
        }
        for (path, old) in previous.entries {
            if observed.contains(&path) {
                continue;
            }
            // A listed parent can prove a whole subtree disappeared. A stalled
            // directory or the unfinished prefix of a bounded walk cannot.
            let gone = path.ancestors().any(|p| {
                !observed.contains(p)
                    && p.parent().is_some_and(|parent| {
                        complete.contains(parent)
                            || (observed.contains(parent)
                                && store.get_by_path(parent).is_some_and(|e| {
                                    e.kind != EntryKind::Directory
                                        && e.symlink_target_is_dir != Some(true)
                                }))
                    })
            });
            if gone {
                store.forget_resume_path(&path);
                changes.push(crate::watch_runtime::to_public_event(
                    store,
                    &FsChangeEvent::Deleted { path },
                    Some(&old),
                ));
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        for change in &mut changes {
            change.detail = Some(std::mem::replace(
                &mut change.kind,
                "changedSinceSnapshot".into(),
            ));
            events.emit_event(clone_event(change));
        }
        if !changes.is_empty() {
            events.emit_batch(changes);
        }
    }
}
