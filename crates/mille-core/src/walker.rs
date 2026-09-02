// Sequential jwalk-based walker (Phase 2.3).
//
// Produces lightweight `WalkedEntry` records; the coalescer (2.4) converts
// these into `Entry` with allocated IDs and pushes them into `EntryStore`.
// No ignore parsing here (2.5), no parallelism (2.4), no rename/compact (2.6).
//
// v0.2 B3: when an `IgnoreMatcher` is supplied to `walk_with_ignore`, ignore
// rules are applied inside jwalk's `process_read_dir` callback — i.e. BEFORE
// jwalk would resolve a symlinked directory and descend its target. This
// matters for pnpm monorepos: a root `.gitignore` listing `node_modules/`
// now stops the walker at the `node_modules` symlink entry itself, rather
// than letting jwalk follow the link into pnpm's 38k-file store.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use jwalk::{Parallelism, WalkDir};
use parking_lot::Mutex;

use crate::entry::EntryKind;
use crate::error::{ErrorCode, FxError};
use crate::ignore::{IgnoreMatcher, IGNORE_FILE_NAMES};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SymlinkPolicy {
    /// Never descend into symlinked directories.
    Never,
    /// Always descend (risk of cycles — not recommended for real trees).
    Always,
    /// Phase 2.3: behaviorally equivalent to Never. PLAN 13.1 adds (dev,inode)
    /// dedup + ancestor-cycle detection to make this the correct default.
    #[default]
    Smart,
}

#[derive(Clone, Debug)]
pub struct WalkOptions {
    /// Max tree depth (None = unlimited). Root is depth 0.
    pub max_depth: Option<usize>,
    pub follow_symlinks: SymlinkPolicy,
    /// When false, entries whose name starts with `.` are skipped.
    pub include_hidden: bool,
    /// If true, the walk root is returned as the first result.
    pub include_root: bool,
    /// Worker threads for parallel walk. 0 or 1 = sequential. SPEC §4.3
    /// recommends capping at 8 to leave headroom for editor work.
    pub parallelism: usize,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: None,
            follow_symlinks: SymlinkPolicy::default(),
            include_hidden: true,
            include_root: false,
            parallelism: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WalkedEntry {
    pub path: PathBuf,
    /// None only for the walk root when `include_root` is true.
    pub parent_path: Option<PathBuf>,
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    pub ctime_ms: i64,
    pub is_symlink: bool,
    /// When `kind == Symlink`, whether the target is a directory (pnpm /
    /// npm workspace links). `None` for non-symlinks or when the target
    /// can't be resolved. UI uses this to render an expandable chevron
    /// (JetBrains/VS Code "folder symlink" affordance).
    pub symlink_target_is_dir: Option<bool>,
    pub is_readonly: bool,
    pub is_hidden: bool,
    pub depth: u16,
}

pub fn walk(root: &Path, options: WalkOptions) -> Result<Vec<WalkedEntry>, FxError> {
    walk_inner(root, options, None)
}

/// Walk with gitignore rules applied BEFORE symlink resolution.
///
/// When `ignore` is `Some`, each child DirEntry's logical path (parent +
/// file_name) is checked against the matcher inside jwalk's
/// `process_read_dir` callback. Ignored directories get
/// `read_children_path = None` so jwalk never recurses into them — critical
/// for pnpm-style layouts where `node_modules/` is a symlink into a central
/// store and letting jwalk canonicalize first would pull in 10k+ unrelated
/// files.
///
/// The matcher is stacked dynamically during the walk: when a child
/// `.gitignore` / `.ignore` / `.rgignore` is encountered, its rules are
/// added to the stack so nested ignore files work the same as the root
/// one. The caller's matcher is cloned into a shared `Arc<Mutex<_>>` so
/// the rayon worker threads can mutate it cooperatively.
pub fn walk_with_ignore(
    root: &Path,
    options: WalkOptions,
    ignore: &IgnoreMatcher,
) -> Result<Vec<WalkedEntry>, FxError> {
    walk_inner(root, options, Some(ignore))
}

/// Stream a walk in bounded batches. The walker keeps its deterministic
/// per-directory ordering, but callers can publish each completed batch
/// instead of waiting for the entire subtree to materialize.
pub fn walk_batched<C, F>(
    root: &Path,
    options: WalkOptions,
    batch_size: usize,
    cancelled: C,
    on_batch: F,
) -> Result<(), FxError>
where
    C: FnMut() -> bool,
    F: FnMut(Vec<WalkedEntry>) -> Result<(), FxError>,
{
    walk_inner_batched(root, options, None, batch_size, cancelled, on_batch)
}

/// Ignore-aware companion to [`walk_batched`].
pub fn walk_with_ignore_batched<C, F>(
    root: &Path,
    options: WalkOptions,
    ignore: &IgnoreMatcher,
    batch_size: usize,
    cancelled: C,
    on_batch: F,
) -> Result<(), FxError>
where
    C: FnMut() -> bool,
    F: FnMut(Vec<WalkedEntry>) -> Result<(), FxError>,
{
    walk_inner_batched(root, options, Some(ignore), batch_size, cancelled, on_batch)
}

fn walk_inner(
    root: &Path,
    options: WalkOptions,
    ignore: Option<&IgnoreMatcher>,
) -> Result<Vec<WalkedEntry>, FxError> {
    let mut out = Vec::new();
    walk_inner_visit(root, options, ignore, |entry| {
        out.push(entry);
        Ok(())
    })?;
    Ok(out)
}

fn walk_inner_batched<C, F>(
    root: &Path,
    options: WalkOptions,
    ignore: Option<&IgnoreMatcher>,
    batch_size: usize,
    mut cancelled: C,
    mut on_batch: F,
) -> Result<(), FxError>
where
    C: FnMut() -> bool,
    F: FnMut(Vec<WalkedEntry>) -> Result<(), FxError>,
{
    let batch_size = batch_size.max(1);
    let mut batch = Vec::with_capacity(batch_size.min(4096));
    walk_inner_visit(root, options, ignore, |entry| {
        if cancelled() {
            return Err(FxError::Cancelled);
        }
        batch.push(entry);
        if batch.len() >= batch_size {
            on_batch(std::mem::take(&mut batch))?;
            // Be cooperative with the host/runtime thread consuming the
            // immutable snapshots and change notices produced by the batch.
            std::thread::yield_now();
        }
        Ok(())
    })?;
    if cancelled() {
        return Err(FxError::Cancelled);
    }
    if !batch.is_empty() {
        on_batch(batch)?;
    }
    Ok(())
}

fn walk_inner_visit<F>(
    root: &Path,
    options: WalkOptions,
    ignore: Option<&IgnoreMatcher>,
    mut visit: F,
) -> Result<(), FxError>
where
    F: FnMut(WalkedEntry) -> Result<(), FxError>,
{
    if !root.exists() {
        return Err(FxError::Io {
            code: ErrorCode::ENOENT,
            path: root.to_path_buf(),
            source: io::Error::from(io::ErrorKind::NotFound),
        });
    }

    let follow = matches!(options.follow_symlinks, SymlinkPolicy::Always);
    let parallelism = match options.parallelism {
        0 | 1 => Parallelism::Serial,
        n => Parallelism::RayonNewPool(n),
    };
    let mut builder = WalkDir::new(root)
        .parallelism(parallelism)
        .follow_links(follow)
        .skip_hidden(false) // we handle hidden filtering ourselves per `include_hidden`
        .sort(true);
    if let Some(d) = options.max_depth {
        builder = builder.max_depth(d);
    }

    // Shared matcher state for the jwalk closure. Cloning an IgnoreMatcher
    // isn't supported (Gitignore doesn't impl Clone), so we hand the Arc
    // the caller's matcher by rebuilding from its underlying ignore files.
    // We also track discovered ignore files during the walk so a nested
    // `.gitignore` takes effect before its sibling directories are
    // descended.
    // Seed the live matcher with the caller's pre-loaded rules so the
    // process_read_dir closure sees them.
    let matcher_arc: Option<Arc<Mutex<IgnoreMatcher>>> =
        ignore.map(|existing| Arc::new(Mutex::new(existing.clone_rules())));

    if let Some(matcher_arc) = matcher_arc.clone() {
        // The callback fires once per directory with the directory's full
        // child list. B3 wants us to emit the ignored entry itself (e.g.
        // `node_modules` the symlink) but block *further* descent into
        // it when discovered as a child — clear `read_children_path`.
        //
        // jwalk's first process_read_dir call is special: `children` is
        // a 1-element list containing the *walk root*, and
        // `parent_path` is the root's parent. If the walk root is itself
        // gitignored (user expanded `node_modules` / `out`), clearing
        // read_children_path on that entry would prevent listing any
        // children — the expand UI would show an empty folder. Never
        // block descent for the walk root itself.
        let walk_root = root.to_path_buf();
        builder = builder.process_read_dir(move |_depth, parent_path, _rds, children| {
            // First pass: scan for newly-appeared ignore files in this
            // directory so nested rules apply to their siblings.
            for dent in children.iter().flatten() {
                let name = match dent.file_name.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                if !dent.file_type.is_file() {
                    continue;
                }
                if IGNORE_FILE_NAMES.contains(&name) {
                    let ignore_file = parent_path.join(name);
                    let _ = matcher_arc.lock().add_from_file(&ignore_file);
                }
            }

            // Second pass: for each child, decide whether jwalk should
            // descend. This runs BEFORE jwalk resolves any symlinks, so
            // `node_modules/` (a symlink) is checked on its logical path
            // — it matches the `node_modules/` gitignore rule and we
            // clear `read_children_path` to block descent into pnpm's
            // store. The entry itself is still yielded, and the emit
            // phase below will mark it `is_ignored` via the matcher.
            let matcher = matcher_arc.lock();
            for dent in children.iter_mut().flatten() {
                let name = match dent.file_name.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                let logical = parent_path.join(name);
                // For directory-like entries (real dirs or symlinks
                // whose target may be a dir) we pass is_dir=true so
                // `pattern/` rules match. jwalk gives us the
                // symlink's own file_type (not the target), so treat
                // symlinks as dir-like for matching purposes.
                let is_dir_like = dent.file_type.is_dir() || dent.file_type.is_symlink();
                if matcher.is_ignored(&logical, is_dir_like) {
                    // Preserve expand-into-ignored-dir: the walk root
                    // must keep its read_children_path so depth-1
                    // listing works when the user opens node_modules.
                    if logical != walk_root {
                        dent.read_children_path = None;
                    }
                }
            }
        });
    }

    for result in builder {
        let dent = match result {
            Ok(d) => d,
            // Permission-denied on a subtree: skip it and continue the walk.
            Err(_) => continue,
        };

        let depth = dent.depth();
        let is_walk_root = depth == 0;
        if is_walk_root && !options.include_root {
            continue;
        }

        let name = match dent.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        let is_hidden = name.starts_with('.') && !is_walk_root;
        if is_hidden && !options.include_hidden {
            continue;
        }

        // Prefer the DirEntry's non-following file_type when available so
        // symlinks stay classified as Symlink (pnpm workspace links). Fall
        // back to metadata() when the entry type is missing.
        let path = dent.path();
        let entry_ft = dent.file_type();
        let is_symlink = entry_ft.is_symlink();

        // Metadata: for symlinks use lstat-equivalent for size/mtime of the
        // link itself; for target-is-dir check, follow once with metadata.
        let meta = match dent.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ft = meta.file_type();

        let (kind, symlink_target_is_dir) = if is_symlink || ft.is_symlink() {
            // Follow once to learn if this is a folder link (pnpm / npm).
            let target_is_dir = std::fs::metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            (EntryKind::Symlink, Some(target_is_dir))
        } else if ft.is_dir() || entry_ft.is_dir() {
            (EntryKind::Directory, None)
        } else if ft.is_file() || entry_ft.is_file() {
            (EntryKind::File, None)
        } else {
            (EntryKind::Unknown, None)
        };

        let size = if matches!(kind, EntryKind::File) {
            meta.len()
        } else {
            0
        };
        let mtime_ms = meta.modified().ok().and_then(system_time_ms).unwrap_or(0);
        let ctime_ms = ctime_ms_from_metadata(&meta).unwrap_or(mtime_ms);
        let is_readonly = meta.permissions().readonly();

        let parent_path = if is_walk_root {
            None
        } else {
            path.parent().map(|p| p.to_path_buf())
        };

        visit(WalkedEntry {
            path,
            parent_path,
            name,
            kind,
            size,
            mtime_ms,
            ctime_ms,
            is_symlink: is_symlink || ft.is_symlink(),
            symlink_target_is_dir,
            is_readonly,
            is_hidden,
            depth: depth as u16,
        })?;
    }

    Ok(())
}

/// Convert a walked entry set into `Entry` records and push them into
/// `EntryStore`, resolving each entry's `parent_id` from a path-indexed map
/// built during the insertion pass.
///
/// Internally topo-sorts by (depth asc, path asc) before insertion so that
/// parents are always processed before their children, regardless of walker
/// order. `Parallelism::Serial` happens to preserve DFS ordering, but
/// `Parallelism::RayonNewPool(n)` does not — without this sort, children
/// whose parent had not yet been seen would silently become spurious roots.
///
/// Returns the list of allocated EntryIds in insertion order. Phase 2.4
/// coalescer: later phases may extend this to stream via a channel rather
/// than materialize the full Vec first.
pub fn populate_store(
    store: &crate::store::EntryStore,
    root: &Path,
    walked: &[WalkedEntry],
    ignore: Option<&crate::ignore::IgnoreMatcher>,
) -> Result<Vec<crate::entry::EntryId>, FxError> {
    populate_store_with_provenance(store, root, walked, ignore, None)
}

/// Populate while retaining the source of ignore decisions. Repository rules
/// and explorer-configured excludes remain independently reversible even
/// though public consumers observe their union as `isIgnored`.
pub fn populate_store_with_provenance(
    store: &crate::store::EntryStore,
    root: &Path,
    walked: &[WalkedEntry],
    repository_ignore: Option<&crate::ignore::IgnoreMatcher>,
    excludes: Option<&crate::ignore::IgnoreMatcher>,
) -> Result<Vec<crate::entry::EntryId>, FxError> {
    use crate::entry::{Entry, EntryId, EntryKind};
    use std::collections::HashMap;

    let mut path_to_id: HashMap<PathBuf, EntryId> = HashMap::with_capacity(walked.len());
    let mut ids = Vec::with_capacity(walked.len());

    // Topo-sort by (depth asc, path asc). Walker output under
    // Parallelism::RayonNewPool(n) does not guarantee parent-before-child
    // order; without this sort, children whose parent hadn't been inserted
    // would look up None in `path_to_id` and attach as spurious roots.
    let mut ordered: Vec<&WalkedEntry> = walked.iter().collect();
    ordered.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.path.cmp(&b.path)));

    for w in ordered {
        // Look in the current-walk map first, then fall back to the
        // store's path index. The fallback matters for subsequent
        // `populate_from_path` calls (lazy `prefetch` in v0.2 B2):
        // the new walk's `path_to_id` starts empty, so children whose
        // parent was inserted by a prior walk would otherwise get
        // `parent_id = None` and appear as spurious roots.
        let parent_id = match &w.parent_path {
            None => None,
            Some(pp) => path_to_id
                .get(pp)
                .copied()
                .or_else(|| store.get_by_path(pp).map(|e| e.id)),
        };

        // Suppress unused-variable warning on non-ignore paths.
        let _ = root;

        let is_dir_like = w.kind == EntryKind::Directory || w.symlink_target_is_dir == Some(true);
        let is_ignored = match repository_ignore {
            Some(m) => m.is_ignored(&w.path, is_dir_like),
            None => false,
        };
        let is_excluded = match excludes {
            Some(m) => m.is_ignored(&w.path, is_dir_like),
            None => false,
        };

        let entry = Entry {
            id: EntryId(0),
            parent_id,
            name: w.name.clone(),
            kind: w.kind,
            size: w.size,
            mtime_ms: w.mtime_ms,
            ctime_ms: w.ctime_ms,
            symlink_target_is_dir: w.symlink_target_is_dir,
            path_segments: None,
            is_ignored,
            is_excluded,
            is_readonly: w.is_readonly,
            is_hidden: w.is_hidden,
        };
        let id = store.insert(w.path.clone(), entry)?;
        path_to_id.insert(w.path.clone(), id);
        ids.push(id);
    }

    Ok(ids)
}

/// Scan walked entries for `.gitignore` / `.ignore` / `.rgignore` files and
/// build a stacked `IgnoreMatcher` that covers all of them. Convenience for
/// the common "walk → build-matcher → populate" flow.
pub fn build_ignore_matcher_from_walk(
    walked: &[WalkedEntry],
) -> Result<crate::ignore::IgnoreMatcher, FxError> {
    let mut matcher = crate::ignore::IgnoreMatcher::new();
    for w in walked {
        if crate::ignore::IGNORE_FILE_NAMES.contains(&w.name.as_str()) {
            matcher.add_from_file(&w.path)?;
        }
    }
    Ok(matcher)
}

fn system_time_ms(t: SystemTime) -> Option<i64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_millis() as i64),
        Err(e) => Some(-(e.duration().as_millis() as i64)),
    }
}

#[cfg(unix)]
fn ctime_ms_from_metadata(m: &std::fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    let secs = m.ctime();
    let nanos = m.ctime_nsec();
    Some(secs.saturating_mul(1_000) + (nanos / 1_000_000))
}

#[cfg(not(unix))]
fn ctime_ms_from_metadata(m: &std::fs::Metadata) -> Option<i64> {
    m.created().ok().and_then(system_time_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_file(dir: &Path, name: &str, contents: &[u8]) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn walk_empty_dir_returns_nothing() {
        let td = TempDir::new().unwrap();
        let entries = walk(td.path(), WalkOptions::default()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn walk_flat_dir_returns_three_files() {
        let td = TempDir::new().unwrap();
        make_file(td.path(), "a", b"1");
        make_file(td.path(), "b", b"22");
        make_file(td.path(), "c", b"333");
        let entries = walk(td.path(), WalkOptions::default()).unwrap();
        assert_eq!(entries.len(), 3);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn walk_nested_tree_preserves_depth_and_parent() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("a/b")).unwrap();
        make_file(&td.path().join("a/b"), "c.txt", b"hello");

        let entries = walk(td.path(), WalkOptions::default()).unwrap();
        let by_name: std::collections::HashMap<_, _> =
            entries.iter().map(|e| (e.name.clone(), e)).collect();

        assert_eq!(by_name["a"].depth, 1);
        assert_eq!(by_name["b"].depth, 2);
        assert_eq!(by_name["c.txt"].depth, 3);
        assert_eq!(by_name["c.txt"].size, 5);
        assert_eq!(by_name["c.txt"].kind, EntryKind::File);
        assert_eq!(by_name["a"].kind, EntryKind::Directory);
        assert_eq!(by_name["b"].parent_path, Some(td.path().join("a")));
    }

    #[test]
    fn walk_skips_hidden_when_configured() {
        let td = TempDir::new().unwrap();
        make_file(td.path(), ".secret", b"x");
        make_file(td.path(), "visible", b"y");

        let opts = WalkOptions {
            include_hidden: false,
            ..Default::default()
        };
        let entries = walk(td.path(), opts).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["visible"]);
    }

    #[test]
    fn walk_includes_root_when_configured() {
        let td = TempDir::new().unwrap();
        make_file(td.path(), "a", b"");
        let opts = WalkOptions {
            include_root: true,
            ..Default::default()
        };
        let entries = walk(td.path(), opts).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].depth, 0);
        assert_eq!(entries[0].parent_path, None);
        assert_eq!(entries[0].kind, EntryKind::Directory);
    }

    #[test]
    fn walk_max_depth_excludes_deeper_entries() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("a/b")).unwrap();
        make_file(&td.path().join("a/b"), "c.txt", b"");

        let opts = WalkOptions {
            max_depth: Some(1),
            ..Default::default()
        };
        let entries = walk(td.path(), opts).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn walk_missing_root_returns_enoent() {
        let td = TempDir::new().unwrap();
        let missing = td.path().join("does-not-exist");
        let err = walk(&missing, WalkOptions::default()).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ENOENT);
    }

    #[test]
    fn walk_output_is_byte_sorted() {
        let td = TempDir::new().unwrap();
        make_file(td.path(), "c", b"");
        make_file(td.path(), "a", b"");
        make_file(td.path(), "b", b"");
        let entries = walk(td.path(), WalkOptions::default()).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn batched_walk_preserves_order_and_bound() {
        let td = TempDir::new().unwrap();
        for name in ["e", "a", "d", "b", "c"] {
            make_file(td.path(), name, b"");
        }
        let mut batches = Vec::new();
        walk_batched(
            td.path(),
            WalkOptions::default(),
            2,
            || false,
            |batch| {
                batches.push(batch);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(batches.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2, 1]);
        let names: Vec<_> = batches
            .iter()
            .flatten()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn batched_walk_cancels_before_publishing_a_second_page() {
        use std::cell::Cell;

        let td = TempDir::new().unwrap();
        for name in ["a", "b", "c", "d"] {
            make_file(td.path(), name, b"");
        }
        let cancelled = Cell::new(false);
        let mut pages = 0;
        let err = walk_batched(
            td.path(),
            WalkOptions::default(),
            2,
            || cancelled.get(),
            |_| {
                pages += 1;
                cancelled.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(err, FxError::Cancelled));
        assert_eq!(pages, 1);
    }

    #[test]
    fn walk_reports_file_size_accurately() {
        let td = TempDir::new().unwrap();
        make_file(
            td.path(),
            "sized",
            b"0123456789012345678901234567890123456789012",
        );
        let entries = walk(td.path(), WalkOptions::default()).unwrap();
        assert_eq!(entries[0].size, 43);
    }

    #[test]
    fn parallel_walk_returns_same_entry_set_as_serial() {
        use std::collections::HashSet;
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("a/b/c")).unwrap();
        fs::create_dir_all(td.path().join("d")).unwrap();
        for n in ["f1.txt", "f2.txt", "f3.txt"] {
            make_file(&td.path().join("a/b"), n, b"x");
        }
        for n in ["g1.txt", "g2.txt"] {
            make_file(&td.path().join("d"), n, b"y");
        }

        let serial = walk(td.path(), WalkOptions::default()).unwrap();
        let parallel = walk(
            td.path(),
            WalkOptions {
                parallelism: 4,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(serial.len(), parallel.len());
        let ser_paths: HashSet<PathBuf> = serial.into_iter().map(|e| e.path).collect();
        let par_paths: HashSet<PathBuf> = parallel.into_iter().map(|e| e.path).collect();
        assert_eq!(ser_paths, par_paths);
    }

    #[test]
    fn populate_store_links_parents_correctly() {
        use crate::store::EntryStore;
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("a/b")).unwrap();
        make_file(&td.path().join("a/b"), "c.txt", b"hi");

        let walked = walk(td.path(), WalkOptions::default()).unwrap();
        let store = EntryStore::new();
        let ids = populate_store(&store, td.path(), &walked, None).unwrap();
        assert_eq!(ids.len(), walked.len());

        let snap = store.snapshot();
        // /a is a root (its parent is the walk root).
        let a = store.get_by_path(&td.path().join("a")).unwrap();
        assert_eq!(a.parent_id, None);
        assert_eq!(snap.roots().len(), 1);
        assert_eq!(snap.roots()[0], a.id);

        // /a/b has parent_id = a.id.
        let b = store.get_by_path(&td.path().join("a/b")).unwrap();
        assert_eq!(b.parent_id, Some(a.id));
        assert_eq!(snap.children_of(a.id), &[b.id]);

        // /a/b/c.txt has parent_id = b.id.
        let c = store.get_by_path(&td.path().join("a/b/c.txt")).unwrap();
        assert_eq!(c.parent_id, Some(b.id));
        assert_eq!(snap.children_of(b.id), &[c.id]);

        // Subtree summaries: three visible nodes under a.
        assert_eq!(snap.subtree_visible_count(a.id), 3);
        assert_eq!(snap.subtree_total_size(a.id), 2);
    }

    #[test]
    fn populate_store_handles_empty_walk() {
        use crate::store::EntryStore;
        let td = TempDir::new().unwrap();
        let walked = walk(td.path(), WalkOptions::default()).unwrap();
        let store = EntryStore::new();
        let ids = populate_store(&store, td.path(), &walked, None).unwrap();
        assert!(ids.is_empty());
        assert_eq!(store.snapshot().entry_count(), 0);
    }

    #[test]
    fn populate_store_flags_ignored_entries_when_matcher_provided() {
        use crate::store::EntryStore;

        let td = TempDir::new().unwrap();
        fs::write(td.path().join(".gitignore"), "*.log\nbuild/\n").unwrap();
        make_file(td.path(), "debug.log", b"");
        make_file(td.path(), "main.rs", b"");
        fs::create_dir(td.path().join("build")).unwrap();
        make_file(&td.path().join("build"), "output.bin", b"");

        let walked = walk(td.path(), WalkOptions::default()).unwrap();
        let matcher = build_ignore_matcher_from_walk(&walked).unwrap();
        assert_eq!(matcher.matcher_count(), 1);

        let store = EntryStore::new();
        populate_store(&store, td.path(), &walked, Some(&matcher)).unwrap();

        let debug = store.get_by_path(&td.path().join("debug.log")).unwrap();
        let main = store.get_by_path(&td.path().join("main.rs")).unwrap();
        let build = store.get_by_path(&td.path().join("build")).unwrap();
        let output = store
            .get_by_path(&td.path().join("build/output.bin"))
            .unwrap();

        assert!(debug.is_ignored);
        assert!(!main.is_ignored);
        assert!(build.is_ignored);
        assert!(output.is_ignored);

        // Subtree visible counts exclude both ignored AND hidden entries.
        // .gitignore is hidden (starts with '.'), debug.log/build/*.bin are
        // ignored — so only main.rs is visible.
        let snap = store.snapshot();
        let total_visible: u32 = snap
            .roots()
            .iter()
            .map(|&r| snap.subtree_visible_count(r))
            .sum();
        assert_eq!(total_visible, 1);
    }

    #[test]
    fn populate_store_large_fixture_completes_under_budget() {
        use crate::store::EntryStore;
        use std::time::Instant;

        let td = TempDir::new().unwrap();
        // Create a broad tree: 20 top-level dirs × 50 files each = 1020 entries.
        // Enough to exercise the code paths without making tests slow.
        for i in 0..20 {
            let dir = td.path().join(format!("d{:02}", i));
            fs::create_dir(&dir).unwrap();
            for j in 0..50 {
                make_file(&dir, &format!("f{:03}.txt", j), b"hello");
            }
        }

        let walked = walk(td.path(), WalkOptions::default()).unwrap();
        assert_eq!(walked.len(), 20 + 20 * 50);

        let store = EntryStore::new();
        let t0 = Instant::now();
        populate_store(&store, td.path(), &walked, None).unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(store.snapshot().entry_count(), 1020);
        // Debug-mode budget is loose; Phase 12 tightens with release bench.
        assert!(elapsed.as_secs() < 3, "populate took {:?}", elapsed);
    }

    #[test]
    fn populate_store_handles_out_of_order_walked_entries() {
        use crate::store::EntryStore;
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("a/b")).unwrap();
        make_file(&td.path().join("a/b"), "c.txt", b"hi");

        let mut walked = walk(td.path(), WalkOptions::default()).unwrap();
        // Reverse the slice so deepest entries appear first — worst case for
        // parent lookup in an unsorted insertion loop.
        walked.reverse();

        let store = EntryStore::new();
        populate_store(&store, td.path(), &walked, None).unwrap();

        let a = store.get_by_path(&td.path().join("a")).unwrap();
        let b = store.get_by_path(&td.path().join("a/b")).unwrap();
        let c = store.get_by_path(&td.path().join("a/b/c.txt")).unwrap();

        assert_eq!(a.parent_id, None);
        assert_eq!(b.parent_id, Some(a.id));
        assert_eq!(c.parent_id, Some(b.id));

        let snap = store.snapshot();
        assert_eq!(snap.roots().len(), 1);
        assert_eq!(snap.roots()[0], a.id);
        assert_eq!(snap.children_of(a.id), &[b.id]);
        assert_eq!(snap.children_of(b.id), &[c.id]);
    }

    #[test]
    fn populate_store_parallel_walk_preserves_structure() {
        use crate::store::EntryStore;
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join("a/b")).unwrap();
        make_file(&td.path().join("a/b"), "c.txt", b"hi");

        let walked = walk(
            td.path(),
            WalkOptions {
                parallelism: 4,
                ..Default::default()
            },
        )
        .unwrap();

        let store = EntryStore::new();
        let ids = populate_store(&store, td.path(), &walked, None).unwrap();
        assert_eq!(ids.len(), walked.len());

        let snap = store.snapshot();
        let a = store.get_by_path(&td.path().join("a")).unwrap();
        assert_eq!(a.parent_id, None);
        assert_eq!(snap.roots().len(), 1);
        assert_eq!(snap.roots()[0], a.id);

        let b = store.get_by_path(&td.path().join("a/b")).unwrap();
        assert_eq!(b.parent_id, Some(a.id));
        assert_eq!(snap.children_of(a.id), &[b.id]);

        let c = store.get_by_path(&td.path().join("a/b/c.txt")).unwrap();
        assert_eq!(c.parent_id, Some(b.id));
        assert_eq!(snap.children_of(b.id), &[c.id]);

        assert_eq!(snap.subtree_visible_count(a.id), 3);
        assert_eq!(snap.subtree_total_size(a.id), 2);
    }

    #[cfg(unix)]
    #[test]
    fn walk_marks_symlink_with_never_policy() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        make_file(td.path(), "target", b"x");
        symlink(td.path().join("target"), td.path().join("link")).unwrap();

        let opts = WalkOptions {
            follow_symlinks: SymlinkPolicy::Never,
            ..Default::default()
        };
        let entries = walk(td.path(), opts).unwrap();
        let link = entries
            .iter()
            .find(|e| e.name == "link")
            .expect("link entry");
        assert!(link.is_symlink);
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(link.symlink_target_is_dir, Some(false));
    }

    /// pnpm-style package link: symlink whose target is a directory must
    /// carry `symlink_target_is_dir = true` so the UI can expand it.
    #[cfg(unix)]
    #[test]
    fn walk_marks_symlink_to_dir() {
        use std::os::unix::fs::symlink;
        let td = TempDir::new().unwrap();
        fs::create_dir(td.path().join("pkg")).unwrap();
        make_file(&td.path().join("pkg"), "index.js", b"x");
        symlink(td.path().join("pkg"), td.path().join("link")).unwrap();

        let entries = walk(td.path(), WalkOptions::default()).unwrap();
        let link = entries
            .iter()
            .find(|e| e.name == "link")
            .expect("link entry");
        assert!(link.is_symlink);
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(link.symlink_target_is_dir, Some(true));
    }

    /// B3 regression: a pnpm-style `node_modules` symlink whose target is a
    /// populated sibling "store" directory must not be descended when the
    /// root `.gitignore` lists `node_modules/`. Before B3 the matcher only
    /// ran in the emit phase AFTER jwalk had already resolved the symlink
    /// and walked the store; the walker touched O(tracked + store) files.
    #[cfg(unix)]
    #[test]
    fn walk_with_ignore_skips_gitignored_symlink_target() {
        use crate::ignore::IgnoreMatcher;
        use std::os::unix::fs::symlink;

        // Set up the "store" sibling with a bunch of files the walker
        // must NOT reach.
        let store = TempDir::new().unwrap();
        for i in 0..60 {
            make_file(store.path(), &format!("store-file-{:03}.js", i), b"x");
        }
        fs::create_dir(store.path().join("nested")).unwrap();
        for i in 0..10 {
            make_file(&store.path().join("nested"), &format!("n{:02}.js", i), b"y");
        }

        // Set up the "repo" with a gitignore'd node_modules symlink.
        let repo = TempDir::new().unwrap();
        fs::write(repo.path().join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir(repo.path().join("src")).unwrap();
        make_file(&repo.path().join("src"), "main.rs", b"fn main() {}");
        symlink(store.path(), repo.path().join("node_modules")).unwrap();

        // Pre-build the matcher from the repo's root .gitignore so it's
        // live when the walker starts. In production this happens in
        // `walk_with_ignore`'s process_read_dir callback as directories
        // are streamed, but loading the root gitignore up-front gives a
        // deterministic fixture.
        let mut matcher = IgnoreMatcher::new();
        matcher
            .add_from_file(&repo.path().join(".gitignore"))
            .unwrap();

        let entries = walk_with_ignore(
            repo.path(),
            WalkOptions {
                include_root: false,
                ..Default::default()
            },
            &matcher,
        )
        .unwrap();

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        // The symlink entry itself is present (file-tree UIs want to show
        // it in the list even when ignored).
        assert!(
            names.contains(&"node_modules"),
            "expected node_modules in {:?}",
            names
        );
        // The repo's real files are present.
        assert!(names.contains(&".gitignore"));
        assert!(names.contains(&"src"));
        assert!(names.contains(&"main.rs"));

        // Not one file under the store target leaked through.
        for e in &entries {
            assert!(
                !e.path.starts_with(store.path()),
                "walker descended into the pnpm-style store: {:?}",
                e.path
            );
            assert!(
                !e.name.starts_with("store-file-"),
                "store file leaked: {}",
                e.name
            );
        }

        // Total entry count is bounded by the real repo contents, not the
        // store. Repo has: .gitignore, src, main.rs, node_modules => 4.
        // Allow a tiny margin for any platform-specific hidden files the
        // fixture happens to carry in /tmp; the store alone would push us
        // past 70.
        assert!(
            entries.len() < 10,
            "expected < 10 entries, got {}: {:?}",
            entries.len(),
            names
        );
    }

    #[test]
    fn walk_with_ignore_picks_up_nested_gitignore() {
        // Even when no root .gitignore exists, a nested one discovered
        // mid-walk must take effect for its siblings. Mirrors how B3's
        // process_read_dir callback augments the matcher dynamically.
        use crate::ignore::IgnoreMatcher;

        let td = TempDir::new().unwrap();
        fs::create_dir(td.path().join("pkg")).unwrap();
        fs::write(td.path().join("pkg/.gitignore"), "build/\n").unwrap();
        fs::create_dir(td.path().join("pkg/build")).unwrap();
        make_file(&td.path().join("pkg/build"), "artifact.bin", b"x");
        make_file(&td.path().join("pkg"), "source.rs", b"fn x() {}");

        let matcher = IgnoreMatcher::new();
        let entries = walk_with_ignore(td.path(), WalkOptions::default(), &matcher).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"source.rs"));
        // The `build` dir itself is yielded (so the tree UI can show the
        // ignored node) but its contents must not have been walked.
        assert!(names.contains(&"build"));
        assert!(
            !names.contains(&"artifact.bin"),
            "nested gitignore didn't block descent: {:?}",
            names
        );
    }
    // A symlink cycle under `SymlinkPolicy::Always` must terminate.
    //
    // `Always` is reachable from JS (`followSymlinks: "true"` maps to it in
    // the binding), and `SymlinkPolicy::Smart` still carries a TODO for
    // (dev, inode) dedup and ancestor-cycle detection. That reads like the
    // walker has no loop protection at all — measured, it does: jwalk stops
    // both an ancestor cycle and a mutual one. These tests pin that, because
    // the protection is inherited from the walk crate rather than owned here,
    // so a swap or a major-version bump could remove it silently and turn an
    // opt-in flag into a hang.
    //
    // Both cases carry a control that follows a *non*-cyclic symlink, so a
    // regression that simply stopped following links could not satisfy them.
    #[test]
    fn always_policy_terminates_on_ancestor_symlink_cycle() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root, root.join("a/b/loop")).unwrap();

        let opts = WalkOptions {
            follow_symlinks: super::SymlinkPolicy::Always,
            ..Default::default()
        };
        // The assertion is that this returns at all.
        let walked = super::walk(root, opts).unwrap();
        assert!(
            walked.len() < 1_000,
            "cycle produced {} entries — loop protection is gone",
            walked.len()
        );
    }

    #[test]
    fn always_policy_terminates_on_mutual_symlink_cycle() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("x")).unwrap();
        std::fs::create_dir_all(root.join("y")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("y"), root.join("x/to_y")).unwrap();
            std::os::unix::fs::symlink(root.join("x"), root.join("y/to_x")).unwrap();
        }

        let opts = WalkOptions {
            follow_symlinks: super::SymlinkPolicy::Always,
            ..Default::default()
        };
        let walked = super::walk(root, opts).unwrap();
        assert!(
            walked.len() < 1_000,
            "mutual cycle produced {} entries — loop protection is gone",
            walked.len()
        );
    }

    // Gated with the cycle tests it controls for. The body only creates a
    // symlink on Unix, so on Windows it used to compare two symlink-free
    // walks, assert `follow > never` on 0 vs 0, and fail every run — while
    // controlling for nothing, since the tests it backstops are themselves
    // `#[cfg(unix)]`. Windows symlink coverage needs its own tests plus a
    // Developer Mode check; see `try_symlink_dir` usage in the JS suite.
    #[cfg(unix)]
    #[test]
    fn always_policy_actually_follows_a_non_cyclic_symlink() {
        // Control for both cycle tests: without this, a walker that had
        // stopped following symlinks entirely would pass them vacuously.
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("real/deep")).unwrap();
        std::fs::write(root.join("real/deep/file.txt"), b"x").unwrap();
        std::fs::create_dir_all(root.join("start")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("start/link")).unwrap();

        let follow = super::walk(
            &root.join("start"),
            WalkOptions {
                follow_symlinks: super::SymlinkPolicy::Always,
                ..Default::default()
            },
        )
        .unwrap();
        let never = super::walk(
            &root.join("start"),
            WalkOptions {
                follow_symlinks: super::SymlinkPolicy::Never,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(
            follow.len() > never.len(),
            "Always must descend further than Never (got {} vs {}); \
             if these are equal the cycle tests above prove nothing",
            follow.len(),
            never.len()
        );
        assert!(
            follow.iter().any(|w| w.name == "file.txt"),
            "expected to reach the symlink target's content, got {:?}",
            follow.iter().map(|w| w.name.as_str()).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod expand_ignored_dir {
    use crate::ignore::IgnoreMatcher;
    use crate::walker::{walk_with_ignore, WalkOptions};
    use std::fs;

    #[test]
    fn depth1_walk_into_gitignored_dir_lists_children() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        fs::write(root.join(".gitignore"), "node_modules/\nout/\n").unwrap();
        let nm = root.join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("left-pad"), "1").unwrap();
        fs::create_dir(nm.join("pkg")).unwrap();
        fs::write(nm.join("pkg").join("index.js"), "x").unwrap();

        let mut matcher = IgnoreMatcher::new();
        matcher.add_from_file(&root.join(".gitignore")).unwrap();

        let opts = WalkOptions {
            max_depth: Some(1),
            include_root: true,
            include_hidden: true,
            ..WalkOptions::default()
        };

        let plain = super::walk(&nm, opts.clone()).unwrap();
        let plain_names: Vec<_> = plain.iter().map(|w| w.name.as_str()).collect();
        assert!(
            plain_names.contains(&"left-pad") && plain_names.contains(&"pkg"),
            "control: plain walk should list children; got {:?}",
            plain_names
        );

        let walked = walk_with_ignore(&nm, opts, &matcher).unwrap();
        let names: Vec<_> = walked.iter().map(|w| w.name.as_str()).collect();
        // When the *walk root* is itself an ignored path (user expanded
        // node_modules), we still need depth-1 children for the tree UI.
        assert!(
            names.contains(&"left-pad") && names.contains(&"pkg"),
            "expected children of ignored dir when walking FROM it; got {:?}",
            names
        );
    }
}
