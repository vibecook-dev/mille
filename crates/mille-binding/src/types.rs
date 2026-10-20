//! JS-boundary `#[napi(object)]` mirrors of mille-core types.
//!
//! Field names stay snake_case in Rust; napi-rs auto-rewrites them to
//! camelCase on the JS side so the emitted shape matches `api.d.ts`.
//!
//! Core Rust types (`mille_core::Entry`, etc.) stay pure-serde for bincode.
//! The `*Js` mirrors here are `#[napi(object)]` for the JS boundary,
//! with `from_core` converters kept `pub(crate)` so mille-core's shape can
//! evolve without disturbing the JS surface.

use napi_derive::napi;

use mille_core::{Entry, EntryId, VisibleRowCount, VisibleRowOut};

/// Narrow an `EntryId(u64)` to `i64` for NAPI. The allocator caps raw ids
/// at 2^53 (api.d.ts contract), so this assertion never fires in practice
/// — but we keep it as a tripwire in case the invariant ever regresses.
#[inline]
fn entry_id_to_i64(id: EntryId) -> i64 {
    let raw = id.raw();
    debug_assert!(
        raw < mille_core::entry::ENTRY_ID_MAX,
        "EntryId exceeded 2^53 JS-safe ceiling: {raw}"
    );
    raw as i64
}

/// Mirror of api.d.ts `Entry`.
#[napi(object)]
pub struct EntryJs {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    /// `EntryKind` numeric variant
    /// (0=File, 1=Directory, 2=Symlink, 3=Unknown, 4=Unavailable).
    pub kind: u8,
    pub size: i64,
    pub mtime_ms: i64,
    pub ctime_ms: i64,
    pub symlink_target_is_dir: Option<bool>,
    pub path_segments: Option<Vec<String>>,
    pub is_ignored: bool,
    pub is_readonly: bool,
    pub is_hidden: bool,
}

impl EntryJs {
    pub(crate) fn from_core(e: &Entry) -> Self {
        Self {
            id: entry_id_to_i64(e.id),
            parent_id: e.parent_id.map(entry_id_to_i64),
            name: e.name.clone(),
            kind: e.kind as u8,
            size: e.size as i64,
            mtime_ms: e.mtime_ms,
            ctime_ms: e.ctime_ms,
            symlink_target_is_dir: e.symlink_target_is_dir,
            path_segments: e.path_segments.clone(),
            is_ignored: e.is_ignored_or_excluded(),
            is_readonly: e.is_readonly,
            is_hidden: e.is_hidden,
        }
    }
}

/// Mirror of api.d.ts `VisibleRow`. Extends `EntryJs` with view metadata.
#[napi(object)]
pub struct VisibleRowJs {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub kind: u8,
    pub size: i64,
    pub mtime_ms: i64,
    pub ctime_ms: i64,
    pub symlink_target_is_dir: Option<bool>,
    pub path_segments: Option<Vec<String>>,
    pub is_ignored: bool,
    pub is_readonly: bool,
    pub is_hidden: bool,
    pub depth: u32,
    pub has_children: bool,
    pub is_expanded: bool,
    /// `true` if the row is a placeholder (real data still in flight).
    pub pending: Option<bool>,
}

impl VisibleRowJs {
    #[allow(dead_code)] // alternate constructor; from_core_row is the live path
    pub(crate) fn from_core(row: &VisibleRowOut, entry: &Entry) -> Self {
        Self {
            id: entry_id_to_i64(entry.id),
            parent_id: entry.parent_id.map(entry_id_to_i64),
            name: entry.name.clone(),
            kind: entry.kind as u8,
            size: entry.size as i64,
            mtime_ms: entry.mtime_ms,
            ctime_ms: entry.ctime_ms,
            symlink_target_is_dir: entry.symlink_target_is_dir,
            path_segments: row
                .path_segments
                .clone()
                .or_else(|| entry.path_segments.clone()),
            is_ignored: entry.is_ignored_or_excluded(),
            is_readonly: entry.is_readonly,
            is_hidden: entry.is_hidden,
            depth: row.depth as u32,
            has_children: row.has_children,
            is_expanded: row.is_expanded,
            pending: None,
        }
    }

    /// Build a VisibleRowJs by resolving the row's id against the snapshot.
    /// Recomputes `is_expanded` against the input set so a stale
    /// `VisibleRowOut::is_expanded` can't drift from the caller's view state.
    pub(crate) fn from_core_row(
        row: &VisibleRowOut,
        snap: &mille_core::StoreSnapshot,
        expanded: &std::collections::HashSet<EntryId>,
    ) -> Self {
        // visible_rows only emits ids whose entry is present in the snapshot.
        let entry = snap
            .get(row.id)
            .expect("visible_rows emitted id without matching entry")
            .as_ref();
        Self {
            id: entry_id_to_i64(entry.id),
            parent_id: entry.parent_id.map(entry_id_to_i64),
            name: entry.name.clone(),
            kind: entry.kind as u8,
            size: entry.size as i64,
            mtime_ms: entry.mtime_ms,
            ctime_ms: entry.ctime_ms,
            symlink_target_is_dir: entry.symlink_target_is_dir,
            path_segments: row
                .path_segments
                .clone()
                .or_else(|| entry.path_segments.clone()),
            is_ignored: entry.is_ignored_or_excluded(),
            is_readonly: entry.is_readonly,
            is_hidden: entry.is_hidden,
            depth: row.depth as u32,
            has_children: row.has_children,
            is_expanded: expanded.contains(&row.id),
            pending: None,
        }
    }
}

/// Mirror of api.d.ts `ListPage`.
#[napi(object)]
pub struct ListPageJs {
    pub entries: Vec<EntryJs>,
    pub total: u32,
    pub has_more: bool,
}

/// Mirror of api.d.ts `VisibleRowCount`.
#[napi(object)]
pub struct VisibleRowCountJs {
    pub known: u32,
    pub pending_expansions: Vec<i64>,
}

impl VisibleRowCountJs {
    #[allow(dead_code)] // wired when JS surface exposes visibleRowCount
    pub(crate) fn from_core(c: &VisibleRowCount) -> Self {
        Self {
            known: c.known,
            pending_expansions: c
                .pending_expansions
                .iter()
                .copied()
                .map(entry_id_to_i64)
                .collect(),
        }
    }
}

/// Mirror of api.d.ts `Decoration`.
#[napi(object)]
pub struct DecorationJs {
    pub badge: Option<String>,
    pub color: Option<String>,
    pub tooltip: Option<String>,
    pub propagate: Option<bool>,
}

/// Mirror of api.d.ts `SearchHit`.
#[napi(object)]
pub struct SearchHitJs {
    pub entry: EntryJs,
    pub score: f64,
    pub matched_indices: Vec<u32>,
}

/// Mirror of api.d.ts `SearchOptions`. `signal` on the public surface is
/// not wired — a synchronous native search has nothing to abort. `kinds`
/// carries `EntryKind` discriminants (0 file, 1 directory, …).
#[napi(object)]
pub struct SearchOptionsJs {
    pub limit: Option<u32>,
    pub include_ignored: Option<bool>,
    pub case_sensitive: Option<bool>,
    pub kinds: Option<Vec<u32>>,
}

/// Mirror of api.d.ts `FileSystemEvent` union.
///
/// NAPI doesn't natively render TS discriminated unions, so we emit a
/// flat object with a `kind` discriminant and all other fields optional.
/// A small TS wrapper in Phase 6 normalizes this back into the
/// api.d.ts union shape for consumers.
///
/// `kind` is one of: 'created' | 'changed' | 'deleted' | 'renamed'
///                 | 'error' | 'warning' | 'overflow'.
#[napi(object)]
pub struct FileSystemEventJs {
    pub kind: String,
    pub id: Option<i64>,
    pub parent_id: Option<i64>,
    pub old_parent_id: Option<i64>,
    pub new_parent_id: Option<i64>,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub entry: Option<EntryJs>,
    pub path: Option<String>,
    /// ErrorCode for `kind: 'error'`, WarningCode for `kind: 'warning'`.
    pub code: Option<String>,
    pub message: Option<String>,
    pub detail: Option<String>,
}

/// Mirror of api.d.ts warning payload. Fires on the `'warning'` channel.
/// `code` is a WarningCode (e.g. `"WNOLSNOTIFY"`); `detail` is a short
/// human-readable follow-up — optional so cheap warnings stay cheap.
#[napi(object)]
pub struct WarningPayloadJs {
    pub code: String,
    pub detail: Option<String>,
}

/// Mirror of api.d.ts error payload. Fires on the `'error'` channel.
/// `code` is an ErrorCode; `message` mirrors `FxError::to_string()`;
/// `path` is the filesystem path involved when applicable.
#[napi(object)]
pub struct ErrorPayloadJs {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

/// Mirror of api.d.ts `ChangeNotice`. Fires on each 'change' /
/// 'change:tree' / 'change:decorations' emission.
#[napi(object)]
pub struct ChangeNoticeJs {
    pub tree_version: u32,
    pub decoration_version: u32,
    pub tree_changed: bool,
    pub decorations_changed: bool,
    pub changed_ids: Vec<i64>,
    pub child_set_changed: Vec<i64>,
    pub decoration_changed_ids: Vec<i64>,
    pub coarse_subtrees: Vec<i64>,
}

/// Mirror of mille_core::ChangeSet. Drained once per coalescer tick via
/// `FileExplorer.takePendingChanges()` and consumed by the per-session
/// delta diff (SPEC §4.9.9). Fields are camelCase on the JS side.
#[napi(object)]
pub struct ChangeSetJs {
    pub changed_ids: Vec<i64>,
    pub subtree_roots_changed: Vec<i64>,
    pub child_set_changed: Vec<i64>,
    pub projection_changed: bool,
    pub reparented_ids: Vec<ReparentJs>,
    pub from_version: u32,
    pub to_version: u32,
}

/// One reparent entry: id plus old/new parent (either may be `null` for
/// root transitions).
#[napi(object)]
pub struct ReparentJs {
    pub id: i64,
    pub old_parent_id: Option<i64>,
    pub new_parent_id: Option<i64>,
}

impl ChangeSetJs {
    pub(crate) fn from_core(cs: &mille_core::ChangeSet) -> Self {
        Self {
            changed_ids: cs
                .changed_ids
                .iter()
                .copied()
                .map(entry_id_to_i64)
                .collect(),
            subtree_roots_changed: cs
                .subtree_roots_changed
                .iter()
                .copied()
                .map(entry_id_to_i64)
                .collect(),
            child_set_changed: cs
                .child_set_changed
                .iter()
                .copied()
                .map(entry_id_to_i64)
                .collect(),
            projection_changed: cs.projection_changed,
            reparented_ids: cs
                .reparented_ids
                .iter()
                .map(|(id, (old, new))| ReparentJs {
                    id: entry_id_to_i64(*id),
                    old_parent_id: old.map(entry_id_to_i64),
                    new_parent_id: new.map(entry_id_to_i64),
                })
                .collect(),
            from_version: cs.from_version as u32,
            to_version: cs.to_version as u32,
        }
    }
}
