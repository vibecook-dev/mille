// EntryStore: the canonical in-memory tree state.
//
// Readers grab a lock-free Arc<StoreSnapshot> via ArcSwap::load.
// Writers serialize on a parking_lot Mutex while cloning+publishing
// the next snapshot, so insert/remove can't lose updates under contention.
//
// Phase 1 keeps StoreSnapshot's entry map flat (BTreeMap); Phase 2 swaps in
// a SumTree<EntryNode> behind the same API for O(log n) summary queries.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};

use crate::changes::ChangeSet;
use crate::entry::{Entry, EntryId, EntryKind};
use crate::error::FxError;
use crate::file_nesting::FileNestingPolicy;
use crate::snapshot::{StoreSnapshot, VisibilityPolicy, MAX_ANCESTOR_WALK};
use crate::sort::SiblingOrder;

pub struct EntryStore {
    inner: ArcSwap<StoreSnapshot>,
    path_to_id: DashMap<Arc<Path>, EntryId>,
    id_to_path: DashMap<EntryId, Arc<Path>>,
    id_counter: AtomicU64,
    write_lock: Mutex<()>,
    // Accumulated structural changes since the last take_pending_changes().
    // Guarded by its own mutex so writers can push under the write_lock without
    // tangling the main snapshot publish path.
    pending_changes: Mutex<ChangeSet>,
    sibling_order: RwLock<SiblingOrder>,
    visibility: RwLock<VisibilityPolicy>,
}

impl EntryStore {
    pub fn new() -> Self {
        Self::with_sibling_order(SiblingOrder::default())
    }

    pub fn with_sibling_order(sibling_order: SiblingOrder) -> Self {
        Self::with_policies(sibling_order, VisibilityPolicy::default())
    }

    pub fn with_policies(sibling_order: SiblingOrder, visibility: VisibilityPolicy) -> Self {
        Self::with_projection(sibling_order, visibility, false)
    }

    pub fn with_projection(
        sibling_order: SiblingOrder,
        visibility: VisibilityPolicy,
        compact_folders: bool,
    ) -> Self {
        Self::with_projection_settings(
            sibling_order,
            visibility,
            compact_folders,
            FileNestingPolicy::default(),
        )
    }

    pub fn with_projection_settings(
        sibling_order: SiblingOrder,
        visibility: VisibilityPolicy,
        compact_folders: bool,
        file_nesting: FileNestingPolicy,
    ) -> Self {
        Self {
            inner: ArcSwap::new(Arc::new(StoreSnapshot::empty_with_projection(
                visibility,
                compact_folders,
                file_nesting,
            ))),
            path_to_id: DashMap::new(),
            id_to_path: DashMap::new(),
            id_counter: AtomicU64::new(0),
            write_lock: Mutex::new(()),
            pending_changes: Mutex::new(ChangeSet::default()),
            sibling_order: RwLock::new(sibling_order),
            visibility: RwLock::new(visibility),
        }
    }

    pub fn snapshot(&self) -> Arc<StoreSnapshot> {
        self.inner.load_full()
    }

    pub fn tree_version(&self) -> u64 {
        self.inner.load().tree_version
    }

    pub fn get_by_id(&self, id: EntryId) -> Option<Arc<Entry>> {
        self.inner.load().entries.get(&id).cloned()
    }

    pub fn get_by_path(&self, path: &Path) -> Option<Arc<Entry>> {
        let id = *self.path_to_id.get(path)?.value();
        self.get_by_id(id)
    }

    /// Resolve an entry identity to its exact indexed filesystem path.
    ///
    /// This is deliberately independent of entry names: multiple configured
    /// roots may share the same basename, and display labels may differ from
    /// their on-disk names.
    pub fn path_for_id(&self, id: EntryId) -> Option<PathBuf> {
        self.id_to_path
            .get(&id)
            .map(|path| path.value().to_path_buf())
    }

    /// Publish that an authoritative direct-child listing completed for a
    /// directory, including the important zero-child case.
    ///
    /// Child records and listing completeness are separate facts: resolving a
    /// URI may insert one partial ancestor chain, while listing an empty
    /// directory inserts no records at all. This marker lets snapshots expose
    /// `unknown` versus `loaded empty` without guessing from map membership.
    pub fn mark_directory_children_loaded(&self, id: EntryId) -> Result<bool, FxError> {
        Ok(!self.mark_directories_children_loaded(&[id])?.is_empty())
    }

    /// Batch form of `mark_directory_children_loaded`. One bounded/full walk
    /// can complete thousands of directories, so publishing them in a single
    /// immutable snapshot avoids an O(n²) clone cascade.
    pub fn mark_directories_children_loaded(
        &self,
        ids: &[EntryId],
    ) -> Result<Vec<EntryId>, FxError> {
        let _guard = self.write_lock.lock();
        let current = self.inner.load_full();
        let mut newly_loaded = Vec::new();
        let mut seen = HashSet::with_capacity(ids.len());
        for &id in ids {
            let entry = current
                .entries
                .get(&id)
                .ok_or_else(|| FxError::InvalidInput(format!("entry {:?} not found", id)))?;
            if entry.kind != EntryKind::Directory && entry.symlink_target_is_dir != Some(true) {
                return Err(FxError::InvalidInput(format!(
                    "entry {:?} is not a directory",
                    id
                )));
            }
            if !current.loaded_directories.contains(&id) && seen.insert(id) {
                newly_loaded.push(id);
            }
        }
        if newly_loaded.is_empty() {
            return Ok(Vec::new());
        }

        let mut next = (*current).clone();
        for &id in &newly_loaded {
            next.loaded_directories.insert(id);
            let count = next.children.get(&id).map_or(0, |children| children.len());
            next.direct_child_counts
                .insert(id, count.min(u32::MAX as usize) as u32);
        }
        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));
        self.record_mutation(prev_tree_version, new_tree_version, |changes| {
            changes
                .child_set_changed
                .extend(newly_loaded.iter().copied());
        });
        Ok(newly_loaded)
    }

    /// Atomically replace the display order of the current workspace roots.
    ///
    /// `roots` must be an exact permutation of the published root ids. Root
    /// identity, paths, entries, and child order are left untouched. Passing
    /// the current order is a no-op and does not advance the tree version.
    pub fn reorder_roots(&self, roots: &[EntryId]) -> Result<u64, FxError> {
        let _guard = self.write_lock.lock();
        let current = self.inner.load_full();

        if roots.len() != current.roots.len() {
            return Err(FxError::InvalidInput(format!(
                "root order must contain exactly {} ids",
                current.roots.len()
            )));
        }
        let requested: HashSet<EntryId> = roots.iter().copied().collect();
        let existing: HashSet<EntryId> = current.roots.iter().copied().collect();
        if requested.len() != roots.len() || requested != existing {
            return Err(FxError::InvalidInput(
                "root order must be an exact permutation of current root ids".into(),
            ));
        }
        if roots == current.roots.as_slice() {
            return Ok(current.tree_version);
        }

        let mut next = (*current).clone();
        next.roots.clear();
        next.roots.extend_from_slice(roots);
        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));
        self.record_mutation(prev_tree_version, new_tree_version, |changes| {
            // Root rows themselves are unchanged, but marking their ids gives
            // the coalescer a bounded structural signal. The host separately
            // compares the ordered root vector and includes it in the delta.
            changes.changed_ids.extend(roots.iter().copied());
        });
        Ok(new_tree_version)
    }

    /// Atomically replace the configured workspace-root set.
    ///
    /// Every supplied entry must describe a disjoint root path with no parent.
    /// Existing root paths retain their identity and known descendants; new
    /// paths receive fresh identities; removed roots lose their complete known
    /// subtrees and both path-index directions in the same publication.
    ///
    /// Returns `(tree_version, added_root_ids, removed_entry_ids)`.
    pub fn replace_roots(
        &self,
        roots: Vec<(PathBuf, Entry)>,
    ) -> Result<(u64, Vec<EntryId>, Vec<EntryId>), FxError> {
        let _guard = self.write_lock.lock();
        let visibility = *self.visibility.read();
        let current = self.inner.load_full();

        let mut unique_paths = HashSet::with_capacity(roots.len());
        for (path, entry) in &roots {
            if !path.is_absolute() {
                return Err(FxError::InvalidInput(format!(
                    "workspace root must be absolute: {path:?}"
                )));
            }
            if !unique_paths.insert(path.clone()) {
                return Err(FxError::InvalidInput(format!(
                    "duplicate workspace root: {path:?}"
                )));
            }
            if entry.parent_id.is_some() {
                return Err(FxError::InvalidInput(
                    "workspace root entries cannot have a parent".into(),
                ));
            }
        }
        for (index, (path, _)) in roots.iter().enumerate() {
            if roots.iter().skip(index + 1).any(|(other, _)| {
                path.starts_with(other.as_path()) || other.starts_with(path.as_path())
            }) {
                return Err(FxError::InvalidInput(
                    "workspace roots cannot overlap".into(),
                ));
            }
        }

        let mut ordered_ids = Vec::with_capacity(roots.len());
        let mut additions: Vec<(PathBuf, EntryId, Arc<Entry>)> = Vec::new();
        for (path, mut entry) in roots {
            if let Some(existing) = self.path_to_id.get(path.as_path()) {
                let id = *existing.value();
                drop(existing);
                let indexed = current.entries.get(&id).ok_or_else(|| {
                    FxError::InternalBug(format!("path index references missing entry {id:?}"))
                })?;
                if indexed.parent_id.is_some() || !current.roots.contains(&id) {
                    return Err(FxError::InvalidInput(format!(
                        "workspace root overlaps an indexed descendant: {path:?}"
                    )));
                }
                ordered_ids.push(id);
                continue;
            }

            let id = EntryId::alloc_from(&self.id_counter)?;
            entry.id = id;
            entry.parent_id = None;
            ordered_ids.push(id);
            additions.push((path, id, Arc::new(entry)));
        }

        let retained: HashSet<EntryId> = ordered_ids.iter().copied().collect();
        let removed_roots: Vec<EntryId> = current
            .roots
            .iter()
            .filter(|id| !retained.contains(id))
            .copied()
            .collect();
        if additions.is_empty()
            && removed_roots.is_empty()
            && ordered_ids.as_slice() == current.roots.as_slice()
        {
            return Ok((current.tree_version, Vec::new(), Vec::new()));
        }

        let mut removed_ids = Vec::new();
        for root_id in removed_roots {
            let mut stack = vec![root_id];
            while let Some(id) = stack.pop() {
                removed_ids.push(id);
                if let Some(children) = current.children.get(&id) {
                    stack.extend(children.iter().copied());
                }
            }
        }
        let removed_set: HashSet<EntryId> = removed_ids.iter().copied().collect();

        let mut next = (*current).clone();
        for id in &removed_ids {
            next.entries.remove(id);
            next.children.remove(id);
            next.loaded_directories.remove(id);
            next.direct_child_counts.remove(id);
            next.descendant_visible_counts.remove(id);
            next.descendant_total_sizes.remove(id);
        }
        for (_, id, entry) in &additions {
            next.entries.insert(*id, Arc::clone(entry));
            next.descendant_visible_counts
                .insert(*id, visibility.includes(entry.as_ref()) as u32);
            next.descendant_total_sizes.insert(*id, entry.size);
        }
        next.roots.clear();
        next.roots.extend_from_slice(&ordered_ids);

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));

        for id in &removed_ids {
            if let Some((_, path)) = self.id_to_path.remove(id) {
                self.path_to_id.remove(path.as_ref());
            }
        }
        for (path, id, _) in &additions {
            let indexed_path: Arc<Path> = Arc::from(path.clone().into_boxed_path());
            self.path_to_id.insert(Arc::clone(&indexed_path), *id);
            self.id_to_path.insert(*id, indexed_path);
        }

        let added_ids: Vec<EntryId> = additions.iter().map(|(_, id, _)| *id).collect();
        self.record_mutation(prev_tree_version, new_tree_version, |changes| {
            changes.changed_ids.extend(removed_set);
            // Root order/membership is structural even when every entry record
            // is retained. Marking all current roots keeps the coalescer awake;
            // the host sends the authoritative ordered root vector separately.
            changes.changed_ids.extend(ordered_ids.iter().copied());
        });

        Ok((new_tree_version, added_ids, removed_ids))
    }

    /// Keep a configured root visible while atomically evicting its stale
    /// descendants after the directory becomes unavailable.
    ///
    /// The root id, path index, and root order are preserved. Repeating the
    /// transition is a version-free no-op.
    pub fn mark_root_unavailable(&self, root_id: EntryId) -> Result<(u64, Vec<EntryId>), FxError> {
        let _guard = self.write_lock.lock();
        let visibility = *self.visibility.read();
        let current = self.inner.load_full();
        let root = current
            .entries
            .get(&root_id)
            .cloned()
            .ok_or_else(|| FxError::InvalidInput(format!("root {:?} not found", root_id)))?;
        if root.parent_id.is_some() || !current.roots.contains(&root_id) {
            return Err(FxError::InvalidInput(format!(
                "entry {:?} is not a workspace root",
                root_id
            )));
        }

        let mut removed_ids = Vec::new();
        let mut stack = current
            .children
            .get(&root_id)
            .map(|children| children.to_vec())
            .unwrap_or_default();
        while let Some(id) = stack.pop() {
            removed_ids.push(id);
            if let Some(children) = current.children.get(&id) {
                stack.extend(children.iter().copied());
            }
        }
        if root.kind == EntryKind::Unavailable && removed_ids.is_empty() {
            return Ok((current.tree_version, Vec::new()));
        }

        let mut unavailable = (*root).clone();
        unavailable.kind = EntryKind::Unavailable;
        unavailable.size = 0;
        unavailable.symlink_target_is_dir = None;
        unavailable.path_segments = None;
        unavailable.is_readonly = true;

        let mut next = (*current).clone();
        for id in &removed_ids {
            next.entries.remove(id);
            next.children.remove(id);
            next.loaded_directories.remove(id);
            next.direct_child_counts.remove(id);
            next.descendant_visible_counts.remove(id);
            next.descendant_total_sizes.remove(id);
        }
        next.entries.insert(root_id, Arc::new(unavailable.clone()));
        next.children.remove(&root_id);
        next.loaded_directories.remove(&root_id);
        next.direct_child_counts.insert(root_id, 0);
        next.descendant_visible_counts
            .insert(root_id, visibility.includes(&unavailable) as u32);
        next.descendant_total_sizes.insert(root_id, 0);

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));

        for id in &removed_ids {
            if let Some((_, path)) = self.id_to_path.remove(id) {
                self.path_to_id.remove(path.as_ref());
            }
        }

        self.record_mutation(prev_tree_version, new_tree_version, |changes| {
            changes.changed_ids.insert(root_id);
            changes.changed_ids.extend(removed_ids.iter().copied());
            changes.child_set_changed.insert(root_id);
        });
        Ok((new_tree_version, removed_ids))
    }

    /// Snapshot the path index below `root`. The returned paths include
    /// `root` itself when it is known. Watch reconciliation uses this to
    /// compare a bounded disk walk with the authoritative in-memory view.
    pub fn paths_under(&self, root: &Path) -> Vec<(PathBuf, EntryId)> {
        self.path_to_id
            .iter()
            .filter(|kv| kv.key().as_ref() == root || kv.key().starts_with(root))
            .map(|kv| (kv.key().to_path_buf(), *kv.value()))
            .collect()
    }

    /// Replace one entry record while preserving its id and tree position.
    /// Parent/name changes remain the responsibility of rename/reparent;
    /// watcher reconciliation uses this for size, timestamp, permissions,
    /// ignore, symlink-target, and kind changes at a stable path.
    pub fn update(&self, id: EntryId, mut updated: Entry) -> Result<bool, FxError> {
        let _guard = self.write_lock.lock();
        let sibling_order = self.sibling_order.read().clone();
        let visibility = *self.visibility.read();
        let current = self.inner.load_full();
        let existing = current
            .entries
            .get(&id)
            .cloned()
            .ok_or_else(|| FxError::InvalidInput(format!("entry {:?} not found", id)))?;

        if updated.parent_id != existing.parent_id || updated.name != existing.name {
            return Err(FxError::InvalidInput(
                "EntryStore::update cannot rename or reparent an entry".into(),
            ));
        }
        updated.id = id;
        if updated == *existing {
            return Ok(false);
        }

        let old_visible = visibility.includes(&existing) as i64;
        let new_visible = visibility.includes(&updated) as i64;
        let visible_delta = new_visible - old_visible;
        let size_delta = updated.size as i128 - existing.size as i128;

        let mut next = (*current).clone();
        next.entries.insert(id, Arc::new(updated));

        let mut summary_changed_ancestors = Vec::new();
        let mut cur = Some(id);
        let mut hops = 0usize;
        while let Some(entry_id) = cur {
            if hops >= MAX_ANCESTOR_WALK {
                break;
            }
            if visible_delta != 0 {
                let slot = next.descendant_visible_counts.entry(entry_id).or_insert(0);
                if visible_delta > 0 {
                    *slot = slot.saturating_add(visible_delta as u32);
                } else {
                    *slot = slot.saturating_sub((-visible_delta) as u32);
                }
            }
            if size_delta != 0 {
                let slot = next.descendant_total_sizes.entry(entry_id).or_insert(0);
                if size_delta > 0 {
                    *slot = slot.saturating_add(size_delta as u64);
                } else {
                    *slot = slot.saturating_sub((-size_delta) as u64);
                }
            }
            if entry_id != id && (visible_delta != 0 || size_delta != 0) {
                summary_changed_ancestors.push(entry_id);
            }
            cur = next.entries.get(&entry_id).and_then(|e| e.parent_id);
            hops += 1;
        }

        // A file can be atomically replaced by a directory (or vice versa)
        // at the same path. Re-sort the stable id inside its sibling list.
        if existing.kind != next.entries[&id].kind
            || existing.symlink_target_is_dir != next.entries[&id].symlink_target_is_dir
            || (sibling_order.sort_by == crate::sort::SortBy::Modified
                && existing.mtime_ms != next.entries[&id].mtime_ms)
        {
            if let Some(parent_id) = existing.parent_id {
                let mut siblings = next.children.remove(&parent_id).unwrap_or_default();
                siblings.sort_by(|a, b| {
                    let ae = next.entries.get(a);
                    let be = next.entries.get(b);
                    match (ae, be) {
                        (Some(ae), Some(be)) => sibling_order.compare(ae, be),
                        _ => a.cmp(b),
                    }
                });
                next.children.insert(parent_id, siblings);
            }
        }

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));
        self.record_mutation(prev_tree_version, new_tree_version, |cs| {
            cs.changed_ids.insert(id);
            cs.subtree_roots_changed
                .extend(summary_changed_ancestors.iter().copied());
        });
        Ok(true)
    }

    pub fn insert(&self, path: PathBuf, mut entry: Entry) -> Result<EntryId, FxError> {
        let _guard = self.write_lock.lock();
        let sibling_order = self.sibling_order.read().clone();
        let visibility = *self.visibility.read();
        // Watch callbacks and an in-flight walker can discover the same path
        // concurrently. Path identity wins: returning the existing id keeps
        // insertion idempotent and prevents duplicate rows/self-echoes.
        if let Some(existing) = self.path_to_id.get(path.as_path()) {
            return Ok(*existing.value());
        }
        let id = EntryId::alloc_from(&self.id_counter)?;
        entry.id = id;
        let parent = entry.parent_id;
        let size = entry.size;

        let current = self.inner.load_full();
        let counts_visible = visibility.includes(&entry);
        let mut next = (*current).clone();
        next.entries.insert(id, Arc::new(entry));

        match parent {
            None => next.roots.push(id),
            Some(parent_id) => {
                let siblings = next.children.entry(parent_id).or_default();
                // IDE-style order: directories first, then files, each group
                // A→Z by name (JetBrains / VS Code Project view).
                let pos = siblings
                    .binary_search_by(|&sib| {
                        let sib_e = next.entries.get(&sib);
                        match (sib_e, next.entries.get(&id)) {
                            (Some(sib_e), Some(entry)) => sibling_order.compare(sib_e, entry),
                            _ => sib.cmp(&id),
                        }
                    })
                    .unwrap_or_else(|e| e);
                siblings.insert(pos, id);
                *next.direct_child_counts.entry(parent_id).or_insert(0) += 1;
            }
        }

        // Seed the new entry's own summaries (inclusive of self).
        next.descendant_visible_counts
            .insert(id, counts_visible as u32);
        next.descendant_total_sizes.insert(id, size);

        // Walk ancestors, bumping each summary. Depth-capped at MAX_ANCESTOR_WALK
        // so a malformed parent_id cycle can't spin us forever. Record an
        // ancestor in the ChangeSet only when its summary actually moves
        // (visible-count bump or non-zero size add); otherwise a hidden
        // zero-byte entry would spuriously flag every ancestor.
        let mut summary_changed_ancestors: Vec<EntryId> = Vec::new();
        let mut cur = parent;
        let mut hops: usize = 0;
        while let Some(a) = cur {
            if hops >= MAX_ANCESTOR_WALK {
                break;
            }
            let mut touched = false;
            if counts_visible {
                *next.descendant_visible_counts.entry(a).or_insert(0) += 1;
                touched = true;
            }
            if size > 0 {
                *next.descendant_total_sizes.entry(a).or_insert(0) += size;
                touched = true;
            } else {
                // Keep the slot materialized for consistency with prior behavior,
                // even when size delta is zero.
                next.descendant_total_sizes.entry(a).or_insert(0);
            }
            if touched {
                summary_changed_ancestors.push(a);
            }
            cur = next.entries.get(&a).and_then(|e| e.parent_id);
            hops += 1;
        }

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));

        let indexed_path: Arc<Path> = Arc::from(path.into_boxed_path());
        self.path_to_id.insert(Arc::clone(&indexed_path), id);
        self.id_to_path.insert(id, indexed_path);

        // Record the mutation in the pending ChangeSet for Phase 7 delta diff.
        self.record_mutation(prev_tree_version, new_tree_version, |cs| {
            cs.changed_ids.insert(id);
            for a in &summary_changed_ancestors {
                cs.subtree_roots_changed.insert(*a);
            }
            // Parent's direct-children list just grew.
            if let Some(parent_id) = parent {
                cs.child_set_changed.insert(parent_id);
            }
        });
        Ok(id)
    }

    pub fn remove(&self, id: EntryId) -> Option<Arc<Entry>> {
        let _guard = self.write_lock.lock();
        let visibility = *self.visibility.read();
        let current = self.inner.load_full();
        let existing = current.entries.get(&id)?.clone();

        let mut next = (*current).clone();

        // Self-only contribution; the cache slot holds subtree totals, which
        // would double-count when descendants' own cache entries are still live.
        let vis_contribution = if visibility.includes(&existing) {
            1u32
        } else {
            0
        };
        let size_contribution = existing.size;
        let parent_for_walk = existing.parent_id;

        next.entries.remove(&id)?;
        next.loaded_directories.remove(&id);
        next.descendant_visible_counts.remove(&id);
        next.descendant_total_sizes.remove(&id);

        match existing.parent_id {
            None => {
                if let Some(pos) = next.roots.iter().position(|&r| r == id) {
                    next.roots.remove(pos);
                }
            }
            Some(parent_id) => {
                if let Some(siblings) = next.children.get_mut(&parent_id) {
                    if let Some(pos) = siblings.iter().position(|&c| c == id) {
                        siblings.remove(pos);
                    }
                }
                if let Some(count) = next.direct_child_counts.get_mut(&parent_id) {
                    *count = count.saturating_sub(1);
                }
            }
        }

        // Ancestor-walk mirrors insert. saturating_sub defends against malformed
        // data where the cache somehow drifted below the contribution. Only
        // flag an ancestor as a changed subtree root when its summary actually
        // moves (mirror of the insert-side tightening).
        let mut summary_changed_ancestors: Vec<EntryId> = Vec::new();
        let mut cur = parent_for_walk;
        let mut hops: usize = 0;
        while let Some(a) = cur {
            if hops >= MAX_ANCESTOR_WALK {
                break;
            }
            let mut touched = false;
            if vis_contribution > 0 {
                if let Some(c) = next.descendant_visible_counts.get_mut(&a) {
                    *c = c.saturating_sub(vis_contribution);
                }
                touched = true;
            }
            if size_contribution > 0 {
                if let Some(s) = next.descendant_total_sizes.get_mut(&a) {
                    *s = s.saturating_sub(size_contribution);
                }
                touched = true;
            }
            if touched {
                summary_changed_ancestors.push(a);
            }
            cur = next.entries.get(&a).and_then(|e| e.parent_id);
            hops += 1;
        }

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));

        if let Some((_, path)) = self.id_to_path.remove(&id) {
            self.path_to_id.remove(path.as_ref());
        }

        // Record the mutation in the pending ChangeSet for Phase 7 delta diff.
        self.record_mutation(prev_tree_version, new_tree_version, |cs| {
            cs.changed_ids.insert(id);
            for a in &summary_changed_ancestors {
                cs.subtree_roots_changed.insert(*a);
            }
            // Parent's direct-children list just shrank.
            if let Some(parent_id) = parent_for_walk {
                cs.child_set_changed.insert(parent_id);
            }
        });

        Some(existing)
    }

    /// Atomically remove an entry and every known descendant. Unlike the
    /// original single-entry `remove`, this cannot leave dangling children or
    /// stale path-index records and is therefore the deletion primitive used
    /// by watcher reconciliation and recursive mutations.
    pub fn remove_subtree(&self, id: EntryId) -> Vec<Arc<Entry>> {
        let _guard = self.write_lock.lock();
        let current = self.inner.load_full();
        let Some(root_entry) = current.entries.get(&id).cloned() else {
            return Vec::new();
        };

        let mut stack = vec![id];
        let mut ids = Vec::new();
        while let Some(next_id) = stack.pop() {
            ids.push(next_id);
            if let Some(children) = current.children.get(&next_id) {
                stack.extend(children.iter().copied());
            }
        }
        let id_set: HashSet<EntryId> = ids.iter().copied().collect();
        let removed: Vec<Arc<Entry>> = ids
            .iter()
            .filter_map(|entry_id| current.entries.get(entry_id).cloned())
            .collect();

        let visible_contribution = current
            .descendant_visible_counts
            .get(&id)
            .copied()
            .unwrap_or(0);
        let size_contribution = current
            .descendant_total_sizes
            .get(&id)
            .copied()
            .unwrap_or(0);
        let parent_for_walk = root_entry.parent_id;

        let mut next = (*current).clone();
        for entry_id in &ids {
            next.entries.remove(entry_id);
            next.children.remove(entry_id);
            next.loaded_directories.remove(entry_id);
            next.direct_child_counts.remove(entry_id);
            next.descendant_visible_counts.remove(entry_id);
            next.descendant_total_sizes.remove(entry_id);
            if let Some(pos) = next.roots.iter().position(|root| root == entry_id) {
                next.roots.remove(pos);
            }
        }
        if let Some(parent_id) = parent_for_walk {
            if let Some(siblings) = next.children.get_mut(&parent_id) {
                siblings.retain(|child| *child != id);
            }
            if let Some(count) = next.direct_child_counts.get_mut(&parent_id) {
                *count = count.saturating_sub(1);
            }
        }

        let mut summary_changed_ancestors = Vec::new();
        let mut cur = parent_for_walk;
        let mut hops = 0usize;
        while let Some(ancestor) = cur {
            if hops >= MAX_ANCESTOR_WALK {
                break;
            }
            if let Some(count) = next.descendant_visible_counts.get_mut(&ancestor) {
                *count = count.saturating_sub(visible_contribution);
            }
            if let Some(size) = next.descendant_total_sizes.get_mut(&ancestor) {
                *size = size.saturating_sub(size_contribution);
            }
            if visible_contribution != 0 || size_contribution != 0 {
                summary_changed_ancestors.push(ancestor);
            }
            cur = next.entries.get(&ancestor).and_then(|e| e.parent_id);
            hops += 1;
        }

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));

        let paths_to_remove: Vec<PathBuf> = self
            .path_to_id
            .iter()
            .filter(|kv| id_set.contains(kv.value()))
            .map(|kv| kv.key().to_path_buf())
            .collect();
        for path in paths_to_remove {
            self.path_to_id.remove(path.as_path());
        }
        for entry_id in &ids {
            self.id_to_path.remove(entry_id);
        }

        self.record_mutation(prev_tree_version, new_tree_version, |cs| {
            cs.changed_ids.extend(ids.iter().copied());
            cs.subtree_roots_changed
                .extend(summary_changed_ancestors.iter().copied());
            if let Some(parent_id) = parent_for_walk {
                cs.child_set_changed.insert(parent_id);
            }
        });

        removed
    }

    pub fn rename(&self, id: EntryId, new_path: PathBuf) -> Result<(), FxError> {
        let _guard = self.write_lock.lock();
        let sibling_order = self.sibling_order.read().clone();
        let current = self.inner.load_full();
        let entry = current
            .entries
            .get(&id)
            .cloned()
            .ok_or_else(|| FxError::InvalidInput(format!("entry {:?} not found", id)))?;

        let old_path = self
            .path_for_id(id)
            .ok_or_else(|| FxError::InternalBug(format!("entry {:?} has no indexed path", id)))?;
        if old_path == new_path {
            return Ok(());
        }
        let new_parent_path = new_path
            .parent()
            .ok_or_else(|| FxError::InvalidInput("new_path has no parent".into()))?;
        let same_parent_path = old_path.parent() == Some(new_parent_path);
        let new_parent_id = if same_parent_path {
            entry.parent_id
        } else {
            Some(
                self.get_by_path(new_parent_path)
                    .map(|parent| parent.id)
                    .ok_or_else(|| {
                        FxError::InvalidInput("destination parent is not indexed".into())
                    })?,
            )
        };
        if new_parent_id.is_some_and(|parent_id| {
            current.get(parent_id).is_none_or(|parent| {
                parent.kind != EntryKind::Directory && parent.symlink_target_is_dir != Some(true)
            })
        }) {
            return Err(FxError::InvalidInput(
                "destination parent is not a directory".into(),
            ));
        }
        if entry.parent_id.is_none() && !same_parent_path {
            return Err(FxError::InvalidInput(
                "workspace roots cannot be moved".into(),
            ));
        }
        let old_parent_id = entry.parent_id;
        let reparenting = old_parent_id != new_parent_id;
        if reparenting {
            let mut cursor = new_parent_id;
            let mut hops = 0usize;
            while let Some(candidate) = cursor {
                if candidate == id {
                    return Err(FxError::InvalidInput(
                        "cannot move an entry into its own subtree".into(),
                    ));
                }
                if hops >= MAX_ANCESTOR_WALK {
                    return Err(FxError::InvalidInput(
                        "destination parent chain is cyclic or too deep".into(),
                    ));
                }
                cursor = current
                    .entries
                    .get(&candidate)
                    .and_then(|item| item.parent_id);
                hops += 1;
            }
        }

        // Capture every known descendant mapping before publishing the renamed
        // entry. Entries store parent ids + basenames rather than full paths,
        // so only the renamed root record changes; descendant ids and records
        // remain stable while their reverse-index prefixes move atomically as a
        // group. This is O(known subtree), not O(on-disk subtree).
        let path_updates: Vec<(PathBuf, EntryId, PathBuf)> = if entry.kind == EntryKind::Directory {
            self.path_to_id
                .iter()
                .filter_map(|kv| {
                    let path = kv.key().as_ref();
                    if path != old_path && !path.starts_with(&old_path) {
                        return None;
                    }
                    let relative = path.strip_prefix(&old_path).ok()?;
                    Some((path.to_path_buf(), *kv.value(), new_path.join(relative)))
                })
                .collect()
        } else {
            vec![(old_path.clone(), id, new_path.clone())]
        };
        let moving_ids: HashSet<EntryId> = path_updates
            .iter()
            .map(|(_, entry_id, _)| *entry_id)
            .collect();
        for (_, _, destination) in &path_updates {
            if let Some(existing) = self.path_to_id.get(destination.as_path()) {
                if !moving_ids.contains(existing.value()) {
                    return Err(FxError::InvalidInput(format!(
                        "destination path is already indexed: {destination:?}"
                    )));
                }
            }
        }

        let new_name = new_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| FxError::InvalidInput("new_path has no valid UTF-8 filename".into()))?
            .to_string();

        let mut next = (*current).clone();

        // Clone the Entry, update name/parent, and reinsert the Arc.
        let mut updated = (*entry).clone();
        updated.name = new_name.clone();
        updated.parent_id = new_parent_id;
        next.entries.insert(id, Arc::new(updated));

        if reparenting {
            let new_parent_id = new_parent_id.expect("reparent destination has a parent id");
            if let Some(old_parent_id) = old_parent_id {
                if let Some(siblings) = next.children.get_mut(&old_parent_id) {
                    siblings.retain(|child| *child != id);
                }
                if let Some(count) = next.direct_child_counts.get_mut(&old_parent_id) {
                    *count = count.saturating_sub(1);
                }
            }
            let mut siblings = next.children.remove(&new_parent_id).unwrap_or_default();
            let insert_pos = siblings
                .binary_search_by(|&sibling| {
                    let sibling_entry = next.entries.get(&sibling);
                    match (sibling_entry, next.entries.get(&id)) {
                        (Some(sibling_entry), Some(updated)) => {
                            sibling_order.compare(sibling_entry, updated)
                        }
                        _ => sibling.cmp(&id),
                    }
                })
                .unwrap_or_else(|position| position);
            siblings.insert(insert_pos, id);
            next.children.insert(new_parent_id, siblings);
            *next.direct_child_counts.entry(new_parent_id).or_insert(0) += 1;

            let visible = current
                .descendant_visible_counts
                .get(&id)
                .copied()
                .unwrap_or(0);
            let size = current
                .descendant_total_sizes
                .get(&id)
                .copied()
                .unwrap_or(0);
            let mut cursor = old_parent_id;
            let mut hops = 0usize;
            while let Some(ancestor) = cursor {
                if hops >= MAX_ANCESTOR_WALK {
                    break;
                }
                if let Some(count) = next.descendant_visible_counts.get_mut(&ancestor) {
                    *count = count.saturating_sub(visible);
                }
                if let Some(total) = next.descendant_total_sizes.get_mut(&ancestor) {
                    *total = total.saturating_sub(size);
                }
                cursor = next.entries.get(&ancestor).and_then(|item| item.parent_id);
                hops += 1;
            }
            let mut cursor = Some(new_parent_id);
            let mut hops = 0usize;
            while let Some(ancestor) = cursor {
                if hops >= MAX_ANCESTOR_WALK {
                    break;
                }
                *next.descendant_visible_counts.entry(ancestor).or_insert(0) += visible;
                *next.descendant_total_sizes.entry(ancestor).or_insert(0) += size;
                cursor = next.entries.get(&ancestor).and_then(|item| item.parent_id);
                hops += 1;
            }
        } else if let Some(parent_id) = old_parent_id {
            // Same-parent rename: re-sort in place.
            if let Some(siblings) = next.children.get_mut(&parent_id) {
                if let Some(pos) = siblings.iter().position(|&c| c == id) {
                    siblings.remove(pos);
                }
                // Rank from the updated entry (name changed, kind unchanged).
                let insert_pos = siblings
                    .binary_search_by(|&sib| {
                        let sib_e = next.entries.get(&sib);
                        match (sib_e, next.entries.get(&id)) {
                            (Some(sib_e), Some(updated)) => sibling_order.compare(sib_e, updated),
                            _ => sib.cmp(&id),
                        }
                    })
                    .unwrap_or_else(|e| e);
                siblings.insert(insert_pos, id);
            }
        }

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        self.inner.store(Arc::new(next));

        // path_to_id update trails snapshot publish, matching insert()'s
        // ordering. Remove the complete old prefix before installing the new
        // one so no stale descendant aliases survive a directory rename.
        for (old, _, _) in &path_updates {
            self.path_to_id.remove(old.as_path());
        }
        for (_, entry_id, new) in path_updates {
            let indexed_path: Arc<Path> = Arc::from(new.into_boxed_path());
            self.path_to_id.insert(Arc::clone(&indexed_path), entry_id);
            self.id_to_path.insert(entry_id, indexed_path);
        }

        // Same-parent rename: parent unchanged, so reparented_ids stays empty.
        self.record_mutation(prev_tree_version, new_tree_version, |cs| {
            cs.changed_ids.insert(id);
            if reparenting {
                cs.reparented_ids.insert(id, (old_parent_id, new_parent_id));
                if let Some(old_parent_id) = old_parent_id {
                    cs.child_set_changed.insert(old_parent_id);
                    cs.subtree_roots_changed.insert(old_parent_id);
                }
                if let Some(new_parent_id) = new_parent_id {
                    cs.child_set_changed.insert(new_parent_id);
                    cs.subtree_roots_changed.insert(new_parent_id);
                }
            } else if let Some(parent_id) = old_parent_id {
                cs.child_set_changed.insert(parent_id);
            }
        });

        Ok(())
    }

    /// Atomically replace every display-only projection policy.
    ///
    /// Entry identities, paths, metadata, and the retained prior snapshot stay
    /// untouched. The newly-published snapshot re-sorts sibling lists and
    /// rebuilds visibility summaries before its version becomes observable.
    pub fn reconfigure_projection(
        &self,
        sibling_order: SiblingOrder,
        visibility: VisibilityPolicy,
        compact_folders: bool,
        file_nesting: FileNestingPolicy,
    ) -> u64 {
        self.reconfigure_projection_inner::<fn(&Path, &Entry) -> bool>(
            sibling_order,
            visibility,
            compact_folders,
            file_nesting,
            None,
        )
    }

    /// Reclassify only configured excludes while retaining every other
    /// projection policy. This is used after mutations whose destination path
    /// may enter or leave an exclude pattern.
    pub fn reconfigure_exclusions<F>(&self, classify_excluded: F) -> u64
    where
        F: Fn(&Path, &Entry) -> bool,
    {
        let snapshot = self.inner.load_full();
        let sibling_order = self.sibling_order.read().clone();
        self.reconfigure_projection_with_exclusions(
            sibling_order,
            snapshot.visibility,
            snapshot.compact_folders,
            snapshot.file_nesting.clone(),
            classify_excluded,
        )
    }

    /// Atomically replace display policies and reclassify configured excludes.
    ///
    /// `classify_excluded` runs once for every indexed path while writers are
    /// serialized. Repository-ignore provenance is retained independently on
    /// each entry, so adding and removing exclude globs is reversible.
    pub fn reconfigure_projection_with_exclusions<F>(
        &self,
        sibling_order: SiblingOrder,
        visibility: VisibilityPolicy,
        compact_folders: bool,
        file_nesting: FileNestingPolicy,
        classify_excluded: F,
    ) -> u64
    where
        F: Fn(&Path, &Entry) -> bool,
    {
        self.reconfigure_projection_inner(
            sibling_order,
            visibility,
            compact_folders,
            file_nesting,
            Some(classify_excluded),
        )
    }

    fn reconfigure_projection_inner<F>(
        &self,
        sibling_order: SiblingOrder,
        visibility: VisibilityPolicy,
        compact_folders: bool,
        file_nesting: FileNestingPolicy,
        classify_excluded: Option<F>,
    ) -> u64
    where
        F: Fn(&Path, &Entry) -> bool,
    {
        let _guard = self.write_lock.lock();
        let current = self.inner.load_full();
        let policy_changed = self.sibling_order.read().ne(&sibling_order)
            || current.visibility != visibility
            || current.compact_folders != compact_folders
            || current.file_nesting != file_nesting;
        if !policy_changed && classify_excluded.is_none() {
            return current.tree_version;
        }

        let mut next = (*current).clone();
        let mut exclusion_changed = Vec::new();
        if let Some(classify_excluded) = classify_excluded {
            for path_and_id in self.path_to_id.iter() {
                let id = *path_and_id.value();
                let Some(existing) = next.entries.get(&id) else {
                    continue;
                };
                let excluded = classify_excluded(path_and_id.key().as_ref(), existing);
                if existing.is_excluded == excluded {
                    continue;
                }
                let mut updated = (**existing).clone();
                updated.is_excluded = excluded;
                next.entries.insert(id, Arc::new(updated));
                exclusion_changed.push(id);
            }
        }
        if !policy_changed && exclusion_changed.is_empty() {
            return current.tree_version;
        }

        next.visibility = visibility;
        next.compact_folders = compact_folders;
        next.file_nesting = file_nesting;

        let entries = &next.entries;
        for siblings in next.children.values_mut() {
            siblings.sort_by(
                |left, right| match (entries.get(left), entries.get(right)) {
                    (Some(left), Some(right)) => sibling_order.compare(left, right),
                    _ => left.cmp(right),
                },
            );
        }

        next.descendant_visible_counts.clear();
        for (id, entry) in &next.entries {
            next.descendant_visible_counts
                .insert(*id, visibility.includes(entry) as u32);
        }
        let visible_ids: Vec<EntryId> = next
            .entries
            .iter()
            .filter_map(|(id, entry)| visibility.includes(entry).then_some(*id))
            .collect();
        for id in visible_ids {
            let mut cursor = next.entries.get(&id).and_then(|entry| entry.parent_id);
            let mut hops = 0usize;
            while let Some(ancestor) = cursor {
                if hops >= MAX_ANCESTOR_WALK {
                    break;
                }
                *next.descendant_visible_counts.entry(ancestor).or_insert(0) += 1;
                cursor = next
                    .entries
                    .get(&ancestor)
                    .and_then(|entry| entry.parent_id);
                hops += 1;
            }
        }

        let prev_tree_version = next.tree_version;
        next.tree_version += 1;
        let new_tree_version = next.tree_version;
        let changed_parents: Vec<EntryId> = next.children.keys().copied().collect();
        let changed_roots: Vec<EntryId> = next.roots.iter().copied().collect();
        *self.sibling_order.write() = sibling_order;
        *self.visibility.write() = visibility;
        self.inner.store(Arc::new(next));
        self.record_mutation(prev_tree_version, new_tree_version, |changes| {
            changes.projection_changed = true;
            changes.changed_ids.extend(exclusion_changed);
            changes.child_set_changed.extend(changed_parents);
            changes.subtree_roots_changed.extend(changed_roots);
        });
        new_tree_version
    }

    /// Push a mutation into the pending ChangeSet. Stamps `from_version` on
    /// the first accumulation after a drain, then keeps `to_version` current.
    fn record_mutation<F>(&self, prev_tree_version: u64, new_tree_version: u64, f: F)
    where
        F: FnOnce(&mut ChangeSet),
    {
        let mut cs = self.pending_changes.lock();
        if cs.is_empty() && cs.from_version == 0 && cs.to_version == 0 {
            cs.from_version = prev_tree_version;
        }
        cs.to_version = new_tree_version;
        f(&mut cs);
    }

    /// Drain the pending ChangeSet, returning what accumulated since the last
    /// call (or since construction). After this call, the internal ChangeSet
    /// is empty and ready to accumulate again.
    pub fn take_pending_changes(&self) -> ChangeSet {
        let mut cs = self.pending_changes.lock();
        std::mem::take(&mut *cs)
    }

    // Test-only: force the id counter near overflow so the cap can be exercised.
    // Kept unconditionally `pub` with `__` sigil so integration tests can reach it.
    #[doc(hidden)]
    pub fn __set_counter_for_tests(&self, value: u64) {
        self.id_counter.store(value, Ordering::Relaxed);
    }
}

impl Default for EntryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryKind};

    /// A workspace-root path that is absolute on the host platform.
    ///
    /// `replace_roots` rejects non-absolute roots, and `Path::is_absolute` is
    /// false for `"/a"` on Windows — it is rooted but carries no drive prefix.
    /// The POSIX literals these tests used could therefore only ever pass on
    /// Unix. Tests that merely `insert` paths do not need this: `insert`
    /// performs no absoluteness check.
    fn abs(relative: &str) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(format!("C:\\{}", relative.replace('/', "\\")))
        }
        #[cfg(not(windows))]
        {
            PathBuf::from(format!("/{relative}"))
        }
    }

    fn leaf(name: &str, parent: Option<EntryId>) -> Entry {
        Entry {
            id: EntryId(0),
            parent_id: parent,
            name: name.into(),
            kind: EntryKind::File,
            size: 0,
            mtime_ms: 0,
            ctime_ms: 0,
            symlink_target_is_dir: None,
            path_segments: None,
            is_ignored: false,
            is_excluded: false,
            is_readonly: false,
            is_hidden: false,
        }
    }

    fn dir(name: &str, parent: Option<EntryId>) -> Entry {
        Entry {
            kind: EntryKind::Directory,
            ..leaf(name, parent)
        }
    }

    #[test]
    fn empty_store_has_version_zero() {
        let s = EntryStore::new();
        assert_eq!(s.tree_version(), 0);
        assert_eq!(s.snapshot().entry_count(), 0);
    }

    #[test]
    fn insert_roundtrip_by_id_and_path() {
        let s = EntryStore::new();
        let id = s.insert("/a".into(), leaf("a", None)).unwrap();
        assert!(s.get_by_id(id).is_some());
        assert_eq!(s.get_by_path(Path::new("/a")).map(|e| e.id), Some(id));
        assert_eq!(s.path_for_id(id), Some(PathBuf::from("/a")));
    }

    #[test]
    fn insert_advances_tree_version() {
        let s = EntryStore::new();
        assert_eq!(s.tree_version(), 0);
        s.insert("/a".into(), leaf("a", None)).unwrap();
        assert_eq!(s.tree_version(), 1);
        s.insert("/b".into(), leaf("b", None)).unwrap();
        assert_eq!(s.tree_version(), 2);
    }

    #[test]
    fn two_inserts_allocate_distinct_ids() {
        let s = EntryStore::new();
        let a = s.insert("/a".into(), leaf("a", None)).unwrap();
        let b = s.insert("/b".into(), leaf("b", None)).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn remove_returns_some_then_none() {
        let s = EntryStore::new();
        let id = s.insert("/a".into(), leaf("a", None)).unwrap();
        assert!(s.remove(id).is_some());
        assert!(s.remove(id).is_none());
        assert!(s.get_by_id(id).is_none());
        assert!(s.get_by_path(Path::new("/a")).is_none());
        assert_eq!(s.path_for_id(id), None);
    }

    #[test]
    fn rename_leaf_updates_path_mapping() {
        let s = EntryStore::new();
        let id = s.insert("/a".into(), leaf("a", None)).unwrap();
        s.rename(id, "/b".into()).unwrap();
        assert!(s.get_by_path(Path::new("/a")).is_none());
        assert_eq!(s.get_by_path(Path::new("/b")).map(|e| e.id), Some(id));
        assert_eq!(s.path_for_id(id), Some(PathBuf::from("/b")));
    }

    #[test]
    fn rename_directory_preserves_descendant_ids_and_path_mappings() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let directory = s.insert("/r/d".into(), dir("d", Some(root))).unwrap();
        let child = s
            .insert("/r/d/child.txt".into(), leaf("child.txt", Some(directory)))
            .unwrap();

        s.rename(directory, "/r/e".into()).unwrap();

        assert!(s.get_by_path(Path::new("/r/d")).is_none());
        assert!(s.get_by_path(Path::new("/r/d/child.txt")).is_none());
        assert_eq!(
            s.get_by_path(Path::new("/r/e")).map(|e| e.id),
            Some(directory)
        );
        assert_eq!(
            s.get_by_path(Path::new("/r/e/child.txt")).map(|e| e.id),
            Some(child)
        );
        assert_eq!(s.path_for_id(directory), Some(PathBuf::from("/r/e")));
        assert_eq!(s.path_for_id(child), Some(PathBuf::from("/r/e/child.txt")));
        assert_eq!(s.get_by_id(directory).unwrap().name, "e");
        assert_eq!(s.get_by_id(child).unwrap().parent_id, Some(directory));
    }

    #[test]
    fn rename_missing_id_is_invalid_input() {
        let s = EntryStore::new();
        let err = s.rename(EntryId(42), "/x".into()).unwrap_err();
        assert_eq!(err.code(), crate::error::ErrorCode::EINVAL);
    }

    #[test]
    fn concurrent_inserts_yield_distinct_ids() {
        use std::collections::HashSet;
        let s = Arc::new(EntryStore::new());
        const THREADS: usize = 4;
        const PER_THREAD: usize = 1000;

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let s = s.clone();
            handles.push(std::thread::spawn(move || {
                let mut ids = Vec::with_capacity(PER_THREAD);
                for i in 0..PER_THREAD {
                    let path: PathBuf = format!("/t{}/f{}", t, i).into();
                    let id = s.insert(path, leaf(&format!("f{}", i), None)).unwrap();
                    ids.push(id);
                }
                ids
            }));
        }

        let mut all = HashSet::new();
        for h in handles {
            for id in h.join().unwrap() {
                assert!(all.insert(id), "duplicate id allocated under contention");
            }
        }
        assert_eq!(all.len(), THREADS * PER_THREAD);
        assert_eq!(s.snapshot().entry_count(), THREADS * PER_THREAD);
    }

    #[test]
    fn insert_root_appears_in_roots_and_is_expandable_when_unwalked() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.roots(), &[root]);
        assert!(snap.children_of(root).is_empty());
        assert_eq!(snap.direct_child_count(root), None);
        // Lazy-expand contract: unvisited directories report has_children so
        // the UI shows a chevron and setExpanded can fire a depth-1 walk.
        assert!(snap.has_children(root));
    }

    #[test]
    fn directory_listing_completion_distinguishes_unknown_partial_and_empty() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();

        // Resolving one path can hydrate a partial child chain. It must not
        // claim that the directory's complete child count is known.
        s.insert("/r/known".into(), leaf("known", Some(root)))
            .unwrap();
        let partial = s.snapshot();
        assert!(!partial.directory_children_loaded(root));
        assert_eq!(partial.projected_child_count(root, false), None);
        assert!(partial.has_children(root));

        assert!(s.mark_directory_children_loaded(root).unwrap());
        let complete = s.snapshot();
        assert!(complete.directory_children_loaded(root));
        assert_eq!(complete.projected_child_count(root, false), Some(1));
        assert!(!s.mark_directory_children_loaded(root).unwrap());

        let empty = s.insert("/empty".into(), dir("empty", None)).unwrap();
        assert!(s.mark_directory_children_loaded(empty).unwrap());
        let complete_empty = s.snapshot();
        assert!(complete_empty.directory_children_loaded(empty));
        assert_eq!(complete_empty.projected_child_count(empty, false), Some(0));
        assert!(!complete_empty.has_children(empty));
    }

    #[test]
    fn reorder_roots_is_atomic_and_preserves_retained_snapshots() {
        let s = EntryStore::new();
        let a = s.insert("/a".into(), dir("a", None)).unwrap();
        let b = s.insert("/b".into(), dir("b", None)).unwrap();
        let c = s.insert("/c".into(), dir("c", None)).unwrap();
        let retained = s.snapshot();
        s.take_pending_changes();
        let previous_version = s.tree_version();

        let version = s.reorder_roots(&[c, a, b]).unwrap();
        assert_eq!(version, previous_version + 1);
        assert_eq!(s.snapshot().roots(), &[c, a, b]);
        assert_eq!(retained.roots(), &[a, b, c]);

        let changes = s.take_pending_changes();
        assert_eq!(changes.from_version, previous_version);
        assert_eq!(changes.to_version, version);
        assert_eq!(changes.changed_ids, HashSet::from([a, b, c]));

        assert_eq!(s.reorder_roots(&[c, a, b]).unwrap(), version);
        assert_eq!(s.tree_version(), version);
        assert!(s.take_pending_changes().is_empty());
    }

    #[test]
    fn reorder_roots_rejects_non_permutations_without_publishing() {
        let s = EntryStore::new();
        let a = s.insert("/a".into(), dir("a", None)).unwrap();
        let b = s.insert("/b".into(), dir("b", None)).unwrap();
        let child = s.insert("/a/child".into(), leaf("child", Some(a))).unwrap();
        s.take_pending_changes();
        let version = s.tree_version();

        for invalid in [&[a][..], &[a, a][..], &[a, child][..]] {
            let err = s.reorder_roots(invalid).unwrap_err();
            assert_eq!(err.code(), crate::error::ErrorCode::EINVAL);
            assert_eq!(s.tree_version(), version);
            assert_eq!(s.snapshot().roots(), &[a, b]);
            assert!(s.take_pending_changes().is_empty());
        }
    }

    #[test]
    fn replace_roots_preserves_retained_ids_and_removes_subtrees_atomically() {
        let s = EntryStore::new();
        let a = s.insert(abs("a"), dir("a", None)).unwrap();
        let child = s.insert(abs("a/child"), leaf("child", Some(a))).unwrap();
        let b = s.insert(abs("b"), dir("b", None)).unwrap();
        let retained = s.snapshot();
        s.take_pending_changes();

        let (version, added, removed) = s
            .replace_roots(vec![(abs("b"), dir("b", None)), (abs("c"), dir("c", None))])
            .unwrap();
        let current = s.snapshot();
        let c = current.roots()[1];
        assert_eq!(current.tree_version(), version);
        assert_eq!(current.roots(), &[b, c]);
        assert_eq!(added, vec![c]);
        assert_eq!(removed, vec![a, child]);
        assert_eq!(s.path_for_id(a), None);
        assert_eq!(s.path_for_id(child), None);
        assert_eq!(s.path_for_id(b), Some(abs("b")));
        assert_eq!(s.path_for_id(c), Some(abs("c")));
        assert_eq!(retained.roots(), &[a, b]);
        assert!(retained.get(child).is_some());

        let changes = s.take_pending_changes();
        assert!(changes.changed_ids.contains(&a));
        assert!(changes.changed_ids.contains(&child));
        assert!(changes.changed_ids.contains(&b));
        assert!(changes.changed_ids.contains(&c));
    }

    #[test]
    fn replace_roots_is_idempotent_and_rejects_ambiguous_paths() {
        let s = EntryStore::new();
        let a = s.insert(abs("a"), dir("a", None)).unwrap();
        let version = s.tree_version();
        s.take_pending_changes();

        assert_eq!(
            s.replace_roots(vec![(abs("a"), dir("a", None))]).unwrap(),
            (version, Vec::new(), Vec::new())
        );
        assert!(s.take_pending_changes().is_empty());

        for invalid in [
            vec![(abs("a"), dir("a", None)), (abs("a"), dir("a", None))],
            vec![
                (abs("a"), dir("a", None)),
                (abs("a/nested"), dir("nested", None)),
            ],
        ] {
            let err = s.replace_roots(invalid).unwrap_err();
            assert_eq!(err.code(), crate::error::ErrorCode::EINVAL);
            assert_eq!(s.tree_version(), version);
            assert_eq!(s.snapshot().roots(), &[a]);
        }
    }

    #[test]
    fn unavailable_root_preserves_identity_and_evicts_descendants_atomically() {
        let s = EntryStore::new();
        let root = s
            .insert("/workspace".into(), dir("workspace", None))
            .unwrap();
        let folder = s
            .insert("/workspace/src".into(), dir("src", Some(root)))
            .unwrap();
        let leaf = s
            .insert(
                "/workspace/src/main.rs".into(),
                leaf("main.rs", Some(folder)),
            )
            .unwrap();
        let retained = s.snapshot();
        s.take_pending_changes();

        let (version, removed) = s.mark_root_unavailable(root).unwrap();
        let current = s.snapshot();
        assert_eq!(current.tree_version(), version);
        assert_eq!(current.roots(), &[root]);
        assert_eq!(current.get(root).unwrap().kind, EntryKind::Unavailable);
        assert!(current.get(root).unwrap().is_readonly);
        assert_eq!(current.children_of(root), &[]);
        assert_eq!(removed, vec![folder, leaf]);
        assert_eq!(s.path_for_id(root), Some(PathBuf::from("/workspace")));
        assert_eq!(s.path_for_id(folder), None);
        assert_eq!(s.path_for_id(leaf), None);
        assert_eq!(retained.get(leaf).unwrap().name, "main.rs");

        let changes = s.take_pending_changes();
        assert!(changes.changed_ids.contains(&root));
        assert!(changes.changed_ids.contains(&folder));
        assert!(changes.changed_ids.contains(&leaf));
        assert!(changes.child_set_changed.contains(&root));

        assert_eq!(
            s.mark_root_unavailable(root).unwrap(),
            (version, Vec::new())
        );
        assert!(s.take_pending_changes().is_empty());
    }

    #[test]
    fn insert_child_updates_parent_children_and_count() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let child = s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.children_of(root), &[child]);
        assert_eq!(snap.direct_child_count(root), Some(1));
        assert!(snap.has_children(root));
    }

    #[test]
    fn siblings_appear_in_sorted_order() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        // Insert out of order; children list should still come back sorted.
        let c = s.insert("/r/c".into(), leaf("c", Some(root))).unwrap();
        let a = s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        let b = s.insert("/r/b".into(), leaf("b", Some(root))).unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.children_of(root), &[a, b, c]);
    }

    #[test]
    fn siblings_use_natural_numeric_order() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let ten = s
            .insert("/r/file10".into(), leaf("file10", Some(root)))
            .unwrap();
        let two = s
            .insert("/r/file2".into(), leaf("file2", Some(root)))
            .unwrap();
        let one = s
            .insert("/r/file1".into(), leaf("file1", Some(root)))
            .unwrap();
        assert_eq!(s.snapshot().children_of(root), &[one, two, ten]);
    }

    #[test]
    fn configured_type_modified_case_and_folder_order_are_applied() {
        use crate::sort::{SiblingOrder, SortBy};

        let by_type = EntryStore::with_sibling_order(
            SiblingOrder::try_new(SortBy::Type, false, false, None).unwrap(),
        );
        let root = by_type.insert("/r".into(), dir("r", None)).unwrap();
        let ts = by_type
            .insert("/r/z.ts".into(), leaf("z.ts", Some(root)))
            .unwrap();
        let js = by_type
            .insert("/r/a.js".into(), leaf("a.js", Some(root)))
            .unwrap();
        let folder = by_type
            .insert("/r/folder.zz".into(), dir("folder.zz", Some(root)))
            .unwrap();
        assert_eq!(by_type.snapshot().children_of(root), &[js, ts, folder]);

        let by_modified = EntryStore::with_sibling_order(
            SiblingOrder::try_new(SortBy::Modified, true, true, None).unwrap(),
        );
        let root = by_modified.insert("/m".into(), dir("m", None)).unwrap();
        let mut older = leaf("Alpha", Some(root));
        older.mtime_ms = 10;
        let older_id = by_modified.insert("/m/Alpha".into(), older).unwrap();
        let mut newer = leaf("alpha", Some(root));
        newer.mtime_ms = 20;
        let newer_id = by_modified.insert("/m/alpha".into(), newer).unwrap();
        assert_eq!(
            by_modified.snapshot().children_of(root),
            &[newer_id, older_id]
        );

        let mut newest = (*by_modified.get_by_id(older_id).unwrap()).clone();
        newest.mtime_ms = 30;
        by_modified.update(older_id, newest).unwrap();
        assert_eq!(
            by_modified.snapshot().children_of(root),
            &[older_id, newer_id]
        );
    }

    #[test]
    fn remove_child_drops_count_and_children_entry() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let a = s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        s.insert("/r/b".into(), leaf("b", Some(root))).unwrap();
        s.remove(a).unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.direct_child_count(root), Some(1));
        assert_eq!(snap.children_of(root).len(), 1);
    }

    #[test]
    fn remove_root_leaves_children_dangling_but_does_not_panic() {
        // Phase 1: rename/remove are leaf-leaning. If a caller removes a root
        // with children, the children's parent_id becomes dangling until a
        // walker-driven rewalk reconciles the subtree. Documented limitation.
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let child = s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        s.remove(root).unwrap();
        let snap = s.snapshot();
        assert!(snap.roots().is_empty());
        assert!(
            snap.get(child).is_some(),
            "child still present though dangling"
        );
    }

    #[test]
    fn tree_version_bumps_on_every_mutation() {
        let s = EntryStore::new();
        let v0 = s.tree_version();
        s.insert("/a".into(), leaf("a", None)).unwrap();
        let v1 = s.tree_version();
        s.insert("/b".into(), leaf("b", None)).unwrap();
        let v2 = s.tree_version();
        assert!(v1 > v0 && v2 > v1);
    }

    #[test]
    fn counter_near_max_hits_entryid_exhausted() {
        let s = EntryStore::new();
        s.__set_counter_for_tests((1u64 << 53) - 1);
        assert!(s.insert("/last".into(), leaf("last", None)).is_ok());
        let err = s.insert("/past".into(), leaf("past", None)).unwrap_err();
        assert!(matches!(err, FxError::EntryIdExhausted));
    }

    // --- Subtree summary tests (commit 2.1) ---

    fn file_with_size(name: &str, parent: Option<EntryId>, size: u64) -> Entry {
        Entry {
            size,
            ..leaf(name, parent)
        }
    }

    #[test]
    fn insert_then_remove_returns_summaries_to_zero() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let child = s
            .insert("/r/f".into(), file_with_size("f", Some(root), 128))
            .unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.subtree_visible_count(root), 2);
        assert_eq!(snap.subtree_total_size(root), 128);

        s.remove(child).unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.subtree_visible_count(root), 1);
        assert_eq!(snap.subtree_total_size(root), 0);

        s.remove(root).unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.subtree_visible_count(root), 0);
        assert_eq!(snap.subtree_total_size(root), 0);
    }

    #[test]
    fn store_insert_accumulates_summary_up_a_chain() {
        let s = EntryStore::new();
        let a = s.insert("/a".into(), dir("a", None)).unwrap();
        let b = s.insert("/a/b".into(), dir("b", Some(a))).unwrap();
        let c = s
            .insert("/a/b/c".into(), file_with_size("c", Some(b), 77))
            .unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.subtree_visible_count(a), 3);
        assert_eq!(snap.subtree_visible_count(b), 2);
        assert_eq!(snap.subtree_visible_count(c), 1);
        assert_eq!(snap.subtree_total_size(a), 77);
        assert_eq!(snap.subtree_total_size(b), 77);
        assert_eq!(snap.subtree_total_size(c), 77);
    }

    #[test]
    fn store_cycle_in_parent_ids_does_not_hang_insert() {
        // Build a snapshot manually containing a cycle, install it behind the
        // ArcSwap, then insert a child that would walk through the cycle.
        // The 128-hop cap guarantees termination.
        use std::time::{Duration, Instant};

        let s = EntryStore::new();
        // Reserve ids via the counter (so allocator is past them).
        let id_x = EntryId::alloc_from(&s.id_counter).unwrap();
        let id_y = EntryId::alloc_from(&s.id_counter).unwrap();

        // Construct a pathological snapshot where x.parent = y and y.parent = x.
        let mut rigged = (*s.snapshot()).clone();
        let x = Entry {
            id: id_x,
            parent_id: Some(id_y),
            ..leaf("x", Some(id_y))
        };
        let y = Entry {
            id: id_y,
            parent_id: Some(id_x),
            ..leaf("y", Some(id_x))
        };
        rigged.entries.insert(id_x, Arc::new(x));
        rigged.entries.insert(id_y, Arc::new(y));
        s.inner.store(Arc::new(rigged));

        let start = Instant::now();
        // Insert a fresh leaf whose parent is id_x; ancestor walk sees the
        // x <-> y cycle and must bail out within 128 hops, not loop forever.
        let result = s.insert("/cycle/z".into(), leaf("z", Some(id_x)));
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "insert with cyclic parent should still return"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "cycle defense failed: insert took {:?}",
            elapsed
        );
    }

    #[test]
    fn remove_dir_with_child_does_not_corrupt_ancestor_cache() {
        // Pre-fix, remove(d) subtracted d's subtree total (2) from root, leaving
        // root's cache at 1 while {root, leaf} are still live — permanently wrong.
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let d = s.insert("/r/d".into(), dir("d", Some(root))).unwrap();
        let leaf_id = s.insert("/r/d/f".into(), leaf("f", Some(d))).unwrap();

        assert_eq!(s.snapshot().subtree_visible_count(root), 3);

        // Remove dir directly (leaf still present): root drops by 1 (d itself), not 2.
        s.remove(d).unwrap();
        assert_eq!(s.snapshot().subtree_visible_count(root), 2);

        // Orphan leaf removal can't find root (parent d is gone) — documented
        // dangling-subtree behavior; root's slot is unaffected.
        s.remove(leaf_id).unwrap();
        assert_eq!(s.snapshot().subtree_visible_count(root), 2);

        // Removing root clears its own cache slot; final count is 0.
        s.remove(root).unwrap();
        assert_eq!(s.snapshot().subtree_visible_count(root), 0);
    }

    #[test]
    fn rename_leaf_updates_stored_entry_name() {
        let s = EntryStore::new();
        let id = s.insert("/old".into(), leaf("old", None)).unwrap();
        s.rename(id, "/new".into()).unwrap();
        assert_eq!(s.get_by_id(id).unwrap().name, "new");
    }

    #[test]
    fn rename_leaf_resorts_siblings() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let a = s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        let b = s.insert("/r/b".into(), leaf("b", Some(root))).unwrap();
        let c = s.insert("/r/c".into(), leaf("c", Some(root))).unwrap();
        assert_eq!(s.snapshot().children_of(root), &[a, b, c]);

        s.rename(b, "/r/zzz".into()).unwrap();
        assert_eq!(s.snapshot().children_of(root), &[a, c, b]);
    }

    #[test]
    fn cross_parent_move_preserves_subtree_ids_paths_and_summaries() {
        let s = EntryStore::new();
        let root_a = s.insert("/a".into(), dir("a", None)).unwrap();
        let root_b = s.insert("/b".into(), dir("b", None)).unwrap();
        let folder = s
            .insert("/a/folder".into(), dir("folder", Some(root_a)))
            .unwrap();
        let leaf = s
            .insert(
                "/a/folder/file.txt".into(),
                file_with_size("file.txt", Some(folder), 42),
            )
            .unwrap();
        let retained = s.snapshot();
        s.take_pending_changes();

        s.rename(folder, "/b/renamed".into()).unwrap();
        let current = s.snapshot();
        assert_eq!(current.get(folder).unwrap().parent_id, Some(root_b));
        assert_eq!(current.get(folder).unwrap().name, "renamed");
        assert_eq!(current.get(leaf).unwrap().parent_id, Some(folder));
        assert_eq!(current.children_of(root_a), &[]);
        assert_eq!(current.children_of(root_b), &[folder]);
        assert_eq!(s.path_for_id(folder), Some(PathBuf::from("/b/renamed")));
        assert_eq!(
            s.path_for_id(leaf),
            Some(PathBuf::from("/b/renamed/file.txt"))
        );
        assert_eq!(s.snapshot().subtree_total_size(root_a), 0);
        assert_eq!(s.snapshot().subtree_total_size(root_b), 42);
        assert_eq!(retained.children_of(root_a), &[folder]);

        let changes = s.take_pending_changes();
        assert_eq!(
            changes.reparented_ids.get(&folder),
            Some(&(Some(root_a), Some(root_b)))
        );
        assert!(changes.child_set_changed.contains(&root_a));
        assert!(changes.child_set_changed.contains(&root_b));
    }

    #[test]
    fn rename_invalidates_new_snapshot_nesting_cache_but_not_retained_snapshot() {
        let s = EntryStore::with_projection_settings(
            SiblingOrder::default(),
            VisibilityPolicy::project_view(),
            false,
            FileNestingPolicy::new([("*.ts".into(), vec!["${capture}.test.ts".into()])], true),
        );
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let source = s
            .insert("/r/source.ts".into(), leaf("source.ts", Some(root)))
            .unwrap();
        let test = s
            .insert(
                "/r/source.test.ts".into(),
                leaf("source.test.ts", Some(root)),
            )
            .unwrap();
        let before = s.snapshot();
        assert_eq!(before.projected_children_of(root, false), vec![source]);
        assert_eq!(before.projected_children_of(source, false), vec![test]);

        s.rename(test, "/r/source.spec.ts".into()).unwrap();
        let after = s.snapshot();
        assert_eq!(after.projected_children_of(root, false), vec![test, source]);
        assert!(after.projected_children_of(source, false).is_empty());
        assert_eq!(before.projected_children_of(source, false), vec![test]);
        assert!(s.take_pending_changes().child_set_changed.contains(&root));
    }

    #[test]
    fn projection_reconfiguration_is_atomic_and_retained_snapshots_stay_stable() {
        let s = EntryStore::with_projection_settings(
            SiblingOrder::default(),
            VisibilityPolicy::project_view(),
            false,
            FileNestingPolicy::default(),
        );
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let directory = s
            .insert("/r/z-dir".into(), dir("z-dir", Some(root)))
            .unwrap();
        let file = s
            .insert("/r/a.txt".into(), leaf("a.txt", Some(root)))
            .unwrap();
        let mut hidden_entry = leaf(".hidden", Some(root));
        hidden_entry.is_hidden = true;
        let hidden = s.insert("/r/.hidden".into(), hidden_entry).unwrap();
        let source = s
            .insert("/r/source.ts".into(), leaf("source.ts", Some(root)))
            .unwrap();
        let test = s
            .insert(
                "/r/source.test.ts".into(),
                leaf("source.test.ts", Some(root)),
            )
            .unwrap();
        let _ = s.take_pending_changes();
        let before = s.snapshot();
        assert_eq!(before.subtree_visible_count(root), 6);
        assert!(before.projected_children_of(root, false).contains(&hidden));
        assert!(before.projected_children_of(root, false).contains(&test));

        let updated_order =
            SiblingOrder::try_new(crate::sort::SortBy::Name, true, false, None).unwrap();
        let updated_visibility = VisibilityPolicy {
            show_hidden_files: false,
            show_ignored_files: true,
        };
        let updated_nesting =
            FileNestingPolicy::new([("*.ts".into(), vec!["${capture}.test.ts".into()])], true);
        let version = s.reconfigure_projection(
            updated_order.clone(),
            updated_visibility,
            true,
            updated_nesting.clone(),
        );
        let after = s.snapshot();
        assert_eq!(version, before.tree_version() + 1);
        assert_eq!(after.tree_version(), version);
        assert_eq!(
            after.projected_children_of(root, false),
            vec![file, source, directory]
        );
        assert_eq!(after.projected_children_of(source, false), vec![test]);
        assert_eq!(after.subtree_visible_count(root), 5);
        assert_eq!(before.subtree_visible_count(root), 6);
        assert!(before.projected_children_of(root, false).contains(&hidden));
        assert!(before.projected_children_of(root, false).contains(&test));

        let changes = s.take_pending_changes();
        assert!(changes.projection_changed);
        assert!(changes.child_set_changed.contains(&root));
        assert!(changes.subtree_roots_changed.contains(&root));
        let unchanged =
            s.reconfigure_projection(updated_order, updated_visibility, true, updated_nesting);
        assert_eq!(unchanged, version);
        assert!(s.take_pending_changes().is_empty());
    }

    #[test]
    fn configured_excludes_are_reversible_without_losing_repository_ignore_provenance() {
        let visibility = VisibilityPolicy {
            show_hidden_files: true,
            show_ignored_files: false,
        };
        let s = EntryStore::with_projection_settings(
            SiblingOrder::default(),
            visibility,
            false,
            FileNestingPolicy::default(),
        );
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let generated = s
            .insert("/r/generated.log".into(), leaf("generated.log", Some(root)))
            .unwrap();
        let mut repository_ignored = leaf("repository.log", Some(root));
        repository_ignored.is_ignored = true;
        let repository_ignored = s
            .insert("/r/repository.log".into(), repository_ignored)
            .unwrap();
        let visible = s
            .insert("/r/visible.txt".into(), leaf("visible.txt", Some(root)))
            .unwrap();
        let _ = s.take_pending_changes();

        let added = s.reconfigure_projection_with_exclusions(
            SiblingOrder::default(),
            visibility,
            false,
            FileNestingPolicy::default(),
            |path, _| path.ends_with("generated.log"),
        );
        let excluded = s.snapshot();
        assert!(excluded.get(generated).unwrap().is_excluded);
        assert!(excluded
            .get(repository_ignored)
            .unwrap()
            .is_ignored_or_excluded());
        assert_eq!(excluded.projected_children_of(root, false), vec![visible]);
        assert_eq!(excluded.subtree_visible_count(root), 2);
        let changes = s.take_pending_changes();
        assert_eq!(changes.changed_ids, HashSet::from([generated]));

        let removed = s.reconfigure_projection_with_exclusions(
            SiblingOrder::default(),
            visibility,
            false,
            FileNestingPolicy::default(),
            |_, _| false,
        );
        let restored = s.snapshot();
        assert_eq!(removed, added + 1);
        assert!(!restored.get(generated).unwrap().is_ignored_or_excluded());
        assert!(restored
            .get(repository_ignored)
            .unwrap()
            .is_ignored_or_excluded());
        assert_eq!(
            restored.projected_children_of(root, false),
            vec![generated, visible]
        );
        assert_eq!(restored.subtree_visible_count(root), 3);
    }

    // --- ChangeSet producer tests (commit 4.1) ---

    #[test]
    fn insert_populates_changeset_with_id_and_ancestors() {
        let s = EntryStore::new();
        let a = s.insert("/a".into(), dir("a", None)).unwrap();
        let b = s.insert("/a/b".into(), leaf("b", Some(a))).unwrap();
        let cs = s.take_pending_changes();
        assert!(cs.changed_ids.contains(&a));
        assert!(cs.changed_ids.contains(&b));
        // Inserting b walks through a as an ancestor, so a is a subtree root.
        assert!(cs.subtree_roots_changed.contains(&a));
        // Root a has no parent, so it doesn't contribute a subtree ancestor itself.
        assert!(cs.reparented_ids.is_empty());
    }

    #[test]
    fn remove_populates_changeset() {
        let s = EntryStore::new();
        let a = s.insert("/a".into(), dir("a", None)).unwrap();
        let b = s.insert("/a/b".into(), leaf("b", Some(a))).unwrap();
        // Drain post-insert to isolate the remove contribution.
        let _ = s.take_pending_changes();

        s.remove(b).unwrap();
        let cs = s.take_pending_changes();
        assert!(cs.changed_ids.contains(&b));
        assert!(cs.subtree_roots_changed.contains(&a));
    }

    #[test]
    fn rename_leaf_populates_changeset_without_reparent() {
        let s = EntryStore::new();
        let id = s.insert("/a".into(), leaf("a", None)).unwrap();
        let _ = s.take_pending_changes();

        s.rename(id, "/b".into()).unwrap();
        let cs = s.take_pending_changes();
        assert!(cs.changed_ids.contains(&id));
        assert!(
            cs.reparented_ids.is_empty(),
            "leaf rename does not reparent"
        );
    }

    #[test]
    fn take_pending_changes_drains_to_empty() {
        let s = EntryStore::new();
        s.insert("/a".into(), leaf("a", None)).unwrap();
        s.insert("/b".into(), leaf("b", None)).unwrap();
        let first = s.take_pending_changes();
        assert!(!first.is_empty());
        let second = s.take_pending_changes();
        assert!(second.is_empty(), "second drain should be empty");
    }

    #[test]
    fn take_pending_changes_stamps_version_range() {
        let s = EntryStore::new();
        let v_before = s.tree_version();
        s.insert("/a".into(), leaf("a", None)).unwrap();
        s.insert("/b".into(), leaf("b", None)).unwrap();
        let v_after = s.tree_version();
        let cs = s.take_pending_changes();
        assert_eq!(cs.from_version, v_before);
        assert_eq!(cs.to_version, v_after);
    }

    #[test]
    fn multiple_mutations_accumulate_into_one_changeset() {
        let s = EntryStore::new();
        let a = s.insert("/a".into(), leaf("a", None)).unwrap();
        let b = s.insert("/b".into(), leaf("b", None)).unwrap();
        let c = s.insert("/c".into(), leaf("c", None)).unwrap();
        let cs = s.take_pending_changes();
        assert!(cs.changed_ids.contains(&a));
        assert!(cs.changed_ids.contains(&b));
        assert!(cs.changed_ids.contains(&c));
        assert_eq!(cs.changed_ids.len(), 3);
    }

    // --- child_set_changed tests (commit 4.2) ---

    #[test]
    fn child_set_changed_populated_on_insert_of_child() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        // Drain the root insertion first to isolate.
        let _ = s.take_pending_changes();
        s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        let cs = s.take_pending_changes();
        assert!(cs.child_set_changed.contains(&root));
    }

    #[test]
    fn child_set_changed_populated_on_remove_of_child() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let a = s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        let _ = s.take_pending_changes();
        s.remove(a).unwrap();
        let cs = s.take_pending_changes();
        assert!(cs.child_set_changed.contains(&root));
    }

    #[test]
    fn child_set_changed_empty_for_root_mutation_without_children() {
        let s = EntryStore::new();
        s.insert("/r".into(), dir("r", None)).unwrap();
        let cs = s.take_pending_changes();
        assert!(
            cs.child_set_changed.is_empty(),
            "root insert has no parent — nothing to flag"
        );
    }

    #[test]
    fn insert_is_idempotent_by_path() {
        let s = EntryStore::new();
        let first = s.insert("/same".into(), leaf("same", None)).unwrap();
        let version = s.tree_version();
        let second = s.insert("/same".into(), leaf("same", None)).unwrap();
        assert_eq!(first, second);
        assert_eq!(s.tree_version(), version);
        assert_eq!(s.snapshot().entry_count(), 1);
    }

    #[test]
    fn update_preserves_id_and_adjusts_ancestor_size() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let file = s.insert("/r/a".into(), leaf("a", Some(root))).unwrap();
        let _ = s.take_pending_changes();

        let mut changed = (*s.get_by_id(file).unwrap()).clone();
        changed.size = 42;
        changed.mtime_ms = 1234;
        assert!(s.update(file, changed).unwrap());
        assert_eq!(s.get_by_path(Path::new("/r/a")).unwrap().id, file);
        assert_eq!(s.snapshot().subtree_total_size(root), 42);
        assert!(s.take_pending_changes().changed_ids.contains(&file));
    }

    #[test]
    fn remove_subtree_removes_descendants_and_paths_atomically() {
        let s = EntryStore::new();
        let root = s.insert("/r".into(), dir("r", None)).unwrap();
        let dir_id = s.insert("/r/d".into(), dir("d", Some(root))).unwrap();
        let leaf_id = s.insert("/r/d/a".into(), leaf("a", Some(dir_id))).unwrap();
        let _ = s.take_pending_changes();

        let removed = s.remove_subtree(dir_id);
        assert_eq!(removed.len(), 2);
        assert!(s.get_by_id(dir_id).is_none());
        assert!(s.get_by_id(leaf_id).is_none());
        assert!(s.get_by_path(Path::new("/r/d/a")).is_none());
        assert_eq!(s.path_for_id(dir_id), None);
        assert_eq!(s.path_for_id(leaf_id), None);
        assert!(s.snapshot().children_of(root).is_empty());
        let cs = s.take_pending_changes();
        assert!(cs.changed_ids.contains(&dir_id));
        assert!(cs.changed_ids.contains(&leaf_id));
        assert!(cs.child_set_changed.contains(&root));
    }
}
