//! Restore matching path identities and rebuild an immutable view from a
//! checkpoint. Directory listings remain provisional until reconciled.
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;

use super::EntryStore;
use crate::entry::{Entry, EntryId, EntryKind, ENTRY_ID_MAX};
use crate::error::FxError;
use crate::resume::{read_checkpoint, write_snapshot_with_policy, CheckpointPolicy, RootStat};

pub(super) type ReservedIds = HashMap<PathBuf, (EntryId, EntryKind)>;

pub struct SnapshotBaseline {
    pub entries: HashMap<PathBuf, Entry>,
    pub complete_directories: Option<HashSet<EntryId>>,
}

impl EntryStore {
    pub fn write_checkpoint(&self, path: &Path, policy: &CheckpointPolicy) -> Result<(), FxError> {
        let (snapshot, next_id, roots) = {
            let _guard = self.write_lock.lock();
            let snapshot = self.snapshot();
            let roots: Vec<_> = snapshot
                .roots()
                .iter()
                .map(|&id| {
                    let path = self.path_for_id(id).ok_or_else(|| {
                        FxError::InternalBug("checkpoint root has no path".into())
                    })?;
                    Ok(RootStat {
                        path,
                        mtime_ms: snapshot.get(id).map_or(0, |entry| entry.mtime_ms),
                    })
                })
                .collect::<Result<_, FxError>>()?;
            (snapshot, self.id_counter.load(Ordering::Relaxed), roots)
        };
        write_snapshot_with_policy(&snapshot, next_id, &roots, path, Some(policy))
    }

    /// Validate the entire saved graph before accepting any identity. The
    /// persisted summary, visibility and child-order caches depend on settings;
    /// the current session rebuilds those using the caller's policies.
    pub fn prepare_resume(
        &mut self,
        path: &Path,
        roots: &[PathBuf],
        warm: bool,
        policy: &CheckpointPolicy,
    ) -> Result<SnapshotBaseline, FxError> {
        let (saved, coverage) = read_checkpoint(path)?;
        let saved_policy = coverage.as_ref().and_then(|c| c.policy.as_ref());
        if saved_policy.is_some_and(|previous| previous != policy) {
            return Err(FxError::InvalidInput(
                "checkpoint traversal settings have changed".into(),
            ));
        }
        // Older snapshots have no traversal policy, so verify their paths on
        // disk before exposing them. Their identities can still be reused.
        let warm = warm && saved_policy.is_some();
        let complete = coverage.map(|c| c.complete);
        let invalid = || FxError::InvalidInput("invalid checkpoint tree or workspace roots".into());
        if saved.root_stats.len() != roots.len()
            || saved.roots.len() != roots.len()
            || saved.next_entry_id > ENTRY_ID_MAX
            || saved.entries.len() > self.cache_limit
        {
            return Err(invalid());
        }
        let expected: HashSet<_> = roots.iter().collect();
        let actual: HashSet<_> = saved.root_stats.iter().map(|stat| &stat.path).collect();
        if expected != actual || actual.len() != roots.len() {
            return Err(invalid());
        }
        let mut descendants: HashMap<EntryId, Vec<EntryId>> = HashMap::new();
        for (&id, entry) in &saved.entries {
            if id != entry.id || id.raw() >= saved.next_entry_id {
                return Err(invalid());
            }
            if let Some(parent) = entry.parent_id {
                let name = Path::new(&entry.name);
                if !matches!(name.components().next(), Some(Component::Normal(n)) if n == name.as_os_str())
                    || !saved.entries.contains_key(&parent)
                {
                    return Err(invalid());
                }
                descendants.entry(parent).or_default().push(id);
            }
        }
        let count = saved.entries.len();
        let mut entries = saved.entries;
        let mut by_path = HashMap::with_capacity(count);
        let mut stack: Vec<_> = saved
            .roots
            .into_iter()
            .zip(saved.root_stats.into_iter().map(|stat| stat.path))
            .collect();
        for (id, _) in &stack {
            if entries.get(id).is_none_or(|e| e.parent_id.is_some()) {
                return Err(invalid());
            }
        }
        while let Some((id, path)) = stack.pop() {
            let entry = entries.remove(&id).ok_or_else(invalid)?;
            for child in descendants.remove(&id).unwrap_or_default() {
                let record = entries.get(&child).ok_or_else(invalid)?;
                stack.push((child, path.join(&record.name)));
            }
            if by_path.insert(path, entry).is_some() {
                return Err(invalid());
            }
        }
        if complete
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id.raw() >= saved.next_entry_id))
            || by_path.len() != count
        {
            return Err(invalid());
        }
        self.resume_ids = Some(parking_lot::Mutex::new(
            by_path
                .iter()
                .map(|(p, e)| (p.clone(), (e.id, e.kind)))
                .collect(),
        ));
        self.id_counter
            .store(saved.next_entry_id, Ordering::Relaxed);
        if warm {
            // Rebuild child order, summaries and visibility using this session's
            // settings. Saved child sets stay provisional until read from disk.
            let mut levels = std::collections::BTreeMap::<usize, Vec<_>>::new();
            for (path, entry) in &by_path {
                if entry.parent_id.is_some() {
                    levels
                        .entry(path.components().count())
                        .or_default()
                        .push((path.clone(), entry.clone()));
                }
            }
            self.insert_batch(
                roots
                    .iter()
                    .map(|path| (path.clone(), by_path[path].clone()))
                    .collect(),
            )?;
            for entries in levels.into_values() {
                self.insert_batch(entries)?;
            }
            self.take_pending_changes();
        }
        Ok(SnapshotBaseline {
            entries: by_path,
            complete_directories: complete.map(|ids| ids.into_iter().collect()),
        })
    }

    pub fn forget_resume_path(&self, path: &Path) {
        if let Some(ids) = &self.resume_ids {
            ids.lock().remove(path);
        }
    }

    pub(super) fn allocate_id(&self, path: &Path, entry: &Entry) -> Result<EntryId, FxError> {
        // The caller holds write_lock. reserve/remove and normal allocation
        // therefore cannot issue the same id to two concurrent observations.
        if let Some((id, kind)) = self
            .resume_ids
            .as_ref()
            .and_then(|ids| ids.lock().remove(path))
        {
            if kind == entry.kind || entry.parent_id.is_none() {
                return Ok(id);
            }
        }
        EntryId::alloc_from(&self.id_counter)
    }
}
