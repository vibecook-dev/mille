//! Async mutation support for FileExplorer (Phase 5 commit 5.5).
//!
//! Each FileExplorer mutation method lives on `FileExplorer` directly, but
//! the supporting helpers (path resolution, stat → Entry conversion,
//! DeleteOptionsJs type) sit here so `explorer.rs` stays focused on the
//! class lifecycle.
//!
//! Design: mutations perform the OS-level operation via `tokio::fs`, then
//! reconcile the in-memory `EntryStore` by insert/rename/remove. Phase 5
//! scope is leaf-only and non-recursive — directory rename/move through
//! subtrees lands with the Phase 2 walker refactor; recursive delete
//! requires an explicit `options.recursive = true`.

use std::path::{Path, PathBuf};

use napi_derive::napi;

use mille_core::{Entry, EntryId, EntryKind, EntryStore, FxError};

/// Options for `FileExplorer.delete`. Mirrors api.d.ts `DeleteOptions`.
/// `trash` defaults to true: soft-delete into workspace `.mille-trash` so the
/// operation is undoable. `trash: false` permanently removes the path.
#[napi(object)]
pub struct DeleteOptionsJs {
    pub trash: Option<bool>,
    pub recursive: Option<bool>,
}

/// Explicit policy for move/copy operations that may cross workspace roots or
/// encounter an existing destination.
#[napi(object)]
pub struct TransferOptionsJs {
    /// Cross-root transfers are denied unless this is explicitly true.
    pub cross_root: Option<bool>,
    /// `"error"` (default), `"rename"`, `"overwrite"`, `"skip"`, or `"merge"`.
    pub collision: Option<String>,
    /// Host-supplied id for progress/cancel tracking. When set, recursive
    /// copies emit `OP_PROGRESS` / `OP_COMPLETE` warnings and honor
    /// `cancelOperation(operationId)`.
    pub operation_id: Option<String>,
    /// Emit progress warnings. Defaults to true when `operation_id` is set.
    pub report_progress: Option<bool>,
}

/// Resolve an entry identity through the store's exact bidirectional path
/// index. Name-based reconstruction is incorrect when workspace roots share a
/// basename and becomes stale if display aliases are introduced.
pub(crate) fn resolve_entry_path(store: &EntryStore, id: EntryId) -> Option<PathBuf> {
    store.path_for_id(id)
}

/// Build an mille-core `Entry` by stat'ing `path`. The returned Entry has
/// `id = EntryId(0)` — the store's insert() overwrites with a fresh id.
pub(crate) async fn stat_to_entry(
    path: &Path,
    parent_id: Option<EntryId>,
    name: String,
) -> Result<Entry, FxError> {
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|e| crate::error::io_to_fx(e, path.to_path_buf()))?;

    let kind = if meta.is_dir() {
        EntryKind::Directory
    } else if meta.file_type().is_symlink() {
        EntryKind::Symlink
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Unknown
    };

    let size = if kind == EntryKind::File {
        meta.len()
    } else {
        0
    };
    let mtime_ms = mtime_ms_from_meta(&meta);
    let ctime_ms = ctime_ms_from_meta(&meta);
    let symlink_target_is_dir = if kind == EntryKind::Symlink {
        tokio::fs::metadata(path).await.ok().map(|m| m.is_dir())
    } else {
        None
    };
    let is_readonly = meta.permissions().readonly();
    let is_hidden = name.starts_with('.');

    Ok(Entry {
        id: EntryId(0),
        parent_id,
        name,
        kind,
        size,
        mtime_ms,
        ctime_ms,
        symlink_target_is_dir,
        path_segments: None,
        is_ignored: false,
        is_excluded: false,
        is_readonly,
        is_hidden,
    })
}

/// `stat_to_entry` through the bounded read pool (M10): the same record, but
/// a stat that blocks in the kernel answers ETIMEDOUT after the deadline
/// instead of parking the caller. Used by the reveal and root-refresh paths.
pub(crate) fn stat_entry(
    reads: &mille_core::ReadBoundary,
    path: &Path,
    parent_id: Option<EntryId>,
    name: String,
) -> Result<Entry, FxError> {
    let output = reads
        .stat(path, false)
        .map_err(|failure| failure.into_fx(path))?;
    let stat = output
        .as_stat()
        .ok_or_else(|| FxError::InternalBug("bounded stat returned no metadata".into()))?;
    let meta = &stat.metadata;

    let kind = if meta.is_dir() {
        EntryKind::Directory
    } else if meta.file_type().is_symlink() {
        EntryKind::Symlink
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Unknown
    };
    let size = if kind == EntryKind::File {
        meta.len()
    } else {
        0
    };
    let symlink_target_is_dir = if kind == EntryKind::Symlink {
        stat.target.as_ref().map(|target| target.is_dir())
    } else {
        None
    };
    let is_hidden = name.starts_with('.');

    Ok(Entry {
        id: EntryId(0),
        parent_id,
        name,
        kind,
        size,
        mtime_ms: mtime_ms_from_meta(meta),
        ctime_ms: ctime_ms_from_meta(meta),
        symlink_target_is_dir,
        path_segments: None,
        is_ignored: false,
        is_excluded: false,
        is_readonly: meta.permissions().readonly(),
        is_hidden,
    })
}

fn mtime_ms_from_meta(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ctime_ms_from_meta(meta: &std::fs::Metadata) -> i64 {
    meta.created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Convert a numeric `kind` from JS (0=File, 1=Directory) into EntryKind.
/// Returns InvalidInput for anything else — create() only accepts leaves
/// and directories; symlink/unknown aren't creatable via this API.
pub(crate) fn kind_from_u8(kind: u8) -> Result<EntryKind, FxError> {
    match kind {
        0 => Ok(EntryKind::File),
        1 => Ok(EntryKind::Directory),
        _ => Err(FxError::InvalidInput(format!(
            "create() kind must be 0 (File) or 1 (Directory), got {kind}"
        ))),
    }
}
