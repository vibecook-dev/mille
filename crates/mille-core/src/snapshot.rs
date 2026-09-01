// Phase 1 commit 1.5: StoreSnapshot now carries the children index, roots list,
// and direct_child_counts summary (SPEC §4.9.2 — the summary ships in every
// delta so visibleRowCount can answer honestly during expansion races).
//
// Children are stored sorted by name using raw byte order. Natural-sort is a
// Phase 9 concern (PLAN 13.9); byte order is adequate for unit tests today.
//
// Phase 2 commit 2.1: augments the BTreeMap storage with descendant-summary
// caches (descendant_visible_counts, descendant_total_sizes) maintained by
// ancestor-walks in EntryStore insert/remove. A full Zed-style SumTree port
// is deferred to Phase 12 behind the same public query API.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use smallvec::SmallVec;

use crate::entry::{Entry, EntryId, EntryKind};
use crate::file_nesting::FileNestingPolicy;

/// Max ancestor hops in any summary update — defends against malformed
/// parent_id cycles (self-reference, etc.). Real trees are ~10-15 deep.
pub(crate) const MAX_ANCESTOR_WALK: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisibilityPolicy {
    pub show_hidden_files: bool,
    pub show_ignored_files: bool,
}

impl VisibilityPolicy {
    pub fn project_view() -> Self {
        Self {
            show_hidden_files: true,
            show_ignored_files: true,
        }
    }

    pub fn includes(self, entry: &Entry) -> bool {
        if is_os_or_vcs_noise(&entry.name) {
            return false;
        }
        (self.show_hidden_files || !entry.is_hidden)
            && (self.show_ignored_files || !entry.is_ignored_or_excluded())
    }
}

fn is_os_or_vcs_noise(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini" | ".git")
}

pub struct StoreSnapshot {
    pub(crate) entries: BTreeMap<EntryId, Arc<Entry>>,
    pub(crate) children: BTreeMap<EntryId, SmallVec<[EntryId; 8]>>,
    /// Directory ids whose immediate child listing has completed.
    ///
    /// This is deliberately separate from `children`: an empty directory has
    /// no child records, while URI hydration can insert a partial ancestor
    /// chain before the directory has ever been listed. Conflating either case
    /// with completeness causes permanent loading spinners or truncated trees.
    pub(crate) loaded_directories: BTreeSet<EntryId>,
    pub(crate) roots: SmallVec<[EntryId; 4]>,
    pub(crate) tree_version: u64,
    pub(crate) direct_child_counts: BTreeMap<EntryId, u32>,
    /// Count of visible (non-ignored, non-hidden) entries in the subtree rooted at
    /// this id, INCLUDING the root itself. A leaf has count 1 if visible.
    pub(crate) descendant_visible_counts: BTreeMap<EntryId, u32>,
    /// Sum of file sizes in the subtree (dirs contribute 0; files their `size`).
    pub(crate) descendant_total_sizes: BTreeMap<EntryId, u64>,
    pub(crate) visibility: VisibilityPolicy,
    pub(crate) compact_folders: bool,
    pub(crate) file_nesting: FileNestingPolicy,
    nesting_cache: Mutex<HashMap<(EntryId, bool), Arc<NestingPlan>>>,
}

impl Clone for StoreSnapshot {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            children: self.children.clone(),
            loaded_directories: self.loaded_directories.clone(),
            roots: self.roots.clone(),
            tree_version: self.tree_version,
            direct_child_counts: self.direct_child_counts.clone(),
            descendant_visible_counts: self.descendant_visible_counts.clone(),
            descendant_total_sizes: self.descendant_total_sizes.clone(),
            visibility: self.visibility,
            compact_folders: self.compact_folders,
            file_nesting: self.file_nesting.clone(),
            // Every published structural mutation clones the snapshot. A new
            // snapshot must plan against its new sibling names/membership;
            // retained older snapshots keep their own valid memoized plans.
            nesting_cache: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for StoreSnapshot {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            children: BTreeMap::new(),
            loaded_directories: BTreeSet::new(),
            roots: SmallVec::new(),
            tree_version: 0,
            direct_child_counts: BTreeMap::new(),
            descendant_visible_counts: BTreeMap::new(),
            descendant_total_sizes: BTreeMap::new(),
            visibility: VisibilityPolicy::default(),
            compact_folders: false,
            file_nesting: FileNestingPolicy::default(),
            nesting_cache: Mutex::new(HashMap::new()),
        }
    }
}

impl StoreSnapshot {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn empty_with_visibility(visibility: VisibilityPolicy) -> Self {
        Self {
            visibility,
            ..Self::default()
        }
    }

    pub fn empty_with_projection(
        visibility: VisibilityPolicy,
        compact_folders: bool,
        file_nesting: FileNestingPolicy,
    ) -> Self {
        Self {
            visibility,
            compact_folders,
            file_nesting,
            ..Self::default()
        }
    }

    pub fn visibility(&self) -> VisibilityPolicy {
        self.visibility
    }

    pub fn compact_folders(&self) -> bool {
        self.compact_folders
    }

    pub fn file_nesting_enabled(&self) -> bool {
        !self.file_nesting.is_empty()
    }

    pub(crate) fn entry_counts_visible(&self, entry: &Entry) -> bool {
        self.visibility.includes(entry)
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn tree_version(&self) -> u64 {
        self.tree_version
    }

    pub fn get(&self, id: EntryId) -> Option<&Arc<Entry>> {
        self.entries.get(&id)
    }

    pub fn roots(&self) -> &[EntryId] {
        &self.roots
    }

    pub fn children_of(&self, id: EntryId) -> &[EntryId] {
        self.children.get(&id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Whether an authoritative direct-child listing has completed for `id`.
    /// A loaded directory may have zero children.
    pub fn directory_children_loaded(&self, id: EntryId) -> bool {
        self.loaded_directories.contains(&id)
    }

    pub fn direct_child_count(&self, id: EntryId) -> Option<u32> {
        self.direct_child_counts.get(&id).copied()
    }

    pub fn has_children(&self, id: EntryId) -> bool {
        if !self.file_nesting.is_empty() {
            let mut projection = ProjectionContext::new(self, false);
            return projection.has_children(id);
        }
        if let Some(kids) = self.children.get(&id) {
            return kids.iter().any(|child_id| {
                self.entries
                    .get(child_id)
                    .is_some_and(|entry| self.entry_counts_visible(entry))
            });
        }
        // Unwalked: show a chevron for real directories and for
        // symlink-to-dir (pnpm/npm workspace links), matching the
        // client mirror. Without this, expand never fires for links.
        match self.entries.get(&id) {
            Some(e) => {
                !self.directory_children_loaded(id)
                    && (e.kind == EntryKind::Directory || e.symlink_target_is_dir == Some(true))
            }
            None => false,
        }
    }

    fn projected_entry(
        &self,
        id: EntryId,
        include_ignored: bool,
    ) -> (EntryId, Option<Vec<String>>) {
        if !self.compact_folders {
            return (id, None);
        }
        let Some(start) = self.entries.get(&id) else {
            return (id, None);
        };
        // Workspace roots retain a stable identity and label. Compaction starts
        // at their children, matching mature IDE project views.
        if start.parent_id.is_none() || start.kind != EntryKind::Directory {
            return (id, None);
        }

        let mut leaf = id;
        let mut segments = vec![start.name.clone()];
        // A projected child-list carries the compact chain's stable leaf id.
        // Reconstruct leading segments when a later query starts from that
        // leaf (rather than from the raw chain head).
        let mut cursor = id;
        for _ in 0..256 {
            let Some(parent_id) = self.entries.get(&cursor).and_then(|entry| entry.parent_id)
            else {
                break;
            };
            let Some(parent) = self.entries.get(&parent_id) else {
                break;
            };
            if parent.parent_id.is_none() || parent.kind != EntryKind::Directory {
                break;
            }
            let mut visible = self.children_of(parent_id).iter().filter(|child_id| {
                self.entries
                    .get(child_id)
                    .is_some_and(|entry| include_ignored || self.entry_counts_visible(entry))
            });
            if visible.next().copied() != Some(cursor) || visible.next().is_some() {
                break;
            }
            segments.insert(0, parent.name.clone());
            cursor = parent_id;
        }
        for _ in 0..256 {
            let Some(children) = self.children.get(&leaf) else {
                break;
            };
            let mut visible = children.iter().filter(|child_id| {
                self.entries
                    .get(child_id)
                    .is_some_and(|entry| include_ignored || self.entry_counts_visible(entry))
            });
            let Some(&only_child) = visible.next() else {
                break;
            };
            if visible.next().is_some() {
                break;
            }
            let Some(child) = self.entries.get(&only_child) else {
                break;
            };
            if child.kind != EntryKind::Directory {
                break;
            }
            segments.push(child.name.clone());
            leaf = only_child;
        }
        if leaf == id && segments.len() == 1 {
            (id, None)
        } else {
            (leaf, Some(segments))
        }
    }

    pub fn projected_children_of(&self, id: EntryId, include_ignored: bool) -> Vec<EntryId> {
        ProjectionContext::new(self, include_ignored).children_of(id)
    }

    pub fn projected_child_count(&self, id: EntryId, include_ignored: bool) -> Option<u32> {
        let mut projection = ProjectionContext::new(self, include_ignored);
        let children = projection.children_of(id);
        let entry = self.entries.get(&id)?;
        // File-nesting parents are files, not filesystem directories. Their
        // projected children are complete whenever their containing directory
        // is complete, and an empty list simply means they are not a nesting
        // parent.
        if entry.kind == EntryKind::File && !children.is_empty() {
            return Some(children.len().min(u32::MAX as usize) as u32);
        }
        if self.directory_children_loaded(id) {
            return Some(children.len().min(u32::MAX as usize) as u32);
        }
        None
    }

    /// Iterate over every (id, entry) pair in the snapshot. Primarily for
    /// bench harnesses and crash-resume serialization.
    pub fn entries_iter(&self) -> impl Iterator<Item = (EntryId, &Arc<Entry>)> + '_ {
        self.entries.iter().map(|(id, e)| (*id, e))
    }

    /// Visible-descendant count for the subtree rooted at `id` (inclusive).
    /// Returns 0 for unknown ids so callers can blindly sum.
    pub fn subtree_visible_count(&self, id: EntryId) -> u32 {
        self.descendant_visible_counts
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    /// Total byte size of files in the subtree rooted at `id` (inclusive).
    /// Returns 0 for unknown ids.
    pub fn subtree_total_size(&self, id: EntryId) -> u64 {
        self.descendant_total_sizes.get(&id).copied().unwrap_or(0)
    }
}

#[derive(Default)]
struct NestingPlan {
    roots: Vec<EntryId>,
    nested: BTreeMap<EntryId, Vec<EntryId>>,
}

struct ProjectionContext<'a> {
    snapshot: &'a StoreSnapshot,
    include_ignored: bool,
    nesting_plans: HashMap<EntryId, Arc<NestingPlan>>,
}

impl<'a> ProjectionContext<'a> {
    fn new(snapshot: &'a StoreSnapshot, include_ignored: bool) -> Self {
        Self {
            snapshot,
            include_ignored,
            nesting_plans: HashMap::new(),
        }
    }

    fn ensure_plan(&mut self, parent_id: EntryId) {
        if self.nesting_plans.contains_key(&parent_id) {
            return;
        }
        if let Some(plan) = self
            .snapshot
            .nesting_cache
            .lock()
            .get(&(parent_id, self.include_ignored))
            .cloned()
        {
            self.nesting_plans.insert(parent_id, plan);
            return;
        }
        let visible: Vec<EntryId> = self
            .snapshot
            .children_of(parent_id)
            .iter()
            .copied()
            .filter(|id| {
                self.snapshot.entries.get(id).is_some_and(|entry| {
                    self.include_ignored || self.snapshot.entry_counts_visible(entry)
                })
            })
            .collect();
        if self.snapshot.file_nesting.is_empty() {
            let plan = Arc::new(NestingPlan {
                roots: visible,
                nested: BTreeMap::new(),
            });
            self.snapshot
                .nesting_cache
                .lock()
                .insert((parent_id, self.include_ignored), plan.clone());
            self.nesting_plans.insert(parent_id, plan);
            return;
        }

        let files: Vec<EntryId> = visible
            .iter()
            .copied()
            .filter(|id| {
                self.snapshot
                    .entries
                    .get(id)
                    .is_some_and(|entry| entry.kind == EntryKind::File)
            })
            .collect();
        let mut names: HashMap<String, Vec<EntryId>> = HashMap::with_capacity(files.len());
        for id in &files {
            let entry = &self.snapshot.entries[id];
            names
                .entry(self.snapshot.file_nesting.name_key(&entry.name))
                .or_default()
                .push(*id);
        }

        let mut claimed = HashSet::new();
        let mut parents = HashSet::new();
        let mut nested = BTreeMap::new();
        let file_rank: HashMap<EntryId, usize> = files
            .iter()
            .enumerate()
            .map(|(rank, id)| (*id, rank))
            .collect();
        for parent_id_candidate in files.iter().copied() {
            if claimed.contains(&parent_id_candidate) {
                continue;
            }
            let parent = &self.snapshot.entries[&parent_id_candidate];
            let mut children = Vec::new();
            for rule in self.snapshot.file_nesting.rules() {
                let Some(capture) =
                    rule.capture(&parent.name, self.snapshot.file_nesting.case_sensitive())
                else {
                    continue;
                };
                for child_name in rule.child_names(&capture) {
                    let key = self.snapshot.file_nesting.name_key(&child_name);
                    let Some(matches) = names.get(&key) else {
                        continue;
                    };
                    for child_id in matches {
                        if *child_id == parent_id_candidate
                            || claimed.contains(child_id)
                            || parents.contains(child_id)
                        {
                            continue;
                        }
                        claimed.insert(*child_id);
                        children.push(*child_id);
                    }
                }
            }
            if !children.is_empty() {
                children.sort_by_key(|id| file_rank.get(id).copied().unwrap_or(usize::MAX));
                parents.insert(parent_id_candidate);
                nested.insert(parent_id_candidate, children);
            }
        }
        let roots = visible
            .into_iter()
            .filter(|id| !claimed.contains(id))
            .collect();
        let plan = Arc::new(NestingPlan { roots, nested });
        self.snapshot
            .nesting_cache
            .lock()
            .insert((parent_id, self.include_ignored), plan.clone());
        self.nesting_plans.insert(parent_id, plan);
    }

    fn children_of(&mut self, id: EntryId) -> Vec<EntryId> {
        let Some(entry) = self.snapshot.entries.get(&id) else {
            return Vec::new();
        };
        if entry.kind == EntryKind::File {
            let Some(parent_id) = entry.parent_id else {
                return Vec::new();
            };
            self.ensure_plan(parent_id);
            return self.nesting_plans[&parent_id]
                .nested
                .get(&id)
                .cloned()
                .unwrap_or_default();
        }

        self.ensure_plan(id);
        self.nesting_plans[&id]
            .roots
            .iter()
            .map(|child_id| {
                self.snapshot
                    .projected_entry(*child_id, self.include_ignored)
                    .0
            })
            .collect()
    }

    fn has_children(&mut self, id: EntryId) -> bool {
        let children = self.children_of(id);
        if !children.is_empty() {
            return true;
        }
        self.snapshot.entries.get(&id).is_some_and(|entry| {
            !self.snapshot.directory_children_loaded(id)
                && (entry.kind == EntryKind::Directory || entry.symlink_target_is_dir == Some(true))
        })
    }
}

// ============================================================================
// Viewport queries (commit 2.2).
// ============================================================================
//
// These are what the NAPI binding calls via MirrorSnapshot (Phase 5). They are
// pure Rust against the BTreeMap storage + the summary caches from 2.1. When
// Phase 12 profiling justifies a sum-tree port these methods become O(log n);
// today they're O(visible rows), which handles 100k entries in ~1ms.

#[derive(Clone, Debug)]
pub struct VisibleRowCount {
    pub known: u32,
    /// Folders that the client has expanded but whose children haven't arrived
    /// yet. Empty when the count is fully resolved; non-empty means the UI can
    /// render a "loading" badge without fudging the scroll-height math.
    pub pending_expansions: SmallVec<[EntryId; 8]>,
}

#[derive(Clone, Debug)]
pub struct VisibleRowsQuery<'a> {
    pub expanded: &'a HashSet<EntryId>,
    pub offset: u32,
    pub limit: u32,
    pub include_ignored: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleRowOut {
    pub id: EntryId,
    pub depth: u16,
    pub has_children: bool,
    pub is_expanded: bool,
    pub path_segments: Option<Vec<String>>,
}

impl StoreSnapshot {
    /// Total rows the viewport would render given current `expanded` state,
    /// plus any expanded folders whose children are still pending.
    pub fn visible_row_count(
        &self,
        expanded: &HashSet<EntryId>,
        include_ignored: bool,
    ) -> VisibleRowCount {
        let mut known: u32 = 0;
        let mut pending: SmallVec<[EntryId; 8]> = SmallVec::new();
        let mut projection = ProjectionContext::new(self, include_ignored);

        for &root in self.roots.iter() {
            self.count_subtree(
                root,
                expanded,
                include_ignored,
                &mut known,
                &mut pending,
                &mut projection,
            );
        }

        VisibleRowCount {
            known,
            pending_expansions: pending,
        }
    }

    /// Iterative DFS over a single subtree — avoids native-stack risk on deep
    /// trees. Emits pending ids for expanded dirs whose children aren't loaded.
    fn count_subtree(
        &self,
        root: EntryId,
        expanded: &HashSet<EntryId>,
        include_ignored: bool,
        known: &mut u32,
        pending: &mut SmallVec<[EntryId; 8]>,
        projection: &mut ProjectionContext<'_>,
    ) {
        let mut stack: Vec<EntryId> = Vec::new();
        stack.push(root);

        while let Some(id) = stack.pop() {
            let Some(entry) = self.entries.get(&id) else {
                continue;
            };
            let visible = include_ignored || self.entry_counts_visible(entry);
            if visible {
                let (projected_id, _) = self.projected_entry(id, include_ignored);
                *known = known.saturating_add(1);
                // Only descend when the parent itself would render — otherwise
                // children would appear as orphan rows with no parent above.
                if expanded.contains(&projected_id) {
                    let children = projection.children_of(projected_id);
                    if !children.is_empty() {
                        for child in children.into_iter().rev() {
                            stack.push(child);
                        }
                    } else if self.entries.get(&projected_id).is_some_and(|entry| {
                        !self.directory_children_loaded(projected_id)
                            && (entry.kind == EntryKind::Directory
                                || entry.symlink_target_is_dir == Some(true))
                    }) {
                        // Expanded but children-map missing: worker hasn't
                        // delivered them yet. Report to caller.
                        pending.push(projected_id);
                    }
                }
            }
        }
    }

    /// Exact logical index of `target` in the flattened visible order.
    ///
    /// Unlike repeated offset-based `visible_rows` probes, this performs one
    /// payload-free DFS and stops as soon as the target is reached.
    pub fn visible_row_index(
        &self,
        target: EntryId,
        expanded: &HashSet<EntryId>,
        include_ignored: bool,
    ) -> Option<u32> {
        let mut index: u32 = 0;
        let mut stack: Vec<EntryId> = Vec::new();
        let mut projection = ProjectionContext::new(self, include_ignored);
        for &root in self.roots.iter().rev() {
            stack.push(root);
        }

        while let Some(id) = stack.pop() {
            let Some(entry) = self.entries.get(&id) else {
                continue;
            };
            let visible = include_ignored || self.entry_counts_visible(entry);
            if !visible {
                continue;
            }
            let (projected_id, _) = self.projected_entry(id, include_ignored);
            if projected_id == target {
                return Some(index);
            }
            index = index.saturating_add(1);
            if expanded.contains(&projected_id) {
                for child in projection.children_of(projected_id).into_iter().rev() {
                    stack.push(child);
                }
            }
        }
        None
    }

    /// Find the next visible entry whose display name starts with `prefix`.
    ///
    /// Traverses entry ids and names only: no `VisibleRowOut` payloads are
    /// allocated. Search starts at `from_id` (or immediately after it when
    /// `skip_current` is true) and wraps once to the beginning, matching IDE
    /// typeahead semantics.
    pub fn visible_prefix_match(
        &self,
        prefix: &str,
        from_id: Option<EntryId>,
        skip_current: bool,
        expanded: &HashSet<EntryId>,
        include_ignored: bool,
    ) -> Option<EntryId> {
        if prefix.is_empty() {
            return None;
        }
        let needle = prefix.to_lowercase();
        let mut before_start: Option<EntryId> = None;
        let mut reached_start = from_id.is_none();
        let mut stack: Vec<EntryId> = self.roots.iter().rev().copied().collect();
        let mut projection = ProjectionContext::new(self, include_ignored);

        while let Some(id) = stack.pop() {
            let Some(entry) = self.entries.get(&id) else {
                continue;
            };
            let visible = include_ignored || self.entry_counts_visible(entry);
            if !visible {
                continue;
            }
            let (projected_id, segments) = self.projected_entry(id, include_ignored);
            let projected_entry = self.entries.get(&projected_id)?;
            let display_name = segments
                .as_ref()
                .map(|parts| parts.join("/"))
                .unwrap_or_else(|| projected_entry.name.clone());
            let matches = display_name.to_lowercase().starts_with(&needle);

            if Some(projected_id) == from_id {
                reached_start = true;
                if matches {
                    if skip_current {
                        before_start.get_or_insert(projected_id);
                    } else {
                        return Some(projected_id);
                    }
                }
            } else if reached_start {
                if matches {
                    return Some(projected_id);
                }
            } else if matches && before_start.is_none() {
                before_start = Some(projected_id);
            }

            if expanded.contains(&projected_id) {
                for child in projection.children_of(projected_id).into_iter().rev() {
                    stack.push(child);
                }
            }
        }

        // An evicted/stale focus id is equivalent to starting at row zero.
        before_start
    }

    /// Visible ids in display order for an offset/limit range.
    ///
    /// This follows the same traversal semantics as `visible_rows` without
    /// constructing row payloads for selection-only consumers.
    pub fn visible_row_ids(&self, query: VisibleRowsQuery<'_>) -> Vec<EntryId> {
        let limit = query.limit as usize;
        if limit == 0 {
            return Vec::new();
        }

        let mut out: Vec<EntryId> = Vec::new();
        let mut skipped: u32 = 0;
        let mut stack: Vec<EntryId> = Vec::new();
        let mut projection = ProjectionContext::new(self, query.include_ignored);
        for &root in self.roots.iter().rev() {
            stack.push(root);
        }

        while let Some(id) = stack.pop() {
            let Some(entry) = self.entries.get(&id) else {
                continue;
            };
            let visible = query.include_ignored || self.entry_counts_visible(entry);
            if visible {
                let (projected_id, _) = self.projected_entry(id, query.include_ignored);
                if skipped < query.offset {
                    skipped += 1;
                } else {
                    out.push(projected_id);
                    if out.len() >= limit {
                        return out;
                    }
                }
                if query.expanded.contains(&projected_id) {
                    for child in projection.children_of(projected_id).into_iter().rev() {
                        stack.push(child);
                    }
                }
            }
        }

        out
    }

    /// Flattened visible rows in display order, sliced by `offset`/`limit`.
    /// Early-terminates the DFS once `limit` rows are collected, so large
    /// subtrees beyond the viewport don't get materialized.
    pub fn visible_rows(&self, query: VisibleRowsQuery<'_>) -> Vec<VisibleRowOut> {
        let limit = query.limit as usize;
        if limit == 0 {
            return Vec::new();
        }

        let mut out: Vec<VisibleRowOut> = Vec::new();
        let mut skipped: u32 = 0;
        let mut stack: Vec<(EntryId, u16)> = Vec::new();
        let mut projection = ProjectionContext::new(self, query.include_ignored);
        for &root in self.roots.iter().rev() {
            stack.push((root, 0));
        }

        while let Some((id, depth)) = stack.pop() {
            let Some(entry) = self.entries.get(&id) else {
                continue;
            };
            let visible = query.include_ignored || self.entry_counts_visible(entry);
            if visible {
                let (projected_id, path_segments) = self.projected_entry(id, query.include_ignored);
                if skipped < query.offset {
                    skipped += 1;
                } else {
                    let has_children = projection.has_children(projected_id);
                    out.push(VisibleRowOut {
                        id: projected_id,
                        depth,
                        has_children,
                        is_expanded: query.expanded.contains(&projected_id),
                        path_segments,
                    });
                    if out.len() >= limit {
                        return out;
                    }
                }
                // Only descend when the parent itself would render — otherwise
                // children would appear as orphan rows at the wrong depth.
                if query.expanded.contains(&projected_id) {
                    let child_depth = depth.saturating_add(1);
                    for child in projection.children_of(projected_id).into_iter().rev() {
                        stack.push((child, child_depth));
                    }
                }
            }
        }

        out
    }

    /// Cross-check helper for tests. Not exposed to production callers.
    #[cfg(test)]
    pub(crate) fn visible_row_count_naive(
        &self,
        expanded: &HashSet<EntryId>,
        include_ignored: bool,
    ) -> u32 {
        self.visible_rows(VisibleRowsQuery {
            expanded,
            offset: 0,
            limit: u32::MAX,
            include_ignored,
        })
        .len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryKind};

    fn mk_entry(
        id: EntryId,
        parent: Option<EntryId>,
        name: &str,
        kind: EntryKind,
        size: u64,
        ignored: bool,
        hidden: bool,
    ) -> Entry {
        Entry {
            id,
            parent_id: parent,
            name: name.into(),
            kind,
            size,
            mtime_ms: 0,
            ctime_ms: 0,
            symlink_target_is_dir: None,
            path_segments: None,
            is_ignored: ignored,
            is_excluded: false,
            is_readonly: false,
            is_hidden: hidden,
        }
    }

    /// Directly seed a snapshot mirroring the EntryStore::insert summary logic.
    /// Local to the unit tests; store-level coverage lives in store.rs.
    #[allow(clippy::too_many_arguments)]
    fn seed(
        snap: &mut StoreSnapshot,
        id: EntryId,
        parent: Option<EntryId>,
        name: &str,
        kind: EntryKind,
        size: u64,
        ignored: bool,
        hidden: bool,
    ) {
        let entry = mk_entry(id, parent, name, kind, size, ignored, hidden);
        let visible = VisibilityPolicy::default().includes(&entry);
        snap.entries.insert(id, Arc::new(entry));
        match parent {
            None => snap.roots.push(id),
            Some(p) => {
                snap.children.entry(p).or_default().push(id);
                *snap.direct_child_counts.entry(p).or_insert(0) += 1;
                // Snapshot unit fixtures describe complete static trees unless
                // they deliberately leave a leaf directory childless to test
                // pending expansion.
                snap.loaded_directories.insert(p);
            }
        }
        snap.descendant_visible_counts.insert(id, visible as u32);
        snap.descendant_total_sizes.insert(id, size);

        // Ancestor-walk mirroring EntryStore::insert.
        let mut cur = parent;
        let mut hops = 0;
        while let Some(a) = cur {
            if hops >= MAX_ANCESTOR_WALK {
                break;
            }
            if visible {
                *snap.descendant_visible_counts.entry(a).or_insert(0) += 1;
            }
            *snap.descendant_total_sizes.entry(a).or_insert(0) += size;
            cur = snap.entries.get(&a).and_then(|e| e.parent_id);
            hops += 1;
        }
    }

    #[test]
    fn subtree_visible_count_unknown_id_is_zero() {
        let snap = StoreSnapshot::empty();
        assert_eq!(snap.subtree_visible_count(EntryId(99)), 0);
        assert_eq!(snap.subtree_total_size(EntryId(99)), 0);
    }

    #[test]
    fn single_visible_leaf_counts_one() {
        let mut snap = StoreSnapshot::empty();
        let id = EntryId(1);
        seed(&mut snap, id, None, "a", EntryKind::File, 42, false, false);
        assert_eq!(snap.subtree_visible_count(id), 1);
        assert_eq!(snap.subtree_total_size(id), 42);
    }

    #[test]
    fn ignored_leaf_zero_count_but_size_still_summed() {
        let mut snap = StoreSnapshot::empty();
        let id = EntryId(1);
        seed(&mut snap, id, None, "a", EntryKind::File, 100, true, false);
        assert_eq!(snap.subtree_visible_count(id), 0);
        assert_eq!(snap.subtree_total_size(id), 100);
    }

    #[test]
    fn hidden_leaf_zero_count_but_size_still_summed() {
        let mut snap = StoreSnapshot::empty();
        let id = EntryId(1);
        seed(&mut snap, id, None, "a", EntryKind::File, 7, false, true);
        assert_eq!(snap.subtree_visible_count(id), 0);
        assert_eq!(snap.subtree_total_size(id), 7);
    }

    #[test]
    fn chain_a_b_c_all_visible_accumulates_up_the_chain() {
        let mut snap = StoreSnapshot::empty();
        let a = EntryId(1);
        let b = EntryId(2);
        let c = EntryId(3);
        seed(
            &mut snap,
            a,
            None,
            "a",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            b,
            Some(a),
            "b",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            c,
            Some(b),
            "c",
            EntryKind::File,
            50,
            false,
            false,
        );
        assert_eq!(snap.subtree_visible_count(a), 3);
        assert_eq!(snap.subtree_visible_count(b), 2);
        assert_eq!(snap.subtree_visible_count(c), 1);
        assert_eq!(snap.subtree_total_size(a), 50);
        assert_eq!(snap.subtree_total_size(b), 50);
        assert_eq!(snap.subtree_total_size(c), 50);
    }

    #[test]
    fn two_files_under_same_dir_sum_sizes() {
        let mut snap = StoreSnapshot::empty();
        let d = EntryId(1);
        let f1 = EntryId(2);
        let f2 = EntryId(3);
        seed(
            &mut snap,
            d,
            None,
            "d",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            f1,
            Some(d),
            "f1",
            EntryKind::File,
            100,
            false,
            false,
        );
        seed(
            &mut snap,
            f2,
            Some(d),
            "f2",
            EntryKind::File,
            200,
            false,
            false,
        );
        assert_eq!(snap.subtree_total_size(d), 300);
        assert_eq!(snap.subtree_visible_count(d), 3);
    }

    // --- Viewport query tests (commit 2.2) ---

    #[test]
    fn empty_snapshot_visible_count_and_rows() {
        let snap = StoreSnapshot::empty();
        let expanded: HashSet<EntryId> = HashSet::new();
        let vc = snap.visible_row_count(&expanded, false);
        assert_eq!(vc.known, 0);
        assert!(vc.pending_expansions.is_empty());
        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 100,
            include_ignored: false,
        });
        assert!(rows.is_empty());
    }

    #[test]
    fn single_root_visible_count_is_one() {
        let mut snap = StoreSnapshot::empty();
        let r = EntryId(1);
        seed(
            &mut snap,
            r,
            None,
            "r",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        let expanded: HashSet<EntryId> = HashSet::new();
        assert_eq!(snap.visible_row_count(&expanded, false).known, 1);
    }

    #[test]
    fn root_with_three_children_expanded_dfs_order_and_depths() {
        let mut snap = StoreSnapshot::empty();
        let r = EntryId(1);
        let a = EntryId(2);
        let b = EntryId(3);
        let c = EntryId(4);
        seed(
            &mut snap,
            r,
            None,
            "r",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(&mut snap, a, Some(r), "a", EntryKind::File, 0, false, false);
        seed(&mut snap, b, Some(r), "b", EntryKind::File, 0, false, false);
        seed(&mut snap, c, Some(r), "c", EntryKind::File, 0, false, false);

        let mut expanded: HashSet<EntryId> = HashSet::new();
        expanded.insert(r);

        assert_eq!(snap.visible_row_count(&expanded, false).known, 4);

        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 100,
            include_ignored: false,
        });
        let ids: Vec<EntryId> = rows.iter().map(|r| r.id).collect();
        let depths: Vec<u16> = rows.iter().map(|r| r.depth).collect();
        assert_eq!(ids, vec![r, a, b, c]);
        assert_eq!(depths, vec![0, 1, 1, 1]);
        assert_eq!(snap.visible_row_index(r, &expanded, false), Some(0));
        assert_eq!(snap.visible_row_index(b, &expanded, false), Some(2));
        assert_eq!(snap.visible_row_index(EntryId(99), &expanded, false), None);
        assert_eq!(
            snap.visible_row_ids(VisibleRowsQuery {
                expanded: &expanded,
                offset: 1,
                limit: 2,
                include_ignored: false,
            }),
            vec![a, b]
        );
    }

    #[test]
    fn visible_prefix_match_starts_after_focus_and_wraps() {
        let mut snap = StoreSnapshot::empty();
        let root = EntryId(1);
        let alpha = EntryId(2);
        let beta = EntryId(3);
        let bravo = EntryId(4);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            alpha,
            Some(root),
            "Alpha",
            EntryKind::File,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            beta,
            Some(root),
            "beta",
            EntryKind::File,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            bravo,
            Some(root),
            "Bravo",
            EntryKind::File,
            0,
            false,
            false,
        );
        let expanded = HashSet::from([root]);

        assert_eq!(
            snap.visible_prefix_match("b", Some(beta), true, &expanded, false),
            Some(bravo)
        );
        assert_eq!(
            snap.visible_prefix_match("b", Some(bravo), true, &expanded, false),
            Some(beta)
        );
        assert_eq!(
            snap.visible_prefix_match("BR", Some(bravo), false, &expanded, false),
            Some(bravo)
        );
        assert_eq!(
            snap.visible_prefix_match("missing", Some(alpha), false, &expanded, false),
            None
        );
    }

    #[test]
    fn root_with_children_collapsed_emits_only_root() {
        let mut snap = StoreSnapshot::empty();
        let r = EntryId(1);
        let a = EntryId(2);
        seed(
            &mut snap,
            r,
            None,
            "r",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(&mut snap, a, Some(r), "a", EntryKind::File, 0, false, false);

        let expanded: HashSet<EntryId> = HashSet::new();
        assert_eq!(snap.visible_row_count(&expanded, false).known, 1);
        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 100,
            include_ignored: false,
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, r);
        assert!(rows[0].has_children);
        assert!(!rows[0].is_expanded);
        assert_eq!(snap.visible_row_index(r, &expanded, false), Some(0));
        assert_eq!(snap.visible_row_index(a, &expanded, false), None);
    }

    #[test]
    fn ignored_child_excluded_unless_include_ignored() {
        let mut snap = StoreSnapshot::empty();
        let r = EntryId(1);
        let good = EntryId(2);
        let ign = EntryId(3);
        seed(
            &mut snap,
            r,
            None,
            "r",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            good,
            Some(r),
            "good",
            EntryKind::File,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            ign,
            Some(r),
            "ign",
            EntryKind::File,
            0,
            true,
            false,
        );

        let mut expanded: HashSet<EntryId> = HashSet::new();
        expanded.insert(r);

        assert_eq!(snap.visible_row_count(&expanded, false).known, 2);
        assert_eq!(snap.visible_row_count(&expanded, true).known, 3);
    }

    #[test]
    fn file_nesting_projects_real_ids_across_all_navigation_queries() {
        let nesting = FileNestingPolicy::new(
            [
                ("*.ts".into(), vec!["${capture}.test.ts".into()]),
                ("*.js".into(), vec!["${capture}.js.map".into()]),
            ],
            true,
        );
        let mut snap =
            StoreSnapshot::empty_with_projection(VisibilityPolicy::project_view(), false, nesting);
        let root = EntryId(1);
        let source = EntryId(2);
        let source_test = EntryId(3);
        let script = EntryId(4);
        let source_map = EntryId(5);
        let note = EntryId(6);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        for (id, name) in [
            (source, "source.ts"),
            (source_test, "source.test.ts"),
            (script, "bundle.js"),
            (source_map, "bundle.js.map"),
            (note, "notes.md"),
        ] {
            seed(
                &mut snap,
                id,
                Some(root),
                name,
                EntryKind::File,
                0,
                false,
                false,
            );
        }

        assert_eq!(
            snap.projected_children_of(root, false),
            vec![source, script, note]
        );
        assert_eq!(snap.projected_children_of(source, false), vec![source_test]);
        assert_eq!(snap.projected_children_of(script, false), vec![source_map]);
        assert_eq!(snap.projected_child_count(source, false), Some(1));
        assert!(snap.has_children(source));

        let expanded = HashSet::from([root, source, script]);
        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 100,
            include_ignored: false,
        });
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![root, source, source_test, script, source_map, note]
        );
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            vec![0, 1, 2, 1, 2, 1]
        );
        assert_eq!(snap.visible_row_count(&expanded, false).known, 6);
        assert_eq!(
            snap.visible_row_ids(VisibleRowsQuery {
                expanded: &expanded,
                offset: 0,
                limit: 100,
                include_ignored: false,
            }),
            vec![root, source, source_test, script, source_map, note]
        );
        assert_eq!(
            snap.visible_row_index(source_map, &expanded, false),
            Some(4)
        );
        assert_eq!(
            snap.visible_prefix_match("source.test", Some(source), true, &expanded, false),
            Some(source_test)
        );
    }

    #[test]
    fn file_nesting_conflicts_are_stable_and_never_form_chains() {
        let nesting = FileNestingPolicy::new(
            [
                ("*.js".into(), vec!["shared.map".into()]),
                (
                    "*.ts".into(),
                    vec!["shared.map".into(), "${capture}.test.ts".into()],
                ),
            ],
            true,
        );
        let mut snap =
            StoreSnapshot::empty_with_projection(VisibilityPolicy::project_view(), false, nesting);
        let root = EntryId(1);
        let js = EntryId(2);
        let ts = EntryId(3);
        let shared = EntryId(4);
        let nested_parent_candidate = EntryId(5);
        let grandchild_candidate = EntryId(6);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        for (id, name) in [
            (js, "a.js"),
            (ts, "a.ts"),
            (shared, "shared.map"),
            (nested_parent_candidate, "a.test.ts"),
            (grandchild_candidate, "a.test.test.ts"),
        ] {
            seed(
                &mut snap,
                id,
                Some(root),
                name,
                EntryKind::File,
                0,
                false,
                false,
            );
        }

        // Sibling order wins cross-rule conflicts: a.js sees shared.map first.
        assert_eq!(snap.projected_children_of(js, false), vec![shared]);
        assert_eq!(
            snap.projected_children_of(ts, false),
            vec![nested_parent_candidate]
        );
        // A claimed child cannot become another virtual parent, so no cycles
        // or recursively surprising chains can emerge from overlapping rules.
        assert!(snap
            .projected_children_of(nested_parent_candidate, false)
            .is_empty());
        assert_eq!(
            snap.projected_children_of(root, false),
            vec![js, ts, grandchild_candidate]
        );
    }

    #[test]
    fn file_nesting_respects_visibility_before_claiming_children() {
        let nesting =
            FileNestingPolicy::new([("*.ts".into(), vec!["${capture}.test.ts".into()])], true);
        let mut snap = StoreSnapshot::empty_with_projection(
            VisibilityPolicy {
                show_hidden_files: false,
                show_ignored_files: true,
            },
            false,
            nesting,
        );
        let root = EntryId(1);
        let source = EntryId(2);
        let hidden_test = EntryId(3);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            source,
            Some(root),
            "source.ts",
            EntryKind::File,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            hidden_test,
            Some(root),
            "source.test.ts",
            EntryKind::File,
            0,
            false,
            true,
        );

        assert_eq!(snap.projected_children_of(root, false), vec![source]);
        assert!(snap.projected_children_of(source, false).is_empty());
        assert!(!snap.has_children(source));
        assert_eq!(snap.projected_children_of(source, true), vec![hidden_test]);
    }

    #[test]
    fn compact_folders_and_file_nesting_compose_without_synthetic_ids() {
        let nesting =
            FileNestingPolicy::new([("*.ts".into(), vec!["${capture}.test.ts".into()])], true);
        let mut snap =
            StoreSnapshot::empty_with_projection(VisibilityPolicy::project_view(), true, nesting);
        let root = EntryId(1);
        let a = EntryId(2);
        let b = EntryId(3);
        let source = EntryId(4);
        let test = EntryId(5);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            a,
            Some(root),
            "a",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            b,
            Some(a),
            "b",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            source,
            Some(b),
            "index.ts",
            EntryKind::File,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            test,
            Some(b),
            "index.test.ts",
            EntryKind::File,
            0,
            false,
            false,
        );

        assert_eq!(snap.projected_children_of(root, false), vec![b]);
        assert_eq!(snap.projected_children_of(b, false), vec![source]);
        assert_eq!(snap.projected_children_of(source, false), vec![test]);
        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &HashSet::from([root, b, source]),
            offset: 0,
            limit: 100,
            include_ignored: false,
        });
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![root, b, source, test]
        );
        assert_eq!(
            rows[1].path_segments.as_deref(),
            Some(&["a".into(), "b".into()][..])
        );
    }

    #[test]
    fn visibility_policy_applies_to_every_projection_query() {
        let mut snap = StoreSnapshot::empty_with_visibility(VisibilityPolicy {
            show_hidden_files: false,
            show_ignored_files: true,
        });
        let root = EntryId(1);
        let visible = EntryId(2);
        let hidden = EntryId(3);
        let ignored = EntryId(4);
        let noise = EntryId(5);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            visible,
            Some(root),
            "visible.txt",
            EntryKind::File,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            hidden,
            Some(root),
            ".env",
            EntryKind::File,
            0,
            false,
            true,
        );
        seed(
            &mut snap,
            ignored,
            Some(root),
            "target",
            EntryKind::Directory,
            0,
            true,
            false,
        );
        seed(
            &mut snap,
            noise,
            Some(root),
            ".DS_Store",
            EntryKind::File,
            0,
            false,
            true,
        );
        let expanded = HashSet::from([root]);

        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 100,
            include_ignored: false,
        });
        let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
        assert_eq!(ids, vec![root, visible, ignored]);
        assert_eq!(snap.visible_row_count(&expanded, false).known, 3);
        assert_eq!(snap.visible_row_index(ignored, &expanded, false), Some(2));
        assert_eq!(
            snap.visible_prefix_match("target", None, false, &expanded, false),
            Some(ignored)
        );

        let all = snap.visible_row_ids(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 100,
            include_ignored: true,
        });
        assert_eq!(all, vec![root, visible, hidden, ignored, noise]);
    }

    #[test]
    fn visibility_policy_removes_empty_disclosure_chevrons() {
        let mut snap = StoreSnapshot::empty_with_visibility(VisibilityPolicy {
            show_hidden_files: false,
            show_ignored_files: true,
        });
        let root = EntryId(1);
        let hidden = EntryId(2);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            hidden,
            Some(root),
            ".env",
            EntryKind::File,
            0,
            false,
            true,
        );
        let expanded = HashSet::new();
        assert!(!snap.has_children(root));
        assert_eq!(snap.projected_child_count(root, false), Some(0));
        assert_eq!(snap.projected_child_count(root, true), Some(1));
        assert!(
            !snap
                .visible_rows(VisibleRowsQuery {
                    expanded: &expanded,
                    offset: 0,
                    limit: 10,
                    include_ignored: false,
                })
                .first()
                .unwrap()
                .has_children
        );
        assert!(
            snap.visible_rows(VisibleRowsQuery {
                expanded: &expanded,
                offset: 0,
                limit: 10,
                include_ignored: true,
            })
            .first()
            .unwrap()
            .has_children
        );
    }

    #[test]
    fn compact_projection_preserves_leaf_identity_across_every_query() {
        let mut snap = StoreSnapshot::empty_with_projection(
            VisibilityPolicy::project_view(),
            true,
            FileNestingPolicy::default(),
        );
        let root = EntryId(1);
        let a = EntryId(2);
        let b = EntryId(3);
        let c = EntryId(4);
        let file = EntryId(5);
        seed(
            &mut snap,
            root,
            None,
            "root",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            a,
            Some(root),
            "a",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            b,
            Some(a),
            "b",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            c,
            Some(b),
            "c",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            file,
            Some(c),
            "file.txt",
            EntryKind::File,
            0,
            false,
            false,
        );
        let expanded = HashSet::from([root, c]);

        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 10,
            include_ignored: false,
        });
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![root, c, file]
        );
        assert_eq!(rows[1].depth, 1);
        assert_eq!(
            rows[1].path_segments.as_deref(),
            Some(["a".to_string(), "b".to_string(), "c".to_string()].as_slice())
        );
        assert_eq!(rows[2].depth, 2);
        assert_eq!(snap.visible_row_count(&expanded, false).known, 3);
        assert_eq!(snap.visible_row_index(c, &expanded, false), Some(1));
        assert_eq!(
            snap.visible_prefix_match("a/b", None, false, &expanded, false),
            Some(c)
        );
        assert_eq!(snap.projected_children_of(root, false), vec![c]);
    }

    #[test]
    fn nested_expansion_pending_when_children_missing() {
        let mut snap = StoreSnapshot::empty();
        let a = EntryId(1);
        let b = EntryId(2);
        seed(
            &mut snap,
            a,
            None,
            "a",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            b,
            Some(a),
            "b",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        // b is expanded but has no children map entry — worker hasn't delivered.

        let mut expanded: HashSet<EntryId> = HashSet::new();
        expanded.insert(a);
        expanded.insert(b);

        let vc = snap.visible_row_count(&expanded, false);
        assert_eq!(vc.known, 2);
        assert_eq!(vc.pending_expansions.as_slice(), &[b]);
    }

    #[test]
    fn viewport_offset_and_limit_slice_correctly() {
        let mut snap = StoreSnapshot::empty();
        let r = EntryId(1);
        seed(
            &mut snap,
            r,
            None,
            "r",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        let mut expanded: HashSet<EntryId> = HashSet::new();
        expanded.insert(r);
        let ids: Vec<EntryId> = (0..10)
            .map(|i| {
                let cid = EntryId(i + 2);
                seed(
                    &mut snap,
                    cid,
                    Some(r),
                    &format!("c{:02}", i),
                    EntryKind::File,
                    0,
                    false,
                    false,
                );
                cid
            })
            .collect();

        // Full set is [r, c00, c01, .., c09] — 11 rows. Offset 3 skips [r, c00, c01].
        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 3,
            limit: 4,
            include_ignored: false,
        });
        let sliced: Vec<EntryId> = rows.iter().map(|r| r.id).collect();
        assert_eq!(sliced, vec![ids[2], ids[3], ids[4], ids[5]]);
    }

    #[test]
    fn invisible_expanded_parent_suppresses_children() {
        // Tree: R (visible) -> D (ignored) -> G (visible). Both R and D are in
        // the expanded set — stale entry for D survives after it becomes ignored.
        let mut snap = StoreSnapshot::empty();
        let r = EntryId(1);
        let d = EntryId(2);
        let g = EntryId(3);
        seed(
            &mut snap,
            r,
            None,
            "r",
            EntryKind::Directory,
            0,
            false,
            false,
        );
        seed(
            &mut snap,
            d,
            Some(r),
            "d",
            EntryKind::Directory,
            0,
            true,
            false,
        );
        seed(&mut snap, g, Some(d), "g", EntryKind::File, 0, false, false);

        let mut expanded: HashSet<EntryId> = HashSet::new();
        expanded.insert(r);
        expanded.insert(d);

        // include_ignored=false: D is suppressed, and G must not leak through.
        let vc = snap.visible_row_count(&expanded, false);
        assert_eq!(vc.known, 1);
        assert!(vc.pending_expansions.is_empty());

        let rows = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 10,
            include_ignored: false,
        });
        let ids: Vec<EntryId> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![r]);

        // include_ignored=true: D becomes visible, so descent resumes.
        let vc_all = snap.visible_row_count(&expanded, true);
        assert_eq!(vc_all.known, 3);

        let rows_all = snap.visible_rows(VisibleRowsQuery {
            expanded: &expanded,
            offset: 0,
            limit: 10,
            include_ignored: true,
        });
        let ids_all: Vec<EntryId> = rows_all.iter().map(|r| r.id).collect();
        assert_eq!(ids_all, vec![r, d, g]);
    }

    #[test]
    fn visible_row_count_matches_naive_on_several_trees() {
        // Table-driven cross-check; a proptest variant follows.
        for seed_shape in 0u32..16 {
            let mut snap = StoreSnapshot::empty();
            let mut next_id: u64 = 1;
            let mut expanded: HashSet<EntryId> = HashSet::new();

            let roots = (seed_shape % 3) + 1;
            for r in 0..roots {
                let rid = EntryId(next_id);
                next_id += 1;
                seed(
                    &mut snap,
                    rid,
                    None,
                    &format!("r{}", r),
                    EntryKind::Directory,
                    0,
                    false,
                    false,
                );
                if seed_shape & 1 != 0 {
                    expanded.insert(rid);
                }
                let children = (seed_shape % 4) + 1;
                for c in 0..children {
                    let cid = EntryId(next_id);
                    next_id += 1;
                    let ignored = seed_shape & 2 != 0 && c == 0;
                    seed(
                        &mut snap,
                        cid,
                        Some(rid),
                        &format!("c{}", c),
                        EntryKind::File,
                        0,
                        ignored,
                        false,
                    );
                }
            }
            let include_ignored = seed_shape & 4 != 0;
            let vc = snap.visible_row_count(&expanded, include_ignored).known;
            let naive = snap.visible_row_count_naive(&expanded, include_ignored);
            assert_eq!(vc, naive, "shape {} diverged", seed_shape);
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        #[test]
        fn prop_visible_row_count_matches_naive(
            // Random tree of up to 32 entries. Each node picks a parent from
            // existing ids (None for the first). Flags flip independently.
            shape in proptest::collection::vec(
                (proptest::bool::ANY, proptest::bool::ANY, proptest::bool::ANY, 0usize..32),
                1..32
            ),
            expand_mask in proptest::num::u64::ANY,
            include_ignored in proptest::bool::ANY,
        ) {
            let mut snap = StoreSnapshot::empty();
            let mut ids: Vec<EntryId> = Vec::new();
            for (i, (is_dir, ignored, hidden, parent_pick)) in shape.iter().enumerate() {
                let id = EntryId((i as u64) + 1);
                let parent = if ids.is_empty() {
                    None
                } else {
                    Some(ids[parent_pick % ids.len()])
                };
                let kind = if *is_dir { EntryKind::Directory } else { EntryKind::File };
                seed(&mut snap, id, parent, &format!("n{}", i), kind, 0, *ignored, *hidden);
                ids.push(id);
            }
            let mut expanded: HashSet<EntryId> = HashSet::new();
            for (i, id) in ids.iter().enumerate() {
                if (expand_mask >> (i % 64)) & 1 == 1 {
                    expanded.insert(*id);
                }
            }
            let vc = snap.visible_row_count(&expanded, include_ignored).known;
            let naive = snap.visible_row_count_naive(&expanded, include_ignored);
            proptest::prop_assert_eq!(vc, naive);

            let rows = snap.visible_rows(VisibleRowsQuery {
                expanded: &expanded,
                offset: 0,
                limit: u32::MAX,
                include_ignored,
            });
            let row_ids = snap.visible_row_ids(VisibleRowsQuery {
                expanded: &expanded,
                offset: 0,
                limit: u32::MAX,
                include_ignored,
            });
            proptest::prop_assert_eq!(
                row_ids,
                rows.iter().map(|row| row.id).collect::<Vec<_>>()
            );
            for id in ids {
                let expected = rows
                    .iter()
                    .position(|row| row.id == id)
                    .map(|index| index as u32);
                proptest::prop_assert_eq!(
                    snap.visible_row_index(id, &expanded, include_ignored),
                    expected
                );
            }
        }
    }
}
