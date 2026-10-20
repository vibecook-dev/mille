//! Store hydration must grow linearly (petition M8).
//!
//! mille 0.3.4 cloned every map of the snapshot on every publication, so each
//! of the ways a store fills cost O(entries) per call and hydration was
//! quadratic: 5,500 entries 0.45 s, 22,000 15 s, a 43,310-entry home folder
//! 44 s. These tests drive the store the way each hydration path does — no
//! filesystem, synthetic walks — and pin the growth two ways:
//!
//!   - bytes allocated, which is deterministic: a 4× larger tree may allocate
//!     at most 6× the bytes. Linear is 4×; the cloning store allocated ~16×,
//!     because the copying *was* the quadratic term.
//!   - wall time, which also sees work that allocates nothing (a scan of the
//!     whole path index per folder was one such term): an 8× larger tree may
//!     take at most 20× the time — linear is 8×, n·log n ~9.5×, quadratic
//!     64×. The ratio is that loose because a shared runner (and this suite's
//!     own parallel tests) can stall either side; sizes are interleaved and
//!     timed best-of-three so one stall cannot decide the verdict.
//!
//! The paths, by what they publish:
//!   - per folder: one `insert_batch` + `paths_under` + listing-complete mark
//!     per directory, top-down — `prefetch` / `resyncProgressive` per folder.
//!   - populate: `populate_store` over one walk — `populateFromRoots` and the
//!     native depth walk (`populateFromPath`).
//!   - per entry: one `insert` per entry — watcher creates and URI resolution.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mille_core::{
    populate_store, Entry, EntryId, EntryKind, EntryStore, IgnoreMatcher, WalkedEntry,
};

/// Counts the bytes each thread allocates, so tests running in parallel do
/// not see each other's allocations.
struct CountingAllocator;

thread_local! {
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

fn count(bytes: usize) {
    // `try_with`: the slot is gone while the thread itself is torn down.
    let _ = ALLOCATED.try_with(|total| total.set(total.get() + bytes as u64));
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn allocated() -> u64 {
    ALLOCATED.with(Cell::get)
}

const MAX_BYTES_GROWTH: f64 = 6.0;
const MAX_TIME_GROWTH: f64 = 20.0;
const DEEP_LEVELS: usize = 24;
const DEEP_FILES: usize = 5;
const WIDE_FANOUT: usize = 500;

#[derive(Clone, Copy, Debug)]
enum Shape {
    /// Few large folders: sibling ordering and per-batch work dominate.
    Wide,
    /// Many small folders nested 24 deep: per-call and per-ancestor work
    /// dominate, and a per-folder hydration makes one call per folder.
    Deep,
}

fn walked(path: PathBuf, parent: Option<PathBuf>, kind: EntryKind, depth: usize) -> WalkedEntry {
    WalkedEntry {
        name: path.file_name().unwrap().to_string_lossy().into_owned(),
        path,
        parent_path: parent,
        kind,
        size: if kind == EntryKind::File { 16 } else { 0 },
        mtime_ms: 0,
        ctime_ms: 0,
        is_symlink: false,
        symlink_target_is_dir: None,
        is_readonly: false,
        is_hidden: false,
        depth: depth as u16,
    }
}

/// A walk of the root plus exactly `total` entries below it, in the walker's
/// own depth-first order.
fn tree(shape: Shape, total: usize) -> Vec<WalkedEntry> {
    let root = PathBuf::from("/m8");
    let mut out = vec![walked(root.clone(), None, EntryKind::Directory, 0)];
    let mut remaining = total;
    let files = |out: &mut Vec<WalkedEntry>, dir: &PathBuf, depth: usize, count: usize| {
        for index in 0..count {
            let path = dir.join(format!("file-{index:03}.txt"));
            out.push(walked(path, Some(dir.clone()), EntryKind::File, depth));
        }
    };
    match shape {
        Shape::Wide => {
            for folder in 0.. {
                if remaining == 0 {
                    break;
                }
                let dir = root.join(format!("folder-{folder:04}"));
                out.push(walked(
                    dir.clone(),
                    Some(root.clone()),
                    EntryKind::Directory,
                    1,
                ));
                remaining -= 1;
                let count = WIDE_FANOUT.min(remaining);
                files(&mut out, &dir, 2, count);
                remaining -= count;
            }
        }
        Shape::Deep => {
            for chain in 0.. {
                if remaining == 0 {
                    break;
                }
                let mut parent = root.clone();
                let mut dir = root.join(format!("chain-{chain:04}"));
                for level in 0..DEEP_LEVELS {
                    if remaining == 0 {
                        break;
                    }
                    out.push(walked(
                        dir.clone(),
                        Some(parent.clone()),
                        EntryKind::Directory,
                        level + 1,
                    ));
                    remaining -= 1;
                    let count = DEEP_FILES.min(remaining);
                    files(&mut out, &dir, level + 2, count);
                    remaining -= count;
                    parent = dir.clone();
                    dir = dir.join(format!("level-{:02}", level + 1));
                }
            }
        }
    }
    out
}

fn entry(walked: &WalkedEntry, parent_id: Option<EntryId>) -> Entry {
    Entry {
        id: EntryId(0),
        parent_id,
        name: walked.name.clone(),
        kind: walked.kind,
        size: walked.size,
        mtime_ms: walked.mtime_ms,
        ctime_ms: walked.ctime_ms,
        symlink_target_is_dir: None,
        path_segments: None,
        is_ignored: false,
        is_excluded: false,
        is_readonly: false,
        is_hidden: false,
    }
}

/// `prefetch` every folder breadth-first: list it, reconcile it against what
/// the store knows below it, publish its children, mark it complete.
fn hydrate_by_folder(walk: &[WalkedEntry]) -> usize {
    let mut by_parent: std::collections::HashMap<&PathBuf, Vec<&WalkedEntry>> =
        std::collections::HashMap::new();
    for item in &walk[1..] {
        by_parent
            .entry(item.parent_path.as_ref().unwrap())
            .or_default()
            .push(item);
    }
    let store = EntryStore::new();
    let root = &walk[0];
    store.insert(root.path.clone(), entry(root, None)).unwrap();
    let mut queue = std::collections::VecDeque::from([&root.path]);
    while let Some(folder) = queue.pop_front() {
        let folder_id = store.get_by_path(folder).unwrap().id;
        let known = store.paths_under(folder);
        assert!(!known.is_empty());
        let children = by_parent.get(folder).map(Vec::as_slice).unwrap_or(&[]);
        let batch = children
            .iter()
            .map(|child| (child.path.clone(), entry(child, Some(folder_id))))
            .collect();
        store.insert_batch(batch).unwrap();
        store.mark_directory_children_loaded(folder_id).unwrap();
        queue.extend(
            children
                .iter()
                .filter(|child| child.kind == EntryKind::Directory)
                .map(|child| &child.path),
        );
    }
    store.snapshot().entry_count()
}

fn hydrate_by_populate(walk: &[WalkedEntry]) -> usize {
    let store = EntryStore::new();
    populate_store(&store, &walk[0].path, walk, None).unwrap();
    store.snapshot().entry_count()
}

/// `populate_store` under a repository-ignore matcher holding one rule file
/// per top-level folder — a home folder of projects, each with its own
/// `.gitignore`, so the number of ignore files grows with the tree.
fn hydrate_by_populate_with_ignores(walk: &[WalkedEntry]) -> usize {
    let mut ignore = IgnoreMatcher::new();
    ignore
        .add_from_string(&walk[0].path, "*.log\nbuild/\n")
        .unwrap();
    for project in walk.iter().filter(|item| item.depth == 1) {
        ignore.add_from_string(&project.path, "*.tmp\n").unwrap();
    }
    let store = EntryStore::new();
    populate_store(&store, &walk[0].path, walk, Some(&ignore)).unwrap();
    store.snapshot().entry_count()
}

fn hydrate_by_entry(walk: &[WalkedEntry]) -> usize {
    let store = EntryStore::new();
    for item in walk {
        let parent_id = item
            .parent_path
            .as_ref()
            .map(|parent| store.get_by_path(parent).unwrap().id);
        store
            .insert(item.path.clone(), entry(item, parent_id))
            .unwrap();
    }
    store.snapshot().entry_count()
}

type Hydrate = fn(&[WalkedEntry]) -> usize;

fn bytes_to_hydrate(walk: &[WalkedEntry], hydrate: Hydrate) -> u64 {
    let before = allocated();
    assert_eq!(hydrate(walk), walk.len());
    allocated() - before
}

fn time_to_hydrate(walk: &[WalkedEntry], hydrate: Hydrate) -> Duration {
    let started = Instant::now();
    assert_eq!(hydrate(walk), walk.len());
    started.elapsed()
}

/// A 4× larger tree allocates at most `MAX_BYTES_GROWTH`× the bytes.
fn assert_linear_bytes(name: &str, hydrate: Hydrate) {
    for shape in [Shape::Wide, Shape::Deep] {
        let small = tree(shape, 4_000);
        let large = tree(shape, 16_000);
        let small_bytes = bytes_to_hydrate(&small, hydrate);
        let large_bytes = bytes_to_hydrate(&large, hydrate);
        let growth = large_bytes as f64 / small_bytes as f64;
        eprintln!("{name} {shape:?} bytes: 4000 {small_bytes}, 16000 {large_bytes} ({growth:.2}×)");
        assert!(
            growth <= MAX_BYTES_GROWTH,
            "{name} {shape:?}: hydrating 4,000 entries allocated {small_bytes} bytes and \
             16,000 allocated {large_bytes} — {growth:.1}× for 4× the entries \
             (bound {MAX_BYTES_GROWTH}×)",
        );
    }
}

/// An 8× larger tree takes at most `MAX_TIME_GROWTH`× the time.
fn assert_linear_time(name: &str, hydrate: Hydrate) {
    for shape in [Shape::Wide, Shape::Deep] {
        let small = tree(shape, 2_000);
        let large = tree(shape, 16_000);
        // Warm the allocator and the code paths before either timing.
        hydrate(&small);
        let mut small_time = Duration::MAX;
        let mut large_time = Duration::MAX;
        for _ in 0..3 {
            small_time = small_time.min(time_to_hydrate(&small, hydrate));
            large_time = large_time.min(time_to_hydrate(&large, hydrate));
        }
        let growth = large_time.as_secs_f64() / small_time.as_secs_f64();
        eprintln!(
            "{name} {shape:?} time: 2000 {small_time:?}, 16000 {large_time:?} ({growth:.2}×)"
        );
        assert!(
            growth <= MAX_TIME_GROWTH,
            "{name} {shape:?}: 2,000 entries took {small_time:?} and 16,000 took \
             {large_time:?} — {growth:.1}× for 8× the entries (bound {MAX_TIME_GROWTH}×)",
        );
    }
}

#[test]
fn per_folder_hydration_is_linear() {
    assert_linear_bytes("per-folder", hydrate_by_folder);
    assert_linear_time("per-folder", hydrate_by_folder);
}

#[test]
fn populate_hydration_is_linear() {
    assert_linear_bytes("populate", hydrate_by_populate);
    assert_linear_time("populate", hydrate_by_populate);
}

#[test]
fn populate_hydration_under_per_project_ignores_is_linear() {
    assert_linear_bytes("populate+ignores", hydrate_by_populate_with_ignores);
    assert_linear_time("populate+ignores", hydrate_by_populate_with_ignores);
}

#[test]
fn per_entry_hydration_is_linear() {
    assert_linear_bytes("per-entry", hydrate_by_entry);
    assert_linear_time("per-entry", hydrate_by_entry);
}

#[test]
fn every_path_hydrates_the_same_tree() {
    for shape in [Shape::Wide, Shape::Deep] {
        let walk = tree(shape, 3_000);
        let stores = [
            {
                let store = EntryStore::new();
                populate_store(&store, &walk[0].path, &walk, None).unwrap();
                store
            },
            {
                let store = EntryStore::new();
                for item in &walk {
                    let parent_id = item
                        .parent_path
                        .as_ref()
                        .map(|parent| store.get_by_path(parent).unwrap().id);
                    store
                        .insert(item.path.clone(), entry(item, parent_id))
                        .unwrap();
                }
                store
            },
        ];
        // Same rows in the same sibling order with the same summaries,
        // whichever way the store was filled.
        let shapes: Vec<_> = stores
            .iter()
            .map(|store| {
                let snapshot = store.snapshot();
                let mut rows = Vec::new();
                let mut stack = vec![(snapshot.roots()[0], walk[0].path.clone())];
                while let Some((id, path)) = stack.pop() {
                    rows.push((
                        path.clone(),
                        snapshot.children_of(id).len(),
                        snapshot.direct_child_count(id),
                        snapshot.subtree_visible_count(id),
                        snapshot.subtree_total_size(id),
                    ));
                    for &child in snapshot.children_of(id).iter().rev() {
                        let name = snapshot.get(child).unwrap().name.clone();
                        stack.push((child, path.join(name)));
                    }
                    assert_eq!(store.path_for_id(id).as_ref(), Some(&path));
                }
                rows
            })
            .collect();
        assert_eq!(shapes[0].len(), walk.len());
        assert_eq!(shapes[0], shapes[1], "{shape:?}");
    }
}

#[test]
fn populate_publishes_once_per_level_and_keeps_known_ids() {
    let walk = tree(Shape::Deep, 600);
    let levels = walk.iter().map(|item| item.depth).max().unwrap() as u64 + 1;

    // A prior, shallower walk already indexed the top two levels — the lazy
    // `prefetch` shape, where a later walk's parents come from the store.
    let store = EntryStore::new();
    let shallow: Vec<WalkedEntry> = walk
        .iter()
        .filter(|item| item.depth <= 1)
        .cloned()
        .collect();
    let known = populate_store(&store, &walk[0].path, &shallow, None).unwrap();
    assert_eq!(known.len(), shallow.len());
    assert_eq!(store.tree_version(), 2, "one publication per level");

    let before = store.tree_version();
    let ids = populate_store(&store, &walk[0].path, &walk, None).unwrap();
    // Two levels were fully known; every other level published exactly once.
    assert_eq!(store.tree_version() - before, levels - 2);
    assert_eq!(ids.len(), walk.len());
    for id in &known {
        assert!(ids.contains(id), "a known path keeps its id");
    }

    // Every entry hangs off the parent its path names: no spurious roots.
    let snapshot = store.snapshot();
    assert_eq!(snapshot.roots().len(), 1);
    for item in &walk {
        let entry = store.get_by_path(&item.path).unwrap();
        let parent = item
            .parent_path
            .as_ref()
            .map(|parent| store.get_by_path(parent).unwrap().id);
        assert_eq!(entry.parent_id, parent, "{:?}", item.path);
    }
    assert_eq!(snapshot.entry_count(), walk.len());
}
