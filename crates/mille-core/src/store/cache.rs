//! Eviction operates on the live store. Previously issued immutable snapshots
//! keep their own entries and child lists.
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::EntryStore;
use crate::entry::EntryId;

#[derive(Default)]
pub(super) struct CacheAccess {
    clock: u64,
    last_used: HashMap<EntryId, u64>,
}

impl EntryStore {
    pub fn with_cache_limit(mut self, limit: usize) -> Self {
        self.resident = Some(arc_swap::ArcSwap::new(self.snapshot()));
        self.cache_limit = limit;
        self
    }

    /// Slots available for children when the workspace roots and the path to
    /// this directory must stay resident.
    pub fn child_capacity(&self, parent: EntryId) -> usize {
        let snapshot = self.snapshot();
        let mut reserved = snapshot.roots().len();
        let mut at = parent;
        while let Some(next) = snapshot.get(at).and_then(|e| e.parent_id) {
            reserved += 1;
            at = next;
        }
        self.cache_limit.saturating_sub(reserved)
    }

    pub fn sort_siblings(&self, entries: &mut [(std::path::PathBuf, crate::Entry)]) {
        let order = self.sibling_order.read();
        entries.sort_by(|(_, a), (_, b)| order.compare(a, b));
    }

    /// Invalidate previously complete listings when an authoritative refresh
    /// observes more rows than the cache can hold.
    pub fn invalidate_listings(&self, ids: impl IntoIterator<Item = EntryId>) -> Vec<EntryId> {
        let _guard = self.write_lock.lock();
        let current = self.inner.load_full();
        let ids: Vec<_> = ids
            .into_iter()
            .filter(|id| current.directory_children_loaded(*id))
            .collect();
        if ids.is_empty() {
            return ids;
        }
        let mut next = (*current).clone();
        for id in &ids {
            next.loaded_directories.remove(id);
        }
        let before = next.tree_version;
        next.tree_version += 1;
        self.publish(next);
        self.record_mutation(before, before + 1, |changes| {
            changes.child_set_changed.extend(ids.iter().copied())
        });
        ids
    }

    pub fn invalidate_truncated_walk(
        &self,
        root: &std::path::Path,
        complete: &[std::path::PathBuf],
    ) -> Vec<EntryId> {
        let complete: HashSet<_> = complete.iter().collect();
        self.invalidate_listings(
            self.paths_under(root)
                .into_iter()
                .filter_map(|(p, id)| (!complete.contains(&p)).then_some(id)),
        )
    }

    /// Evict cold subtrees, preserving every root and the requested working
    /// path. A page can nominate individual children for the remaining slots.
    pub fn trim_cache(&self, requested: Option<EntryId>, page: &[EntryId]) -> usize {
        if self.cache_limit == usize::MAX {
            return 0;
        }
        let current = self.inner.load();
        if current.entry_count() <= self.cache_limit && requested.is_none() {
            return 0;
        }
        drop(current);
        let _guard = self.write_lock.lock();
        let current = self.inner.load_full();
        let mut access = self.cache_access.lock();
        let mut pinned: HashSet<_> = current.roots().iter().copied().collect();
        if let Some(id) = requested {
            access.clock += 1;
            let stamp = access.clock;
            let mut path = Vec::new();
            let mut cursor = Some(id);
            while let Some(id) = cursor {
                let Some(entry) = current.get(id) else { break };
                if pinned.contains(&id) {
                    break;
                }
                path.push(id);
                cursor = entry.parent_id;
            }
            for id in path.into_iter().rev().chain(page.iter().copied()) {
                if pinned.len() == self.cache_limit {
                    break;
                }
                pinned.insert(id);
                access.last_used.insert(id, stamp);
            }
            if access.last_used.len() > self.cache_limit.saturating_mul(2) {
                access.last_used.retain(|id, _| current.get(*id).is_some());
            }
        }
        let excess = current.entry_count().saturating_sub(self.cache_limit);
        if excess == 0 {
            return 0;
        }

        let mut candidates: Vec<_> = current
            .entries_iter()
            .filter(|(id, _)| !pinned.contains(id))
            .map(|(id, entry)| {
                let last_used = access
                    .last_used
                    .get(&id)
                    .max(
                        entry
                            .parent_id
                            .as_ref()
                            .and_then(|parent| access.last_used.get(parent)),
                    )
                    .copied()
                    .unwrap_or(0);
                (last_used, Reverse(id))
            })
            .collect();
        candidates.sort_unstable();
        let mut removed = HashSet::new();
        let mut stack = Vec::new();
        for (_, Reverse(id)) in candidates {
            if removed.len() >= excess {
                break;
            }
            stack.push(id);
            while let Some(child) = stack.pop() {
                if removed.insert(child) {
                    stack.extend(current.children_of(child));
                }
            }
        }

        let mut next = (*current).clone();
        let mut parents = HashSet::new();
        let mut ancestors = HashSet::new();
        for &id in &removed {
            if let Some(parent) = current.get(id).and_then(|e| e.parent_id) {
                if !removed.contains(&parent) {
                    parents.insert(parent);
                    let visible = current.subtree_visible_count(id);
                    let size = current.subtree_total_size(id);
                    let mut at = Some(parent);
                    for _ in 0..crate::snapshot::MAX_ANCESTOR_WALK {
                        let Some(ancestor) = at else { break };
                        ancestors.insert(ancestor);
                        if let Some(n) = next.descendant_visible_counts.get_mut(&ancestor) {
                            *n = n.saturating_sub(visible);
                        }
                        if let Some(n) = next.descendant_total_sizes.get_mut(&ancestor) {
                            *n = n.saturating_sub(size);
                        }
                        at = current.get(ancestor).and_then(|e| e.parent_id);
                    }
                }
            }
            next.entries.remove(&id);
            next.children.remove(&id);
            next.loaded_directories.remove(&id);
            next.direct_child_counts.remove(&id);
            next.descendant_visible_counts.remove(&id);
            next.descendant_total_sizes.remove(&id);
            access.last_used.remove(&id);
            if let Some((_, path)) = self.id_to_path.remove(&id) {
                self.path_to_id.remove(path.as_ref());
            }
        }
        for &parent in &parents {
            next.loaded_directories.remove(&parent);
            if let Some(mut children) = next.children.remove(&parent) {
                Arc::make_mut(&mut children).retain(|id| !removed.contains(id));
                if children.is_empty() {
                    next.direct_child_counts.remove(&parent);
                } else {
                    next.direct_child_counts
                        .insert(parent, children.len() as u32);
                    next.children.insert(parent, children);
                }
            }
        }
        let before = next.tree_version;
        next.tree_version += 1;
        self.publish(next);
        let count = removed.len();
        self.record_mutation(before, before + 1, |changes| {
            changes.changed_ids.extend(removed);
            changes.child_set_changed.extend(parents);
            changes.subtree_roots_changed.extend(ancestors);
        });
        count
    }
}
