//! MirrorSnapshot — JS-visible wrapper around `Arc<StoreSnapshot>`.
//!
//! SPEC §4.9: reads go through an immutable snapshot so the JS side can
//! use identity comparison (`===`) for concurrent-safe rendering. The
//! inner `Arc` is unchanged between deltas, so two `getSnapshot()` calls
//! between the same versions share an identity-equal inner pointer.
//!
//! Wave 2 surface: tree_version, decoration_version (stub), roots(),
//! get_by_id, direct_child_count, has_children. visible_rows /
//! visible_row_count land in wave 3 (commit 5.4). Decorations ship in
//! Phase 9.

use std::collections::HashSet;
use std::sync::Arc;

use napi::bindgen_prelude::Buffer;
use napi_derive::napi;
use serde::Serialize;

use mille_core::{EntryId, StoreSnapshot};

use crate::types::{EntryJs, VisibleRowCountJs, VisibleRowJs};

/// Input shape for `MirrorSnapshot.visibleRows()`. Flat record so JS can pass
/// the expanded-id set + viewport window in one object. Sort options land in
/// Phase 9 (natural sort).
#[napi(object)]
pub struct VisibleRowsOptionsJs {
    pub expanded: Vec<i64>,
    pub offset: u32,
    pub limit: u32,
    pub include_ignored: Option<bool>,
}

/// Immutable snapshot of the tree at a specific tree-version.
#[napi]
pub struct MirrorSnapshot {
    pub(crate) inner: Arc<StoreSnapshot>,
}

#[napi]
impl MirrorSnapshot {
    /// Monotonic tree-version at the moment of capture.
    #[napi(getter, js_name = "treeVersion", catch_unwind)]
    pub fn tree_version(&self) -> u32 {
        self.inner.tree_version() as u32
    }

    /// Decoration version. Phase 9 wires to a real counter; 0 until then.
    #[napi(getter, js_name = "decorationVersion", catch_unwind)]
    pub fn decoration_version(&self) -> u32 {
        0
    }

    #[napi(getter, js_name = "showHiddenFiles", catch_unwind)]
    pub fn show_hidden_files(&self) -> bool {
        self.inner.visibility().show_hidden_files
    }

    #[napi(getter, js_name = "showIgnoredFiles", catch_unwind)]
    pub fn show_ignored_files(&self) -> bool {
        self.inner.visibility().show_ignored_files
    }

    #[napi(getter, js_name = "compactFolders", catch_unwind)]
    pub fn compact_folders(&self) -> bool {
        self.inner.compact_folders()
    }

    /// Workspace roots in the order they were registered.
    #[napi(catch_unwind)]
    pub fn roots(&self) -> Vec<EntryJs> {
        self.inner
            .roots()
            .iter()
            .filter_map(|id| {
                self.inner
                    .get(*id)
                    .map(|arc| EntryJs::from_core(arc.as_ref()))
            })
            .collect()
    }

    /// Lookup an entry by id. Returns None if the id isn't in this snapshot.
    #[napi(js_name = "getById", catch_unwind)]
    pub fn get_by_id(&self, id: i64) -> Option<EntryJs> {
        // i64→u64 is lossless for non-negative ids; negative ids never exist
        // (allocator caps at 2^53, well below i64::MAX) so out-of-range is
        // the caller's problem — they get None from the lookup anyway.
        let eid = EntryId(id as u64);
        self.inner
            .get(eid)
            .map(|arc| EntryJs::from_core(arc.as_ref()))
    }

    /// Direct-child count for a directory id. None if the id isn't known
    /// or isn't a container.
    #[napi(js_name = "directChildCount", catch_unwind)]
    pub fn direct_child_count(&self, id: i64) -> Option<u32> {
        let eid = EntryId(id as u64);
        self.inner.direct_child_count(eid)
    }

    /// True once an authoritative direct-child listing completed. This is
    /// distinct from `hasChildren`: a loaded directory can be empty.
    #[napi(js_name = "directoryChildrenLoaded", catch_unwind)]
    pub fn directory_children_loaded(&self, id: i64) -> bool {
        self.inner.directory_children_loaded(EntryId(id as u64))
    }

    #[napi(js_name = "projectedChildCount", catch_unwind)]
    pub fn projected_child_count(&self, id: i64, include_ignored: Option<bool>) -> Option<u32> {
        self.inner
            .projected_child_count(EntryId(id as u64), include_ignored.unwrap_or(false))
    }

    /// True when the entry has a visible child or is an unloaded directory
    /// that may have children (the disclosure-chevron contract).
    #[napi(js_name = "hasChildren", catch_unwind)]
    pub fn has_children(&self, id: i64) -> bool {
        let eid = EntryId(id as u64);
        self.inner.has_children(eid)
    }

    /// Immediate children of a directory id, in wire (id-allocation) order.
    /// Empty when the id is unknown or not a container. The host walks
    /// the tree via this to serialize the full entry set into the
    /// snapshot frame (Phase 8 commit 8.6).
    #[napi(js_name = "childrenOf", catch_unwind)]
    pub fn children_of(&self, id: i64) -> Vec<i64> {
        let eid = EntryId(id as u64);
        self.inner
            .children_of(eid)
            .iter()
            .map(|i| i.raw() as i64)
            .collect()
    }

    #[napi(js_name = "projectedChildrenOf", catch_unwind)]
    pub fn projected_children_of(&self, id: i64, include_ignored: Option<bool>) -> Vec<i64> {
        self.inner
            .projected_children_of(EntryId(id as u64), include_ignored.unwrap_or(false))
            .into_iter()
            .map(|entry_id| entry_id.raw() as i64)
            .collect()
    }

    /// Flattened viewport rows for the current expanded-set and window.
    /// `include_ignored` defaults to false, matching SPEC §4.9.2.
    #[napi(js_name = "visibleRows", catch_unwind)]
    pub fn visible_rows(&self, options: VisibleRowsOptionsJs) -> Vec<VisibleRowJs> {
        let expanded: HashSet<EntryId> = options
            .expanded
            .iter()
            .map(|id| EntryId(*id as u64))
            .collect();

        let query = mille_core::VisibleRowsQuery {
            expanded: &expanded,
            offset: options.offset,
            limit: options.limit,
            include_ignored: options.include_ignored.unwrap_or(false),
        };

        self.inner
            .visible_rows(query)
            .into_iter()
            .map(|row| VisibleRowJs::from_core_row(&row, self.inner.as_ref(), &expanded))
            .collect()
    }

    /// Flattened visible ids for selection-only consumers. Avoids creating
    /// and marshaling complete row objects.
    #[napi(js_name = "visibleRowIds", catch_unwind)]
    pub fn visible_row_ids(&self, options: VisibleRowsOptionsJs) -> Vec<i64> {
        let expanded: HashSet<EntryId> = options
            .expanded
            .iter()
            .map(|id| EntryId(*id as u64))
            .collect();
        self.inner
            .visible_row_ids(mille_core::VisibleRowsQuery {
                expanded: &expanded,
                offset: options.offset,
                limit: options.limit,
                include_ignored: options.include_ignored.unwrap_or(false),
            })
            .into_iter()
            .map(|id| id.raw() as i64)
            .collect()
    }

    /// Exact logical index in the flattened visible order. Performs one
    /// native DFS and returns no row payloads.
    #[napi(js_name = "visibleRowIndex", catch_unwind)]
    pub fn visible_row_index(
        &self,
        id: i64,
        expanded: Vec<i64>,
        include_ignored: Option<bool>,
    ) -> Option<u32> {
        let expanded_set: HashSet<EntryId> =
            expanded.iter().map(|raw| EntryId(*raw as u64)).collect();
        self.inner.visible_row_index(
            EntryId(id as u64),
            &expanded_set,
            include_ignored.unwrap_or(false),
        )
    }

    /// Payload-free full-order fallback for UI typeahead.
    #[napi(js_name = "visiblePrefixMatch", catch_unwind)]
    pub fn visible_prefix_match(
        &self,
        prefix: String,
        from_id: Option<i64>,
        skip_current: bool,
        expanded: Vec<i64>,
        include_ignored: Option<bool>,
    ) -> Option<i64> {
        let expanded_set: HashSet<EntryId> =
            expanded.iter().map(|raw| EntryId(*raw as u64)).collect();
        self.inner
            .visible_prefix_match(
                &prefix,
                from_id.map(|raw| EntryId(raw as u64)),
                skip_current,
                &expanded_set,
                include_ignored.unwrap_or(false),
            )
            .map(|id| id.raw() as i64)
    }

    /// Bulk-path sibling of `visibleRows` — returns the enriched rows
    /// (entry fields + viewport flags inlined) as a bincode-encoded
    /// `Buffer` instead of marshaling each row through NAPI. For
    /// viewports above `bulk::BINCODE_THRESHOLD` (SPEC §4.8: 100 rows),
    /// one Buffer hop beats per-row object marshaling by a large margin;
    /// the TS wrapper (Phase 6) sniffs the return type and decodes.
    ///
    /// Schema is `Vec<EnrichedRow>` with the bincode `standard()` config
    /// — same config used by `mille_core::resume::write_snapshot`, so the
    /// decoders can share a codec module if convenient.
    ///
    /// `visibleRows` (struct-marshaled) remains the preferred path for
    /// small viewports and stays the default for API parity.
    #[napi(js_name = "visibleRowsBin", catch_unwind)]
    pub fn visible_rows_bin(&self, options: VisibleRowsOptionsJs) -> napi::Result<Buffer> {
        let expanded: HashSet<EntryId> = options
            .expanded
            .iter()
            .map(|id| EntryId(*id as u64))
            .collect();

        let query = mille_core::VisibleRowsQuery {
            expanded: &expanded,
            offset: options.offset,
            limit: options.limit,
            include_ignored: options.include_ignored.unwrap_or(false),
        };

        // VisibleRowOut carries only id + depth + has_children; the TS
        // decoder needs the full Entry data too, so we pair each row
        // with its Entry into a flat serde-friendly record. Drop rows
        // whose id vanished between the query and the lookup — the
        // snapshot is a single Arc, so this shouldn't happen, but the
        // lookup returns Option and skipping is cheaper than unwrapping.
        let rows = self.inner.visible_rows(query);
        let enriched: Vec<EnrichedRow> = rows
            .iter()
            .filter_map(|r| {
                self.inner.get(r.id).map(|entry| EnrichedRow {
                    entry_raw_id: entry.id.raw(),
                    parent_raw_id: entry.parent_id.map(|p| p.raw()),
                    name: entry.name.clone(),
                    kind: entry.kind as u8,
                    size: entry.size,
                    mtime_ms: entry.mtime_ms,
                    ctime_ms: entry.ctime_ms,
                    symlink_target_is_dir: entry.symlink_target_is_dir,
                    path_segments: r
                        .path_segments
                        .clone()
                        .or_else(|| entry.path_segments.clone()),
                    is_ignored: entry.is_ignored_or_excluded(),
                    is_readonly: entry.is_readonly,
                    is_hidden: entry.is_hidden,
                    depth: r.depth,
                    has_children: r.has_children,
                    is_expanded: expanded.contains(&r.id),
                })
            })
            .collect();

        crate::bulk::encode_bulk(&enriched)
    }

    /// Honest scroll-height metric: `known` rows plus any expanded folders
    /// whose children haven't arrived yet (so the UI can render a loading
    /// badge without fudging offsets).
    #[napi(js_name = "visibleRowCount", catch_unwind)]
    pub fn visible_row_count(
        &self,
        expanded: Vec<i64>,
        include_ignored: Option<bool>,
    ) -> VisibleRowCountJs {
        let expanded_set: HashSet<EntryId> =
            expanded.iter().map(|id| EntryId(*id as u64)).collect();

        let result = self
            .inner
            .visible_row_count(&expanded_set, include_ignored.unwrap_or(false));

        VisibleRowCountJs {
            known: result.known,
            pending_expansions: result
                .pending_expansions
                .into_iter()
                .map(|id| id.raw() as i64)
                .collect(),
        }
    }
}

/// Flat, serde-friendly record produced by `visibleRowsBin`. One struct
/// per viewport row combining Entry fields with the row-specific flags
/// (`depth`, `has_children`, `is_expanded`) so the TS decoder has
/// everything it needs in a single pass.
///
/// Field order matters: bincode encodes positionally, so the TS
/// decoder's schema must match exactly. Keep this `pub(crate)` so
/// additions are caught at the binding boundary.
///
/// IDs are sent as `u64` rather than `EntryId` to keep the wire format
/// self-contained (no dependency on mille-core's `EntryId` serde impl from
/// the TS side). u64 is within the JS safe-integer range given the
/// 2^53 cap enforced by `EntryId::alloc_from`.
#[derive(Debug, Serialize)]
pub(crate) struct EnrichedRow {
    pub entry_raw_id: u64,
    pub parent_raw_id: Option<u64>,
    pub name: String,
    /// `EntryKind as u8` — File=0 / Directory=1 / Symlink=2 / Unknown=3.
    pub kind: u8,
    pub size: u64,
    pub mtime_ms: i64,
    pub ctime_ms: i64,
    pub symlink_target_is_dir: Option<bool>,
    pub path_segments: Option<Vec<String>>,
    pub is_ignored: bool,
    pub is_readonly: bool,
    pub is_hidden: bool,
    pub depth: u16,
    pub has_children: bool,
    pub is_expanded: bool,
}
