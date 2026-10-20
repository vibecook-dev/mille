//! Crash-resume: disk-serialization of `StoreSnapshot`.
//!
//! Pairs with `snapshot.rs` (in-memory types live there). SPEC §9.4:
//! mirror of @parcel/watcher's writeSnapshot + getEventsSince pattern, but
//! file-format and walk use our own crates. Phase 4.4 (next commit) adds
//! the resume-diff walker that reconciles a loaded snapshot against the
//! current on-disk state.
//!
//! Write path is tmp-file + fsync + atomic rename so a mid-crash partial
//! write never replaces a valid snapshot. Format-version gate rejects
//! future-version snapshots with FxError::Unsupported rather than silently
//! misinterpreting bytes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bincode::config as bincode_config;
use serde::{Deserialize, Serialize};

use crate::entry::{Entry, EntryId};
use crate::error::{ErrorCode, FxError};
use crate::snapshot::StoreSnapshot;
use crate::walker::WalkedEntry;

/// Bump when the on-disk shape changes in a way that makes older readers
/// unsafe. Callers get `FxError::Unsupported` on mismatch.
pub const CURRENT_FORMAT_VERSION: u32 = 2;

/// Stable bincode config. The workspace is pinned to bincode 2.x; the
/// `standard()` preset is the same one `entry.rs` already round-trips with,
/// so Entry encoding stays byte-compatible.
fn bincode_cfg() -> bincode_config::Configuration {
    bincode_config::standard()
}

/// Per-root stat snapshot used by `events_since` to detect changes since
/// the snapshot was taken. We capture mtime at root granularity; per-entry
/// diffing happens against the `entries` map directly.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RootStat {
    pub path: PathBuf,
    pub mtime_ms: i64,
}

/// On-disk snapshot format. Versioned so future format bumps fail loudly
/// rather than silently. Shape mirrors `StoreSnapshot` but with plain
/// owned containers (no Arc, no SmallVec) so serde/bincode can drive it
/// mechanically.
#[derive(Serialize, Deserialize, Debug)]
pub struct ResumeSnapshot {
    pub format_version: u32,
    pub mille_version: String,
    pub created_ms: i64,
    pub tree_version: u64,
    pub roots: Vec<EntryId>,
    pub entries: BTreeMap<EntryId, Entry>,
    pub children: BTreeMap<EntryId, Vec<EntryId>>,
    pub direct_child_counts: BTreeMap<EntryId, u32>,
    pub descendant_visible_counts: BTreeMap<EntryId, u32>,
    pub descendant_total_sizes: BTreeMap<EntryId, u64>,
    /// The next EntryId the allocator would have returned. Callers seed
    /// their id_counter with this on restore.
    pub next_entry_id: u64,
    pub root_stats: Vec<RootStat>,
}

/// Convert an in-memory `StoreSnapshot` + allocator/root-stat metadata
/// into the serde-ready `ResumeSnapshot`. Deref Arc<Entry> into owned
/// `Entry` so bincode can encode without chasing pointers.
pub(crate) fn snapshot_to_resume(
    snap: &StoreSnapshot,
    next_entry_id: u64,
    root_stats: &[RootStat],
) -> ResumeSnapshot {
    let mut entries: BTreeMap<EntryId, Entry> = BTreeMap::new();
    for (id, arc) in snap.entries_iter() {
        entries.insert(id, (**arc).clone());
    }

    let mut children: BTreeMap<EntryId, Vec<EntryId>> = BTreeMap::new();
    let mut direct_child_counts: BTreeMap<EntryId, u32> = BTreeMap::new();
    let mut descendant_visible_counts: BTreeMap<EntryId, u32> = BTreeMap::new();
    let mut descendant_total_sizes: BTreeMap<EntryId, u64> = BTreeMap::new();

    for (id, _) in snap.entries_iter() {
        let kids = snap.children_of(id);
        if !kids.is_empty() {
            children.insert(id, kids.to_vec());
        }
        if let Some(c) = snap.direct_child_count(id) {
            direct_child_counts.insert(id, c);
        }
        descendant_visible_counts.insert(id, snap.subtree_visible_count(id));
        descendant_total_sizes.insert(id, snap.subtree_total_size(id));
    }

    ResumeSnapshot {
        format_version: CURRENT_FORMAT_VERSION,
        mille_version: env!("CARGO_PKG_VERSION").to_string(),
        created_ms: now_ms(),
        tree_version: snap.tree_version(),
        roots: snap.roots().to_vec(),
        entries,
        children,
        direct_child_counts,
        descendant_visible_counts,
        descendant_total_sizes,
        next_entry_id,
        root_stats: root_stats.to_vec(),
    }
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Serialize a `StoreSnapshot` to disk at `path`. Atomic: writes to
/// `path.tmp`, fsyncs, renames over `path`. A crash mid-write leaves the
/// previous valid snapshot intact; the tmp file is best-effort cleaned up
/// on any failure.
pub fn write_snapshot(
    snap: &StoreSnapshot,
    next_entry_id: u64,
    root_stats: &[RootStat],
    path: &Path,
) -> Result<(), FxError> {
    require_parent(path)?;
    write_resume(&snapshot_to_resume(snap, next_entry_id, root_stats), path)
}

/// Require the parent to exist — creating it would silently mask a
/// misconfigured caller-supplied path.
fn require_parent(path: &Path) -> Result<(), FxError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(FxError::Io {
                code: ErrorCode::ENOENT,
                path: parent.to_path_buf(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            });
        }
    }
    Ok(())
}

/// [`write_snapshot`] straight from walked entries, without an
/// [`EntryStore`](crate::store::EntryStore).
///
/// Writes what [`populate_store`](crate::walker::populate_store) into a
/// fresh `EntryStore` followed by `write_snapshot(&store.snapshot(), …)`
/// writes — the same ids, parents, children in the store's default sibling
/// order, child counts, visible counts and sizes; only `created_ms`
/// differs — at a fraction of the cost: no per-entry snapshot bookkeeping,
/// no path index, no ancestor walk per entry. (Subtree sums are not capped
/// at the store's 128-level ancestor walk, which only matters below that
/// depth.)
///
/// `root_stats` names the roots, in order; `events_since` pairs the i-th
/// root with the i-th `RootStat`. A root's own entry is taken from `walked`
/// (an entry with `parent_path: None` at that path) or, when absent, made
/// up as a directory with the stat's mtime. Every other entry's
/// `parent_path` must be a root or another walked entry, and every entry
/// without a parent must be a root: anything else is `InvalidInput`, as is
/// a path walked twice.
pub fn write_snapshot_from(
    walked: &[WalkedEntry],
    root_stats: &[RootStat],
    path: &Path,
) -> Result<(), FxError> {
    write_snapshot_from_with(walked, root_stats, path, Durability::Synced)
}

/// [`write_snapshot_from`], choosing how the file reaches the disk.
///
/// On a USB SSD the two `fsync`s are most of a snapshot write: 90–540 ms
/// (median 186 ms) for a 1.2 MB snapshot, against about 1 ms to write and
/// rename it unsynced (flux dogfood, docs/prototype/mille-changes.md
/// Phase D). [`Durability::Unsynced`] is for a snapshot that is only a hint.
pub fn write_snapshot_from_with(
    walked: &[WalkedEntry],
    root_stats: &[RootStat],
    path: &Path,
    durability: Durability,
) -> Result<(), FxError> {
    require_parent(path)?;
    let resume = resume_from_walked(walked, root_stats)?;
    let bytes = bincode::serde::encode_to_vec(&resume, bincode_cfg())
        .map_err(|e| FxError::InternalBug(format!("bincode encode: {e}")))?;
    write_atomically(path, &bytes, durability)
}

/// How a snapshot write reaches the disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// `fsync` the file, rename it into place, `fsync` the directory: a write
    /// that returned `Ok` survives a power loss. [`write_snapshot`]'s
    /// behaviour.
    #[default]
    Synced,
    /// Write and rename, no `fsync`. Still atomic if the process dies (the
    /// path holds the old file or the new one), but after a power loss or a
    /// kernel panic it may hold the previous snapshot, or one that
    /// [`read_snapshot`] rejects. For a snapshot that only saves work: the
    /// caller must treat an unreadable one as absent, and must not depend
    /// on it being the latest.
    Unsynced,
}

/// The `ResumeSnapshot` of [`write_snapshot_from`].
pub(crate) fn resume_from_walked(
    walked: &[WalkedEntry],
    root_stats: &[RootStat],
) -> Result<ResumeSnapshot, FxError> {
    use crate::entry::EntryKind;
    use crate::snapshot::VisibilityPolicy;
    use crate::sort::SiblingOrder;

    // The roots `walked` lacks.
    let made_up: Vec<WalkedEntry> = root_stats
        .iter()
        .filter(|rs| {
            !walked
                .iter()
                .any(|w| w.parent_path.is_none() && w.path == rs.path)
        })
        .map(|rs| WalkedEntry {
            path: rs.path.clone(),
            parent_path: None,
            name: rs
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            kind: EntryKind::Directory,
            size: 0,
            mtime_ms: rs.mtime_ms,
            ctime_ms: rs.mtime_ms,
            is_symlink: false,
            symlink_target_is_dir: None,
            is_readonly: false,
            is_hidden: false,
            depth: 0,
        })
        .collect();

    // Ids as `populate_store` hands them out on a fresh store: in
    // (depth, path) order, from 0.
    let mut ordered: Vec<&WalkedEntry> = walked.iter().chain(made_up.iter()).collect();
    ordered.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.path.cmp(&b.path)));
    let mut ids: HashMap<&Path, EntryId> = HashMap::with_capacity(ordered.len());
    for (i, w) in ordered.iter().enumerate() {
        if ids.insert(w.path.as_path(), EntryId(i as u64)).is_some() {
            return Err(FxError::InvalidInput(format!(
                "path walked twice: {:?}",
                w.path
            )));
        }
    }

    let visibility = VisibilityPolicy::default();
    let mut entries: BTreeMap<EntryId, Entry> = BTreeMap::new();
    let mut parents: Vec<Option<EntryId>> = Vec::with_capacity(ordered.len());
    let mut roots: Vec<Option<EntryId>> = vec![None; root_stats.len()];
    // Each entry's own visible count and size, then folded into its
    // ancestors below.
    let mut visible: Vec<u32> = Vec::with_capacity(ordered.len());
    let mut sizes: Vec<u64> = Vec::with_capacity(ordered.len());
    let mut levels = 0u64;
    for (i, w) in ordered.iter().enumerate() {
        if i == 0 || ordered[i - 1].depth != w.depth {
            levels += 1;
        }
        let id = EntryId(i as u64);
        let parent_id = match &w.parent_path {
            Some(pp) => {
                let parent = *ids.get(pp.as_path()).ok_or_else(|| {
                    FxError::InvalidInput(format!("{:?}: parent {:?} was not walked", w.path, pp))
                })?;
                // The subtree sums below rely on a parent sorting first.
                if parent.0 as usize >= i {
                    return Err(FxError::InvalidInput(format!(
                        "{:?}: depth {} is not below its parent's",
                        w.path, w.depth
                    )));
                }
                Some(parent)
            }
            None => {
                let at = root_stats
                    .iter()
                    .position(|rs| rs.path == w.path)
                    .ok_or_else(|| {
                        FxError::InvalidInput(format!("{:?} has no parent and no RootStat", w.path))
                    })?;
                roots[at] = Some(id);
                None
            }
        };
        let entry = Entry {
            id,
            parent_id,
            name: w.name.clone(),
            kind: w.kind,
            size: w.size,
            mtime_ms: w.mtime_ms,
            ctime_ms: w.ctime_ms,
            symlink_target_is_dir: w.symlink_target_is_dir,
            path_segments: None,
            is_ignored: false,
            is_excluded: false,
            is_readonly: w.is_readonly,
            is_hidden: w.is_hidden,
        };
        visible.push(visibility.includes(&entry) as u32);
        sizes.push(entry.size);
        parents.push(parent_id);
        entries.insert(id, entry);
    }

    // Children grouped by parent in id order, then stably sorted the way the
    // store sorts a level's incoming siblings.
    let order = SiblingOrder::default();
    let mut children: BTreeMap<EntryId, Vec<EntryId>> = BTreeMap::new();
    for (i, parent) in parents.iter().enumerate() {
        if let Some(p) = parent {
            children.entry(*p).or_default().push(EntryId(i as u64));
        }
    }
    for kids in children.values_mut() {
        kids.sort_by(|a, b| order.compare(&entries[a], &entries[b]));
    }
    let direct_child_counts: BTreeMap<EntryId, u32> = children
        .iter()
        .map(|(p, kids)| (*p, kids.len() as u32))
        .collect();

    // Subtree sums, deepest first: a child's id is always above its
    // parent's, since it sorts at a greater depth.
    for i in (0..ordered.len()).rev() {
        if let Some(p) = parents[i] {
            let p = p.0 as usize;
            visible[p] += visible[i];
            sizes[p] += sizes[i];
        }
    }
    let descendant_visible_counts = visible
        .into_iter()
        .enumerate()
        .map(|(i, v)| (EntryId(i as u64), v))
        .collect();
    let descendant_total_sizes = sizes
        .into_iter()
        .enumerate()
        .map(|(i, s)| (EntryId(i as u64), s))
        .collect();

    Ok(ResumeSnapshot {
        format_version: CURRENT_FORMAT_VERSION,
        mille_version: env!("CARGO_PKG_VERSION").to_string(),
        created_ms: now_ms(),
        tree_version: levels,
        // Every RootStat has an entry: walked, or made up above.
        roots: roots.into_iter().flatten().collect(),
        next_entry_id: ordered.len() as u64,
        entries,
        children,
        direct_child_counts,
        descendant_visible_counts,
        descendant_total_sizes,
        root_stats: root_stats.to_vec(),
    })
}

/// Encode `resume` and write it to `path` atomically.
fn write_resume(resume: &ResumeSnapshot, path: &Path) -> Result<(), FxError> {
    let bytes = bincode::serde::encode_to_vec(resume, bincode_cfg())
        .map_err(|e| FxError::InternalBug(format!("bincode encode: {e}")))?;
    write_atomically(path, &bytes, Durability::Synced)
}

/// Write `bytes` to `path` so a crash leaves either the old file or the new
/// one: `path.tmp`, fsync, rename over `path`, fsync of the parent (the
/// fsyncs only when `Synced`).
pub(crate) fn write_atomically(
    path: &Path,
    bytes: &[u8],
    durability: Durability,
) -> Result<(), FxError> {
    let synced = durability == Durability::Synced;
    let tmp_path = path.with_extension("tmp");

    // Cleanup-on-failure closure: best-effort unlink of the tmp file.
    let cleanup = || {
        let _ = std::fs::remove_file(&tmp_path);
    };

    let write_result = (|| -> Result<(), FxError> {
        let mut f =
            File::create(&tmp_path).map_err(|e| ErrorCode::from_io_error(&e, tmp_path.clone()))?;
        f.write_all(bytes)
            .map_err(|e| ErrorCode::from_io_error(&e, tmp_path.clone()))?;
        if synced {
            f.sync_all()
                .map_err(|e| ErrorCode::from_io_error(&e, tmp_path.clone()))?;
        }
        Ok(())
    })();

    if let Err(e) = write_result {
        cleanup();
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let err = ErrorCode::from_io_error(&e, path.to_path_buf());
        cleanup();
        return Err(err);
    }

    // Durability: POSIX requires fsync on the parent directory to make the
    // rename itself durable against power loss. Without this, a crash
    // between the kernel's rename and the directory-metadata flush can
    // leave the dirent pointing at the old file even though we returned Ok.
    // Best-effort: failure to open/fsync the parent must NOT fail the
    // snapshot — the rename already succeeded; this is belt-and-suspenders.
    //
    // Windows parent-durability (FlushFileBuffers on a directory handle)
    // has different semantics and is tracked as a PLAN 13.x follow-up.
    #[cfg(unix)]
    {
        if synced {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    if let Ok(dir) = std::fs::File::open(parent) {
                        let _ = dir.sync_all();
                    }
                }
            }
        }
    }

    Ok(())
}

/// Load a `ResumeSnapshot` from disk. Returns `FxError::Unsupported` if the
/// on-disk format_version doesn't match `CURRENT_FORMAT_VERSION`.
pub fn read_snapshot(path: &Path) -> Result<ResumeSnapshot, FxError> {
    let mut f = File::open(path).map_err(|e| ErrorCode::from_io_error(&e, path.to_path_buf()))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .map_err(|e| ErrorCode::from_io_error(&e, path.to_path_buf()))?;

    // Peek the leading field before decoding the version-specific body. A
    // provenance field was added to Entry in format v2, so trying to decode a
    // v1 body as v2 would otherwise fail with UnexpectedEnd before callers
    // receive the intended, actionable version-mismatch error.
    let (format_version, _used): (u32, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode_cfg())
            .map_err(|e| FxError::InternalBug(format!("bincode version decode: {e}")))?;
    if format_version != CURRENT_FORMAT_VERSION {
        return Err(FxError::Unsupported(format!(
            "snapshot format v{}, expected v{}",
            format_version, CURRENT_FORMAT_VERSION
        )));
    }

    let (resume, _used): (ResumeSnapshot, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode_cfg())
            .map_err(|e| FxError::InternalBug(format!("bincode decode: {e}")))?;

    Ok(resume)
}

// ============================================================================
// events_since — resume-diff walk (commit 4.4).
// ============================================================================

/// Event kinds emitted by the resume walk. Shaped to match
/// `watcher::FsChangeEvent` semantically so callers can forward into the
/// same downstream pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeEvent {
    /// Path exists now but didn't in the snapshot.
    Appeared { path: PathBuf },
    /// Path was in the snapshot but no longer exists.
    Disappeared { path: PathBuf },
    /// Path exists in both but mtime / size differs.
    Modified { path: PathBuf },
    /// Root itself disappeared — caller should treat subtree as gone.
    RootDisappeared { root: PathBuf },
}

/// Materialize the absolute path of an entry by walking up its parent chain
/// until we hit a root. Snapshots store the root's on-disk path on
/// `RootStat`; intermediate entries only carry their segment name, so we
/// join `root_stats[idx].path + segment-chain`.
///
/// Returns None if the id isn't in the snapshot, or if the ancestor chain
/// is cyclic / terminates in a non-root (shouldn't happen for snapshots
/// produced by `write_snapshot`).
fn entry_path(snap: &ResumeSnapshot, id: EntryId) -> Option<PathBuf> {
    // Depth cap mirrors snapshot::MAX_ANCESTOR_WALK — defends against cycles.
    const MAX_HOPS: usize = 128;

    // Walk up, collecting segments child→root. Stop when we hit an entry
    // whose parent_id is None (a root in the store sense).
    let mut segments: Vec<&str> = Vec::new();
    let mut cur = id;
    let mut root_id: Option<EntryId> = None;
    for _ in 0..MAX_HOPS {
        let entry = snap.entries.get(&cur)?;
        segments.push(entry.name.as_str());
        match entry.parent_id {
            None => {
                root_id = Some(cur);
                break;
            }
            Some(p) => cur = p,
        }
    }
    let root_id = root_id?;

    // Find the on-disk path of this root via its position in snap.roots.
    let root_pos = snap.roots.iter().position(|&r| r == root_id)?;
    let root_path = snap.root_stats.get(root_pos).map(|rs| rs.path.clone())?;

    // segments is child→root-segment. Drop the root segment (last pushed) —
    // its on-disk path comes from RootStat, not from the entry's own name.
    segments.pop();
    segments.reverse();

    let mut out = root_path;
    for seg in segments {
        out.push(seg);
    }
    Some(out)
}

/// Walk each root in the snapshot and diff it against the current on-disk
/// state. Returns the set of events a just-resumed watcher would need to
/// emit to bring subscribers up to date. Output is sorted by (variant,
/// path) so replay into the event pipeline is deterministic.
///
/// A directory below a root that does not answer (stalled, dataless, an OS
/// error) fails the call with [`FxError::Incomplete`]: its children are
/// unknown, and diffing them as absent would report every one of them
/// `Disappeared` — "the drive is asleep" replayed as "everything was
/// deleted".
pub fn events_since(snap: &ResumeSnapshot) -> Result<Vec<ResumeEvent>, FxError> {
    events_since_bounded(snap, crate::read_boundary::ReadBoundary::shared_default())
}

/// [`events_since`] through a caller-owned boundary (deadline, dataless
/// policy; the test seam).
pub(crate) fn events_since_bounded(
    snap: &ResumeSnapshot,
    reads: &crate::read_boundary::ReadBoundary,
) -> Result<Vec<ResumeEvent>, FxError> {
    use crate::walker::{walk_bounded, WalkOptions};

    let mut out: Vec<ResumeEvent> = Vec::new();

    // Build the snapshot path-set once: (mtime_ms, size) keyed by absolute
    // path. Entries whose path can't be materialized (unreachable from any
    // root) are skipped — they can't be diffed against disk either way.
    let mut snap_paths: HashMap<PathBuf, (i64, u64)> = HashMap::with_capacity(snap.entries.len());
    for (id, entry) in snap.entries.iter() {
        if let Some(p) = entry_path(snap, *id) {
            snap_paths.insert(p, (entry.mtime_ms, entry.size));
        }
    }

    for root_stat in &snap.root_stats {
        let root = &root_stat.path;

        // Root-gone short-circuit: before walking, check the root itself.
        // Missing root → emit RootDisappeared and skip per-entry diffing for
        // this subtree (would otherwise flood with Disappeared events).
        if !root.exists() {
            out.push(ResumeEvent::RootDisappeared { root: root.clone() });
            continue;
        }

        let walked = match walk_bounded(
            root,
            WalkOptions {
                include_hidden: true,
                include_root: true,
                ..WalkOptions::default()
            },
            None,
            reads,
        )
        .and_then(crate::walker::WalkReport::into_complete)
        {
            Ok(w) => w,
            Err(FxError::Io {
                code: ErrorCode::ENOENT,
                ..
            }) => {
                out.push(ResumeEvent::RootDisappeared { root: root.clone() });
                continue;
            }
            Err(e) => return Err(e),
        };

        // Collect the disk view keyed by absolute path.
        let mut disk_paths: HashMap<PathBuf, (i64, u64)> = HashMap::with_capacity(walked.len());
        for w in &walked {
            disk_paths.insert(w.path.clone(), (w.mtime_ms, w.size));
        }

        // Scope the snapshot-side path-set to this root's subtree so we
        // don't mis-flag entries from sibling roots as Disappeared here.
        let snap_subtree: HashSet<&PathBuf> =
            snap_paths.keys().filter(|p| p.starts_with(root)).collect();

        // Appeared: on disk, not in snapshot.
        for p in disk_paths.keys() {
            if !snap_paths.contains_key(p) {
                out.push(ResumeEvent::Appeared { path: p.clone() });
            }
        }

        // Disappeared + Modified.
        for snap_p in snap_subtree {
            match disk_paths.get(snap_p) {
                None => out.push(ResumeEvent::Disappeared {
                    path: snap_p.clone(),
                }),
                Some(&(disk_mtime, disk_size)) => {
                    let (snap_mtime, snap_size) = snap_paths[snap_p];
                    if disk_mtime != snap_mtime || disk_size != snap_size {
                        out.push(ResumeEvent::Modified {
                            path: snap_p.clone(),
                        });
                    }
                }
            }
        }
    }

    // Deterministic output so replay into the event pipeline is stable.
    out.sort_by(|a, b| {
        fn key(e: &ResumeEvent) -> (u8, &Path) {
            match e {
                ResumeEvent::Appeared { path } => (0, path),
                ResumeEvent::Disappeared { path } => (1, path),
                ResumeEvent::Modified { path } => (2, path),
                ResumeEvent::RootDisappeared { root } => (3, root),
            }
        }
        key(a).cmp(&key(b))
    });

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryId, EntryKind};
    use crate::store::EntryStore;
    use crate::walker::{walk, WalkOptions};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    // Helper: shove a small tree into a fresh EntryStore. Returns the
    // EntryStore and the allocated root id.
    fn build_small_tree() -> (EntryStore, EntryId) {
        let store = EntryStore::new();
        let root = store
            .insert(
                "/root".into(),
                Entry {
                    id: EntryId(0),
                    parent_id: None,
                    name: "root".into(),
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_ms: 100,
                    ctime_ms: 100,
                    symlink_target_is_dir: None,
                    path_segments: None,
                    is_ignored: false,
                    is_excluded: false,
                    is_readonly: false,
                    is_hidden: false,
                },
            )
            .unwrap();
        store
            .insert(
                "/root/a".into(),
                Entry {
                    id: EntryId(0),
                    parent_id: Some(root),
                    name: "a".into(),
                    kind: EntryKind::File,
                    size: 42,
                    mtime_ms: 200,
                    ctime_ms: 200,
                    symlink_target_is_dir: None,
                    path_segments: None,
                    is_ignored: false,
                    is_excluded: false,
                    is_readonly: false,
                    is_hidden: false,
                },
            )
            .unwrap();
        (store, root)
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (store, _root) = build_small_tree();
        let snap = store.snapshot();
        let td = TempDir::new().unwrap();
        let path = td.path().join("snap.bin");

        let root_stats = vec![RootStat {
            path: "/root".into(),
            mtime_ms: 100,
        }];

        write_snapshot(&snap, 99, &root_stats, &path).unwrap();
        let loaded = read_snapshot(&path).unwrap();

        assert_eq!(loaded.format_version, CURRENT_FORMAT_VERSION);
        assert_eq!(loaded.tree_version, snap.tree_version());
        assert_eq!(loaded.roots, snap.roots().to_vec());
        assert_eq!(loaded.entries.len(), snap.entry_count());
        assert_eq!(loaded.next_entry_id, 99);
        assert_eq!(loaded.root_stats, root_stats);
    }

    #[test]
    fn write_is_atomic_on_rename_success() {
        let (store, _root) = build_small_tree();
        let snap = store.snapshot();
        let td = TempDir::new().unwrap();
        let path = td.path().join("snap.bin");

        write_snapshot(&snap, 0, &[], &path).unwrap();

        assert!(path.exists(), "final snapshot file must exist");
        assert!(
            !path.with_extension("tmp").exists(),
            "tmp must be gone after successful rename"
        );
    }

    #[test]
    fn read_from_missing_path_errors() {
        let td = TempDir::new().unwrap();
        let missing = td.path().join("does-not-exist.bin");
        let err = read_snapshot(&missing).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ENOENT);
    }

    #[test]
    fn read_format_version_mismatch_errors() {
        // Hand-craft a synthetic ResumeSnapshot with a bogus version and
        // write it directly so we can verify the gate.
        let td = TempDir::new().unwrap();
        let path = td.path().join("bogus.bin");

        let resume = ResumeSnapshot {
            format_version: 999,
            mille_version: "fake".into(),
            created_ms: 0,
            tree_version: 0,
            roots: vec![],
            entries: BTreeMap::new(),
            children: BTreeMap::new(),
            direct_child_counts: BTreeMap::new(),
            descendant_visible_counts: BTreeMap::new(),
            descendant_total_sizes: BTreeMap::new(),
            next_entry_id: 0,
            root_stats: vec![],
        };
        let bytes = bincode::serde::encode_to_vec(&resume, bincode_cfg()).unwrap();
        fs::write(&path, bytes).unwrap();

        let err = read_snapshot(&path).unwrap_err();
        assert!(matches!(err, FxError::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn legacy_format_is_rejected_before_decoding_its_entry_shape() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("legacy.bin");
        let version_only = bincode::serde::encode_to_vec(1u32, bincode_cfg()).unwrap();
        fs::write(&path, version_only).unwrap();

        let err = read_snapshot(&path).unwrap_err();
        assert!(matches!(err, FxError::Unsupported(_)), "got {err:?}");
        assert!(err.to_string().contains("format v1"));
    }

    #[test]
    fn roundtrip_preserves_tree_version_and_roots() {
        let (store, root) = build_small_tree();
        let snap = store.snapshot();
        let td = TempDir::new().unwrap();
        let path = td.path().join("snap.bin");

        write_snapshot(&snap, 0, &[], &path).unwrap();
        let loaded = read_snapshot(&path).unwrap();

        assert_eq!(loaded.tree_version, snap.tree_version());
        assert_eq!(loaded.roots, vec![root]);
    }

    #[test]
    fn roundtrip_preserves_summary_caches() {
        let (store, root) = build_small_tree();
        let snap = store.snapshot();
        let td = TempDir::new().unwrap();
        let path = td.path().join("snap.bin");

        write_snapshot(&snap, 0, &[], &path).unwrap();
        let loaded = read_snapshot(&path).unwrap();

        // Root subtree: 2 visible (root + a), 42 bytes total.
        assert_eq!(
            loaded.descendant_visible_counts.get(&root).copied(),
            Some(2)
        );
        assert_eq!(loaded.descendant_total_sizes.get(&root).copied(), Some(42));
    }

    #[test]
    fn write_snapshot_removes_tmp_and_fsyncs_parent() {
        // Documents the write_snapshot durability contract: after Ok,
        // the target is in place, the .tmp sibling is gone, and the file
        // is readable back. The parent-dir fsync itself is invisible to
        // userspace — we can't reproduce power loss in a unit test — but
        // this pins down the observable post-conditions so regressions in
        // the rename/cleanup sequence are caught.
        let (store, _root) = build_small_tree();
        let snap = store.snapshot();
        let td = TempDir::new().unwrap();
        let path = td.path().join("snap.bin");

        write_snapshot(&snap, 7, &[], &path).unwrap();

        assert!(path.exists(), "target snapshot file must exist after Ok");
        assert!(
            !path.with_extension("tmp").exists(),
            "tmp sibling must be gone after successful rename"
        );
        let loaded = read_snapshot(&path).expect("target must be readable back");
        assert_eq!(loaded.next_entry_id, 7);
    }

    #[test]
    fn write_to_missing_parent_errors_enoent() {
        let (store, _root) = build_small_tree();
        let snap = store.snapshot();
        let td = TempDir::new().unwrap();
        let path = td.path().join("nope").join("snap.bin");

        let err = write_snapshot(&snap, 0, &[], &path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ENOENT);
    }

    // ===== events_since tests (commit 4.4) =====

    /// Walk a real directory, populate an EntryStore, build a ResumeSnapshot.
    fn snapshot_of_real_dir(root: &Path) -> ResumeSnapshot {
        let store = EntryStore::new();
        let walked = walk(
            root,
            WalkOptions {
                include_hidden: true,
                include_root: true,
                ..WalkOptions::default()
            },
        )
        .unwrap();
        crate::walker::populate_store(&store, root, &walked, None).unwrap();

        // populate_store drained the id counter; the next_entry_id we report
        // is the count of entries allocated (ids 0, 1, ... N-1).
        let counter = AtomicU64::new(walked.len() as u64);

        let snap = store.snapshot();
        let root_mtime = fs::metadata(root)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let root_stats = vec![RootStat {
            path: root.to_path_buf(),
            mtime_ms: root_mtime,
        }];

        snapshot_to_resume(&snap, counter.load(Ordering::Relaxed), &root_stats)
    }

    #[test]
    fn events_since_empty_when_nothing_changed() {
        let td = TempDir::new().unwrap();
        fs::write(td.path().join("a.txt"), b"hi").unwrap();
        fs::create_dir(td.path().join("sub")).unwrap();
        fs::write(td.path().join("sub").join("b.txt"), b"world").unwrap();

        let snap = snapshot_of_real_dir(td.path());
        let events = events_since(&snap).unwrap();
        assert!(
            events.is_empty(),
            "unchanged tree must not emit events, got {events:?}"
        );
    }

    #[test]
    fn events_since_detects_new_file() {
        let td = TempDir::new().unwrap();
        fs::write(td.path().join("keep.txt"), b"hi").unwrap();
        let snap = snapshot_of_real_dir(td.path());

        fs::write(td.path().join("fresh.txt"), b"new!").unwrap();

        let events = events_since(&snap).unwrap();
        let appeared: Vec<&PathBuf> = events
            .iter()
            .filter_map(|e| match e {
                ResumeEvent::Appeared { path } => Some(path),
                _ => None,
            })
            .collect();
        assert!(
            appeared.iter().any(|p| p.ends_with("fresh.txt")),
            "expected Appeared(fresh.txt), got {events:?}"
        );
    }

    #[test]
    fn events_since_detects_deleted_file() {
        let td = TempDir::new().unwrap();
        fs::write(td.path().join("goner.txt"), b"bye").unwrap();
        fs::write(td.path().join("keeper.txt"), b"still here").unwrap();
        let snap = snapshot_of_real_dir(td.path());

        fs::remove_file(td.path().join("goner.txt")).unwrap();

        let events = events_since(&snap).unwrap();
        let gone: Vec<&PathBuf> = events
            .iter()
            .filter_map(|e| match e {
                ResumeEvent::Disappeared { path } => Some(path),
                _ => None,
            })
            .collect();
        assert!(
            gone.iter().any(|p| p.ends_with("goner.txt")),
            "expected Disappeared(goner.txt), got {events:?}"
        );
    }

    #[test]
    fn events_since_detects_modified_file() {
        let td = TempDir::new().unwrap();
        let f = td.path().join("mut.txt");
        fs::write(&f, b"v1").unwrap();
        let snap = snapshot_of_real_dir(td.path());

        // Alter size (always flags Modified even on coarse mtime filesystems).
        // Brief sleep improves mtime-diff coverage on sub-second granularity.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&f, b"v2-longer-content").unwrap();

        let events = events_since(&snap).unwrap();
        let modified: Vec<&PathBuf> = events
            .iter()
            .filter_map(|e| match e {
                ResumeEvent::Modified { path } => Some(path),
                _ => None,
            })
            .collect();
        assert!(
            modified.iter().any(|p| p.ends_with("mut.txt")),
            "expected Modified(mut.txt), got {events:?}"
        );
    }

    #[test]
    fn events_since_detects_root_gone() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("doomed");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.txt"), b"bye").unwrap();

        let snap = snapshot_of_real_dir(&root);

        fs::remove_dir_all(&root).unwrap();

        let events = events_since(&snap).unwrap();
        let root_gone: Vec<&PathBuf> = events
            .iter()
            .filter_map(|e| match e {
                ResumeEvent::RootDisappeared { root } => Some(root),
                _ => None,
            })
            .collect();
        assert_eq!(
            root_gone.len(),
            1,
            "expected one RootDisappeared, got {events:?}"
        );
        assert_eq!(*root_gone[0], root);
        // No per-entry Disappeared spam — the whole subtree is folded.
        let per_entry_gone: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ResumeEvent::Disappeared { .. }))
            .collect();
        assert!(
            per_entry_gone.is_empty(),
            "root-gone short-circuits per-entry Disappeared, got {per_entry_gone:?}"
        );
    }

    #[test]
    fn events_since_handles_multiple_roots() {
        let td_a = TempDir::new().unwrap();
        let td_b = TempDir::new().unwrap();
        fs::write(td_a.path().join("a.txt"), b"A").unwrap();
        fs::write(td_b.path().join("b.txt"), b"B").unwrap();

        // Populate both roots into one EntryStore so they share the entries map.
        let store = EntryStore::new();
        for root in [td_a.path(), td_b.path()] {
            let walked = walk(
                root,
                WalkOptions {
                    include_hidden: true,
                    include_root: true,
                    ..WalkOptions::default()
                },
            )
            .unwrap();
            crate::walker::populate_store(&store, root, &walked, None).unwrap();
        }

        let snap = store.snapshot();
        let root_stats = vec![
            RootStat {
                path: td_a.path().to_path_buf(),
                mtime_ms: 0,
            },
            RootStat {
                path: td_b.path().to_path_buf(),
                mtime_ms: 0,
            },
        ];
        let resume = snapshot_to_resume(&snap, 0, &root_stats);

        // Mutate only root A; root B stays untouched.
        fs::write(td_a.path().join("new.txt"), b"x").unwrap();

        let events = events_since(&resume).unwrap();
        let in_b: Vec<_> = events
            .iter()
            .filter(|e| match e {
                ResumeEvent::Appeared { path }
                | ResumeEvent::Disappeared { path }
                | ResumeEvent::Modified { path } => path.starts_with(td_b.path()),
                ResumeEvent::RootDisappeared { root } => root.starts_with(td_b.path()),
            })
            .collect();

        assert!(
            events.iter().any(|e| matches!(
                e,
                ResumeEvent::Appeared { path } if path.ends_with("new.txt")
            )),
            "expected Appeared(new.txt) in root A, got {events:?}"
        );
        assert!(
            in_b.is_empty(),
            "root B should have no events, got {in_b:?}"
        );
    }

    /// A stalled subdirectory used to vanish from the walk, and every file
    /// the snapshot knew below it came back `Disappeared`.
    #[test]
    fn events_since_fails_incomplete_instead_of_reporting_a_stalled_subtree_deleted() {
        use crate::read_boundary::tests::{boundary, Gate};
        use crate::read_boundary::{ReadFailure, ReadKind};
        use std::sync::Arc;

        let td = TempDir::new().unwrap();
        let season = td.path().join("Show/Season 01");
        fs::create_dir_all(&season).unwrap();
        for ep in 1..=3 {
            fs::write(season.join(format!("S01E0{ep}.mkv")), b"x").unwrap();
        }
        let snap = snapshot_of_real_dir(td.path());

        let gate = Arc::new(Gate::default());
        gate.park(ReadKind::ListDir, &season);
        let reads = boundary(&gate, 150);
        let err = events_since_bounded(&snap, &reads).unwrap_err();
        match &err {
            FxError::Incomplete { unread } => {
                assert_eq!(unread.len(), 1);
                assert_eq!(unread[0].path, season);
                assert!(matches!(unread[0].failure, ReadFailure::TimedOut { .. }));
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
        assert_eq!(err.code(), ErrorCode::ETIMEDOUT);
        gate.release_all();

        // Once the directory answers, nothing changed and nothing is reported.
        let start = std::time::Instant::now();
        while reads.stuck_reads() > 0 {
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(events_since_bounded(&snap, &reads).unwrap().is_empty());
    }

    // ===== write_snapshot_from =====

    /// A tree that exercises every field the store derives: nesting, sizes,
    /// hidden and noise names (visible counts), natural order with digits
    /// and folders on top (sibling order), and a link.
    fn varied_tree() -> TempDir {
        let td = TempDir::new().unwrap();
        let root = td.path();
        for dir in [
            "Show/Season 10",
            "Show/Season 2",
            "Show/.hidden",
            "Movies/Heat (1995)",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let files: [(&str, usize); 9] = [
            ("Show/Season 2/ep10.mkv", 10),
            ("Show/Season 2/ep2.mkv", 2),
            ("Show/Season 2/Ep1.mkv", 1),
            ("Show/Season 10/ep1.mkv", 7),
            ("Show/.hidden/x.mkv", 5),
            ("Show/.DS_Store", 3),
            ("Movies/Heat (1995)/Heat (1995).mkv", 4096),
            ("Movies/a-file-before-folders.nfo", 9),
            ("top.txt", 0),
        ];
        for (rel, len) in files {
            fs::write(root.join(rel), vec![b'x'; len]).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("Heat (1995)", root.join("Movies/heat-link")).unwrap();
        td
    }

    fn walk_all(root: &Path) -> Vec<WalkedEntry> {
        walk(
            root,
            WalkOptions {
                include_hidden: true,
                include_root: true,
                ..WalkOptions::default()
            },
        )
        .unwrap()
    }

    fn root_stats_of(root: &Path) -> Vec<RootStat> {
        let mtime_ms = fs::metadata(root)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        vec![RootStat {
            path: root.to_path_buf(),
            mtime_ms,
        }]
    }

    fn assert_same_snapshot(got: &ResumeSnapshot, want: &ResumeSnapshot) {
        assert_eq!(got.format_version, want.format_version);
        assert_eq!(got.tree_version, want.tree_version);
        assert_eq!(got.roots, want.roots);
        assert_eq!(got.entries, want.entries);
        assert_eq!(got.children, want.children);
        assert_eq!(got.direct_child_counts, want.direct_child_counts);
        assert_eq!(
            got.descendant_visible_counts,
            want.descendant_visible_counts
        );
        assert_eq!(got.descendant_total_sizes, want.descendant_total_sizes);
        assert_eq!(got.next_entry_id, want.next_entry_id);
        assert_eq!(got.root_stats, want.root_stats);
    }

    /// The store-free path writes exactly what populate_store + write_snapshot
    /// write, field for field (bar `created_ms`), from the same walk, in any
    /// input order.
    #[test]
    fn write_snapshot_from_matches_the_entry_store_path() {
        let td = varied_tree();
        let mut walked = walk_all(td.path());
        let stats = root_stats_of(td.path());
        let want = {
            let store = EntryStore::new();
            let ids = crate::walker::populate_store(&store, td.path(), &walked, None).unwrap();
            assert_eq!(ids.len(), walked.len());
            snapshot_to_resume(&store.snapshot(), walked.len() as u64, &stats)
        };
        assert_same_snapshot(&resume_from_walked(&walked, &stats).unwrap(), &want);
        walked.reverse();
        assert_same_snapshot(&resume_from_walked(&walked, &stats).unwrap(), &want);

        // And through the file, synced or not.
        let out = td.path().join("snap.bin");
        write_snapshot_from(&walked, &stats, &out).unwrap();
        assert!(!out.with_extension("tmp").exists());
        assert_same_snapshot(&read_snapshot(&out).unwrap(), &want);
        let unsynced = td.path().join("unsynced.bin");
        write_snapshot_from_with(&walked, &stats, &unsynced, Durability::Unsynced).unwrap();
        assert!(!unsynced.with_extension("tmp").exists());
        assert_same_snapshot(&read_snapshot(&unsynced).unwrap(), &want);
    }

    /// flux builds no root entry: it is made up from the RootStat, and the
    /// snapshot diffs like the store-built one.
    #[test]
    fn write_snapshot_from_makes_up_a_missing_root_and_diffs_like_the_store_path() {
        let td = varied_tree();
        let snap_dir = TempDir::new().unwrap();
        let out = snap_dir.path().join("snap.bin");
        let walked: Vec<WalkedEntry> = walk_all(td.path())
            .into_iter()
            .filter(|w| w.parent_path.is_some())
            .collect();
        let stats = root_stats_of(td.path());
        write_snapshot_from(&walked, &stats, &out).unwrap();
        let loaded = read_snapshot(&out).unwrap();
        let store_built = snapshot_of_real_dir(td.path());
        // Same tree; only the root's own entry is made up (no ctime, no
        // hidden flag), so compare the rest.
        assert_eq!(loaded.roots, store_built.roots);
        assert_eq!(loaded.children, store_built.children);
        let root = loaded.roots[0];
        for (id, entry) in &store_built.entries {
            if *id != root {
                assert_eq!(&loaded.entries[id], entry);
            }
        }
        assert_eq!(loaded.entries[&root].parent_id, None);
        assert_eq!(loaded.entries[&root].kind, EntryKind::Directory);
        assert!(events_since(&loaded).unwrap().is_empty());

        fs::write(td.path().join("Show/Season 2/ep3.mkv"), b"new").unwrap();
        fs::remove_file(td.path().join("Show/Season 10/ep1.mkv")).unwrap();
        fs::write(
            td.path().join("Movies/a-file-before-folders.nfo"),
            b"longer now",
        )
        .unwrap();
        let got = events_since(&loaded).unwrap();
        assert_eq!(got, events_since(&store_built).unwrap());
        for want in [
            ResumeEvent::Appeared {
                path: td.path().join("Show/Season 2/ep3.mkv"),
            },
            ResumeEvent::Disappeared {
                path: td.path().join("Show/Season 10/ep1.mkv"),
            },
            ResumeEvent::Modified {
                path: td.path().join("Movies/a-file-before-folders.nfo"),
            },
        ] {
            assert!(got.contains(&want), "{want:?} missing from {got:?}");
        }
    }

    #[test]
    fn write_snapshot_from_rejects_malformed_walks() {
        let td = varied_tree();
        let walked = walk_all(td.path());
        let stats = root_stats_of(td.path());

        let mut twice = walked.clone();
        twice.push(walked[1].clone());
        assert!(matches!(
            resume_from_walked(&twice, &stats),
            Err(FxError::InvalidInput(_))
        ));

        let mut orphan = walked.clone();
        orphan.retain(|w| !w.path.ends_with("Show"));
        assert!(matches!(
            resume_from_walked(&orphan, &stats),
            Err(FxError::InvalidInput(_))
        ));

        // A root the caller gave no RootStat for.
        assert!(matches!(
            resume_from_walked(&walked, &[]),
            Err(FxError::InvalidInput(_))
        ));

        let mut shallow = walked.clone();
        for w in &mut shallow {
            if w.path.ends_with("Show/Season 2/ep2.mkv") {
                w.depth = 1;
            }
        }
        assert!(matches!(
            resume_from_walked(&shallow, &stats),
            Err(FxError::InvalidInput(_))
        ));
    }
}
