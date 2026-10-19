// Bounded walker (Phase 2.3; petition M10).
//
// Produces lightweight `WalkedEntry` records; the coalescer (2.4) converts
// these into `Entry` with allocated IDs and pushes them into `EntryStore`.
//
// M10: every directory listing and every `stat` the walk makes goes through a
// `ReadBoundary` (read_boundary.rs), so a read that blocks in the kernel — an
// iCloud-evicted folder downloading, a hung network mount — costs the walk one
// deadline instead of holding the engine. That is why the walk owns its loop
// rather than delegating it to jwalk: jwalk's `fs::read_dir` is the one call
// that needed bounding, and nothing outside the crate can reach it. The loop
// keeps jwalk's observable shape — depth-first, children byte-sorted, the same
// depth, hidden, symlink and ignore rules — and reads up to `parallelism`
// directories ahead on the boundary's helper threads, so a parallel walk is
// still parallel and its output is still deterministic.
//
// v0.2 B3: when an `IgnoreMatcher` is supplied, ignore rules are applied to a
// directory's children BEFORE the walk would descend a symlinked directory.
// This matters for pnpm monorepos: a root `.gitignore` listing `node_modules/`
// stops the walker at the `node_modules` symlink entry itself, rather than
// letting it follow the link into pnpm's 38k-file store.

use std::fs::{FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::entry::EntryKind;
use crate::error::{ErrorCode, FxError};
use crate::ignore::IgnoreMatcher;
use crate::read_boundary::{
    DirListing, ListOptions, ListedChild, PathStat, ReadBoundary, ReadFailure, ReadOutcome,
    ReadSignal, Ticket, WAIT_SLICE,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SymlinkPolicy {
    /// Never descend into symlinked directories.
    Never,
    /// Always descend. Cycles are cut by comparing each followed link's
    /// canonical target with the directories above it.
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
    /// When false, entries whose name starts with `.` are not emitted. Their
    /// contents are still walked, as they were under jwalk.
    pub include_hidden: bool,
    /// If true, the walk root is returned as the first result.
    pub include_root: bool,
    /// Directories read ahead concurrently. 0 or 1 = one at a time. SPEC
    /// §4.3 recommends capping at 8 to leave headroom for editor work.
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

/// A directory the walk meant to read and could not, because the read never
/// answered: it stalled past the deadline, was refused while an earlier read
/// into the same path is still stuck, or is a dataless placeholder the
/// boundary will not download. Its own entry was emitted; its children are
/// unknown, so a caller must neither mark its listing complete nor treat the
/// children it already knows as deleted.
#[derive(Clone, Debug)]
pub struct UnreadDirectory {
    pub path: PathBuf,
    pub failure: ReadFailure,
}

/// The result of a bounded walk.
#[must_use = "`unread` directories are unknown, not empty; check it or call `into_complete`"]
#[derive(Debug, Default)]
pub struct WalkReport {
    /// Walked entries in walk order. Empty for the batched form, whose
    /// entries went to `on_batch`.
    pub entries: Vec<WalkedEntry>,
    /// Directories whose read did not answer. Never the walk root: a root
    /// that cannot be read fails the walk with the read's typed error.
    pub unread: Vec<UnreadDirectory>,
    /// Ignore files the walk read and emitted, with their text, in walk
    /// order, so a caller classifies entries without opening them again.
    /// Collected only when the walk was given an ignore matcher.
    pub ignore_files: Vec<(PathBuf, String)>,
}

impl WalkReport {
    /// The entries, if every directory answered; otherwise
    /// [`FxError::Incomplete`] carrying `unread`. For callers that need the
    /// whole tree or nothing.
    pub fn into_complete(self) -> Result<Vec<WalkedEntry>, FxError> {
        if self.unread.is_empty() {
            Ok(self.entries)
        } else {
            Err(FxError::Incomplete {
                unread: self.unread,
            })
        }
    }
}

/// Walk `root` through the process-wide default [`ReadBoundary`].
///
/// All or nothing: if any directory below the root does not answer (stall,
/// dataless placeholder, OS error), this fails with [`FxError::Incomplete`]
/// rather than returning a tree with that subtree silently missing — a caller
/// diffing the result against an earlier walk would otherwise read "drive
/// asleep" as "everything below it deleted". Use [`walk_bounded`] to keep the
/// partial result and act on `unread` directly.
pub fn walk(root: &Path, options: WalkOptions) -> Result<Vec<WalkedEntry>, FxError> {
    walk_bounded(root, options, None, ReadBoundary::shared_default())?.into_complete()
}

/// Walk with gitignore rules applied BEFORE symlink resolution.
///
/// When `ignore` is `Some`, each child's logical path (parent + file name)
/// is checked against the matcher as soon as its directory is listed.
/// Ignored directories are emitted but never read — critical for pnpm-style
/// layouts where `node_modules/` is a symlink into a central store and
/// following it first would pull in 10k+ unrelated files.
///
/// The rules stack dynamically: when a child `.gitignore` / `.ignore` /
/// `.rgignore` is listed, its rules apply to its siblings and everything
/// below them, so nested ignore files work the same as the root one. The
/// caller's matcher is consulted first and is never modified.
///
/// Fails with [`FxError::Incomplete`] when a directory does not answer, as
/// [`walk`] does.
pub fn walk_with_ignore(
    root: &Path,
    options: WalkOptions,
    ignore: &IgnoreMatcher,
) -> Result<Vec<WalkedEntry>, FxError> {
    walk_bounded(root, options, Some(ignore), ReadBoundary::shared_default())?.into_complete()
}

/// Stream a walk in bounded batches. The walker keeps its deterministic
/// per-directory ordering, but callers can publish each completed batch
/// instead of waiting for the entire subtree to materialize.
///
/// Every batch is delivered; then, if a directory did not answer, the walk
/// fails with [`FxError::Incomplete`], so the caller knows the batches it
/// published are not the whole tree. [`walk_bounded_batched`] reports the
/// same directories without failing.
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
    walk_bounded_batched(
        root,
        options,
        None,
        ReadBoundary::shared_default(),
        batch_size,
        cancelled,
        on_batch,
    )?
    .into_complete()
    .map(|_| ())
}

/// Ignore-aware companion to [`walk_batched`]; fails with
/// [`FxError::Incomplete`] the same way.
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
    walk_bounded_batched(
        root,
        options,
        Some(ignore),
        ReadBoundary::shared_default(),
        batch_size,
        cancelled,
        on_batch,
    )?
    .into_complete()
    .map(|_| ())
}

/// Walk through `reads`, reporting what could not be read.
///
/// The plain `walk*` functions use the process-wide default boundary and
/// drop the report. An engine with its own deadline, stuck cap and dataless
/// policy calls this form: a stalled subdirectory then arrives in
/// `unread` while the rest of the tree walks normally, and a stalled root
/// fails the walk with `ETIMEDOUT` / `EDATALESS` for that root.
pub fn walk_bounded(
    root: &Path,
    options: WalkOptions,
    ignore: Option<&IgnoreMatcher>,
    reads: &ReadBoundary,
) -> Result<WalkReport, FxError> {
    let mut entries = Vec::new();
    let mut report = walk_core(root, &options, ignore, reads, &mut || false, &mut |entry| {
        entries.push(entry);
        Ok(())
    })?;
    report.entries = entries;
    Ok(report)
}

/// Batched form of [`walk_bounded`]. `cancelled` is also honoured while the
/// walk waits on a read.
pub fn walk_bounded_batched<C, F>(
    root: &Path,
    options: WalkOptions,
    ignore: Option<&IgnoreMatcher>,
    reads: &ReadBoundary,
    batch_size: usize,
    mut cancelled: C,
    mut on_batch: F,
) -> Result<WalkReport, FxError>
where
    C: FnMut() -> bool,
    F: FnMut(Vec<WalkedEntry>) -> Result<(), FxError>,
{
    let batch_size = batch_size.max(1);
    let mut batch = Vec::with_capacity(batch_size.min(4096));
    let report = walk_core(
        root,
        &options,
        ignore,
        reads,
        &mut cancelled,
        &mut |entry| {
            batch.push(entry);
            if batch.len() >= batch_size {
                on_batch(std::mem::take(&mut batch))?;
                // Be cooperative with the host/runtime thread consuming the
                // immutable snapshots and change notices produced by the batch.
                std::thread::yield_now();
            }
            Ok(())
        },
    )?;
    if cancelled() {
        return Err(FxError::Cancelled);
    }
    if !batch.is_empty() {
        on_batch(batch)?;
    }
    Ok(report)
}

/// Most listed-but-not-yet-emitted directories a walk holds before it stops
/// reading ahead. Bounds memory when the consumer is slower than the disk.
const LISTED_AHEAD_MAX: usize = 1024;

/// A directory the walk will read. Lives in `Walk::dirs` from the moment its
/// parent is listed until its subtree has been emitted.
struct Dir {
    path: PathBuf,
    depth: usize,
    /// Canonical path, kept only when following symlinks (cycle checks).
    canonical: Option<PathBuf>,
    parent: Option<usize>,
    dataless: bool,
    state: DirState,
}

enum DirState {
    /// Not yet submitted (read-ahead is full).
    Waiting,
    Reading(Ticket),
    /// Listed and planned; `next` is the next child to emit.
    Listed {
        children: Vec<Child>,
        next: usize,
    },
    /// The read did not answer; reported in `WalkReport::unread`.
    Unread(ReadFailure),
    /// The read failed with an OS error: emitted without children.
    Empty,
}

struct Child {
    entry: Option<WalkedEntry>,
    /// Text of an emitted ignore file, reported in `WalkReport::ignore_files`.
    ignore: Option<String>,
    dir: Option<usize>,
}

struct Walk<'a> {
    options: &'a WalkOptions,
    follow: bool,
    list: ListOptions,
    ignore: Option<&'a IgnoreMatcher>,
    /// Ignore files found during this walk. Consulted after the caller's
    /// rules — the order one stack would have held them in. A directory is
    /// always planned after its ancestors, so the rules that apply to any
    /// path sit in the same order however the reads interleave.
    nested: IgnoreMatcher,
    reads: &'a ReadBoundary,
    signal: Arc<ReadSignal>,
    /// Slots are never reused, so a stale id in `waiting` cannot alias a
    /// newer directory; a finished slot shrinks to `None`.
    dirs: Vec<Option<Box<Dir>>>,
    /// Directories to read next, nearest-in-walk-order on top.
    waiting: Vec<usize>,
    reading: Vec<usize>,
    read_ahead: usize,
    listed_ahead: usize,
}

fn walk_core(
    root: &Path,
    options: &WalkOptions,
    ignore: Option<&IgnoreMatcher>,
    reads: &ReadBoundary,
    cancelled: &mut dyn FnMut() -> bool,
    visit: &mut dyn FnMut(WalkedEntry) -> Result<(), FxError>,
) -> Result<WalkReport, FxError> {
    let follow = matches!(options.follow_symlinks, SymlinkPolicy::Always);
    let mut report = WalkReport::default();

    let output = reads.stat(root, follow).map_err(|failure| match failure {
        // `Path::exists`, which this replaced, called every OS failure
        // "missing". Unanswered reads keep their own typed error.
        ReadFailure::Io(_) => missing_root(root),
        other => other.into_fx(root),
    })?;
    let stat = output
        .as_stat()
        .ok_or_else(|| FxError::InternalBug("bounded stat returned no metadata".into()))?;
    if stat.metadata.file_type().is_symlink() && stat.target.is_none() {
        return Err(missing_root(root));
    }

    if options.include_root {
        if let Some(entry) = root_entry(root, stat, follow) {
            if cancelled() {
                return Err(FxError::Cancelled);
            }
            visit(entry)?;
        }
    }
    let lists_root = options.max_depth.is_none_or(|limit| limit >= 1);
    if !lists_root || !stat.is_dir_like() {
        return Ok(report);
    }

    let mut walk = Walk {
        options,
        follow,
        list: ListOptions {
            follow_symlinks: follow,
            ignore_contents: ignore.is_some(),
        },
        ignore,
        nested: IgnoreMatcher::new(),
        reads,
        signal: ReadSignal::new(),
        dirs: Vec::new(),
        waiting: Vec::new(),
        reading: Vec::new(),
        read_ahead: options.parallelism.max(1),
        listed_ahead: 0,
    };
    let root_dir = walk.add_dir(Dir {
        path: root.to_path_buf(),
        depth: 0,
        canonical: if follow { stat.canonical.clone() } else { None },
        parent: None,
        dataless: stat.is_dataless(),
        state: DirState::Waiting,
    });
    walk.await_dir(root_dir, cancelled)?;
    match walk.take_unlisted(root_dir) {
        None => walk.listed_ahead -= 1,
        // A root that lists with an OS error walks as an empty directory, as
        // it did under jwalk. Only a read that never answered fails the walk.
        Some(None) => return Ok(report),
        Some(Some(failure)) => return Err(failure.into_fx(root)),
    }

    let mut stack = vec![root_dir];
    while let Some(&top) = stack.last() {
        walk.pump(cancelled)?;
        let Some(child) = walk.next_child(top) else {
            stack.pop();
            walk.dirs[top] = None;
            continue;
        };
        if let Some(entry) = child.entry {
            if cancelled() {
                return Err(FxError::Cancelled);
            }
            if let Some(contents) = child.ignore {
                report.ignore_files.push((entry.path.clone(), contents));
            }
            visit(entry)?;
        }
        let Some(dir) = child.dir else {
            continue;
        };
        walk.await_dir(dir, cancelled)?;
        match walk.take_unlisted(dir) {
            None => {
                walk.listed_ahead -= 1;
                stack.push(dir);
            }
            // Unreadable (EACCES and friends): emitted, no children — the
            // jwalk behaviour every caller already handles.
            Some(None) => {}
            Some(Some(failure)) => report.unread.push(UnreadDirectory {
                path: walk.dirs[dir]
                    .take()
                    .map(|dir| dir.path)
                    .unwrap_or_default(),
                failure,
            }),
        }
    }
    Ok(report)
}

impl Walk<'_> {
    fn add_dir(&mut self, dir: Dir) -> usize {
        self.dirs.push(Some(Box::new(dir)));
        self.dirs.len() - 1
    }

    fn dir(&self, id: usize) -> &Dir {
        self.dirs[id]
            .as_deref()
            .expect("walk directory slot is live")
    }

    fn dir_mut(&mut self, id: usize) -> &mut Dir {
        self.dirs[id]
            .as_deref_mut()
            .expect("walk directory slot is live")
    }

    /// For a resolved directory: `None` when it is listed, `Some(None)` when
    /// it read as empty (OS error, slot released), `Some(Some(failure))`
    /// when the read never answered (slot kept for its path).
    fn take_unlisted(&mut self, id: usize) -> Option<Option<ReadFailure>> {
        match &self.dir(id).state {
            DirState::Listed { .. } => None,
            DirState::Unread(failure) => Some(Some(failure.clone())),
            _ => {
                self.dirs[id] = None;
                Some(None)
            }
        }
    }

    fn next_child(&mut self, id: usize) -> Option<Child> {
        let DirState::Listed { children, next } = &mut self.dir_mut(id).state else {
            return None;
        };
        let child = children.get_mut(*next)?;
        *next += 1;
        Some(Child {
            entry: child.entry.take(),
            ignore: child.ignore.take(),
            dir: child.dir.take(),
        })
    }

    fn submit(&mut self, id: usize) {
        let dir = self.dir(id);
        let ticket = self
            .reads
            .list_dir(&dir.path, self.list, dir.dataless, Some(&self.signal));
        self.dir_mut(id).state = DirState::Reading(ticket);
        self.reading.push(id);
    }

    /// Plan every read that has finished, then refill read-ahead.
    fn pump(&mut self, cancelled: &mut dyn FnMut() -> bool) -> Result<(), FxError> {
        let mut index = 0;
        while index < self.reading.len() {
            let id = self.reading[index];
            let DirState::Reading(ticket) = &self.dir(id).state else {
                self.reading.swap_remove(index);
                continue;
            };
            match ticket.poll() {
                Some(outcome) => {
                    self.reading.swap_remove(index);
                    self.resolve(id, outcome)?;
                }
                None => index += 1,
            }
        }
        while self.reading.len() < self.read_ahead && self.listed_ahead < LISTED_AHEAD_MAX {
            let Some(id) = self.waiting.pop() else {
                break;
            };
            if self.dirs[id]
                .as_deref()
                .is_some_and(|dir| matches!(dir.state, DirState::Waiting))
            {
                if cancelled() {
                    return Err(FxError::Cancelled);
                }
                self.submit(id);
            }
        }
        Ok(())
    }

    fn resolve(&mut self, id: usize, outcome: ReadOutcome) -> Result<(), FxError> {
        self.dir_mut(id).state = match outcome {
            Ok(output) => {
                let listing = output.as_listing().ok_or_else(|| {
                    FxError::InternalBug("bounded listing returned no children".into())
                })?;
                let children = self.plan(id, listing);
                self.listed_ahead += 1;
                DirState::Listed { children, next: 0 }
            }
            Err(ReadFailure::Io(_)) => DirState::Empty,
            Err(ReadFailure::Cancelled) => return Err(FxError::Cancelled),
            Err(failure) => DirState::Unread(failure),
        };
        Ok(())
    }

    /// Block until directory `id` is listed or has failed, planning anything
    /// else that lands meanwhile.
    fn await_dir(&mut self, id: usize, cancelled: &mut dyn FnMut() -> bool) -> Result<(), FxError> {
        loop {
            let seen = self.signal.seen();
            self.pump(cancelled)?;
            let wait = match &self.dir(id).state {
                DirState::Waiting => {
                    // Needed now: read it even when read-ahead is full.
                    self.submit(id);
                    continue;
                }
                DirState::Reading(ticket) => ticket.until_deadline(),
                _ => return Ok(()),
            };
            if cancelled() {
                return Err(FxError::Cancelled);
            }
            self.signal.wait_past(seen, wait.min(WAIT_SLICE));
        }
    }

    /// Plan one listed directory: emitted entries, and which children the
    /// walk will read.
    fn plan(&mut self, id: usize, listing: &DirListing) -> Vec<Child> {
        let (dir_path, depth) = {
            let dir = self.dir(id);
            (dir.path.clone(), dir.depth)
        };
        // Ignore files first, so their rules govern their own siblings.
        if self.ignore.is_some() {
            for listed in &listing.children {
                if let (true, Some(contents)) =
                    (listed.effective_type.is_file(), &listed.ignore_contents)
                {
                    let _ = self
                        .nested
                        .add_from_contents(&dir_path.join(&listed.name), contents);
                }
            }
        }

        let child_depth = depth + 1;
        let reads_at_depth = self
            .options
            .max_depth
            .is_none_or(|limit| child_depth < limit);
        let mut children = Vec::with_capacity(listing.children.len());
        let mut descents = Vec::new();
        for listed in &listing.children {
            // A followed link that does not resolve is dropped entirely, as
            // jwalk dropped it.
            if self.follow && listed.follow_failed {
                continue;
            }
            let followed_dir =
                self.follow && listed.file_type.is_symlink() && listed.effective_type.is_dir();
            if followed_dir && self.is_cycle(id, listed.canonical.as_deref()) {
                continue;
            }
            let path = dir_path.join(&listed.name);
            let descend =
                reads_at_depth && listed.effective_type.is_dir() && !self.is_pruned(&path, listed);
            let entry = walked_entry(listed, &path, child_depth, self.options);
            let ignore = entry.as_ref().and(listed.ignore_contents.as_ref()).cloned();
            let dir = descend.then(|| {
                let canonical = if !self.follow {
                    None
                } else if followed_dir {
                    listed.canonical.clone()
                } else {
                    self.dir(id)
                        .canonical
                        .as_ref()
                        .map(|parent| parent.join(&listed.name))
                };
                let child = self.add_dir(Dir {
                    path,
                    depth: child_depth,
                    canonical,
                    parent: Some(id),
                    dataless: listed.is_dataless(),
                    state: DirState::Waiting,
                });
                descents.push(child);
                child
            });
            children.push(Child { entry, ignore, dir });
        }
        // First child on top: the order the depth-first emission needs them.
        self.waiting.extend(descents.into_iter().rev());
        children
    }

    /// A followed link whose canonical target is this directory or one above
    /// it would walk forever.
    fn is_cycle(&self, id: usize, target: Option<&Path>) -> bool {
        let Some(target) = target else {
            return false;
        };
        let mut current = Some(id);
        while let Some(index) = current {
            let dir = self.dir(index);
            if dir.canonical.as_deref() == Some(target) {
                return true;
            }
            current = dir.parent;
        }
        false
    }

    /// Ignore rules stop descent. Symlinks count as directory-like so a
    /// `pattern/` rule matches a pnpm `node_modules` link before it is read.
    fn is_pruned(&self, path: &Path, listed: &ListedChild) -> bool {
        let Some(ignore) = self.ignore else {
            return false;
        };
        let dir_like = listed.effective_type.is_dir() || listed.effective_type.is_symlink();
        ignore
            .decision(path, dir_like)
            .or_else(|| self.nested.decision(path, dir_like))
            .unwrap_or(false)
    }
}

fn missing_root(root: &Path) -> FxError {
    FxError::Io {
        code: ErrorCode::ENOENT,
        path: root.to_path_buf(),
        source: io::Error::from(io::ErrorKind::NotFound),
    }
}

fn root_entry(root: &Path, stat: &PathStat, follow: bool) -> Option<WalkedEntry> {
    let name_os = root.file_name().unwrap_or(root.as_os_str());
    let name = name_os.to_str()?.to_string();
    let path = root
        .parent()
        .map(|parent| parent.join(name_os))
        .unwrap_or_else(|| root.to_path_buf());
    let (entry_type, metadata) = if follow && stat.metadata.file_type().is_symlink() {
        let target = stat.target.as_ref()?;
        (target.file_type(), target)
    } else {
        (stat.metadata.file_type(), &stat.metadata)
    };
    let target_is_dir = stat.target.as_ref().map(Metadata::is_dir);
    Some(entry_from_metadata(
        path,
        None,
        name,
        entry_type,
        metadata,
        target_is_dir,
        false,
        0,
    ))
}

fn walked_entry(
    listed: &ListedChild,
    path: &Path,
    depth: usize,
    options: &WalkOptions,
) -> Option<WalkedEntry> {
    let name = listed.name.to_str()?.to_string();
    let is_hidden = name.starts_with('.');
    if is_hidden && !options.include_hidden {
        return None;
    }
    let metadata = listed.metadata.as_ref()?;
    Some(entry_from_metadata(
        path.to_path_buf(),
        path.parent().map(Path::to_path_buf),
        name,
        listed.effective_type,
        metadata,
        listed.target_is_dir,
        is_hidden,
        depth,
    ))
}

// One record per field of `WalkedEntry`; a struct for the arguments would only
// rename them.
#[allow(clippy::too_many_arguments)]
fn entry_from_metadata(
    path: PathBuf,
    parent_path: Option<PathBuf>,
    name: String,
    entry_type: FileType,
    metadata: &Metadata,
    target_is_dir: Option<bool>,
    is_hidden: bool,
    depth: usize,
) -> WalkedEntry {
    // Prefer the listing's non-following type so symlinks stay classified as
    // Symlink (pnpm workspace links); the metadata type backs it up.
    let ft = metadata.file_type();
    let is_symlink = entry_type.is_symlink() || ft.is_symlink();
    let (kind, symlink_target_is_dir) = if is_symlink {
        (EntryKind::Symlink, Some(target_is_dir.unwrap_or(false)))
    } else if ft.is_dir() || entry_type.is_dir() {
        (EntryKind::Directory, None)
    } else if ft.is_file() || entry_type.is_file() {
        (EntryKind::File, None)
    } else {
        (EntryKind::Unknown, None)
    };

    let size = if matches!(kind, EntryKind::File) {
        metadata.len()
    } else {
        0
    };
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(system_time_ms)
        .unwrap_or(0);
    let ctime_ms = ctime_ms_from_metadata(metadata).unwrap_or(mtime_ms);

    WalkedEntry {
        path,
        parent_path,
        name,
        kind,
        size,
        mtime_ms,
        ctime_ms,
        is_symlink,
        symlink_target_is_dir,
        is_readonly: metadata.permissions().readonly(),
        is_hidden,
        depth: depth as u16,
    }
}

/// Convert a walked entry set into `Entry` records and push them into
/// `EntryStore`, resolving each entry's `parent_id` from the store's path
/// index, one published batch per depth level.
///
/// Internally topo-sorts by (depth asc, path asc) before insertion so that
/// parents are always processed before their children, regardless of walker
/// order. `Parallelism::Serial` happens to preserve DFS ordering, but
/// `Parallelism::RayonNewPool(n)` does not — without this sort, children
/// whose parent had not yet been seen would silently become spurious roots.
///
/// Returns the id of every walked entry, level by level: a path the store
/// already indexed keeps its id (insertion is idempotent by path) and is
/// listed first within its level, followed by the ids allocated for the
/// level in insertion order. Phase 2.4 coalescer: later phases may extend
/// this to stream via a channel rather than materialize the full Vec first.
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

    let mut ids = Vec::with_capacity(walked.len());

    // Topo-sort by (depth asc, path asc). Walker output under
    // Parallelism::RayonNewPool(n) does not guarantee parent-before-child
    // order; without this sort, children whose parent hadn't been inserted
    // would look up None in the path index and attach as spurious roots.
    let mut ordered: Vec<&WalkedEntry> = walked.iter().collect();
    ordered.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.path.cmp(&b.path)));

    // Suppress unused-variable warning on non-ignore paths.
    let _ = root;

    // Publish one batch per depth level rather than one snapshot per entry
    // (M8). Every parent of a level sits in an earlier level or was already
    // in the store, so by the time a level is built its parents are
    // published and the store's path index resolves them — including parents
    // inserted by a prior walk (lazy `prefetch` in v0.2 B2), which would
    // otherwise get `parent_id = None` and appear as spurious roots. A tree
    // costs one publication per level, and each sibling list is merged once
    // per level instead of re-inserted per child.
    for level in ordered.chunk_by(|a, b| a.depth == b.depth) {
        let batch: Vec<(PathBuf, Entry)> = level
            .iter()
            .filter(|w| match store.get_by_path(&w.path) {
                Some(existing) => {
                    ids.push(existing.id);
                    false
                }
                None => true,
            })
            .map(|w| {
                let parent_id = w
                    .parent_path
                    .as_ref()
                    .and_then(|pp| store.get_by_path(pp))
                    .map(|parent| parent.id);

                let is_dir_like =
                    w.kind == EntryKind::Directory || w.symlink_target_is_dir == Some(true);
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
                (w.path.clone(), entry)
            })
            .collect();
        // A path indexed between the filter above and this publication (URI
        // resolution does not take the caller's policy gate) is skipped by
        // `insert_batch`: it keeps the id it was given there, unlisted here.
        ids.extend(store.insert_batch(batch)?);
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

/// M10: what a walk does when one of its reads never answers.
#[cfg(test)]
mod bounded_reads {
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::error::ErrorCode;
    use crate::read_boundary::tests::{boundary, Gate};
    use crate::read_boundary::{ReadFailure, ReadKind};
    use crate::walker::{walk_bounded, walk_bounded_batched, WalkOptions};

    fn tree() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        for dir in ["a/x", "slow/y", "z"] {
            fs::create_dir_all(td.path().join(dir)).unwrap();
        }
        fs::write(td.path().join("slow/y/deep.txt"), b"d").unwrap();
        fs::write(td.path().join("a/x/file.txt"), b"f").unwrap();
        td
    }

    #[test]
    fn a_stalled_subdirectory_is_reported_unread_and_the_rest_walks() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = boundary(&gate, 200);
        let start = Instant::now();
        let report = walk_bounded(
            td.path(),
            WalkOptions {
                parallelism: 4,
                ..WalkOptions::default()
            },
            None,
            &reads,
        )
        .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        let names: Vec<_> = report.entries.iter().map(|e| e.name.as_str()).collect();
        // The stalled directory itself is listed by its parent; only its
        // children are unknown.
        assert_eq!(names, ["a", "x", "file.txt", "slow", "z"]);
        assert_eq!(report.unread.len(), 1);
        assert_eq!(report.unread[0].path, slow);
        assert!(matches!(
            report.unread[0].failure,
            ReadFailure::TimedOut { .. }
        ));

        // While it is still stuck a second walk refuses it at once.
        let start = Instant::now();
        let again = walk_bounded(td.path(), WalkOptions::default(), None, &reads).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "{:?}",
            start.elapsed()
        );
        assert!(matches!(again.unread[0].failure, ReadFailure::Stuck { .. }));

        // Once the read returns, a retry walks the whole tree.
        gate.release_all();
        let start = Instant::now();
        while reads.stuck_reads() > 0 {
            assert!(start.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(5));
        }
        let healed = walk_bounded(td.path(), WalkOptions::default(), None, &reads).unwrap();
        assert!(healed.unread.is_empty());
        assert!(healed.entries.iter().any(|e| e.name == "deep.txt"));
    }

    /// The all-or-nothing forms (`walk`, `walk_with_ignore`, the batched
    /// pair) go through `into_complete`: a stalled subtree is an error, not
    /// a smaller tree.
    #[test]
    fn into_complete_turns_unread_directories_into_an_incomplete_error() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = boundary(&gate, 150);
        let report = walk_bounded(td.path(), WalkOptions::default(), None, &reads).unwrap();
        let err = report.into_complete().unwrap_err();
        assert_eq!(err.code(), ErrorCode::ETIMEDOUT);
        match err {
            crate::error::FxError::Incomplete { unread } => {
                assert_eq!(unread.len(), 1);
                assert_eq!(unread[0].path, slow);
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
        gate.release_all();

        let healthy = tree();
        let entries = walk_bounded(healthy.path(), WalkOptions::default(), None, &reads)
            .unwrap()
            .into_complete()
            .unwrap();
        assert!(entries.iter().any(|e| e.name == "deep.txt"));
    }

    #[test]
    fn a_stalled_root_fails_the_walk_with_etimedout() {
        let td = tree();
        for kind in [ReadKind::Stat, ReadKind::ListDir] {
            let gate = Arc::new(Gate::default());
            gate.park(kind, td.path());
            let reads = boundary(&gate, 100);
            let err = walk_bounded(td.path(), WalkOptions::default(), None, &reads).unwrap_err();
            assert_eq!(err.code(), ErrorCode::ETIMEDOUT, "{kind:?}: {err}");
            gate.release_all();
        }
    }

    #[test]
    fn a_batched_walk_stops_waiting_when_cancelled() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        gate.park(ReadKind::ListDir, td.path());
        let reads = boundary(&gate, 60_000);
        let cancel = Arc::new(AtomicBool::new(false));
        let flip = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flip.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        let err = walk_bounded_batched(
            td.path(),
            WalkOptions::default(),
            None,
            &reads,
            16,
            || cancel.load(Ordering::SeqCst),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ECANCELED);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        gate.release_all();
    }

    #[test]
    fn the_report_carries_emitted_ignore_files_in_walk_order() {
        let td = tree();
        fs::write(td.path().join(".gitignore"), "z/\n").unwrap();
        fs::write(td.path().join("a/.ignore"), "x/\n").unwrap();
        let gate = Arc::new(Gate::default());
        let reads = boundary(&gate, 1_000);
        let matcher = crate::ignore::IgnoreMatcher::new();
        let report = walk_bounded(
            td.path(),
            WalkOptions {
                parallelism: 4,
                ..WalkOptions::default()
            },
            Some(&matcher),
            &reads,
        )
        .unwrap();
        let files: Vec<_> = report
            .ignore_files
            .iter()
            .map(|(path, text)| {
                (
                    path.strip_prefix(td.path()).unwrap().to_path_buf(),
                    text.as_str(),
                )
            })
            .collect();
        assert_eq!(
            files,
            [(".gitignore".into(), "z/\n"), ("a/.ignore".into(), "x/\n")]
        );
        // Both rule files pruned what they name, without a second open.
        let names: Vec<_> = report.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"x") && !names.contains(&"file.txt"),
            "{names:?}"
        );
        assert!(names.contains(&"z"));
    }
}
