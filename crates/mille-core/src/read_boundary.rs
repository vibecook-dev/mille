//! Time-bounded native reads (petition M10).
//!
//! Every directory listing and every `stat` the walker, expansion, prefetch
//! and reveal paths make goes through a `ReadBoundary`. The reason is a
//! kernel behaviour, not a slow disk: on macOS an iCloud-evicted folder
//! ("dataless") blocks `readdir` inside the kernel while the File Provider
//! downloads it, and VibeField measured one Keynote package holding a read
//! for minutes. NFS/SMB/FUSE mounts hang the same way. A blocked syscall
//! cannot be interrupted, so the bound is built around that fact:
//!
//! - **Reads run on a small helper pool**, never on the caller's thread. The
//!   caller waits on a ticket; when the read makes no progress for
//!   `timeout`, the caller gets `ReadFailure::TimedOut` and moves on, while
//!   the kernel call keeps its helper thread until it returns. The deadline
//!   is a *stall* deadline — every entry read and every `lstat` ticks
//!   progress — so a huge-but-moving directory on a slow share is never cut
//!   off, and a call that is stuck in the kernel is.
//! - **The first response gets its own deadline.** A spun-down hard disk or a
//!   sleeping NAS blocks the *first* call into it for as long as the platters
//!   take to spin up, with no progress to tick, and then answers normally.
//!   `first_response` bounds a read until it shows its first sign of life;
//!   `timeout` (the stall deadline) bounds every gap after that. The default
//!   keeps them equal, which is the behaviour the IDE explorer was built on;
//!   [`ReadBoundaryOptions::media`] gives a media library 30 s to wake a disk
//!   and still 2 s per stall, so "the drive is asleep" is never reported as
//!   "the drive is empty".
//! - **Stuck reads are capped.** The pool has `concurrency + max_stuck`
//!   threads at most. Once `max_stuck` reads are past their deadline, new
//!   reads are refused at once (`Saturated`) instead of queueing behind them.
//! - **No pile-up on one path.** A request for a path (or a descendant of a
//!   path) whose read is stuck is refused at once (`Stuck`): a lookup below a
//!   dataless folder blocks on the same materialization, so a second thread
//!   would buy nothing. A request that matches a read still in progress
//!   joins it instead of issuing a second kernel call.
//! - **`close` never waits for the kernel.** Queued reads are dropped,
//!   waiters return `Cancelled`, and stuck helpers exit when their call
//!   eventually returns. Nothing joins them, so dispose cannot hang.
//!
//! The pool threads also carry the dataless policy. With
//! `materialize_dataless` off (the default), each helper sets
//! `IOPOL_MATERIALIZE_DATALESS_FILES_OFF` on itself, so the kernel answers
//! `EDEADLK` instead of starting a download, and a directory already known
//! to be `SF_DATALESS` is not read at all — both surface as
//! `ReadFailure::Dataless`. Listing is a metadata operation; materializing a
//! dataless directory under the File Provider can fetch a whole package (the
//! Keynote case), so a folder browser must not do it implicitly. With the
//! option on, helpers set the policy to ON and the read may download — and
//! is still bounded by the deadline above.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::{FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::error::{ErrorCode, FxError};
use crate::ignore::IGNORE_FILE_NAMES;
use crate::scan_list::{ScanListOptions, ScanListing};

/// Default stall deadline. VibeField bounds its own reads outside mille at
/// 2 s; a healthy local `getdirentries`/`lstat` never comes near it, and
/// because the deadline measures *no progress*, a large directory on a slow
/// share keeps reading as long as entries keep arriving.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(2_000);

/// First-response deadline of [`ReadBoundaryOptions::media`]. How long a
/// disk takes to spin up from standby is hardware-specific (SMART attribute
/// 3, `Spin_Up_Time`), and a NAS may wake several disks in turn, so this errs
/// long. It only delays the verdict on a dead mount, never a healthy read.
pub const MEDIA_FIRST_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default number of reads allowed to sit past their deadline at once. Each
/// holds one parked helper thread until the kernel returns.
pub const DEFAULT_MAX_STUCK_READS: usize = 16;

/// Healthy concurrent reads for a boundary built without an explicit value.
const DEFAULT_CONCURRENCY: usize = 8;

/// Idle helpers exit after this long, so a quiet explorer holds no threads.
const IDLE_KEEPALIVE: Duration = Duration::from_secs(10);

/// How long a waiter spins on a running read before parking, and how long an
/// idle helper spins for the next read before parking.
const SPIN_BEFORE_PARK: Duration = Duration::from_micros(50);

/// Upper bound on one condvar wait, so a waiter notices cancellation and a
/// moved progress mark without a notification per entry read.
pub(crate) const WAIT_SLICE: Duration = Duration::from_millis(50);

/// Test seam: runs on the helper thread immediately before the read's first
/// syscall, exactly where the kernel would block. CI cannot evict iCloud
/// files, so the suites install a hook that parks chosen paths instead, and
/// can tick `progress` to model a read that is slow but moving.
pub trait ReadHook: Send + Sync {
    fn before_read(&self, kind: ReadKind, path: &Path, progress: &ReadProgress);
}

/// Which bounded operation a read is. Part of the join key: a `stat` and a
/// listing of the same path are different reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReadKind {
    /// `lstat`, plus `stat` through a symlink.
    Stat,
    /// `readdir` plus an `lstat` per child.
    ListDir,
    /// The `.gitignore`-family files directly inside a directory.
    IgnoreFiles,
    /// `opendir` only: can this directory be read at all.
    OpenDir,
}

#[derive(Clone)]
pub struct ReadBoundaryOptions {
    /// Stall deadline: a read that has answered at least once and then makes
    /// no progress for this long times out.
    pub timeout: Duration,
    /// Deadline for a read's *first* sign of life, measured from when it was
    /// queued. `None` uses `timeout`. Set it longer than `timeout` for
    /// storage that sleeps (see the module comment): a read still waiting for
    /// a disk to spin up is not stuck, and is not counted against
    /// `max_stuck`, until this passes.
    pub first_response: Option<Duration>,
    /// Reads allowed past their deadline before new reads are refused.
    pub max_stuck: usize,
    /// Healthy concurrent reads (helper threads beyond the stuck budget).
    pub concurrency: usize,
    /// Let reads download dataless (cloud-evicted) directories. Off by
    /// default — see the module comment.
    pub materialize_dataless: bool,
    /// Test seam; `None` in production.
    pub hook: Option<Arc<dyn ReadHook>>,
}

impl Default for ReadBoundaryOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_READ_TIMEOUT,
            first_response: None,
            max_stuck: DEFAULT_MAX_STUCK_READS,
            concurrency: DEFAULT_CONCURRENCY,
            materialize_dataless: false,
            hook: None,
        }
    }
}

impl ReadBoundaryOptions {
    /// A profile for media libraries on disks that sleep: 30 s for the first
    /// response ([`MEDIA_FIRST_RESPONSE_TIMEOUT`]), the default 2 s stall
    /// deadline after that, and no dataless materialization.
    pub fn media() -> Self {
        Self {
            first_response: Some(MEDIA_FIRST_RESPONSE_TIMEOUT),
            ..Self::default()
        }
    }
}

impl std::fmt::Debug for ReadBoundaryOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadBoundaryOptions")
            .field("timeout", &self.timeout)
            .field("first_response", &self.first_response)
            .field("max_stuck", &self.max_stuck)
            .field("concurrency", &self.concurrency)
            .field("materialize_dataless", &self.materialize_dataless)
            .field("hook", &self.hook.is_some())
            .finish()
    }
}

/// Why a bounded read produced no answer. Everything except `Io` means the
/// read did not complete, so the caller knows nothing new about the path and
/// must not treat the absence of children as authoritative.
#[derive(Clone, Debug)]
pub enum ReadFailure {
    /// No progress for the deadline. The kernel call continues in the
    /// background; a later request retries once it has returned.
    TimedOut { stalled_for: Duration },
    /// A read into this path, or into one of its ancestors, is still stuck.
    Stuck { blocked_on: PathBuf },
    /// `max_stuck` reads are stuck; no new read is started until one returns.
    Saturated { stuck: usize },
    /// A cloud placeholder whose contents are not local, and the boundary
    /// does not materialize.
    Dataless,
    /// The boundary was closed (explorer disposed) or the caller cancelled.
    Cancelled,
    /// The operating system answered with an error.
    Io(IoFailure),
}

impl ReadFailure {
    /// True when the read did not complete (deadline, refusal, dataless,
    /// cancellation) rather than completing with an OS error.
    pub fn is_unanswered(&self) -> bool {
        !matches!(self, ReadFailure::Io(_))
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            ReadFailure::TimedOut { .. }
            | ReadFailure::Stuck { .. }
            | ReadFailure::Saturated { .. } => ErrorCode::ETIMEDOUT,
            ReadFailure::Dataless => ErrorCode::EDATALESS,
            ReadFailure::Cancelled => ErrorCode::ECANCELED,
            ReadFailure::Io(io) => {
                ErrorCode::from_io_error(&io.to_io_error(), PathBuf::new()).code()
            }
        }
    }

    /// The typed, path-carrying error a caller reports for this read.
    pub fn into_fx(self, path: &Path) -> FxError {
        let typed = |code: ErrorCode, kind: io::ErrorKind, message: String| FxError::Io {
            code,
            path: path.to_path_buf(),
            source: io::Error::new(kind, message),
        };
        match self {
            ReadFailure::TimedOut { stalled_for } => typed(
                ErrorCode::ETIMEDOUT,
                io::ErrorKind::TimedOut,
                format!(
                    "native read made no progress for {} ms; it continues in the \
                     background and a later request will retry",
                    stalled_for.as_millis()
                ),
            ),
            ReadFailure::Stuck { blocked_on } => typed(
                ErrorCode::ETIMEDOUT,
                io::ErrorKind::TimedOut,
                format!(
                    "a read into {} is still stuck; not starting another until it returns",
                    blocked_on.display()
                ),
            ),
            ReadFailure::Saturated { stuck } => typed(
                ErrorCode::ETIMEDOUT,
                io::ErrorKind::TimedOut,
                format!("{stuck} native reads are stuck; refusing new reads until one returns"),
            ),
            ReadFailure::Dataless => typed(
                ErrorCode::EDATALESS,
                io::ErrorKind::Other,
                "not downloaded: a cloud placeholder (dataless), and this explorer does \
                 not materialize"
                    .to_string(),
            ),
            ReadFailure::Cancelled => FxError::Cancelled,
            ReadFailure::Io(io) => ErrorCode::from_io_error(&io.to_io_error(), path.to_path_buf()),
        }
    }
}

/// A cloneable copy of an `io::Error`: one read's outcome is shared by every
/// request that joined it, and `io::Error` is not `Clone`.
#[derive(Clone, Debug)]
pub struct IoFailure {
    raw: Option<i32>,
    kind: io::ErrorKind,
    message: String,
}

impl IoFailure {
    fn from_io(err: &io::Error) -> Self {
        Self {
            raw: err.raw_os_error(),
            kind: err.kind(),
            message: err.to_string(),
        }
    }

    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw
    }

    pub fn to_io_error(&self) -> io::Error {
        match self.raw {
            Some(raw) => io::Error::from_raw_os_error(raw),
            None => io::Error::new(self.kind, self.message.clone()),
        }
    }
}

/// Progress mark for one read: nanoseconds since the boundary's epoch of the
/// last thing the read accomplished. The waiter's deadline is measured from
/// it, so ticking is what separates "slow" from "stuck".
pub struct ReadProgress {
    epoch: Instant,
    last: AtomicU64,
    /// The read has answered at least once (any `tick`), so the stall
    /// deadline applies from here on instead of the first-response one.
    responded: AtomicBool,
}

impl ReadProgress {
    fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            last: AtomicU64::new(0),
            responded: AtomicBool::new(false),
        }
    }

    /// Record that the read accomplished something: an entry, a batch, a
    /// returned syscall. The first tick also ends the first-response wait.
    pub fn tick(&self) {
        self.mark();
        if !self.responded.load(Ordering::Relaxed) {
            self.responded.store(true, Ordering::Release);
        }
    }

    /// A helper picked the read up. That is activity — the queue wait is
    /// over — but not a response from the filesystem, which is what a
    /// sleeping disk withholds.
    fn start(&self) {
        self.mark();
    }

    fn mark(&self) {
        let nanos = self.epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.last.store(nanos.max(1), Ordering::Release);
    }

    fn responded(&self) -> bool {
        self.responded.load(Ordering::Acquire)
    }

    fn last(&self) -> Option<Instant> {
        match self.last.load(Ordering::Acquire) {
            0 => None,
            nanos => Some(self.epoch + Duration::from_nanos(nanos)),
        }
    }
}

/// `lstat` of a path, plus what a walker needs to treat it as a root.
#[derive(Debug)]
pub struct PathStat {
    /// `lstat` of the path itself.
    pub metadata: Metadata,
    /// For a symlink, `stat` through it; `None` when it does not resolve.
    pub target: Option<Metadata>,
    /// The canonical path, when the request asked for it.
    pub canonical: Option<PathBuf>,
}

impl PathStat {
    /// The path, or the directory a symlink resolves to, is a dataless
    /// placeholder.
    pub fn is_dataless(&self) -> bool {
        is_dataless(&self.metadata) || self.target.as_ref().is_some_and(is_dataless)
    }

    pub fn is_dir_like(&self) -> bool {
        self.metadata.is_dir() || self.target.as_ref().is_some_and(Metadata::is_dir)
    }
}

/// One child of a bounded listing.
#[derive(Debug)]
pub struct ListedChild {
    pub name: OsString,
    /// `readdir`'s own type for the entry: never follows a symlink.
    pub file_type: FileType,
    /// The type the walk acts on: the target's type for a followed symlink,
    /// otherwise `file_type`.
    pub effective_type: FileType,
    /// `lstat` of the child, or `stat` through it for a followed symlink.
    /// `None` when that call failed (the entry vanished mid-listing).
    pub metadata: Option<Metadata>,
    /// For a symlink the listing does not follow: whether it points at a
    /// directory.
    pub target_is_dir: Option<bool>,
    /// For a followed symlink to a directory: its canonical target, which the
    /// walker compares against the directories above it to stop cycles.
    pub canonical: Option<PathBuf>,
    /// A followed symlink that does not resolve. Such entries are dropped,
    /// matching the walk crate this replaced.
    pub follow_failed: bool,
    /// Text of a `.gitignore`-family file, when the listing asked for it.
    pub ignore_contents: Option<String>,
}

impl ListedChild {
    pub fn is_dataless(&self) -> bool {
        self.metadata.as_ref().is_some_and(is_dataless)
    }
}

/// A bounded directory listing, sorted by name (byte order).
#[derive(Debug, Default)]
pub struct DirListing {
    pub children: Vec<ListedChild>,
}

/// What a listing reads besides names and `lstat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ListOptions {
    /// Follow symlinks: report the target's type and metadata, and resolve
    /// the canonical target of directory links.
    pub follow_symlinks: bool,
    /// Read the text of ignore files (by name, when they are files or link
    /// to files), so the caller never opens them itself.
    pub ignore_contents: bool,
}

/// An ignore file read by `ReadBoundary::ignore_files`.
#[derive(Clone, Debug)]
pub struct IgnoreFile {
    pub path: PathBuf,
    pub contents: String,
}

// Each output is allocated once per read and shared behind an `Arc`, so the
// size of the largest variant is never copied around.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ReadOutput {
    Stat(PathStat),
    Listing(DirListing),
    IgnoreFiles(Vec<IgnoreFile>),
    Opened,
    /// A scan listing (`walk_scan`): identity and times for every child,
    /// names in one arena.
    Scan(ScanListing),
}

impl ReadOutput {
    pub fn as_stat(&self) -> Option<&PathStat> {
        match self {
            ReadOutput::Stat(stat) => Some(stat),
            _ => None,
        }
    }

    pub fn as_listing(&self) -> Option<&DirListing> {
        match self {
            ReadOutput::Listing(listing) => Some(listing),
            _ => None,
        }
    }

    pub fn as_scan(&self) -> Option<&ScanListing> {
        match self {
            ReadOutput::Scan(listing) => Some(listing),
            _ => None,
        }
    }
}

pub type ReadOutcome = Result<Arc<ReadOutput>, ReadFailure>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Op {
    Stat { canonical: bool },
    List(ListOptions),
    Scan(ScanListOptions),
    IgnoreFiles,
    OpenDir,
}

impl Op {
    fn kind(&self) -> ReadKind {
        match self {
            Op::Stat { .. } => ReadKind::Stat,
            // A scan listing blocks exactly where a plain listing does, so
            // the test seam treats them alike; the join key tells them apart.
            Op::List(_) | Op::Scan(_) => ReadKind::ListDir,
            Op::IgnoreFiles => ReadKind::IgnoreFiles,
            Op::OpenDir => ReadKind::OpenDir,
        }
    }
}

type FlightKey = (Op, PathBuf);

struct Flight {
    key: FlightKey,
    submitted: Instant,
    progress: ReadProgress,
    started: AtomicBool,
    /// Mirrors `outcome.is_some()` so a waiter can spin on it lock-free.
    finished: AtomicBool,
    /// Live tickets. A queued read nobody waits for is dropped unstarted.
    interest: AtomicUsize,
    outcome: Mutex<Option<ReadOutcome>>,
    done: Condvar,
    /// Walks to wake when this read finishes (see `ReadSignal`).
    listeners: Mutex<Vec<Arc<ReadSignal>>>,
}

impl Flight {
    fn path(&self) -> &Path {
        &self.key.1
    }

    /// The last moment this read did something: submission, start, or the
    /// latest progress tick.
    fn last_activity(&self) -> Instant {
        match self.progress.last() {
            Some(at) if at > self.submitted => at,
            _ => self.submitted,
        }
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// In the kernel and past the deadline.
    fn is_stuck(&self, now: Instant, shared: &Shared) -> bool {
        self.started.load(Ordering::Acquire)
            && !self.is_finished()
            && now.saturating_duration_since(self.last_activity()) >= shared.deadline(self)
    }

    fn finish(&self, outcome: ReadOutcome) {
        let mut slot = self.outcome.lock();
        if slot.is_none() {
            *slot = Some(outcome);
            self.finished.store(true, Ordering::Release);
        }
        self.done.notify_all();
        drop(slot);
        for signal in std::mem::take(&mut *self.listeners.lock()) {
            signal.notify();
        }
    }

    fn wake_listeners(&self) {
        for signal in self.listeners.lock().iter() {
            signal.notify();
        }
    }
}

struct State {
    queue: VecDeque<Arc<Flight>>,
    /// Queued and running reads, for joining and for the stuck checks.
    flights: HashMap<FlightKey, Arc<Flight>>,
    threads: usize,
    idle: usize,
    /// Idle helpers spinning for the next read (at most one; see
    /// `next_flight`).
    spinning: usize,
    closed: bool,
}

struct Shared {
    /// Stall deadline, once a read has answered.
    timeout: Duration,
    /// Deadline until a read first answers.
    first_response: Duration,
    max_stuck: usize,
    max_threads: usize,
    materialize_dataless: bool,
    hook: Option<Arc<dyn ReadHook>>,
    epoch: Instant,
    closed: AtomicBool,
    /// Mirrors `state.queue.len()`, so an idle helper can watch for work
    /// without taking the lock.
    queued: AtomicUsize,
    state: Mutex<State>,
    work: Condvar,
}

/// Closes the pool when the last `ReadBoundary` handle goes away. Helper
/// threads hold `Shared`, not this, so they never keep a boundary open.
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Handle to one bounded read pool. Cheap to clone; every clone shares the
/// pool, its deadline and its stuck accounting.
#[derive(Clone)]
pub struct ReadBoundary {
    shared: Arc<Shared>,
    _owner: Arc<Owner>,
}

impl std::fmt::Debug for ReadBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadBoundary")
            .field("timeout", &self.shared.timeout)
            .field("first_response", &self.shared.first_response)
            .field("max_stuck", &self.shared.max_stuck)
            .field("max_threads", &self.shared.max_threads)
            .field("materialize_dataless", &self.shared.materialize_dataless)
            .finish()
    }
}

impl ReadBoundary {
    pub fn new(options: ReadBoundaryOptions) -> Self {
        let max_stuck = options.max_stuck.max(1);
        let timeout = options.timeout.max(Duration::from_millis(1));
        let shared = Arc::new(Shared {
            timeout,
            first_response: options
                .first_response
                .map_or(timeout, |first| first.max(Duration::from_millis(1))),
            max_stuck,
            max_threads: options.concurrency.max(1) + max_stuck,
            materialize_dataless: options.materialize_dataless,
            hook: options.hook,
            epoch: Instant::now(),
            closed: AtomicBool::new(false),
            queued: AtomicUsize::new(0),
            state: Mutex::new(State {
                queue: VecDeque::new(),
                flights: HashMap::new(),
                threads: 0,
                idle: 0,
                spinning: 0,
                closed: false,
            }),
            work: Condvar::new(),
        });
        Self {
            _owner: Arc::new(Owner(Arc::clone(&shared))),
            shared,
        }
    }

    /// The process-wide boundary behind the plain `walk*` functions, with
    /// default options. Never closed.
    pub fn shared_default() -> &'static ReadBoundary {
        static DEFAULT: OnceLock<ReadBoundary> = OnceLock::new();
        DEFAULT.get_or_init(|| ReadBoundary::new(ReadBoundaryOptions::default()))
    }

    /// The stall deadline: how long a read that has answered may go without
    /// progress.
    pub fn timeout(&self) -> Duration {
        self.shared.timeout
    }

    /// The first-response deadline: how long a read may wait for its first
    /// answer (equal to [`timeout`](Self::timeout) unless configured).
    pub fn first_response(&self) -> Duration {
        self.shared.first_response
    }

    pub fn materializes_dataless(&self) -> bool {
        self.shared.materialize_dataless
    }

    /// `lstat` the path (and `stat` through a symlink), bounded.
    pub fn stat(&self, path: &Path, canonical: bool) -> Result<Arc<ReadOutput>, ReadFailure> {
        self.submit(path, Op::Stat { canonical }).wait()
    }

    /// Start listing `dir`. The read runs in the background until the ticket
    /// is waited on; `known_dataless` short-circuits a directory the caller
    /// already saw `SF_DATALESS` on, when the boundary does not materialize.
    ///
    /// `signal`, when given, is notified as soon as the read finishes, so a
    /// walk can keep several reads in flight and act on whichever lands first.
    pub fn list_dir(
        &self,
        dir: &Path,
        options: ListOptions,
        known_dataless: bool,
        signal: Option<&Arc<ReadSignal>>,
    ) -> Ticket {
        self.submit_listing(dir, Op::List(options), known_dataless, signal)
    }

    /// Start a scan listing of `dir` (see `scan_list`): every child's
    /// identity and times in one read, through the platform's bulk lister.
    pub(crate) fn scan_dir(
        &self,
        dir: &Path,
        options: ScanListOptions,
        known_dataless: bool,
        signal: &Arc<ReadSignal>,
    ) -> Ticket {
        self.submit_listing(dir, Op::Scan(options), known_dataless, Some(signal))
    }

    fn submit_listing(
        &self,
        dir: &Path,
        op: Op,
        known_dataless: bool,
        signal: Option<&Arc<ReadSignal>>,
    ) -> Ticket {
        if known_dataless && !self.shared.materialize_dataless {
            return Ticket::ready(&self.shared, Err(ReadFailure::Dataless));
        }
        let ticket = self.submit(dir, op);
        if let (Some(flight), Some(signal)) = (&ticket.flight, signal) {
            flight.listeners.lock().push(Arc::clone(signal));
            // It may have finished before the listener was in place.
            if flight.is_finished() {
                signal.notify();
            }
        }
        ticket
    }

    /// The ignore files directly inside `dir`, bounded. Missing or unreadable
    /// files are skipped, as `IgnoreMatcher::add_from_file` callers did.
    pub fn ignore_files(&self, dir: &Path) -> Result<Vec<IgnoreFile>, ReadFailure> {
        let output = self.submit(dir, Op::IgnoreFiles).wait()?;
        match &*output {
            ReadOutput::IgnoreFiles(files) => Ok(files.clone()),
            _ => Ok(Vec::new()),
        }
    }

    /// Open `dir` without listing it, bounded.
    pub fn open_dir(&self, dir: &Path) -> Result<(), ReadFailure> {
        self.submit(dir, Op::OpenDir).wait().map(|_| ())
    }

    /// Reads currently past their deadline.
    pub fn stuck_reads(&self) -> usize {
        let now = Instant::now();
        let state = self.shared.state.lock();
        state
            .flights
            .values()
            .filter(|flight| flight.is_stuck(now, &self.shared))
            .count()
    }

    /// Stop the pool without waiting for the kernel: queued reads are
    /// dropped, every waiter returns `Cancelled`, new reads are refused, and
    /// helpers exit as their calls return. Idempotent.
    pub fn close(&self) {
        self.shared.close();
    }

    fn submit(&self, path: &Path, op: Op) -> Ticket {
        let shared = &self.shared;
        if shared.closed.load(Ordering::Acquire) {
            return Ticket::ready(shared, Err(ReadFailure::Cancelled));
        }
        let now = Instant::now();
        let key: FlightKey = (op, path.to_path_buf());
        let mut state = shared.state.lock();
        if state.closed {
            return Ticket::ready(shared, Err(ReadFailure::Cancelled));
        }
        if let Some(flight) = state.flights.get(&key) {
            // Join a read that is still running. A finished one lingers in
            // the map until its helper re-takes the lock to remove it; its
            // answer predates this request, so it gets a read of its own
            // (`remove_flight` only removes its own `Arc`).
            if !flight.is_finished() && !flight.is_stuck(now, shared) {
                flight.interest.fetch_add(1, Ordering::AcqRel);
                return Ticket::joined(shared, Arc::clone(flight));
            }
        }
        let mut stuck = 0;
        for flight in state.flights.values() {
            if flight.is_stuck(now, shared) {
                if path.starts_with(flight.path()) {
                    return Ticket::ready(
                        shared,
                        Err(ReadFailure::Stuck {
                            blocked_on: flight.path().to_path_buf(),
                        }),
                    );
                }
                stuck += 1;
            }
        }
        if stuck >= shared.max_stuck {
            return Ticket::ready(shared, Err(ReadFailure::Saturated { stuck }));
        }

        let flight = Arc::new(Flight {
            key: key.clone(),
            submitted: now,
            progress: ReadProgress::new(shared.epoch),
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            interest: AtomicUsize::new(1),
            outcome: Mutex::new(None),
            done: Condvar::new(),
            listeners: Mutex::new(Vec::new()),
        });
        state.flights.insert(key, Arc::clone(&flight));
        state.queue.push_back(Arc::clone(&flight));
        shared.queued.fetch_add(1, Ordering::Release);
        if state.queue.len() > state.idle && state.threads < shared.max_threads {
            state.threads += 1;
            let worker = Arc::clone(shared);
            let spawned = std::thread::Builder::new()
                .name("mille-read".into())
                .spawn(move || worker_main(worker));
            if spawned.is_err() {
                state.threads -= 1;
            }
        }
        shared.work.notify_one();
        drop(state);
        Ticket::joined(shared, flight)
    }
}

impl Shared {
    /// The deadline that applies to `flight` now: the first-response one
    /// until it has answered, the stall one after.
    fn deadline(&self, flight: &Flight) -> Duration {
        if flight.progress.responded() {
            self.timeout
        } else {
            self.first_response
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        let queued: Vec<_> = state.queue.drain(..).collect();
        self.queued.store(0, Ordering::Release);
        for flight in &queued {
            state.flights.remove(&flight.key);
            flight.finish(Err(ReadFailure::Cancelled));
        }
        // Running reads keep their thread until the kernel returns; wake their
        // waiters now so they observe `closed` and leave.
        for flight in state.flights.values() {
            flight.done.notify_all();
            flight.wake_listeners();
        }
        self.work.notify_all();
    }
}

/// A pending bounded read. Dropping it withdraws interest: a read nobody is
/// waiting for is not started.
pub struct Ticket {
    shared: Arc<Shared>,
    flight: Option<Arc<Flight>>,
    ready: Option<ReadOutcome>,
}

impl Ticket {
    fn ready(shared: &Arc<Shared>, outcome: ReadOutcome) -> Self {
        Self {
            shared: Arc::clone(shared),
            flight: None,
            ready: Some(outcome),
        }
    }

    fn joined(shared: &Arc<Shared>, flight: Arc<Flight>) -> Self {
        Self {
            shared: Arc::clone(shared),
            flight: Some(flight),
            ready: None,
        }
    }

    /// The outcome if the read has one now: finished, stalled past the
    /// deadline (`TimedOut`), or the boundary closed. `None` while it is
    /// still within its deadline.
    pub fn poll(&self) -> Option<ReadOutcome> {
        if let Some(outcome) = &self.ready {
            return Some(outcome.clone());
        }
        let flight = self.flight.as_ref()?;
        if flight.is_finished() {
            return flight.outcome.lock().clone();
        }
        if self.shared.closed.load(Ordering::Acquire) {
            return Some(Err(ReadFailure::Cancelled));
        }
        let stalled_for = Instant::now().saturating_duration_since(flight.last_activity());
        (stalled_for >= self.shared.deadline(flight))
            .then_some(Err(ReadFailure::TimedOut { stalled_for }))
    }

    /// Time left before this read counts as stalled, at its current progress.
    pub fn until_deadline(&self) -> Duration {
        match &self.flight {
            Some(flight) if self.ready.is_none() => self
                .shared
                .deadline(flight)
                .saturating_sub(Instant::now().saturating_duration_since(flight.last_activity())),
            _ => Duration::ZERO,
        }
    }

    /// Wait for the read, at most until it has made no progress for the
    /// boundary's deadline.
    pub fn wait(self) -> ReadOutcome {
        self.wait_with(|| false)
    }

    /// `wait`, returning `Cancelled` as soon as `cancelled` reports true.
    pub fn wait_with<C: FnMut() -> bool>(mut self, mut cancelled: C) -> ReadOutcome {
        if let Some(outcome) = self.ready.take() {
            return outcome;
        }
        let Some(flight) = self.flight.clone() else {
            return Err(ReadFailure::Cancelled);
        };
        // A cached listing takes the helper tens of microseconds — less than
        // parking and waking this thread. Spin that long before sleeping, or a
        // hot walk pays a scheduler round trip per directory.
        let spin_until = Instant::now() + SPIN_BEFORE_PARK;
        while !flight.is_finished() && Instant::now() < spin_until {
            std::hint::spin_loop();
        }
        let mut slot = flight.outcome.lock();
        loop {
            if let Some(outcome) = slot.as_ref() {
                return outcome.clone();
            }
            if self.shared.closed.load(Ordering::Acquire) || cancelled() {
                return Err(ReadFailure::Cancelled);
            }
            let activity = flight.last_activity();
            let now = Instant::now();
            let stalled_for = now.saturating_duration_since(activity);
            // Re-read every slice: the first answer switches the read from
            // the first-response deadline to the stall deadline.
            let deadline = self.shared.deadline(&flight);
            if stalled_for >= deadline {
                return Err(ReadFailure::TimedOut { stalled_for });
            }
            let remaining = deadline - stalled_for;
            flight.done.wait_for(&mut slot, remaining.min(WAIT_SLICE));
        }
    }
}

/// Wakes a walk when any read it started finishes. One per walk: the walk
/// keeps several listings in flight and plans whichever lands first, so a
/// directory's children are queued without waiting for the depth-first
/// emission to reach it — the parallelism jwalk got from its own workers.
#[derive(Default)]
pub struct ReadSignal {
    seq: AtomicU64,
    lock: Mutex<()>,
    cv: Condvar,
}

impl ReadSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A token for `wait_past`: take it before checking for finished reads.
    pub fn seen(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    fn notify(&self) {
        self.seq.fetch_add(1, Ordering::AcqRel);
        let _guard = self.lock.lock();
        self.cv.notify_all();
    }

    /// Return once anything finished after `seen` was taken, or `timeout`
    /// passes. Spins briefly first, as `Ticket::wait_with` does.
    pub fn wait_past(&self, seen: u64, timeout: Duration) {
        let start = Instant::now();
        let spin_until = start + SPIN_BEFORE_PARK.min(timeout);
        while self.seen() == seen && Instant::now() < spin_until {
            std::hint::spin_loop();
        }
        let deadline = start + timeout;
        let mut guard = self.lock.lock();
        while self.seen() == seen {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            self.cv.wait_for(&mut guard, deadline - now);
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(flight) = &self.flight {
            flight.interest.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn worker_main(shared: Arc<Shared>) {
    set_thread_materialization(shared.materialize_dataless);
    let mut state = shared.state.lock();
    while let Some(flight) = next_flight(&shared, &mut state) {
        MutexGuard::unlocked(&mut state, || {
            // A panic here (a hook, or a bug) must still finish the flight,
            // or its path would read as stuck forever and the thread count
            // would drift.
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&shared, &flight)))
                    .unwrap_or_else(|_| {
                        Err(ReadFailure::Io(IoFailure {
                            raw: None,
                            kind: io::ErrorKind::Other,
                            message: "native read helper panicked".into(),
                        }))
                    });
            flight.finish(outcome);
        });
        remove_flight(&mut state, &flight);
    }
}

/// The next read for this helper, or `None` when it should exit (closed, or
/// idle past `IDLE_KEEPALIVE`; the thread count is already decremented).
///
/// An idle helper spins for `SPIN_BEFORE_PARK` before parking — the mirror
/// of the waiter's spin in `Ticket::wait_with`. A walk submits its next read
/// microseconds after this one finishes; parking and being woken for it cost
/// more than the read itself on a cached tree (measured: ~25 µs per directory
/// on a serial `walk_scan` of 8.5k directories, load ~20). The spinning
/// helper counts as idle, so `submit` hands it the read instead of spawning.
/// Only one helper spins at a time: one is enough to take the next read
/// without a wake-up, and the rest would only burn CPU.
fn next_flight(shared: &Shared, state: &mut MutexGuard<'_, State>) -> Option<Arc<Flight>> {
    let mut spun = false;
    loop {
        if state.closed {
            state.threads -= 1;
            return None;
        }
        if let Some(flight) = state.queue.pop_front() {
            shared.queued.fetch_sub(1, Ordering::AcqRel);
            if flight.interest.load(Ordering::Acquire) == 0 {
                // Every requester gave up before a thread was free.
                remove_flight(state, &flight);
                flight.finish(Err(ReadFailure::Cancelled));
                continue;
            }
            flight.started.store(true, Ordering::Release);
            flight.progress.start();
            return Some(flight);
        }
        state.idle += 1;
        if !spun && state.spinning == 0 {
            spun = true;
            state.spinning += 1;
            MutexGuard::unlocked(state, || {
                let until = Instant::now() + SPIN_BEFORE_PARK;
                while shared.queued.load(Ordering::Acquire) == 0
                    && !shared.closed.load(Ordering::Acquire)
                    && Instant::now() < until
                {
                    std::hint::spin_loop();
                }
            });
            state.spinning -= 1;
            state.idle -= 1;
            continue;
        }
        spun = true;
        let timed_out = shared.work.wait_for(state, IDLE_KEEPALIVE).timed_out();
        state.idle -= 1;
        if timed_out && state.queue.is_empty() {
            state.threads -= 1;
            return None;
        }
    }
}

fn remove_flight(state: &mut State, flight: &Arc<Flight>) {
    if state
        .flights
        .get(&flight.key)
        .is_some_and(|current| Arc::ptr_eq(current, flight))
    {
        state.flights.remove(&flight.key);
    }
}

fn run(shared: &Shared, flight: &Flight) -> ReadOutcome {
    let (op, path) = &flight.key;
    let progress = &flight.progress;
    if let Some(hook) = &shared.hook {
        hook.before_read(op.kind(), path, progress);
        progress.tick();
    }
    let result = match op {
        Op::Stat { canonical } => stat_path(path, *canonical).map(ReadOutput::Stat),
        Op::List(options) => list_dir(path, *options, progress).map(ReadOutput::Listing),
        Op::Scan(options) => {
            crate::scan_list::scan_dir(path, options, progress).map(ReadOutput::Scan)
        }
        Op::IgnoreFiles => read_ignore_files(path, progress).map(ReadOutput::IgnoreFiles),
        Op::OpenDir => std::fs::read_dir(path).map(|_| ReadOutput::Opened),
    };
    result
        .map(Arc::new)
        .map_err(|err| classify(&err, shared.materialize_dataless))
}

/// Map an OS error from a helper thread. With materialization off the kernel
/// refuses a dataless object with `EDEADLK` — that is the placeholder state,
/// not a lock conflict.
fn classify(err: &io::Error, materialize_dataless: bool) -> ReadFailure {
    if !materialize_dataless && is_materialization_refusal(err) {
        return ReadFailure::Dataless;
    }
    ReadFailure::Io(IoFailure::from_io(err))
}

#[cfg(target_os = "macos")]
fn is_materialization_refusal(err: &io::Error) -> bool {
    err.raw_os_error() == Some(darwin::EDEADLK)
}

#[cfg(not(target_os = "macos"))]
fn is_materialization_refusal(_err: &io::Error) -> bool {
    false
}

fn stat_path(path: &Path, canonical: bool) -> io::Result<PathStat> {
    let metadata = std::fs::symlink_metadata(path)?;
    let target = if metadata.file_type().is_symlink() {
        std::fs::metadata(path).ok()
    } else {
        None
    };
    let canonical = if canonical {
        std::fs::canonicalize(path).ok()
    } else {
        None
    };
    Ok(PathStat {
        metadata,
        target,
        canonical,
    })
}

fn list_dir(dir: &Path, options: ListOptions, progress: &ReadProgress) -> io::Result<DirListing> {
    let mut children = Vec::new();
    for item in std::fs::read_dir(dir)? {
        progress.tick();
        // A failed entry or type read is dropped, as the walk crate did.
        let Ok(item) = item else { continue };
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        let name = item.file_name();
        let path = dir.join(&name);
        let mut child = ListedChild {
            name,
            file_type,
            effective_type: file_type,
            metadata: None,
            target_is_dir: None,
            canonical: None,
            follow_failed: false,
            ignore_contents: None,
        };
        if options.follow_symlinks && file_type.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(metadata) => {
                    child.effective_type = metadata.file_type();
                    if metadata.is_dir() {
                        match std::fs::canonicalize(&path) {
                            Ok(target) => child.canonical = Some(target),
                            Err(_) => child.follow_failed = true,
                        }
                    }
                    child.metadata = Some(metadata);
                }
                Err(_) => child.follow_failed = true,
            }
        } else {
            child.metadata = std::fs::symlink_metadata(&path).ok();
            let is_link = file_type.is_symlink()
                || child
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.file_type().is_symlink());
            if is_link {
                child.target_is_dir = Some(
                    std::fs::metadata(&path)
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false),
                );
            }
        }
        progress.tick();
        let ignore_named = child
            .name
            .to_str()
            .is_some_and(|name| IGNORE_FILE_NAMES.contains(&name));
        if options.ignore_contents && ignore_named {
            let readable = child.effective_type.is_file()
                || (child.file_type.is_symlink() && child.target_is_dir == Some(false));
            if readable {
                child.ignore_contents = std::fs::read_to_string(&path).ok();
                progress.tick();
            }
        }
        children.push(child);
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(DirListing { children })
}

fn read_ignore_files(dir: &Path, progress: &ReadProgress) -> io::Result<Vec<IgnoreFile>> {
    let mut files = Vec::new();
    for name in IGNORE_FILE_NAMES {
        let path = dir.join(name);
        progress.tick();
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    files.push(IgnoreFile { path, contents });
                }
            }
            Ok(_) => {}
            // The lookup itself had to materialize `dir`: report the
            // placeholder rather than "no ignore files here".
            Err(err) if is_materialization_refusal(&err) => return Err(err),
            Err(_) => {}
        }
    }
    Ok(files)
}

/// The placeholder flag on a stat result (macOS `SF_DATALESS`).
#[cfg(target_os = "macos")]
pub fn is_dataless(metadata: &Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    metadata.st_flags() & darwin::SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
pub fn is_dataless(_metadata: &Metadata) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn set_thread_materialization(materialize: bool) {
    // A failure (an OS without this iotype answers EINVAL) leaves the process
    // default in force; the deadline still bounds whatever the kernel does.
    let _ = darwin::set_thread_materialization(materialize);
}

#[cfg(not(target_os = "macos"))]
fn set_thread_materialization(_materialize: bool) {}

/// The calling thread's dataless materialization policy:
/// `Some(true)` on, `Some(false)` off, `None` when it follows the process
/// default or the OS cannot say. Exposed so tests can witness the policy a
/// helper thread runs with.
#[cfg(target_os = "macos")]
pub fn thread_materializes_dataless() -> Option<bool> {
    match darwin::thread_materialization() {
        Ok(darwin::IOPOL_MATERIALIZE_DATALESS_FILES_ON) => Some(true),
        Ok(darwin::IOPOL_MATERIALIZE_DATALESS_FILES_OFF) => Some(false),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn thread_materializes_dataless() -> Option<bool> {
    None
}

#[cfg(target_os = "macos")]
mod darwin {
    use std::ffi::c_int;
    use std::io;

    // <sys/resource.h>. The libc crate does not bind these yet.
    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
    const IOPOL_SCOPE_THREAD: c_int = 1;
    pub(super) const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;
    pub(super) const IOPOL_MATERIALIZE_DATALESS_FILES_ON: c_int = 2;
    // <sys/stat.h>
    pub(super) const SF_DATALESS: u32 = 0x4000_0000;
    // <sys/errno.h>: what the kernel answers a thread that may not
    // materialize when it touches a dataless object.
    pub(super) const EDEADLK: i32 = 11;

    extern "C" {
        fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
        fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
    }

    pub(super) fn set_thread_materialization(materialize: bool) -> io::Result<()> {
        let policy = if materialize {
            IOPOL_MATERIALIZE_DATALESS_FILES_ON
        } else {
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF
        };
        // SAFETY: plain syscall wrapper; the arguments are integers from the
        // header and it touches only the calling thread's policy.
        let rc = unsafe {
            setiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_THREAD,
                policy,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn thread_materialization() -> io::Result<c_int> {
        // SAFETY: as above; reads the calling thread's policy.
        let rc = unsafe {
            getiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_THREAD,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    /// The injectable stand-in for a kernel that blocks: reads of a parked
    /// path wait on the helper thread until released, exactly where
    /// `getdirentries` would have blocked on an evicted folder.
    #[derive(Default)]
    pub(crate) struct Gate {
        parked: Mutex<HashSet<(ReadKind, PathBuf)>>,
        released: Condvar,
        entered: Mutex<HashMap<PathBuf, usize>>,
        inside: AtomicUsize,
        peak: AtomicUsize,
        /// Paths whose read is slow but moving: `steps` sleeps, ticking each.
        slow: Mutex<HashMap<PathBuf, (usize, Duration)>>,
        panics: Mutex<HashSet<PathBuf>>,
        policies: Mutex<Vec<Option<bool>>>,
    }

    impl Gate {
        pub(crate) fn park(&self, kind: ReadKind, path: &Path) {
            self.parked.lock().insert((kind, path.to_path_buf()));
        }

        pub(crate) fn release(&self, kind: ReadKind, path: &Path) {
            self.parked.lock().remove(&(kind, path.to_path_buf()));
            self.released.notify_all();
        }

        pub(crate) fn release_all(&self) {
            self.parked.lock().clear();
            self.released.notify_all();
        }

        pub(crate) fn entered(&self, path: &Path) -> usize {
            self.entered.lock().get(path).copied().unwrap_or(0)
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    impl ReadHook for Gate {
        fn before_read(&self, kind: ReadKind, path: &Path, progress: &ReadProgress) {
            *self.entered.lock().entry(path.to_path_buf()).or_default() += 1;
            self.policies.lock().push(thread_materializes_dataless());
            let inside = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(inside, Ordering::SeqCst);
            if self.panics.lock().contains(path) {
                self.inside.fetch_sub(1, Ordering::SeqCst);
                panic!("probe: read hook panicked");
            }
            let slow = self.slow.lock().get(path).copied();
            if let Some((steps, step)) = slow {
                for _ in 0..steps {
                    std::thread::sleep(step);
                    progress.tick();
                }
            }
            let key = (kind, path.to_path_buf());
            let mut parked = self.parked.lock();
            while parked.contains(&key) {
                self.released
                    .wait_for(&mut parked, Duration::from_millis(20));
            }
            drop(parked);
            self.inside.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub(crate) fn boundary(gate: &Arc<Gate>, timeout_ms: u64) -> ReadBoundary {
        ReadBoundary::new(ReadBoundaryOptions {
            timeout: Duration::from_millis(timeout_ms),
            hook: Some(Arc::clone(gate) as Arc<dyn ReadHook>),
            ..ReadBoundaryOptions::default()
        })
    }

    fn tree() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        for dir in ["slow/sub", "fast"] {
            fs::create_dir_all(td.path().join(dir)).unwrap();
        }
        fs::write(td.path().join("slow/inside.txt"), b"x").unwrap();
        fs::write(td.path().join("fast/a.txt"), b"a").unwrap();
        td
    }

    fn names(outcome: &ReadOutcome) -> Vec<String> {
        let output = outcome.as_ref().expect("listing");
        output
            .as_listing()
            .unwrap()
            .children
            .iter()
            .map(|child| child.name.to_string_lossy().into_owned())
            .collect()
    }

    fn list(reads: &ReadBoundary, path: &Path) -> Ticket {
        reads.list_dir(path, ListOptions::default(), false, None)
    }

    /// Poll until `done` holds, failing after `limit`.
    fn eventually(limit: Duration, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < limit,
                "condition not reached in {limit:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stalled_read_answers_timed_out_on_time_while_other_paths_read() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = boundary(&gate, 200);

        let start = Instant::now();
        let stalled = list(&reads, &slow);
        // Another directory in the same boundary answers while the first is
        // blocked in the "kernel".
        let fast = list(&reads, &td.path().join("fast")).wait();
        assert_eq!(names(&fast), ["a.txt"]);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "fast read waited on the stalled one"
        );

        let outcome = stalled.wait();
        let elapsed = start.elapsed();
        assert!(
            matches!(outcome, Err(ReadFailure::TimedOut { .. })),
            "expected TimedOut, got {outcome:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(200),
            "answered early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1_200),
            "answered late: {elapsed:?}"
        );
        assert_eq!(
            outcome.unwrap_err().into_fx(&slow).code(),
            ErrorCode::ETIMEDOUT
        );
        gate.release_all();
    }

    #[test]
    fn stuck_path_refuses_repeat_reads_and_reads_below_it_without_piling_up() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = boundary(&gate, 100);
        assert!(list(&reads, &slow).wait().is_err());
        assert_eq!(reads.stuck_reads(), 1);

        for path in [slow.clone(), slow.join("sub")] {
            let start = Instant::now();
            let outcome = list(&reads, &path).wait();
            assert!(
                matches!(&outcome, Err(ReadFailure::Stuck { blocked_on }) if *blocked_on == slow),
                "{path:?}: {outcome:?}"
            );
            assert!(
                start.elapsed() < Duration::from_millis(50),
                "refusal waited"
            );
        }
        let start = Instant::now();
        assert!(matches!(
            reads.stat(&slow, false),
            Err(ReadFailure::Stuck { .. })
        ));
        assert!(start.elapsed() < Duration::from_millis(50));
        // One kernel call for the stuck path, however often it was asked for.
        assert_eq!(gate.entered(&slow), 1);
        gate.release_all();
    }

    #[test]
    fn retry_succeeds_once_the_stuck_read_returns() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = boundary(&gate, 100);
        assert!(matches!(
            list(&reads, &slow).wait(),
            Err(ReadFailure::TimedOut { .. })
        ));

        // The folder "materializes": the parked call returns.
        gate.release(ReadKind::ListDir, &slow);
        eventually(Duration::from_secs(2), || reads.stuck_reads() == 0);
        let retry = list(&reads, &slow).wait();
        assert_eq!(names(&retry), ["inside.txt", "sub"]);
        assert_eq!(gate.entered(&slow), 2);
    }

    #[test]
    fn requests_for_a_read_in_progress_join_it() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = boundary(&gate, 5_000);
        let first = list(&reads, &slow);
        eventually(Duration::from_secs(2), || gate.entered(&slow) == 1);
        let second = list(&reads, &slow);
        gate.release(ReadKind::ListDir, &slow);
        assert_eq!(names(&first.wait()), ["inside.txt", "sub"]);
        assert_eq!(names(&second.wait()), ["inside.txt", "sub"]);
        assert_eq!(
            gate.entered(&slow),
            1,
            "the second request issued its own read"
        );
    }

    #[test]
    fn a_read_that_keeps_making_progress_is_not_cut_off() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        // 8 × 40 ms = 320 ms in total, never 100 ms without progress.
        gate.slow
            .lock()
            .insert(slow.clone(), (8, Duration::from_millis(40)));
        let reads = boundary(&gate, 100);
        let start = Instant::now();
        let outcome = list(&reads, &slow).wait();
        assert_eq!(names(&outcome), ["inside.txt", "sub"]);
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn stuck_cap_holds_under_many_stuck_paths() {
        let gate = Arc::new(Gate::default());
        let td = tempfile::tempdir().unwrap();
        let mut stuck = Vec::new();
        for i in 0..24 {
            let path = td.path().join(format!("stuck-{i:02}"));
            fs::create_dir(&path).unwrap();
            gate.park(ReadKind::ListDir, &path);
            stuck.push(path);
        }
        let healthy = td.path().join("healthy");
        fs::create_dir(&healthy).unwrap();
        let reads = ReadBoundary::new(ReadBoundaryOptions {
            timeout: Duration::from_millis(150),
            max_stuck: 3,
            concurrency: 2,
            hook: Some(Arc::clone(&gate) as Arc<dyn ReadHook>),
            ..ReadBoundaryOptions::default()
        });

        let start = Instant::now();
        let tickets: Vec<_> = stuck.iter().map(|path| list(&reads, path)).collect();
        for ticket in tickets {
            let outcome = ticket.wait();
            assert!(
                matches!(
                    outcome,
                    Err(ReadFailure::TimedOut { .. } | ReadFailure::Saturated { .. })
                ),
                "{outcome:?}"
            );
        }
        // Every request answered on its own deadline, not one after another.
        assert!(
            start.elapsed() < Duration::from_millis(1_500),
            "{:?}",
            start.elapsed()
        );
        // Never more threads in the "kernel" than concurrency + max_stuck.
        assert!(gate.peak() <= 5, "peak {} blocked reads", gate.peak());
        assert!(reads.stuck_reads() >= 3);

        // With the stuck budget spent, a read of a healthy path is refused at
        // once rather than queued behind the stuck ones.
        let start = Instant::now();
        assert!(matches!(
            list(&reads, &healthy).wait(),
            Err(ReadFailure::Saturated { .. })
        ));
        assert!(start.elapsed() < Duration::from_millis(50));

        gate.release_all();
        eventually(Duration::from_secs(2), || reads.stuck_reads() == 0);
        assert!(list(&reads, &healthy).wait().is_ok());
    }

    #[test]
    fn close_releases_waiters_without_waiting_for_the_kernel() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = boundary(&gate, 60_000);
        let waiter = {
            let ticket = list(&reads, &slow);
            std::thread::spawn(move || {
                let start = Instant::now();
                (ticket.wait(), start.elapsed())
            })
        };
        eventually(Duration::from_secs(2), || gate.entered(&slow) == 1);

        let start = Instant::now();
        reads.close();
        assert!(start.elapsed() < Duration::from_millis(100), "close waited");
        let (outcome, waited) = waiter.join().unwrap();
        assert!(
            matches!(outcome, Err(ReadFailure::Cancelled)),
            "{outcome:?}"
        );
        assert!(waited < Duration::from_secs(5), "waiter stayed {waited:?}");
        assert!(matches!(
            list(&reads, &td.path().join("fast")).wait(),
            Err(ReadFailure::Cancelled)
        ));
        // The parked helper is still in the "kernel"; releasing it lets the
        // thread finish and exit on its own.
        gate.release_all();
    }

    #[test]
    fn a_read_nobody_waits_for_is_never_started() {
        let gate = Arc::new(Gate::default());
        let td = tempfile::tempdir().unwrap();
        let paths: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|name| {
                let path = td.path().join(name);
                fs::create_dir(&path).unwrap();
                path
            })
            .collect();
        // Two threads in total (concurrency 1 + stuck budget 1), both parked.
        let reads = ReadBoundary::new(ReadBoundaryOptions {
            timeout: Duration::from_secs(30),
            max_stuck: 1,
            concurrency: 1,
            hook: Some(Arc::clone(&gate) as Arc<dyn ReadHook>),
            ..ReadBoundaryOptions::default()
        });
        gate.park(ReadKind::ListDir, &paths[0]);
        gate.park(ReadKind::ListDir, &paths[1]);
        let a = list(&reads, &paths[0]);
        let b = list(&reads, &paths[1]);
        eventually(Duration::from_secs(2), || {
            gate.entered(&paths[0]) == 1 && gate.entered(&paths[1]) == 1
        });
        drop(list(&reads, &paths[2]));
        gate.release_all();
        assert!(a.wait().is_ok());
        assert!(b.wait().is_ok());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(gate.entered(&paths[2]), 0, "abandoned read was started");
    }

    #[test]
    fn a_panicking_read_finishes_instead_of_reading_as_stuck() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.panics.lock().insert(slow.clone());
        let reads = boundary(&gate, 100);
        assert!(matches!(
            list(&reads, &slow).wait(),
            Err(ReadFailure::Io(_))
        ));
        assert_eq!(reads.stuck_reads(), 0);
        gate.panics.lock().clear();
        assert!(list(&reads, &slow).wait().is_ok());
    }

    #[test]
    fn a_known_dataless_directory_is_not_read_unless_materializing() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        let reads = boundary(&gate, 1_000);
        let outcome = reads
            .list_dir(&slow, ListOptions::default(), true, None)
            .wait();
        assert!(matches!(outcome, Err(ReadFailure::Dataless)), "{outcome:?}");
        assert_eq!(gate.entered(&slow), 0);
        assert_eq!(
            outcome.unwrap_err().into_fx(&slow).code(),
            ErrorCode::EDATALESS
        );

        let materializing = ReadBoundary::new(ReadBoundaryOptions {
            materialize_dataless: true,
            hook: Some(Arc::clone(&gate) as Arc<dyn ReadHook>),
            ..ReadBoundaryOptions::default()
        });
        let outcome = materializing
            .list_dir(&slow, ListOptions::default(), true, None)
            .wait();
        assert_eq!(names(&outcome), ["inside.txt", "sub"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_refusing_to_materialize_reads_as_dataless() {
        let refusal = io::Error::from_raw_os_error(darwin::EDEADLK);
        assert!(matches!(classify(&refusal, false), ReadFailure::Dataless));
        // With materialization on, EDEADLK is an ordinary error again.
        assert!(matches!(classify(&refusal, true), ReadFailure::Io(_)));
    }

    /// The policy wiring, witnessed from inside the helper thread: the
    /// thread that issues the read is the one that carries the policy.
    #[cfg(target_os = "macos")]
    #[test]
    fn helper_threads_carry_the_dataless_policy() {
        let td = tree();
        let fast = td.path().join("fast");
        for (materialize, expected) in [(false, Some(false)), (true, Some(true))] {
            let gate = Arc::new(Gate::default());
            let reads = ReadBoundary::new(ReadBoundaryOptions {
                materialize_dataless: materialize,
                hook: Some(Arc::clone(&gate) as Arc<dyn ReadHook>),
                ..ReadBoundaryOptions::default()
            });
            assert!(list(&reads, &fast).wait().is_ok());
            assert_eq!(*gate.policies.lock(), vec![expected]);
        }
        // The calling thread is untouched.
        assert_eq!(thread_materializes_dataless(), None);
    }

    #[test]
    fn ignore_files_are_read_inside_the_bound() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        fs::write(td.path().join(".gitignore"), "target/\n").unwrap();
        fs::write(td.path().join(".ignore"), "*.log\n").unwrap();
        let reads = boundary(&gate, 1_000);
        let files = reads.ignore_files(td.path()).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|file| {
                file.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, [".gitignore", ".ignore"]);
        assert_eq!(files[0].contents, "target/\n");

        gate.park(ReadKind::IgnoreFiles, td.path());
        let blocked = boundary(&gate, 100);
        assert!(matches!(
            blocked.ignore_files(td.path()),
            Err(ReadFailure::TimedOut { .. })
        ));
        gate.release_all();
    }

    /// A disk spinning up: the first answer is withheld for `wake`, then the
    /// read answers and (optionally) stalls again for `then_stall`.
    struct SpinUp {
        wake: Duration,
        then_stall: Option<Duration>,
    }

    impl ReadHook for SpinUp {
        fn before_read(&self, _kind: ReadKind, _path: &Path, progress: &ReadProgress) {
            std::thread::sleep(self.wake);
            if let Some(stall) = self.then_stall {
                progress.tick();
                std::thread::sleep(stall);
            }
        }
    }

    fn spin_up_boundary(
        stall_ms: u64,
        first_ms: Option<u64>,
        wake_ms: u64,
        then_stall_ms: Option<u64>,
    ) -> ReadBoundary {
        ReadBoundary::new(ReadBoundaryOptions {
            timeout: Duration::from_millis(stall_ms),
            first_response: first_ms.map(Duration::from_millis),
            hook: Some(Arc::new(SpinUp {
                wake: Duration::from_millis(wake_ms),
                then_stall: then_stall_ms.map(Duration::from_millis),
            })),
            ..ReadBoundaryOptions::default()
        })
    }

    #[test]
    fn a_read_waking_a_disk_gets_the_first_response_deadline() {
        let td = tree();
        let fast = td.path().join("fast");
        // 400 ms without a sign of life: fine under a 3 s first-response
        // deadline even though the stall deadline is 100 ms.
        let patient = spin_up_boundary(100, Some(3_000), 400, None);
        let start = Instant::now();
        assert_eq!(names(&list(&patient, &fast).wait()), ["a.txt"]);
        assert!(start.elapsed() >= Duration::from_millis(400));

        // The same disk under the stall deadline alone is reported stalled.
        let impatient = spin_up_boundary(100, None, 400, None);
        assert_eq!(impatient.first_response(), impatient.timeout());
        assert!(matches!(
            list(&impatient, &fast).wait(),
            Err(ReadFailure::TimedOut { .. })
        ));
    }

    #[test]
    fn after_the_first_answer_the_stall_deadline_applies() {
        let td = tree();
        let fast = td.path().join("fast");
        // Answers at once, then stalls for 1.5 s: the generous first-response
        // deadline must not cover a stall after the read has answered.
        let reads = spin_up_boundary(100, Some(10_000), 0, Some(1_500));
        let start = Instant::now();
        let outcome = list(&reads, &fast).wait();
        assert!(
            matches!(outcome, Err(ReadFailure::TimedOut { .. })),
            "{outcome:?}"
        );
        assert!(
            start.elapsed() < Duration::from_millis(1_200),
            "waited {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_read_awaiting_its_first_answer_is_not_stuck_before_its_deadline() {
        let gate = Arc::new(Gate::default());
        let td = tree();
        let slow = td.path().join("slow");
        gate.park(ReadKind::ListDir, &slow);
        let reads = ReadBoundary::new(ReadBoundaryOptions {
            timeout: Duration::from_millis(50),
            first_response: Some(Duration::from_secs(5)),
            hook: Some(Arc::clone(&gate) as Arc<dyn ReadHook>),
            ..ReadBoundaryOptions::default()
        });
        let first = list(&reads, &slow);
        eventually(Duration::from_secs(2), || gate.entered(&slow) == 1);
        std::thread::sleep(Duration::from_millis(200));
        // Well past the stall deadline, but still inside the first-response
        // one: not stuck, so a second request joins instead of being refused.
        assert_eq!(reads.stuck_reads(), 0);
        let second = list(&reads, &slow);
        gate.release_all();
        assert_eq!(names(&first.wait()), ["inside.txt", "sub"]);
        assert_eq!(names(&second.wait()), ["inside.txt", "sub"]);
        assert_eq!(gate.entered(&slow), 1);
    }

    #[test]
    fn the_media_profile_waits_30_s_for_a_disk_and_2_s_per_stall() {
        let options = ReadBoundaryOptions::media();
        assert_eq!(options.first_response, Some(Duration::from_secs(30)));
        assert_eq!(options.timeout, DEFAULT_READ_TIMEOUT);
        assert!(!options.materialize_dataless);
        let reads = ReadBoundary::new(options);
        assert_eq!(reads.first_response(), MEDIA_FIRST_RESPONSE_TIMEOUT);
        assert_eq!(reads.timeout(), Duration::from_secs(2));
    }
}
