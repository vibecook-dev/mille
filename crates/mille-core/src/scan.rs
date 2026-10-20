//! Streaming, media-grade directory scan (`walk_scan`).
//!
//! [`walk`](crate::walker::walk) and friends build one `WalkedEntry` per
//! file — a full path, a second full path for the parent, a UTF-8 name copy —
//! and throw away the identity the `lstat` already fetched. A media scanner
//! needs the opposite trade: per entry, *what it is* (`dev`, `ino`, birth
//! time, nanosecond mtime, size), and no allocation it does not ask for.
//!
//! `walk_scan` hands the caller each child as a borrowed [`ScanEntry`] next
//! to a [`DirCtx`] for its directory. The directory's path is built once per
//! directory, never per entry; names are `&OsStr` out of the listing's arena,
//! so a name that is not UTF-8 is delivered rather than dropped. Directory
//! reads go through the caller's [`ReadBoundary`] — the same stall deadline,
//! stuck cap and dataless policy as every other Mille read — using the
//! fastest lister the platform has (see `scan_list`).
//!
//! The walk is **unordered** and **streaming**:
//!
//! - Up to `parallelism` directories are read at once; whichever listing
//!   lands first is delivered first. Within a directory, children arrive in
//!   filesystem order, consecutively, with the same `DirCtx`.
//! - A directory's own entry is always delivered before its children,
//!   because a directory is only read after the visitor has seen it and
//!   returned [`Visit::Continue`]. [`Visit::SkipChildren`] therefore costs
//!   nothing: the directory is never opened.
//! - Nothing is retained after the visitor returns, so memory is bounded by
//!   the listings in flight and the directories still queued, not by the
//!   size of the tree.
//!
//! Directories that could not be read come back in [`ScanReport::unread`],
//! never silently: a stalled NAS share or an `EACCES` folder is *unknown*,
//! and a caller diffing against a catalog must not read it as *empty*.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::entry::EntryKind;
use crate::error::{ErrorCode, FxError};
use crate::ignore::IgnoreMatcher;
use crate::read_boundary::{
    ReadBoundary, ReadFailure, ReadOutcome, ReadSignal, Ticket, WAIT_SLICE,
};
use crate::scan_list::{ListerKind, Record, ScanListOptions};
use crate::walker::{SymlinkPolicy, UnreadDirectory};

/// One child of a scanned directory, borrowed from its listing for the
/// duration of the visitor call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanEntry<'a> {
    /// The name exactly as the filesystem stores it (not necessarily UTF-8).
    pub name: &'a OsStr,
    /// What the walk acts on: for a followed symlink, the target's kind.
    /// `Symlink` means a link that was not followed, or did not resolve.
    /// Never `Unavailable`.
    pub kind: EntryKind,
    /// The entry itself is a symbolic link (followed or not).
    pub is_symlink: bool,
    /// Logical size in bytes of a file (a followed link's target); 0 for
    /// every other kind.
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Birth (creation) time, nanoseconds since the Unix epoch, where the
    /// filesystem records one: APFS/HFS+ always; Linux through `statx` on
    /// ext4/xfs/btrfs; `None` otherwise.
    pub btime_ns: Option<i64>,
    /// Device of the filesystem holding the entry (`st_dev`). 0 on Windows.
    pub dev: u64,
    /// File serial number (`st_ino`). With `dev`, the entry's identity:
    /// stable across renames and moves within the filesystem. 0 on Windows.
    pub ino: u64,
    /// Platform flags: `st_flags` on macOS (`UF_HIDDEN`, `SF_DATALESS`, …),
    /// `STATX_ATTR_*` on Linux's `statx` path, `FILE_ATTRIBUTE_*` on
    /// Windows; 0 where unknown.
    pub flags: u32,
    /// Hard links to a file (`st_nlink`): above 1, another path shares this
    /// `(dev, ino)` — the Sonarr/Radarr hardlink layout. Always 1 for
    /// directories, whose link counts are filesystem-specific.
    pub nlink: u64,
}

impl ScanEntry<'_> {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }

    /// `(dev, ino)`.
    pub fn identity(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }

    /// A macOS cloud placeholder (`SF_DATALESS`) whose contents are not on
    /// this machine. Reading it would download it.
    pub fn is_dataless(&self) -> bool {
        cfg!(target_os = "macos") && self.flags & SF_DATALESS != 0
    }
}

/// `<sys/stat.h>`.
const SF_DATALESS: u32 = 0x4000_0000;

/// The directory a batch of [`ScanEntry`]s belongs to.
#[derive(Clone, Copy, Debug)]
pub struct DirCtx<'a> {
    /// The directory's path: the walk root, joined with each name on the way
    /// down. Built once per directory.
    pub path: &'a Path,
    /// Depth of the directory; the root is 0, so its entries are at
    /// `depth + 1`.
    pub depth: usize,
    /// Walk-local id: 0 for the root, then unique per directory. Children of
    /// one directory are delivered consecutively, so a change of `id` means
    /// the previous directory is complete.
    pub id: u64,
    /// The parent directory's `id`; `None` for the root.
    pub parent: Option<u64>,
    /// The directory's own `(dev, ino)` (its target's, if reached through a
    /// symlink).
    pub dev: u64,
    pub ino: u64,
}

impl DirCtx<'_> {
    /// `path.join(name)`, for the entries the caller decides to keep.
    pub fn join(&self, name: &OsStr) -> PathBuf {
        self.path.join(name)
    }
}

/// What the visitor wants done with a directory entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Visit {
    /// Keep going; descend into a directory.
    #[default]
    Continue,
    /// Do not read this directory. No effect on other kinds.
    SkipChildren,
}

/// Which files a scan delivers, decided from the name alone so the lister
/// can drop a file before its `stat`. Directories are never filtered.
///
/// Two filters are the same (and may share an in-flight read) only if they
/// are clones of one `FileFilter`.
#[derive(Clone)]
pub struct FileFilter(Arc<dyn Fn(&OsStr) -> bool + Send + Sync>);

impl FileFilter {
    /// Keep the files `keep` returns true for. Runs on read helper threads.
    pub fn new<F>(keep: F) -> Self
    where
        F: Fn(&OsStr) -> bool + Send + Sync + 'static,
    {
        Self(Arc::new(keep))
    }

    /// Keep files whose extension (after the last `.`) is one of
    /// `extensions`, ASCII case-insensitively. A leading `.` is optional:
    /// `["mkv", ".MP4"]`.
    pub fn extensions<I, S>(extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let wanted: Vec<Vec<u8>> = extensions
            .into_iter()
            .map(|ext| {
                ext.as_ref()
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
                    .into_bytes()
            })
            .collect();
        Self::new(move |name| {
            let bytes = name.as_encoded_bytes();
            let Some(dot) = bytes.iter().rposition(|b| *b == b'.') else {
                return false;
            };
            let ext = &bytes[dot + 1..];
            wanted.iter().any(|want| want.eq_ignore_ascii_case(ext))
        })
    }

    /// Whether a file named `name` is delivered.
    pub fn keeps(&self, name: &OsStr) -> bool {
        (self.0)(name)
    }

    pub(crate) fn same_as(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }

    pub(crate) fn identity(&self) -> usize {
        Arc::as_ptr(&self.0).cast::<()>() as usize
    }
}

impl fmt::Debug for FileFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileFilter")
            .field(&format_args!("{:#x}", self.identity()))
            .finish()
    }
}

/// A cancellation flag shared between the caller and a running scan. The
/// scan checks it between directories and while waiting on a read, and
/// returns [`FxError::Cancelled`].
#[derive(Clone, Debug, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Which directory lister a scan uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScanLister {
    /// The platform's bulk lister (`getattrlistbulk` on macOS, `statx` on
    /// Linux), falling back per directory to `Portable`.
    #[default]
    Auto,
    /// `std::fs::read_dir` + `DirEntry::metadata` only. For comparison and
    /// for filesystems where the bulk path misbehaves.
    Portable,
}

/// How [`walk_scan`] walks.
#[derive(Clone)]
pub struct ScanOptions {
    /// Deepest level delivered; the root's children are depth 1. `None` is
    /// unlimited, `Some(0)` delivers nothing, `Some(1)` lists the root only.
    pub max_depth: Option<usize>,
    /// - `Never` (default): symlinks are delivered as `Symlink` and never
    ///   followed.
    /// - `Smart`: follow symlinks, but walk a linked directory only if its
    ///   `(dev, ino)` has not been walked yet in this scan. Real directories
    ///   are always walked. No `canonicalize` per link. The decision does
    ///   not depend on which listing answers first: links are decided only
    ///   once everything reachable without crossing a link has been walked
    ///   (then, level by level, everything reachable through the links
    ///   chosen so far), in path order. So a link to a directory of the same
    ///   tree is never walked, and of several links to one outside
    ///   directory, the first by path is.
    /// - `Always`: follow symlinks; a linked directory that is one of its own
    ///   ancestors (by `(dev, ino)`) is delivered but not walked.
    pub follow_symlinks: SymlinkPolicy,
    /// Deliver names starting with `.`. When false, a hidden directory is
    /// not walked either.
    pub include_hidden: bool,
    /// Skip OS/NAS litter — `.DS_Store`, AppleDouble `._*`, `@eaDir`,
    /// `#recycle`, `.Trash-*`, `Thumbs.db`, … — files and directories alike
    /// ([`DEFAULT_IGNORE_PATTERNS`](crate::scan_list::DEFAULT_IGNORE_PATTERNS)).
    pub default_ignores: bool,
    /// The caller's gitignore-style rules. An ignored entry is neither
    /// delivered nor walked.
    pub ignore: Option<Arc<IgnoreMatcher>>,
    /// Names of ignore files (gitignore format) honoured where they are
    /// found, governing their directory and everything below it — e.g.
    /// `.plexignore`. Their text is read inside the bounded listing.
    pub ignore_file_names: Vec<OsString>,
    /// Which files to deliver; `None` delivers every file.
    pub file_filter: Option<FileFilter>,
    /// Directory reads in flight at once (0 is treated as 1). Bounded by the
    /// boundary's own `concurrency` as well.
    pub parallelism: usize,
    /// Checked between directories and while waiting on reads.
    pub cancel: Option<CancelFlag>,
    pub lister: ScanLister,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: None,
            follow_symlinks: SymlinkPolicy::Never,
            include_hidden: true,
            default_ignores: true,
            ignore: None,
            ignore_file_names: Vec::new(),
            file_filter: None,
            parallelism: 8,
            cancel: None,
            lister: ScanLister::Auto,
        }
    }
}

impl ScanOptions {
    /// A media-library profile: follow symlinks with `(dev, ino)` dedup
    /// (`Smart`), skip hidden entries and the default litter, honour
    /// `.plexignore` and `.ignore` files.
    pub fn media() -> Self {
        Self {
            follow_symlinks: SymlinkPolicy::Smart,
            include_hidden: false,
            ignore_file_names: vec![".plexignore".into(), ".ignore".into()],
            ..Self::default()
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancelFlag::is_cancelled)
    }

    fn list_options(&self) -> ScanListOptions {
        ScanListOptions {
            follow_symlinks: !matches!(self.follow_symlinks, SymlinkPolicy::Never),
            include_hidden: self.include_hidden,
            default_ignores: self.default_ignores,
            ignore_file_names: self.ignore_file_names.clone().into(),
            file_filter: self.file_filter.clone(),
            lister: self.lister,
        }
    }
}

impl fmt::Debug for ScanOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanOptions")
            .field("max_depth", &self.max_depth)
            .field("follow_symlinks", &self.follow_symlinks)
            .field("include_hidden", &self.include_hidden)
            .field("default_ignores", &self.default_ignores)
            .field("ignore", &self.ignore.as_ref().map(|m| m.matcher_count()))
            .field("ignore_file_names", &self.ignore_file_names)
            .field("file_filter", &self.file_filter)
            .field("parallelism", &self.parallelism)
            .field("cancel", &self.cancel)
            .field("lister", &self.lister)
            .finish()
    }
}

/// Directories each lister served.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListerCounts {
    /// macOS `getattrlistbulk`.
    pub bulk: u64,
    /// Linux `getdents64` + `statx`.
    pub statx: u64,
    /// `read_dir` + `DirEntry::metadata` (by choice, or as the fallback).
    pub portable: u64,
}

/// What a scan saw besides the entries it delivered.
#[must_use = "`unread` directories are unknown, not empty"]
#[derive(Debug, Default)]
pub struct ScanReport {
    /// Directories below the root whose children are unknown: the read
    /// stalled past its deadline, was refused (stuck, saturated), hit a
    /// dataless placeholder, or failed with an OS error (`EACCES`, a
    /// directory removed mid-scan). Their own entry was delivered. A catalog
    /// must keep what it knew under them. Never the root: a root that cannot
    /// be read fails the scan.
    pub unread: Vec<UnreadDirectory>,
    /// Directories listed, the root included.
    pub dirs_listed: u64,
    /// Entries delivered to the visitor.
    pub entries: u64,
    /// Entries dropped by `include_hidden`, the default ignores, ignore rules
    /// or the file filter.
    pub skipped: u64,
    /// Entries listed but gone (or unreadable) by the time they were
    /// stat'ed.
    pub vanished: u64,
    /// Directories reached through a symlink and not walked: already walked
    /// (`Smart`) or an ancestor of the link (`Always`).
    pub links_not_walked: u64,
    /// The paths of those links (`links_not_walked` of them), each a
    /// directory entry the visitor saw whose children were never read. A
    /// caller that records which directories a scan listed must leave
    /// these out: under `Smart` the decision is taken after the visitor
    /// returned for the link.
    pub links_skipped: Vec<PathBuf>,
    /// Ignore files honoured, with their text, as they were found.
    pub ignore_files: Vec<(PathBuf, String)>,
    pub listers: ListerCounts,
}

impl ScanReport {
    /// Every directory below the root answered.
    pub fn is_complete(&self) -> bool {
        self.unread.is_empty()
    }
}

/// Scan `root`, calling `visit` with each entry the options keep.
///
/// The root itself is not delivered; its `(dev, ino)` is in the first
/// [`DirCtx`]. The root must be a directory (or a link to one) that answers:
/// otherwise the scan fails with the read's typed error — `ENOENT`,
/// `ENOTDIR`, `EACCES`, `ETIMEDOUT` for a root that did not answer in time,
/// `EDATALESS` for a cloud placeholder. Everything below the root that does
/// not answer is reported in [`ScanReport::unread`] instead.
///
/// For a library on a disk that sleeps, give `reads` a long first-response
/// deadline ([`ReadBoundaryOptions::media`](crate::ReadBoundaryOptions::media)).
///
/// The walk is synchronous; from async code run it on a blocking thread.
///
/// ```no_run
/// use std::path::Path;
/// use mille_core::{walk_scan, FileFilter, ReadBoundary, ReadBoundaryOptions, ScanOptions, Visit};
///
/// let reads = ReadBoundary::new(ReadBoundaryOptions::media());
/// let options = ScanOptions {
///     file_filter: Some(FileFilter::extensions(["mkv", "mp4", "srt"])),
///     ..ScanOptions::media()
/// };
/// let mut files = Vec::new();
/// let report = walk_scan(Path::new("/Volumes/Media/Movies"), &options, &reads, |dir, e| {
///     if e.is_file() {
///         // The only per-entry allocation is the one the caller makes.
///         files.push((dir.join(e.name), e.identity(), e.size, e.mtime_ns));
///     }
///     Visit::Continue
/// })?;
/// for unread in &report.unread {
///     // Unknown, not empty: keep whatever was known below `unread.path`.
///     eprintln!("not read: {:?} ({:?})", unread.path, unread.failure.code());
/// }
/// # Ok::<(), mille_core::FxError>(())
/// ```
pub fn walk_scan<F>(
    root: &Path,
    options: &ScanOptions,
    reads: &ReadBoundary,
    visit: F,
) -> Result<ScanReport, FxError>
where
    F: FnMut(&DirCtx<'_>, &ScanEntry<'_>) -> Visit,
{
    if options.is_cancelled() {
        return Err(FxError::Cancelled);
    }
    let stat = reads
        .stat(root, false)
        .map_err(|failure| failure.into_fx(root))?;
    let stat = stat
        .as_stat()
        .ok_or_else(|| FxError::InternalBug("bounded stat returned no metadata".into()))?;
    // A root symlink is always resolved: it is how the caller named the
    // library, whatever the policy for links inside it.
    let resolved = if stat.metadata.file_type().is_symlink() {
        stat.target
            .as_ref()
            .ok_or_else(|| typed(ErrorCode::ENOENT, root))?
    } else {
        &stat.metadata
    };
    if !resolved.is_dir() {
        return Err(typed(ErrorCode::ENOTDIR, root));
    }
    let mut report = ScanReport::default();
    if options.max_depth == Some(0) {
        return Ok(report);
    }
    let identity = Record::from_metadata(resolved);
    let follow = options.follow_symlinks;
    let root_job = DirJob {
        path: root.to_path_buf(),
        depth: 0,
        id: 0,
        parent: None,
        dev: identity.dev,
        ino: identity.ino,
        dataless: stat.is_dataless(),
        ancestors: (follow == SymlinkPolicy::Always).then(|| {
            Arc::new(Ancestor {
                dev: identity.dev,
                ino: identity.ino,
                up: None,
            })
        }),
    };
    let mut scan = Scan {
        options,
        reads,
        list: options.list_options(),
        signal: ReadSignal::new(),
        pending: vec![root_job],
        inflight: Vec::new(),
        walked: HashSet::new(),
        links: Vec::new(),
        nested: IgnoreMatcher::new(),
        next_id: 1,
        report: &mut report,
    };
    if follow == SymlinkPolicy::Smart {
        scan.walked.insert((identity.dev, identity.ino));
    }
    scan.run(visit)?;
    Ok(report)
}

fn typed(code: ErrorCode, path: &Path) -> FxError {
    let source = match code {
        ErrorCode::ENOTDIR => std::io::Error::other("not a directory"),
        _ => std::io::Error::from(std::io::ErrorKind::NotFound),
    };
    FxError::Io {
        code,
        path: path.to_path_buf(),
        source,
    }
}

/// A directory queued or being read.
struct DirJob {
    path: PathBuf,
    depth: usize,
    id: u64,
    parent: Option<u64>,
    dev: u64,
    ino: u64,
    /// Seen `SF_DATALESS` on its entry: the boundary refuses it without a
    /// syscall unless it materializes.
    dataless: bool,
    /// `(dev, ino)` of this directory and every one above it, for
    /// `SymlinkPolicy::Always` only.
    ancestors: Option<Arc<Ancestor>>,
}

struct Ancestor {
    dev: u64,
    ino: u64,
    up: Option<Arc<Ancestor>>,
}

impl Ancestor {
    fn contains(start: &Option<Arc<Ancestor>>, dev: u64, ino: u64) -> bool {
        let mut at = start.as_deref();
        while let Some(node) = at {
            if node.dev == dev && node.ino == ino {
                return true;
            }
            at = node.up.as_deref();
        }
        false
    }
}

struct Scan<'a> {
    options: &'a ScanOptions,
    reads: &'a ReadBoundary,
    list: ScanListOptions,
    signal: Arc<ReadSignal>,
    /// Directories to read, most recently found on top: the walk goes deep
    /// first, which keeps this short.
    pending: Vec<DirJob>,
    inflight: Vec<(DirJob, Ticket)>,
    /// `(dev, ino)` of every directory walked, for `SymlinkPolicy::Smart`.
    walked: HashSet<(u64, u64)>,
    /// `SymlinkPolicy::Smart`: directory links found since the last
    /// decision, held until nothing is pending or in flight (see
    /// `decide_links`).
    links: Vec<DirJob>,
    /// Ignore files found on the way down (`ignore_file_names`).
    nested: IgnoreMatcher,
    next_id: u64,
    report: &'a mut ScanReport,
}

impl Scan<'_> {
    fn run<F>(&mut self, mut visit: F) -> Result<(), FxError>
    where
        F: FnMut(&DirCtx<'_>, &ScanEntry<'_>) -> Visit,
    {
        let parallelism = self.options.parallelism.max(1);
        loop {
            if self.options.is_cancelled() {
                return Err(FxError::Cancelled);
            }
            self.fill(parallelism);
            if self.inflight.is_empty() {
                // Nothing pending either: the links found so far can now be
                // decided against everything walked.
                if self.decide_links() {
                    continue;
                }
                return Ok(());
            }
            // Take the token before polling, so a read that finishes between
            // the poll and the wait still wakes the wait.
            let seen = self.signal.seen();
            let mut resolved = false;
            let mut index = 0;
            while index < self.inflight.len() {
                match self.inflight[index].1.poll() {
                    Some(outcome) => {
                        let (job, _ticket) = self.inflight.swap_remove(index);
                        self.resolve(job, outcome, &mut visit)?;
                        resolved = true;
                        // Refill at once, so idle helpers get this listing's
                        // subdirectories without waiting for the rest of the
                        // pass.
                        self.fill(parallelism);
                    }
                    None => index += 1,
                }
            }
            if !resolved {
                let wait = self
                    .inflight
                    .iter()
                    .map(|(_, ticket)| ticket.until_deadline())
                    .min()
                    .unwrap_or(WAIT_SLICE)
                    .min(WAIT_SLICE);
                self.signal.wait_past(seen, wait);
            }
        }
    }

    /// `SymlinkPolicy::Smart`: walk each held directory link whose target
    /// was not walked, in path order, and report the rest. Called only when
    /// nothing is pending or in flight, so the set it decides against —
    /// every directory reachable without crossing a link, and then through
    /// the links chosen in earlier rounds — does not depend on the order
    /// listings answered in. (Deciding a link as soon as its listing landed
    /// made `Alias/X -> ../Real/X` walked or not depending on whether
    /// `Alias/` or `Real/` answered first.) Returns whether any link was
    /// queued.
    fn decide_links(&mut self) -> bool {
        if self.links.is_empty() {
            return false;
        }
        let mut links = std::mem::take(&mut self.links);
        links.sort_by(|a, b| a.path.cmp(&b.path));
        let mut queued = false;
        for mut link in links {
            if self.walked.insert((link.dev, link.ino)) {
                link.id = self.next_id;
                self.next_id += 1;
                self.pending.push(link);
                queued = true;
            } else {
                self.report.links_not_walked += 1;
                self.report.links_skipped.push(link.path);
            }
        }
        queued
    }

    /// Submit queued directories until `parallelism` reads are in flight.
    fn fill(&mut self, parallelism: usize) {
        while self.inflight.len() < parallelism {
            let Some(job) = self.pending.pop() else {
                break;
            };
            let ticket =
                self.reads
                    .scan_dir(&job.path, self.list.clone(), job.dataless, &self.signal);
            self.inflight.push((job, ticket));
        }
    }

    fn resolve<F>(
        &mut self,
        job: DirJob,
        outcome: ReadOutcome,
        visit: &mut F,
    ) -> Result<(), FxError>
    where
        F: FnMut(&DirCtx<'_>, &ScanEntry<'_>) -> Visit,
    {
        let output = match outcome {
            Ok(output) => output,
            Err(ReadFailure::Cancelled) => return Err(FxError::Cancelled),
            // The root is the library itself: if it does not answer, there
            // is nothing to report entries against.
            Err(failure) if job.id == 0 => return Err(failure.into_fx(&job.path)),
            Err(failure) => {
                self.report.unread.push(UnreadDirectory {
                    path: job.path,
                    failure,
                });
                return Ok(());
            }
        };
        let listing = output
            .as_scan()
            .ok_or_else(|| FxError::InternalBug("bounded scan returned no listing".into()))?;

        let report = &mut *self.report;
        report.dirs_listed += 1;
        report.skipped += listing.skipped();
        report.vanished += listing.vanished();
        match listing.lister() {
            ListerKind::Bulk => report.listers.bulk += 1,
            ListerKind::Statx => report.listers.statx += 1,
            ListerKind::Portable => report.listers.portable += 1,
        }
        // Ignore files first, so their rules govern their own siblings.
        for (name, text) in listing.ignore_files() {
            let file = job.path.join(name);
            // A file with a bad glob is skipped whole, as `add_from_file`
            // callers have always treated ignore rules: best effort.
            let _ = self.nested.add_from_contents(&file, text);
            report.ignore_files.push((file, text.clone()));
        }

        let options = self.options;
        let ctx = DirCtx {
            path: &job.path,
            depth: job.depth,
            id: job.id,
            parent: job.parent,
            dev: job.dev,
            ino: job.ino,
        };
        let child_depth = job.depth + 1;
        let descends = options.max_depth.is_none_or(|limit| child_depth < limit);
        let matcher = options.ignore.as_deref();
        // Ignore rules match on paths; reuse one buffer per directory.
        let mut scratch =
            (matcher.is_some() || self.nested.matcher_count() > 0).then(|| job.path.clone());
        // Subdirectories to read, in listing order. Under `Smart`, linked
        // directories are held in `self.links` and decided once the walk
        // has nothing else to read (`decide_links`).
        let mut real: Vec<DirJob> = Vec::new();

        for entry in listing.iter() {
            if let Some(path) = scratch.as_mut() {
                path.push(entry.name);
                let dir_like = entry.kind == EntryKind::Directory;
                let ignored = matcher
                    .and_then(|m| m.decision(path, dir_like))
                    .or_else(|| self.nested.decision(path, dir_like))
                    .unwrap_or(false);
                path.pop();
                if ignored {
                    report.skipped += 1;
                    continue;
                }
            }
            report.entries += 1;
            let verdict = visit(&ctx, &entry);
            if !descends || verdict == Visit::SkipChildren || entry.kind != EntryKind::Directory {
                continue;
            }
            if options.follow_symlinks == SymlinkPolicy::Always
                && entry.is_symlink
                && Ancestor::contains(&job.ancestors, entry.dev, entry.ino)
            {
                report.links_not_walked += 1;
                report.links_skipped.push(job.path.join(entry.name));
                continue;
            }
            let child = DirJob {
                path: job.path.join(entry.name),
                depth: child_depth,
                id: 0,
                parent: Some(job.id),
                dev: entry.dev,
                ino: entry.ino,
                dataless: entry.is_dataless(),
                ancestors: job.ancestors.as_ref().map(|up| {
                    Arc::new(Ancestor {
                        dev: entry.dev,
                        ino: entry.ino,
                        up: Some(Arc::clone(up)),
                    })
                }),
            };
            if options.follow_symlinks == SymlinkPolicy::Smart {
                if entry.is_symlink {
                    self.links.push(child);
                    continue;
                }
                self.walked.insert(entry.identity());
            }
            real.push(child);
        }
        for mut child in real {
            child.id = self.next_id;
            self.next_id += 1;
            self.pending.push(child);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::read_boundary::tests::{boundary, Gate};
    use crate::read_boundary::{ReadBoundaryOptions, ReadKind};

    /// An entry as delivered, owned, keyed by its full path.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Seen {
        kind: EntryKind,
        is_symlink: bool,
        size: u64,
        mtime_ns: i64,
        btime_ns: Option<i64>,
        dev: u64,
        ino: u64,
        flags: u32,
        nlink: u64,
        depth: usize,
    }

    fn seen(dir: &DirCtx<'_>, e: &ScanEntry<'_>) -> Seen {
        Seen {
            kind: e.kind,
            is_symlink: e.is_symlink,
            size: e.size,
            mtime_ns: e.mtime_ns,
            btime_ns: e.btime_ns,
            dev: e.dev,
            ino: e.ino,
            flags: e.flags,
            nlink: e.nlink,
            depth: dir.depth + 1,
        }
    }

    fn reads() -> ReadBoundary {
        ReadBoundary::new(ReadBoundaryOptions::default())
    }

    fn scan(
        root: &Path,
        options: &ScanOptions,
        reads: &ReadBoundary,
    ) -> (BTreeMap<PathBuf, Seen>, ScanReport) {
        let mut out = BTreeMap::new();
        let report = walk_scan(root, options, reads, |dir, e| {
            let previous = out.insert(dir.join(e.name), seen(dir, e));
            assert!(previous.is_none(), "{:?} delivered twice", dir.join(e.name));
            Visit::Continue
        })
        .unwrap();
        (out, report)
    }

    /// Each delivered path below `root`, `/`-separated on every platform so
    /// one set of expectations holds on Windows too, where the lossy string
    /// of `Movies\Heat (1995)` would never equal `Movies/Heat (1995)`.
    fn rel(root: &Path, map: &BTreeMap<PathBuf, Seen>) -> BTreeSet<String> {
        map.keys()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
    }

    /// What `lstat` says about `path`, normalised the way the scan
    /// documents (size for files only, link count 1 for directories).
    #[cfg(unix)]
    fn from_lstat(path: &Path) -> (EntryKind, u64, i64, Option<i64>, u64, u64, u64, u32) {
        use std::os::unix::fs::MetadataExt;
        let m = fs::symlink_metadata(path).unwrap();
        let kind = crate::scan_list::kind_of(m.file_type());
        let flags = {
            #[cfg(target_os = "macos")]
            {
                std::os::macos::fs::MetadataExt::st_flags(&m)
            }
            #[cfg(not(target_os = "macos"))]
            {
                0
            }
        };
        (
            kind,
            if kind == EntryKind::File { m.len() } else { 0 },
            m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            m.created().ok().map(crate::scan_list::system_time_ns),
            m.dev(),
            m.ino(),
            if kind == EntryKind::Directory {
                1
            } else {
                m.nlink()
            },
            flags,
        )
    }

    /// Movie-library-shaped tree with sidecars, a hardlink and a symlink.
    fn library() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let movie = root.join("Movies/Heat (1995)");
        let season = root.join("TV/Show/Season 01");
        fs::create_dir_all(&movie).unwrap();
        fs::create_dir_all(&season).unwrap();
        fs::write(movie.join("Heat (1995).mkv"), vec![0u8; 4096]).unwrap();
        fs::write(movie.join("Heat (1995).nfo"), b"<movie/>").unwrap();
        fs::write(movie.join("poster.jpg"), b"jpg").unwrap();
        for ep in 1..=3 {
            fs::write(
                season.join(format!("Show S01E0{ep}.mkv")),
                vec![1u8; ep * 100],
            )
            .unwrap();
        }
        fs::write(season.join("Show S01E01.en.srt"), b"1\n").unwrap();
        fs::hard_link(
            movie.join("Heat (1995).mkv"),
            root.join("Movies/heat-hardlink.mkv"),
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("Heat (1995)", root.join("Movies/heat-link")).unwrap();
        td
    }

    /// Every identity and time field equals what `lstat` reports, for the
    /// platform's bulk lister and for the portable one, and the two agree.
    #[cfg(unix)]
    #[test]
    fn identity_fields_match_std_metadata_for_both_listers() {
        let td = library();
        let mut views = Vec::new();
        for lister in [ScanLister::Auto, ScanLister::Portable] {
            let options = ScanOptions {
                lister,
                ..ScanOptions::default()
            };
            let (map, report) = scan(td.path(), &options, &reads());
            assert!(report.is_complete());
            assert_eq!(map.len(), 14, "{:#?}", rel(td.path(), &map));
            for (path, got) in &map {
                let (kind, size, mtime_ns, btime_ns, dev, ino, nlink, flags) = from_lstat(path);
                assert_eq!(got.kind, kind, "{path:?}");
                assert_eq!(got.size, size, "{path:?}");
                assert_eq!(got.mtime_ns, mtime_ns, "{path:?}");
                assert_eq!(got.btime_ns, btime_ns, "{path:?}");
                assert_eq!((got.dev, got.ino), (dev, ino), "{path:?}");
                assert_eq!(got.nlink, nlink, "{path:?}");
                assert_eq!(got.flags, flags, "{path:?}");
            }
            let hard = &map[&td.path().join("Movies/heat-hardlink.mkv")];
            let orig = &map[&td.path().join("Movies/Heat (1995)/Heat (1995).mkv")];
            assert_eq!((hard.dev, hard.ino, hard.nlink), (orig.dev, orig.ino, 2));
            let link = &map[&td.path().join("Movies/heat-link")];
            assert_eq!((link.kind, link.is_symlink), (EntryKind::Symlink, true));
            if lister == ScanLister::Auto && cfg!(any(target_os = "macos", target_os = "linux")) {
                assert_eq!(report.listers.portable, 0, "{:?}", report.listers);
            } else {
                assert_eq!(report.listers.portable, report.dirs_listed);
            }
            views.push(map);
        }
        assert_eq!(views[0], views[1]);
    }

    /// APFS records a birth time; a rewrite moves mtime and keeps btime and
    /// the inode — what lets flux tell "same file, new bytes" from "new file".
    #[cfg(target_os = "macos")]
    #[test]
    fn btime_is_reported_on_apfs_and_survives_a_rewrite() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("Movie (2020).mkv");
        fs::write(&file, b"one").unwrap();
        let (before, _) = scan(td.path(), &ScanOptions::default(), &reads());
        let before = before[&file].clone();
        let btime = before.btime_ns.expect("APFS records a birth time");
        assert!(btime > 1_600_000_000_000_000_000, "{btime}");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&file, b"two, longer").unwrap();
        let (after, _) = scan(td.path(), &ScanOptions::default(), &reads());
        let after = &after[&file];
        assert_eq!(after.btime_ns, before.btime_ns);
        assert_eq!((after.dev, after.ino), (before.dev, before.ino));
        assert!(after.mtime_ns > before.mtime_ns);
        assert_eq!(after.size, 11);
    }

    #[test]
    fn the_file_filter_keeps_matching_files_and_every_directory() {
        let td = library();
        let options = ScanOptions {
            file_filter: Some(FileFilter::extensions(["MKV", ".srt"])),
            ..ScanOptions::default()
        };
        for lister in [ScanLister::Auto, ScanLister::Portable] {
            let options = ScanOptions {
                lister,
                ..options.clone()
            };
            let (map, report) = scan(td.path(), &options, &reads());
            let names = rel(td.path(), &map);
            assert_eq!(
                names,
                BTreeSet::from(
                    [
                        "Movies",
                        "Movies/Heat (1995)",
                        "Movies/Heat (1995)/Heat (1995).mkv",
                        "Movies/heat-hardlink.mkv",
                        "TV",
                        "TV/Show",
                        "TV/Show/Season 01",
                        "TV/Show/Season 01/Show S01E01.en.srt",
                        "TV/Show/Season 01/Show S01E01.mkv",
                        "TV/Show/Season 01/Show S01E02.mkv",
                        "TV/Show/Season 01/Show S01E03.mkv",
                    ]
                    .map(String::from)
                ),
                "{lister:?}"
            );
            // nfo, poster and (unix) the unfollowed `heat-link`: a link that
            // is not followed is filtered by its own name.
            let links = u64::from(cfg!(unix));
            assert_eq!(report.skipped, 2 + links, "{lister:?}");
        }
        let filter = FileFilter::extensions(["mkv"]);
        assert!(filter.keeps(OsStr::new("a.b.MkV")));
        assert!(!filter.keeps(OsStr::new("mkv")));
        assert!(!filter.keeps(OsStr::new("a.mkv.part")));
    }

    #[test]
    fn default_ignores_skip_litter_files_and_whole_litter_directories() {
        let td = library();
        let root = td.path();
        for dir in ["@eaDir/Heat.mkv", "#recycle/old", ".Trash-1000/files"] {
            fs::create_dir_all(root.join("Movies").join(dir)).unwrap();
        }
        fs::write(
            root.join("Movies/@eaDir/Heat.mkv/SYNOINDEX_MEDIA_INFO"),
            b"x",
        )
        .unwrap();
        fs::write(root.join("Movies/#recycle/old/Old (1990).mkv"), b"x").unwrap();
        for junk in [".DS_Store", "._Heat (1995).mkv", "Thumbs.db"] {
            fs::write(root.join("Movies").join(junk), b"x").unwrap();
        }
        fs::write(root.join("Movies/.hidden.mkv"), b"x").unwrap();

        let (map, report) = scan(root, &ScanOptions::default(), &reads());
        let names = rel(root, &map);
        assert!(names.contains("Movies/.hidden.mkv"), "hidden is not litter");
        for gone in [
            "Movies/@eaDir",
            "Movies/#recycle",
            "Movies/.Trash-1000",
            "Movies/.DS_Store",
            "Movies/._Heat (1995).mkv",
            "Movies/Thumbs.db",
        ] {
            assert!(!names.contains(gone), "{gone} delivered");
        }
        assert!(!names
            .iter()
            .any(|n| n.contains("SYNOINDEX") || n.contains("Old (1990)")));
        assert_eq!(report.skipped, 6);

        // `include_hidden: false` also drops dot-names, and does not walk
        // hidden directories; turning the defaults off delivers everything.
        let media = ScanOptions::media();
        let (map, _) = scan(root, &media, &reads());
        assert!(!rel(root, &map).contains("Movies/.hidden.mkv"));
        let raw = ScanOptions {
            default_ignores: false,
            ..ScanOptions::default()
        };
        let (map, _) = scan(root, &raw, &reads());
        let names = rel(root, &map);
        assert!(names.contains("Movies/#recycle/old/Old (1990).mkv"));
        assert!(names.contains("Movies/._Heat (1995).mkv"));
    }

    #[test]
    fn ignore_rules_and_nested_ignore_files_prune_what_they_name() {
        let td = library();
        let root = td.path();
        fs::create_dir_all(root.join("Movies/Heat (1995)/Extras")).unwrap();
        fs::write(root.join("Movies/Heat (1995)/Extras/trailer.mkv"), b"x").unwrap();
        fs::write(
            root.join("TV/Show/.plexignore"),
            "*.srt\nSeason 01/Show S01E03.mkv\n",
        )
        .unwrap();
        let mut matcher = IgnoreMatcher::new();
        matcher.add_from_string(root, "Extras/\n").unwrap();
        let options = ScanOptions {
            ignore: Some(Arc::new(matcher)),
            ignore_file_names: vec![".plexignore".into()],
            ..ScanOptions::default()
        };
        for lister in [ScanLister::Auto, ScanLister::Portable] {
            let options = ScanOptions {
                lister,
                ..options.clone()
            };
            let (map, report) = scan(root, &options, &reads());
            let names = rel(root, &map);
            assert!(!names.iter().any(|n| n.contains("Extras")), "{names:?}");
            assert!(!names.contains("TV/Show/Season 01/Show S01E01.en.srt"));
            assert!(!names.contains("TV/Show/Season 01/Show S01E03.mkv"));
            assert!(names.contains("TV/Show/Season 01/Show S01E02.mkv"));
            // The ignore file itself is an ordinary (delivered) file.
            assert!(names.contains("TV/Show/.plexignore"));
            assert_eq!(
                report.ignore_files,
                vec![(
                    root.join("TV/Show/.plexignore"),
                    "*.srt\nSeason 01/Show S01E03.mkv\n".to_string()
                )]
            );
        }
    }

    /// APFS refuses names that are not UTF-8 (EILSEQ), so on macOS this
    /// checks only that the attempt is refused; on Linux the name must come
    /// back byte for byte. The arena itself is tested in `scan_list`.
    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_delivered_not_dropped() {
        use std::os::unix::ffi::OsStrExt;
        let td = tempfile::tempdir().unwrap();
        let raw: &[u8] = b"Am\xe9lie (2001)";
        let dir = td.path().join(OsStr::from_bytes(raw));
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(err) if err.raw_os_error() == Some(92) && cfg!(target_os = "macos") => return,
            Err(err) => panic!("create {dir:?}: {err}"),
        }
        fs::write(dir.join("film.mkv"), b"x").unwrap();
        for lister in [ScanLister::Auto, ScanLister::Portable] {
            let mut names = Vec::new();
            let report = walk_scan(
                td.path(),
                &ScanOptions {
                    lister,
                    ..ScanOptions::default()
                },
                &reads(),
                |dir, e| {
                    names.push(dir.join(e.name));
                    Visit::Continue
                },
            )
            .unwrap();
            assert!(report.is_complete());
            assert_eq!(names.len(), 2, "{names:?}");
            assert!(names.contains(&dir));
            assert!(names.contains(&dir.join("film.mkv")));
        }
    }

    #[test]
    fn skip_children_never_opens_the_directory() {
        let td = library();
        let season = td.path().join("TV/Show/Season 01");
        let gate = Arc::new(Gate::default());
        let reads = boundary(&gate, 5_000);
        let mut delivered = Vec::new();
        let report = walk_scan(td.path(), &ScanOptions::default(), &reads, |dir, e| {
            delivered.push(dir.join(e.name));
            if e.name == "Season 01" {
                Visit::SkipChildren
            } else {
                Visit::Continue
            }
        })
        .unwrap();
        assert!(report.is_complete());
        assert!(delivered.contains(&season));
        assert!(!delivered
            .iter()
            .any(|p| p.starts_with(&season) && *p != season));
        assert_eq!(gate.entered(&season), 0);
        assert_eq!(gate.entered(&td.path().join("TV/Show")), 1);
    }

    #[test]
    fn a_stalled_subdirectory_is_unread_and_everything_else_is_delivered() {
        let td = library();
        let season = td.path().join("TV/Show/Season 01");
        let gate = Arc::new(Gate::default());
        gate.park(ReadKind::ListDir, &season);
        let reads = boundary(&gate, 200);
        let start = Instant::now();
        let (map, report) = scan(td.path(), &ScanOptions::default(), &reads);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(!report.is_complete());
        assert_eq!(report.unread.len(), 1);
        assert_eq!(report.unread[0].path, season);
        assert!(matches!(
            report.unread[0].failure,
            ReadFailure::TimedOut { .. }
        ));
        // The stalled directory's own entry is delivered; nothing below it.
        assert!(map.contains_key(&season));
        assert!(!map.keys().any(|p| p.starts_with(&season) && *p != season));
        assert!(map.contains_key(&td.path().join("Movies/Heat (1995)/poster.jpg")));
        gate.release_all();
    }

    #[test]
    fn a_stalled_root_fails_with_etimedout_and_a_waking_one_gets_its_first_response_deadline() {
        let td = library();
        let gate = Arc::new(Gate::default());
        gate.park(ReadKind::ListDir, td.path());
        let impatient = boundary(&gate, 100);
        let err = walk_scan(td.path(), &ScanOptions::default(), &impatient, |_, _| {
            Visit::Continue
        })
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ETIMEDOUT);
        gate.release_all();

        // A disk that takes 900 ms to answer its first read, under a 300 ms
        // stall deadline and a 10 s first-response deadline: a full scan.
        // Once answered, a read has the stall deadline only, so it is wide
        // enough for a busy CI runner to finish a tiny listing.
        let gate = Arc::new(Gate::default());
        gate.park(ReadKind::ListDir, td.path());
        let patient = ReadBoundary::new(ReadBoundaryOptions {
            timeout: Duration::from_millis(300),
            first_response: Some(Duration::from_secs(10)),
            hook: Some(Arc::clone(&gate) as Arc<dyn crate::read_boundary::ReadHook>),
            ..ReadBoundaryOptions::default()
        });
        let waker = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(900));
                gate.release_all();
            })
        };
        let (map, report) = scan(td.path(), &ScanOptions::default(), &patient);
        waker.join().unwrap();
        assert!(report.is_complete(), "{:?}", report.unread);
        // `library()` holds 13 entries, plus its `heat-link` symlink on Unix.
        assert_eq!(map.len(), 13 + usize::from(cfg!(unix)));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_subdirectory_is_unread_with_its_os_error() {
        use std::os::unix::fs::PermissionsExt;
        let td = library();
        let locked = td.path().join("TV/Show");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything; the check is meaningless there.
        let readable = fs::read_dir(&locked).is_ok();
        let (map, report) = scan(td.path(), &ScanOptions::default(), &reads());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            return;
        }
        assert_eq!(report.unread.len(), 1);
        assert_eq!(report.unread[0].path, locked);
        assert_eq!(report.unread[0].failure.code(), ErrorCode::EACCES);
        assert!(!report.unread[0].failure.is_unanswered());
        assert!(map.contains_key(&locked));
    }

    #[test]
    fn a_missing_or_file_root_fails_with_its_own_code() {
        let td = library();
        let missing = td.path().join("nope");
        let err = walk_scan(&missing, &ScanOptions::default(), &reads(), |_, _| {
            Visit::Continue
        })
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ENOENT);
        let file = td.path().join("Movies/heat-hardlink.mkv");
        let err = walk_scan(&file, &ScanOptions::default(), &reads(), |_, _| {
            Visit::Continue
        })
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ENOTDIR);
    }

    #[test]
    fn a_cancel_flag_stops_the_scan() {
        let td = library();
        let cancel = CancelFlag::new();
        let options = ScanOptions {
            cancel: Some(cancel.clone()),
            parallelism: 1,
            ..ScanOptions::default()
        };
        let mut delivered = 0;
        let err = walk_scan(td.path(), &options, &reads(), |_, _| {
            delivered += 1;
            cancel.cancel();
            Visit::Continue
        })
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ECANCELED);
        // The rest of the first directory, at most.
        assert!(delivered <= 2, "{delivered}");
    }

    #[test]
    fn max_depth_bounds_what_is_delivered() {
        let td = library();
        let depth = |max_depth| {
            let options = ScanOptions {
                max_depth,
                ..ScanOptions::default()
            };
            let (map, _) = scan(td.path(), &options, &reads());
            map.values().map(|s| s.depth).max().unwrap_or(0)
        };
        assert_eq!(depth(Some(0)), 0);
        assert_eq!(depth(Some(1)), 1);
        assert_eq!(depth(Some(2)), 2);
        assert_eq!(depth(None), 4);
        let (map, _) = scan(
            td.path(),
            &ScanOptions {
                max_depth: Some(1),
                ..ScanOptions::default()
            },
            &reads(),
        );
        assert_eq!(
            rel(td.path(), &map),
            BTreeSet::from(["Movies".into(), "TV".into()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_policies_follow_dedupe_and_cut_cycles() {
        use std::os::unix::fs::symlink;
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        fs::create_dir_all(root.join("real/inner")).unwrap();
        fs::write(root.join("real/inner/a.mkv"), b"aaaa").unwrap();
        symlink("real", root.join("alias")).unwrap();
        // A link back to the root: a cycle under every following policy.
        symlink("..", root.join("real/up")).unwrap();
        symlink("missing", root.join("dangling")).unwrap();
        for lister in [ScanLister::Auto, ScanLister::Portable] {
            let run = |follow_symlinks| {
                scan(
                    root,
                    &ScanOptions {
                        follow_symlinks,
                        lister,
                        ..ScanOptions::default()
                    },
                    &reads(),
                )
            };

            let (never, report) = run(SymlinkPolicy::Never);
            assert_eq!(never[&root.join("alias")].kind, EntryKind::Symlink);
            assert!(!never.contains_key(&root.join("alias/inner")));
            assert_eq!(report.links_not_walked, 0);

            // Smart: the link resolves to `real`, already walked: delivered
            // as a directory reached through a link, not walked again.
            let (smart, report) = run(SymlinkPolicy::Smart);
            let alias = &smart[&root.join("alias")];
            assert_eq!((alias.kind, alias.is_symlink), (EntryKind::Directory, true));
            assert_eq!(alias.ino, smart[&root.join("real")].ino);
            assert!(!smart.contains_key(&root.join("alias/inner")));
            assert!(smart.contains_key(&root.join("real/inner/a.mkv")));
            assert_eq!(smart[&root.join("dangling")].kind, EntryKind::Symlink);
            assert_eq!(report.links_not_walked, 2, "alias + up: {lister:?}");

            // Always: every non-ancestor link is walked (a.mkv twice); the
            // link to an ancestor is delivered but not walked.
            let (always, report) = run(SymlinkPolicy::Always);
            assert!(always.contains_key(&root.join("alias/inner/a.mkv")));
            assert!(always.contains_key(&root.join("real/inner/a.mkv")));
            assert!(always.contains_key(&root.join("real/up")));
            assert!(!always.contains_key(&root.join("real/up/real")));
            // `alias/up` points at the root too.
            assert_eq!(report.links_not_walked, 2, "{lister:?}");
        }
    }

    /// INT-5. A link was decided as soon as its own listing landed, so with
    /// `Alias/X -> Real/X` in one root, X's files came out twice when
    /// `Alias/` answered before `Real/` (link walked, then the real
    /// directory too) and once otherwise; which of two links to one outside
    /// directory was walked flipped the same way. Here each order is forced
    /// by parking the other sibling until the first one has been delivered.
    #[cfg(unix)]
    #[test]
    fn smart_link_decisions_do_not_depend_on_which_listing_answers_first() {
        use std::os::unix::fs::symlink;
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let movie = root.join("Real/The Matrix (1999)");
        fs::create_dir_all(&movie).unwrap();
        fs::write(movie.join("The Matrix (1999).mkv"), b"mkv").unwrap();
        fs::create_dir_all(root.join("Alias")).unwrap();
        symlink(&movie, root.join("Alias/The Matrix (1999)")).unwrap();
        // Two links to one directory outside the root.
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("extra.mkv"), b"x").unwrap();
        symlink(outside.path(), root.join("Alias/b-outside")).unwrap();
        symlink(outside.path(), root.join("Real/a-outside")).unwrap();

        let options = ScanOptions {
            follow_symlinks: SymlinkPolicy::Smart,
            ..ScanOptions::default()
        };
        let mut views = Vec::new();
        for (first, second) in [("Alias", "Real"), ("Real", "Alias")] {
            let (first, second) = (root.join(first), root.join(second));
            let gate = Arc::new(Gate::default());
            gate.park(ReadKind::ListDir, &second);
            let reads = boundary(&gate, 10_000);
            let mut delivered = BTreeSet::new();
            let report = walk_scan(root, &options, &reads, |dir, e| {
                if dir.path == first {
                    gate.release(ReadKind::ListDir, &second);
                }
                let path = dir.join(e.name);
                delivered.insert(path.strip_prefix(root).unwrap().to_path_buf());
                Visit::Continue
            })
            .unwrap();
            gate.release_all();
            assert!(report.is_complete(), "{:?}", report.unread);
            views.push((delivered, report.links_not_walked, report.links_skipped));
        }
        assert_eq!(views[0], views[1], "the walk depends on listing order");
        let (delivered, not_walked, skipped) = &views[0];
        assert!(delivered.contains(Path::new("Real/The Matrix (1999)/The Matrix (1999).mkv")));
        // The alias is delivered, as a directory reached through a link, and
        // never walked: its target is part of the tree.
        assert!(delivered.contains(Path::new("Alias/The Matrix (1999)")));
        assert!(!delivered.contains(Path::new("Alias/The Matrix (1999)/The Matrix (1999).mkv")));
        // Of the two links to one outside directory, the first by path.
        assert!(delivered.contains(Path::new("Alias/b-outside/extra.mkv")));
        assert!(!delivered.contains(Path::new("Real/a-outside/extra.mkv")));
        assert_eq!(*not_walked, 2);
        assert_eq!(
            skipped,
            &vec![
                root.join("Alias/The Matrix (1999)"),
                root.join("Real/a-outside")
            ]
        );
    }

    #[test]
    fn parallel_and_serial_scans_deliver_the_same_entries() {
        let td = library();
        let one = ScanOptions {
            parallelism: 1,
            ..ScanOptions::default()
        };
        let many = ScanOptions {
            parallelism: 16,
            ..ScanOptions::default()
        };
        // Windows lists with `read_dir`, whose metadata is the copy NTFS keeps
        // in the parent directory's index, and NTFS refreshes that copy
        // lazily. `library()` moves `TV`'s mtime when it creates `TV/Show`, so
        // the first scan can still read the old value and the second the new
        // one (seen on windows-latest: 1.2 ms apart, btime equal). That is
        // the filesystem settling, not the two scans disagreeing.
        let settled = |mut map: BTreeMap<PathBuf, Seen>| {
            if cfg!(windows) {
                for seen in map.values_mut() {
                    if seen.kind == EntryKind::Directory {
                        seen.mtime_ns = 0;
                    }
                }
            }
            map
        };
        assert_eq!(
            settled(scan(td.path(), &one, &reads()).0),
            settled(scan(td.path(), &many, &reads()).0)
        );
    }

    /// The streaming contract: a directory's entry arrives before anything
    /// inside it, and a directory's children arrive together.
    #[test]
    fn a_directory_is_delivered_before_its_children_and_its_children_together() {
        let td = library();
        let mut delivered_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        let mut finished: BTreeSet<u64> = BTreeSet::new();
        let mut current: Option<u64> = None;
        let mut ids: HashMap<PathBuf, u64> = HashMap::new();
        let report = walk_scan(td.path(), &ScanOptions::default(), &reads(), |dir, e| {
            if current != Some(dir.id) {
                if let Some(previous) = current {
                    assert!(finished.insert(previous), "directory {previous} resumed");
                }
                assert!(!finished.contains(&dir.id));
                current = Some(dir.id);
            }
            if dir.id != 0 {
                assert!(
                    delivered_dirs.contains(dir.path),
                    "{:?} before its entry",
                    dir.path
                );
            }
            let id = *ids.entry(dir.path.to_path_buf()).or_insert(dir.id);
            assert_eq!(id, dir.id);
            if e.is_dir() {
                delivered_dirs.insert(dir.join(e.name));
            }
            Visit::Continue
        })
        .unwrap();
        assert_eq!(report.dirs_listed, 6);
    }

    /// The real test library: every file the generator wrote is delivered
    /// with the identity `lstat` reports, and the litter it planted is not.
    #[cfg(unix)]
    #[test]
    fn the_tiny_test_library_is_scanned_completely() {
        let root = Path::new("/Volumes/SamsungRed/flux-work/media/library-tiny");
        let manifest = root.with_extension("manifest.jsonl");
        let (Ok(text), true) = (fs::read_to_string(&manifest), root.is_dir()) else {
            eprintln!("skipped: {root:?} not present");
            return;
        };
        let files: BTreeSet<String> = text
            .lines()
            .filter_map(|line| {
                let start = line.find("\"path\": \"")? + 9;
                let end = start + line[start..].find('"')?;
                Some(line[start..end].to_string())
            })
            .collect();
        assert_eq!(files.len(), 424);

        let everything = ScanOptions {
            default_ignores: false,
            ..ScanOptions::default()
        };
        let (map, report) = scan(root, &everything, &reads());
        assert!(report.is_complete());
        let delivered: BTreeSet<String> = map
            .iter()
            .filter(|(_, s)| s.kind == EntryKind::File)
            .map(|(p, _)| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(delivered, files);
        for (path, got) in &map {
            let (kind, size, mtime_ns, btime_ns, dev, ino, nlink, _) = from_lstat(path);
            assert_eq!(
                (
                    got.kind,
                    got.size,
                    got.mtime_ns,
                    got.btime_ns,
                    got.dev,
                    got.ino,
                    got.nlink
                ),
                (kind, size, mtime_ns, btime_ns, dev, ino, nlink),
                "{path:?}"
            );
        }

        let (media, _) = scan(root, &ScanOptions::media(), &reads());
        let kept: BTreeSet<String> = media
            .iter()
            .filter(|(_, s)| s.kind == EntryKind::File)
            .map(|(p, _)| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        let expected: BTreeSet<String> = files
            .iter()
            .filter(|f| {
                !f.split('/').any(|part| {
                    part.starts_with('.') || crate::scan_list::is_default_ignored(part.as_bytes())
                })
            })
            .cloned()
            .collect();
        assert_eq!(kept, expected);
        assert_eq!(
            files.len() - kept.len(),
            7,
            "2 × .DS_Store, 2 × @eaDir, 2 × Thumbs.db, .hidden.mkv"
        );
    }
}
