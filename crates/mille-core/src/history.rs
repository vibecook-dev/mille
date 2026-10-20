//! Restart without a rescan: replay what changed under a root since a saved
//! point, from the volume's own change journal (macOS FSEvents `sinceWhen`).
//!
//! [`events_since`](crate::resume::events_since) answers "what changed while
//! nobody watched" by walking the whole root and diffing it against a
//! snapshot: O(tree), however little changed. FSEvents keeps a per-volume
//! journal of changed paths (`/.fseventsd`) under a monotonic event id, and
//! asking it for everything after a saved id is O(changes).
//!
//! The protocol (Apple, *File System Events Programming Guide*, "Using
//! persistent event ids"; research/07 §6.3):
//!
//! 1. Start the live watcher on the root **before** catching up, so nothing
//!    falls between the two.
//! 2. [`HistoryToken::read`] the saved token and call [`replay_since`]:
//!    - [`Replay::Changes`]: list each of
//!      [`dirs_to_rescan`](HistoryChanges::dirs_to_rescan) again (one level
//!      deep), and walk each of
//!      [`rescan_subtrees`](HistoryChanges::rescan_subtrees) whole;
//!    - [`Replay::Rescan`]: the journal cannot answer for this root (see
//!      [`RescanReason`]): walk the root.
//!
//!    Then save [`ReplayReport::next`] — only once that catch-up has been
//!    applied. A crash in between replays the same changes again, which is
//!    harmless; saving first could lose them.
//! 3. While running, save a fresh token only at a point where everything
//!    that happened before it has been applied: capture it
//!    ([`HistoryToken::capture`]) *before* a full scan starts and save it
//!    when the scan has committed; likewise around a watcher batch. A token
//!    that is too old only makes the next replay longer.
//!
//! On other platforms there is no persistent journal: [`HistoryToken::capture`]
//! and [`replay_since`] return [`FxError::Unsupported`] and the caller walks
//! (on Linux a directory-mtime-pruned walk is the cheap fallback). So does a
//! macOS volume without a journal (FSEvents keeps none on network volumes,
//! nor where `.fseventsd/no_log` exists).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::{ErrorCode, FxError};

/// A point in a root's change history: the volume's journal identity, and
/// the last event id the caller had applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryToken {
    /// The root, as the caller names it. Replayed paths are reported under
    /// this path even if FSEvents resolves it to another one.
    pub root: PathBuf,
    /// The volume's FSEvents journal UUID (`FSEventsCopyUUIDForDevice`). A
    /// different one means another journal: the volume was reformatted, its
    /// journal was reset, or another disk is mounted at the same path.
    pub volume_uuid: String,
    /// `FSEventsGetCurrentEventId()` when the token was captured.
    pub event_id: u64,
    /// The root directory's inode: a root replaced by another directory of
    /// the same name has a history that is not this one's.
    pub root_ino: u64,
    /// Wall-clock capture time, milliseconds since the Unix epoch.
    pub saved_at_ms: i64,
}

/// First line of a token file. Bump the number when the layout changes.
const TOKEN_MAGIC: &str = "mille-history-token 1";

impl HistoryToken {
    /// The current point in `root`'s history. Capture it before the work it
    /// will vouch for starts (a full scan, a watcher batch), and save it
    /// once that work is done.
    ///
    /// Errors: the root's own `stat` error (`ENOENT`, `EACCES`, …),
    /// `ENOTDIR`, or [`FxError::Unsupported`] where the volume keeps no
    /// journal (and on every platform but macOS).
    pub fn capture(root: &Path) -> Result<Self, FxError> {
        platform::capture(root)
    }

    /// Save the token at `path`, atomically (a crash leaves the old token or
    /// the new one).
    pub fn write(&self, path: &Path) -> Result<(), FxError> {
        crate::resume::write_atomically(path, &self.encode(), crate::resume::Durability::Synced)
    }

    /// Load a token saved by [`write`](Self::write). A token of another
    /// format version is [`FxError::Unsupported`]; a damaged one is
    /// [`FxError::InvalidInput`]. Either way: walk the root, save a new one.
    pub fn read(path: &Path) -> Result<Self, FxError> {
        let bytes =
            std::fs::read(path).map_err(|e| ErrorCode::from_io_error(&e, path.to_path_buf()))?;
        Self::decode(&bytes)
    }

    /// Text, one `key=value` per line, the root last: its bytes run to the
    /// end of the file, so any name round-trips.
    fn encode(&self) -> Vec<u8> {
        let mut out = format!(
            "{TOKEN_MAGIC}\nvolume_uuid={}\nevent_id={}\nroot_ino={}\nsaved_at_ms={}\nroot=",
            self.volume_uuid, self.event_id, self.root_ino, self.saved_at_ms
        )
        .into_bytes();
        out.extend_from_slice(&path_bytes(&self.root));
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, FxError> {
        let bad = |what: &str| FxError::InvalidInput(format!("history token: {what}"));
        let magic_end = bytes
            .iter()
            .position(|b| *b == b'\n')
            .unwrap_or(bytes.len());
        let magic = &bytes[..magic_end];
        if magic != TOKEN_MAGIC.as_bytes() {
            return Err(if magic.starts_with(b"mille-history-token ") {
                FxError::Unsupported(format!(
                    "history token {:?}, expected {TOKEN_MAGIC:?}",
                    String::from_utf8_lossy(magic)
                ))
            } else {
                bad("not a history token")
            });
        }
        const ROOT: &[u8] = b"\nroot=";
        let at = bytes
            .windows(ROOT.len())
            .position(|w| w == ROOT)
            .ok_or_else(|| bad("no root"))?;
        let header = std::str::from_utf8(&bytes[magic_end..at]).map_err(|_| bad("not UTF-8"))?;
        let root = path_from_bytes(&bytes[at + ROOT.len()..]);
        let (mut uuid, mut id, mut ino, mut saved) = (None, None, None, None);
        for line in header.lines().filter(|l| !l.is_empty()) {
            let (key, value) = line.split_once('=').ok_or_else(|| bad(line))?;
            match key {
                "volume_uuid" => uuid = Some(value.to_string()),
                "event_id" => id = value.parse().ok(),
                "root_ino" => ino = value.parse().ok(),
                "saved_at_ms" => saved = value.parse().ok(),
                _ => {}
            }
        }
        Ok(Self {
            root,
            volume_uuid: uuid.ok_or_else(|| bad("volume_uuid"))?,
            event_id: id.ok_or_else(|| bad("event_id"))?,
            root_ino: ino.ok_or_else(|| bad("root_ino"))?,
            saved_at_ms: saved.ok_or_else(|| bad("saved_at_ms"))?,
        })
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Limits on a replay; past any of them a walk is the better answer.
#[derive(Clone, Debug)]
pub struct ReplayOptions {
    /// A token older than this is not replayed ([`RescanReason::TooOld`]).
    /// FSEvents trims its journal on its own schedule; this bounds how much
    /// the caller trusts it to still be whole. `None`: no limit.
    pub max_age: Option<Duration>,
    /// More history records than this and the replay stops
    /// ([`RescanReason::TooManyEvents`]): by then most of the tree is
    /// changed anyway.
    pub max_events: usize,
    /// FSEvents must finish delivering history (`HistoryDone`) within this
    /// ([`RescanReason::TimedOut`]). It reads its journal from the saved id
    /// on, so the cost grows with everything that happened on the *volume*
    /// since, not only under the root.
    pub timeout: Duration,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            max_age: Some(Duration::from_secs(30 * 24 * 3600)),
            max_events: 100_000,
            timeout: Duration::from_secs(30),
        }
    }
}

/// What a replay found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replay {
    /// The journal covers everything since the token.
    Changes(HistoryChanges),
    /// The journal cannot answer for this root: walk it.
    Rescan(RescanReason),
}

/// Why a replay answered [`Replay::Rescan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RescanReason {
    /// The volume's journal UUID changed (reformatted, journal reset or
    /// purged, another disk at the same path).
    VolumeChanged,
    /// The saved event id is ahead of the volume's current one (the
    /// journal was reset).
    IdAhead,
    /// FSEvents' event ids wrapped around.
    IdsWrapped,
    /// The root is another directory now (its inode changed).
    RootReplaced,
    /// The root itself, or a directory above it, was renamed, moved or
    /// deleted (`RootChanged`).
    RootChanged,
    /// FSEvents dropped history for the root as a whole (`MustScanSubDirs`
    /// on the root).
    HistoryIncomplete,
    /// The token is older than [`ReplayOptions::max_age`].
    TooOld,
    /// More than [`ReplayOptions::max_events`] records.
    TooManyEvents,
    /// No `HistoryDone` within [`ReplayOptions::timeout`].
    TimedOut,
}

/// Every path under the root the journal names since the token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryChanges {
    /// One entry per path (flags merged across its records), sorted by path.
    /// Paths are under [`HistoryToken::root`] as the caller named it.
    pub changed: Vec<HistoryEvent>,
    /// Directories FSEvents could not keep history for (it dropped events
    /// there, or a volume was mounted or unmounted there): walk each whole.
    pub rescan_subtrees: Vec<PathBuf>,
    /// History records delivered, before merging by path.
    pub records: u64,
}

impl HistoryChanges {
    /// Nothing changed under the root.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.rescan_subtrees.is_empty()
    }

    /// The directories whose listings changed: each changed path's parent
    /// (`root` for the root itself), once, sorted. Listing each again one
    /// level deep finds every file created, removed, renamed or modified in
    /// it, and every directory that appeared (walk those whole). Flags are
    /// *accumulated* per path, so they say what may have happened, not the
    /// order: the listing says what is there now.
    pub fn dirs_to_rescan(&self, root: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .changed
            .iter()
            .map(|e| match e.path.parent() {
                Some(parent) if e.path != root && parent.starts_with(root) => parent.to_path_buf(),
                _ => root.to_path_buf(),
            })
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }
}

/// A path the journal names, with everything it says about it since the
/// token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEvent {
    pub path: PathBuf,
    /// The id of its last record.
    pub last_id: u64,
    pub flags: HistoryFlags,
}

/// FSEvents item flags, accumulated: what may have happened to the path,
/// not in which order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryFlags {
    pub created: bool,
    pub removed: bool,
    pub renamed: bool,
    /// Contents written.
    pub modified: bool,
    /// Inode metadata, ownership, extended attributes or Finder info.
    pub metadata: bool,
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
}

/// A replay's answer, and the token to save once it has been applied.
#[derive(Clone, Debug)]
pub struct ReplayReport {
    pub replay: Replay,
    /// Captured before the replay started: everything after it is replayed
    /// next time, whatever this replay delivered.
    pub next: HistoryToken,
    pub elapsed: Duration,
}

/// Replay `token.root`'s history since `token`. Blocking (it runs FSEvents
/// on a thread of its own and waits for it).
///
/// Errors: the root's own `stat` error — a root that is gone or asleep is
/// the caller's "unavailable" case, not a change set — and
/// [`FxError::Unsupported`] where there is no journal.
pub fn replay_since(
    token: &HistoryToken,
    options: &ReplayOptions,
) -> Result<ReplayReport, FxError> {
    let started = Instant::now();
    let next = HistoryToken::capture(&token.root)?;
    let reason = if next.volume_uuid != token.volume_uuid {
        Some(RescanReason::VolumeChanged)
    } else if next.root_ino != token.root_ino {
        Some(RescanReason::RootReplaced)
    } else if token.event_id > next.event_id {
        Some(RescanReason::IdAhead)
    } else if options.max_age.is_some_and(|max| {
        next.saved_at_ms.saturating_sub(token.saved_at_ms) > max.as_millis() as i64
    }) {
        Some(RescanReason::TooOld)
    } else {
        None
    };
    let replay = match reason {
        Some(reason) => Replay::Rescan(reason),
        None => platform::replay(token, options)?,
    };
    Ok(ReplayReport {
        replay,
        next,
        elapsed: started.elapsed(),
    })
}

/// Raw FSEvents records → [`Replay`]. Platform-independent so it is tested
/// everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn interpret(
    records: &[(PathBuf, u32, u64)],
    canonical_root: &Path,
    root: &Path,
) -> Replay {
    use flags::*;
    let under = |p: &Path| -> Option<PathBuf> {
        p.strip_prefix(canonical_root).ok().map(|rel| {
            if rel.as_os_str().is_empty() {
                root.to_path_buf()
            } else {
                root.join(rel)
            }
        })
    };
    let mut merged: std::collections::BTreeMap<PathBuf, HistoryEvent> = Default::default();
    let mut subtrees: Vec<PathBuf> = Vec::new();
    for (path, flag, id) in records {
        let (flag, id) = (*flag, *id);
        if flag & EVENT_IDS_WRAPPED != 0 {
            return Replay::Rescan(RescanReason::IdsWrapped);
        }
        if flag & ROOT_CHANGED != 0 {
            return Replay::Rescan(RescanReason::RootChanged);
        }
        let lost = flag & (MUST_SCAN_SUB_DIRS | MOUNT | UNMOUNT) != 0;
        if lost && canonical_root.starts_with(path) {
            // At or above the root: nothing below it can be trusted.
            return Replay::Rescan(RescanReason::HistoryIncomplete);
        }
        let Some(path) = under(path) else {
            // Above the root (`WatchRoot` bookkeeping) or elsewhere.
            continue;
        };
        if lost {
            subtrees.push(path);
            continue;
        }
        let e = merged.entry(path.clone()).or_insert_with(|| HistoryEvent {
            path,
            last_id: id,
            flags: HistoryFlags::default(),
        });
        e.last_id = e.last_id.max(id);
        let f = &mut e.flags;
        f.created |= flag & ITEM_CREATED != 0;
        f.removed |= flag & ITEM_REMOVED != 0;
        f.renamed |= flag & ITEM_RENAMED != 0;
        f.modified |= flag & ITEM_MODIFIED != 0;
        f.metadata |= flag
            & (ITEM_INODE_META_MOD | ITEM_FINDER_INFO_MOD | ITEM_CHANGE_OWNER | ITEM_XATTR_MOD)
            != 0;
        f.is_file |= flag & ITEM_IS_FILE != 0;
        f.is_dir |= flag & ITEM_IS_DIR != 0;
        f.is_symlink |= flag & ITEM_IS_SYMLINK != 0;
    }
    subtrees.sort();
    subtrees.dedup();
    // A subtree to walk whole covers every change below it.
    let changed = merged
        .into_values()
        .filter(|e| !subtrees.iter().any(|s| e.path.starts_with(s)))
        .collect();
    Replay::Changes(HistoryChanges {
        changed,
        rescan_subtrees: subtrees,
        records: records.len() as u64,
    })
}

/// `FSEventStreamEventFlags` (`<CoreServices/FSEvents.h>`).
#[allow(dead_code)]
mod flags {
    pub const MUST_SCAN_SUB_DIRS: u32 = 0x0000_0001;
    pub const USER_DROPPED: u32 = 0x0000_0002;
    pub const KERNEL_DROPPED: u32 = 0x0000_0004;
    pub const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
    pub const HISTORY_DONE: u32 = 0x0000_0010;
    pub const ROOT_CHANGED: u32 = 0x0000_0020;
    pub const MOUNT: u32 = 0x0000_0040;
    pub const UNMOUNT: u32 = 0x0000_0080;
    pub const ITEM_CREATED: u32 = 0x0000_0100;
    pub const ITEM_REMOVED: u32 = 0x0000_0200;
    pub const ITEM_INODE_META_MOD: u32 = 0x0000_0400;
    pub const ITEM_RENAMED: u32 = 0x0000_0800;
    pub const ITEM_MODIFIED: u32 = 0x0000_1000;
    pub const ITEM_FINDER_INFO_MOD: u32 = 0x0000_2000;
    pub const ITEM_CHANGE_OWNER: u32 = 0x0000_4000;
    pub const ITEM_XATTR_MOD: u32 = 0x0000_8000;
    pub const ITEM_IS_FILE: u32 = 0x0001_0000;
    pub const ITEM_IS_DIR: u32 = 0x0002_0000;
    pub const ITEM_IS_SYMLINK: u32 = 0x0004_0000;
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub(super) fn capture(_root: &Path) -> Result<HistoryToken, FxError> {
        Err(FxError::Unsupported(
            "a persistent change journal (FSEvents) exists on macOS only".into(),
        ))
    }

    pub(super) fn replay(
        _token: &HistoryToken,
        _options: &ReplayOptions,
    ) -> Result<Replay, FxError> {
        Err(FxError::Unsupported(
            "a persistent change journal (FSEvents) exists on macOS only".into(),
        ))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    use super::*;

    pub(super) fn capture(root: &Path) -> Result<HistoryToken, FxError> {
        // The id first: the token vouches for nothing that happened before
        // it, so it must not be later than the moment of capture.
        let event_id = unsafe { ffi::FSEventsGetCurrentEventId() };
        let meta = std::fs::metadata(root)
            .map_err(|e| ErrorCode::from_io_error(&e, root.to_path_buf()))?;
        if !meta.is_dir() {
            return Err(FxError::Io {
                code: ErrorCode::ENOTDIR,
                path: root.to_path_buf(),
                source: std::io::Error::other("not a directory"),
            });
        }
        let volume_uuid = volume_uuid(meta.dev()).ok_or_else(|| {
            FxError::Unsupported(format!("{root:?}: the volume keeps no FSEvents journal"))
        })?;
        Ok(HistoryToken {
            root: root.to_path_buf(),
            volume_uuid,
            event_id,
            root_ino: meta.ino(),
            saved_at_ms: now_ms(),
        })
    }

    fn volume_uuid(dev: u64) -> Option<String> {
        // SAFETY: plain CoreFoundation calls; every object created here is
        // released here.
        unsafe {
            let uuid = ffi::FSEventsCopyUUIDForDevice(dev as libc::dev_t);
            if uuid.is_null() {
                return None;
            }
            let text = ffi::CFUUIDCreateString(std::ptr::null(), uuid);
            ffi::CFRelease(uuid);
            if text.is_null() {
                return None;
            }
            let mut buf = [0 as c_char; 64];
            let ok = ffi::CFStringGetCString(
                text,
                buf.as_mut_ptr(),
                buf.len() as ffi::CFIndex,
                ffi::K_CF_STRING_ENCODING_UTF8,
            );
            ffi::CFRelease(text);
            (ok != 0).then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
        }
    }

    pub(super) fn replay(token: &HistoryToken, options: &ReplayOptions) -> Result<Replay, FxError> {
        // FSEvents reports real paths (`/private/var`, not `/var`).
        let canonical = std::fs::canonicalize(&token.root)
            .map_err(|e| ErrorCode::from_io_error(&e, token.root.clone()))?;
        let c_root = CString::new(canonical.as_os_str().as_bytes())
            .map_err(|_| FxError::InvalidInput(format!("{canonical:?}: NUL in path")))?;
        let (since, max, timeout) = (token.event_id, options.max_events, options.timeout);
        // The stream runs on a run loop: give it a thread of its own rather
        // than the caller's (which may already run one).
        let collected = std::thread::Builder::new()
            .name("mille-history-replay".into())
            .spawn(move || stream_history(&c_root, since, max, timeout))
            .map_err(|e| FxError::InternalBug(format!("spawn history replay: {e}")))?
            .join()
            .map_err(|_| FxError::InternalBug("history replay thread panicked".into()))??;
        Ok(if collected.overflow {
            Replay::Rescan(RescanReason::TooManyEvents)
        } else if collected.done || collected.decisive {
            // `interpret` turns a decisive record into its `Rescan`.
            interpret(&collected.records, &canonical, &token.root)
        } else {
            Replay::Rescan(RescanReason::TimedOut)
        })
    }

    #[derive(Default)]
    struct Collected {
        records: Vec<(PathBuf, u32, u64)>,
        /// `HistoryDone` seen.
        done: bool,
        /// Past `max` records.
        overflow: bool,
        /// A record that ends the replay early (ids wrapped, root changed,
        /// history lost): `interpret` reads it from `records`.
        decisive: bool,
        max: usize,
    }

    extern "C" fn on_events(
        _stream: ffi::FSEventStreamRef,
        info: *mut c_void,
        count: usize,
        paths: *mut c_void,
        event_flags: *const u32,
        ids: *const u64,
    ) {
        // Never unwind into CoreServices.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `info` is the `Collected` that `stream_history` owns
            // and outlives the stream; the callback runs on that thread,
            // inside `CFRunLoopRunInMode`, while nothing else touches it.
            // `paths`, `flags` and `ids` hold `count` items (FSEvents
            // contract, `char **` paths without `UseCFTypes`).
            let collected = unsafe { &mut *(info as *mut Collected) };
            let paths = paths as *const *const c_char;
            for i in 0..count {
                let (flag, id, path) = unsafe { (*event_flags.add(i), *ids.add(i), *paths.add(i)) };
                if flag & flags::HISTORY_DONE != 0 {
                    collected.done = true;
                    continue;
                }
                if collected.records.len() >= collected.max {
                    collected.overflow = true;
                    continue;
                }
                if flag & (flags::EVENT_IDS_WRAPPED | flags::ROOT_CHANGED) != 0 {
                    collected.decisive = true;
                }
                if path.is_null() {
                    continue;
                }
                let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
                let path = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
                collected.records.push((path, flag, id));
            }
        }));
    }

    fn stream_history(
        root: &CStr,
        since: u64,
        max: usize,
        timeout: Duration,
    ) -> Result<Collected, FxError> {
        let collected = Box::into_raw(Box::new(Collected {
            max,
            ..Collected::default()
        }));
        // SAFETY: CoreFoundation/CoreServices calls with objects created and
        // released here; `collected` is reclaimed below after the stream is
        // released, and read between run-loop slices only (the callback
        // runs inside them, on this thread).
        unsafe {
            let path =
                ffi::CFStringCreateWithFileSystemRepresentation(std::ptr::null(), root.as_ptr());
            if path.is_null() {
                drop(Box::from_raw(collected));
                return Err(FxError::InternalBug("CFStringCreate failed".into()));
            }
            let values = [path];
            let paths = ffi::CFArrayCreate(
                std::ptr::null(),
                values.as_ptr(),
                1,
                &ffi::kCFTypeArrayCallBacks,
            );
            ffi::CFRelease(path);
            if paths.is_null() {
                drop(Box::from_raw(collected));
                return Err(FxError::InternalBug("CFArrayCreate failed".into()));
            }
            let context = ffi::FSEventStreamContext {
                version: 0,
                info: collected.cast(),
                retain: None,
                release: None,
                copy_description: None,
            };
            let stream = ffi::FSEventStreamCreate(
                std::ptr::null(),
                on_events,
                &context,
                paths,
                since,
                0.0,
                ffi::CREATE_FLAG_NO_DEFER
                    | ffi::CREATE_FLAG_WATCH_ROOT
                    | ffi::CREATE_FLAG_FILE_EVENTS,
            );
            ffi::CFRelease(paths);
            if stream.is_null() {
                drop(Box::from_raw(collected));
                return Err(FxError::InternalBug("FSEventStreamCreate failed".into()));
            }
            ffi::FSEventStreamScheduleWithRunLoop(
                stream,
                ffi::CFRunLoopGetCurrent(),
                ffi::kCFRunLoopDefaultMode,
            );
            if ffi::FSEventStreamStart(stream) == 0 {
                ffi::FSEventStreamInvalidate(stream);
                ffi::FSEventStreamRelease(stream);
                drop(Box::from_raw(collected));
                return Err(FxError::InternalBug("FSEventStreamStart failed".into()));
            }
            let deadline = Instant::now() + timeout;
            loop {
                {
                    let c = &*collected;
                    if c.done || c.overflow || c.decisive || Instant::now() >= deadline {
                        break;
                    }
                }
                ffi::CFRunLoopRunInMode(ffi::kCFRunLoopDefaultMode, 0.05, 1);
            }
            if (*collected).done {
                // Deliver anything that already happened but is still held
                // back: by the time this returns, the callback has seen
                // every event up to now.
                ffi::FSEventStreamFlushSync(stream);
            }
            ffi::FSEventStreamStop(stream);
            ffi::FSEventStreamInvalidate(stream);
            ffi::FSEventStreamRelease(stream);
            Ok(*Box::from_raw(collected))
        }
    }

    #[allow(non_upper_case_globals, non_snake_case)]
    mod ffi {
        use std::os::raw::{c_char, c_long, c_uchar, c_void};

        pub type CFRef = *const c_void;
        pub type CFIndex = c_long;
        pub type FSEventStreamRef = *mut c_void;

        pub const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
        pub const CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
        pub const CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
        pub const CREATE_FLAG_FILE_EVENTS: u32 = 0x0000_0010;

        pub type FSEventStreamCallback = extern "C" fn(
            FSEventStreamRef,
            *mut c_void,
            usize,
            *mut c_void,
            *const u32,
            *const u64,
        );

        #[repr(C)]
        pub struct FSEventStreamContext {
            pub version: CFIndex,
            pub info: *mut c_void,
            pub retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
            pub release: Option<extern "C" fn(*const c_void)>,
            pub copy_description: Option<extern "C" fn(*const c_void) -> CFRef>,
        }

        /// Only its address is used (`kCFTypeArrayCallBacks`).
        #[repr(C)]
        #[allow(dead_code)]
        pub struct CFArrayCallBacks {
            version: CFIndex,
            retain: *const c_void,
            release: *const c_void,
            copy_description: *const c_void,
            equal: *const c_void,
        }

        #[link(name = "CoreServices", kind = "framework")]
        extern "C" {
            pub fn FSEventStreamCreate(
                allocator: CFRef,
                callback: FSEventStreamCallback,
                context: *const FSEventStreamContext,
                paths_to_watch: CFRef,
                since_when: u64,
                latency: f64,
                flags: u32,
            ) -> FSEventStreamRef;
            pub fn FSEventStreamScheduleWithRunLoop(
                stream: FSEventStreamRef,
                run_loop: CFRef,
                run_loop_mode: CFRef,
            );
            pub fn FSEventStreamStart(stream: FSEventStreamRef) -> c_uchar;
            pub fn FSEventStreamFlushSync(stream: FSEventStreamRef);
            pub fn FSEventStreamStop(stream: FSEventStreamRef);
            pub fn FSEventStreamInvalidate(stream: FSEventStreamRef);
            pub fn FSEventStreamRelease(stream: FSEventStreamRef);
            pub fn FSEventsGetCurrentEventId() -> u64;
            pub fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFRef;
        }

        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            pub static kCFRunLoopDefaultMode: CFRef;
            pub static kCFTypeArrayCallBacks: CFArrayCallBacks;
            pub fn CFRelease(cf: CFRef);
            pub fn CFRunLoopGetCurrent() -> CFRef;
            pub fn CFRunLoopRunInMode(
                mode: CFRef,
                seconds: f64,
                return_after_source: c_uchar,
            ) -> i32;
            pub fn CFArrayCreate(
                allocator: CFRef,
                values: *const CFRef,
                count: CFIndex,
                callbacks: *const CFArrayCallBacks,
            ) -> CFRef;
            pub fn CFStringCreateWithFileSystemRepresentation(
                allocator: CFRef,
                buffer: *const c_char,
            ) -> CFRef;
            pub fn CFUUIDCreateString(allocator: CFRef, uuid: CFRef) -> CFRef;
            pub fn CFStringGetCString(
                string: CFRef,
                buffer: *mut c_char,
                size: CFIndex,
                encoding: u32,
            ) -> c_uchar;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::flags::*;
    use super::*;

    fn token(root: &str) -> HistoryToken {
        HistoryToken {
            root: PathBuf::from(root),
            volume_uuid: "EBE8553E-83E8-422C-A5D1-FC7B02CD9DCC".into(),
            event_id: 1_234_567_890_123,
            root_ino: 42,
            saved_at_ms: 1_760_000_000_000,
        }
    }

    #[test]
    fn a_token_round_trips_through_its_file_whatever_the_root_is_called() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("root.history");
        for root in ["/Volumes/Media/Movies", "/odd\nroot=name/é"] {
            let t = token(root);
            t.write(&file).unwrap();
            assert_eq!(HistoryToken::read(&file).unwrap(), t);
        }
        assert!(!file.with_extension("tmp").exists());

        let other_version = b"mille-history-token 2\nevent_id=1\nroot=/x".to_vec();
        std::fs::write(&file, other_version).unwrap();
        assert!(matches!(
            HistoryToken::read(&file),
            Err(FxError::Unsupported(_))
        ));
        for damaged in [
            &b"PK\x03\x04"[..],
            b"mille-history-token 1\nvolume_uuid=u\nevent_id=x\nroot_ino=1\nsaved_at_ms=1\nroot=/r",
            b"mille-history-token 1\nvolume_uuid=u\nevent_id=1\nroot_ino=1\nsaved_at_ms=1",
        ] {
            std::fs::write(&file, damaged).unwrap();
            assert!(
                matches!(HistoryToken::read(&file), Err(FxError::InvalidInput(_))),
                "{damaged:?}"
            );
        }
    }

    const FILE: u32 = ITEM_IS_FILE;
    const DIR: u32 = ITEM_IS_DIR;

    fn rec(path: &str, flags: u32, id: u64) -> (PathBuf, u32, u64) {
        (PathBuf::from(path), flags, id)
    }

    /// Records under the canonical root come back under the caller's root,
    /// one per path with flags merged, sorted; records outside are ignored.
    #[test]
    fn records_merge_per_path_under_the_callers_root() {
        let canonical = Path::new("/private/lib");
        let root = Path::new("/lib");
        let records = [
            rec("/private/lib/Movies/b.mkv", ITEM_CREATED | FILE, 10),
            rec("/private/lib/Movies/a.mkv", ITEM_REMOVED | FILE, 11),
            rec("/private/lib/Movies/b.mkv", ITEM_MODIFIED | FILE, 12),
            rec("/private/lib/Show/S02", ITEM_CREATED | DIR, 13),
            rec("/private/lib", ITEM_INODE_META_MOD | DIR, 14),
            rec("/private", ITEM_INODE_META_MOD | DIR, 15),
            rec("/elsewhere/x.mkv", ITEM_CREATED | FILE, 16),
        ];
        let Replay::Changes(changes) = interpret(&records, canonical, root) else {
            panic!("expected changes")
        };
        let got: Vec<(&Path, u64)> = changes
            .changed
            .iter()
            .map(|e| (e.path.as_path(), e.last_id))
            .collect();
        assert_eq!(
            got,
            vec![
                (Path::new("/lib"), 14),
                (Path::new("/lib/Movies/a.mkv"), 11),
                (Path::new("/lib/Movies/b.mkv"), 12),
                (Path::new("/lib/Show/S02"), 13),
            ]
        );
        let b = &changes.changed[2].flags;
        assert!(b.created && b.modified && b.is_file && !b.removed && !b.is_dir);
        assert!(changes.changed[3].flags.is_dir);
        assert_eq!(changes.records, 7);
        assert_eq!(
            changes.dirs_to_rescan(root),
            vec![
                PathBuf::from("/lib"),
                PathBuf::from("/lib/Movies"),
                PathBuf::from("/lib/Show"),
            ]
        );
    }

    #[test]
    fn lost_history_below_the_root_is_a_subtree_to_walk_and_at_or_above_it_a_rescan() {
        let root = Path::new("/lib");
        let records = [
            rec("/lib/Show/S01/e1.mkv", ITEM_CREATED | FILE, 1),
            rec("/lib/Show", MUST_SCAN_SUB_DIRS | USER_DROPPED, 2),
            rec("/lib/Movies/m.mkv", ITEM_MODIFIED | FILE, 3),
        ];
        let Replay::Changes(changes) = interpret(&records, root, root) else {
            panic!("expected changes")
        };
        assert_eq!(changes.rescan_subtrees, vec![PathBuf::from("/lib/Show")]);
        // Covered by the subtree walk.
        assert_eq!(changes.changed.len(), 1);
        assert_eq!(changes.changed[0].path, Path::new("/lib/Movies/m.mkv"));
        assert!(!changes.is_empty());

        for lost in ["/lib", "/"] {
            let records = [rec(lost, MUST_SCAN_SUB_DIRS | KERNEL_DROPPED, 4)];
            assert_eq!(
                interpret(&records, root, root),
                Replay::Rescan(RescanReason::HistoryIncomplete)
            );
        }
        assert_eq!(
            interpret(&[rec("/lib", ROOT_CHANGED, 5)], root, root),
            Replay::Rescan(RescanReason::RootChanged)
        );
        assert_eq!(
            interpret(&[rec("/", EVENT_IDS_WRAPPED, 6)], root, root),
            Replay::Rescan(RescanReason::IdsWrapped)
        );
        assert_eq!(
            interpret(&[], root, root),
            Replay::Changes(HistoryChanges::default())
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn there_is_no_journal_off_macos() {
        let td = tempfile::tempdir().unwrap();
        assert!(matches!(
            HistoryToken::capture(td.path()),
            Err(FxError::Unsupported(_))
        ));
        assert!(matches!(
            replay_since(&token("/x"), &ReplayOptions::default()),
            Err(FxError::Unsupported(_))
        ));
    }

    /// Replays until `done` holds (fseventsd ingests events a few ms after
    /// they happen), or 5 s.
    #[cfg(target_os = "macos")]
    fn replay_until(
        token: &HistoryToken,
        done: impl Fn(&HistoryChanges) -> bool,
    ) -> (HistoryChanges, HistoryToken) {
        let start = Instant::now();
        loop {
            let report = replay_since(token, &ReplayOptions::default()).unwrap();
            let Replay::Changes(changes) = report.replay else {
                panic!("expected changes, got {:?}", report.replay)
            };
            if done(&changes) || start.elapsed() > Duration::from_secs(5) {
                return (changes, report.next);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A token after which nothing under `root` has been numbered for
    /// 300 ms. Under load fseventsd numbers events late (seen here: more
    /// than 800 ms after they happened, and out of order across paths).
    /// That is safe — an event is never numbered at or below a token
    /// captured before it was numbered, so lateness only makes a later
    /// replay a superset — but a test that asserts exact sets must start
    /// from a quiet point.
    #[cfg(target_os = "macos")]
    fn quiet_token(root: &Path) -> HistoryToken {
        let start = Instant::now();
        loop {
            let token = HistoryToken::capture(root).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            let quiet = matches!(
                replay_since(&token, &ReplayOptions::default()).unwrap().replay,
                Replay::Changes(ref c) if c.is_empty()
            );
            if quiet || start.elapsed() > Duration::from_secs(10) {
                return token;
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_replay_names_what_changed_since_the_token_and_nothing_else() {
        use std::fs;
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("library");
        fs::create_dir_all(root.join("Show/S01")).unwrap();
        for f in ["Show/S01/e1.mkv", "old.mkv", "gone.mkv", "movie.mkv"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        let token = quiet_token(&root);
        assert_eq!(token.root, root);

        fs::write(root.join("new.mkv"), b"new").unwrap();
        fs::write(root.join("movie.mkv"), b"rewritten").unwrap();
        fs::remove_file(root.join("gone.mkv")).unwrap();
        fs::rename(root.join("old.mkv"), root.join("renamed.mkv")).unwrap();
        fs::create_dir(root.join("Show/S02")).unwrap();
        fs::write(root.join("Show/S02/e1.mkv"), b"x").unwrap();

        let expected: Vec<PathBuf> = [
            "Show/S02",
            "Show/S02/e1.mkv",
            "gone.mkv",
            "movie.mkv",
            "new.mkv",
            "old.mkv",
            "renamed.mkv",
        ]
        .iter()
        .map(|p| root.join(p))
        .collect();
        let has_all = |c: &HistoryChanges| {
            expected
                .iter()
                .all(|p| c.changed.iter().any(|e| &e.path == p))
        };
        let (changes, next) = replay_until(&token, has_all);
        assert!(has_all(&changes), "{changes:#?}");
        assert!(changes.rescan_subtrees.is_empty());
        assert!(
            !changes
                .changed
                .iter()
                .any(|e| e.path == root.join("Show/S01/e1.mkv")),
            "an untouched file is not history: {changes:#?}"
        );
        let gone = changes
            .changed
            .iter()
            .find(|e| e.path == root.join("gone.mkv"))
            .unwrap();
        assert!(gone.flags.removed && gone.flags.is_file);
        assert_eq!(
            changes.dirs_to_rescan(&root),
            vec![root.clone(), root.join("Show"), root.join("Show/S02")]
        );
        assert_eq!(next.volume_uuid, token.volume_uuid);
        assert!(next.event_id >= token.event_id);

        // Once all that is numbered, the token a replay hands back replays
        // nothing.
        let quiet = quiet_token(&root);
        let next = replay_since(&token, &ReplayOptions::default())
            .unwrap()
            .next;
        assert!(next.event_id >= quiet.event_id);
        let report = replay_since(&next, &ReplayOptions::default()).unwrap();
        assert_eq!(report.replay, Replay::Changes(HistoryChanges::default()));

        // A file changed after that is the only change.
        fs::write(root.join("Show/S01/e1.mkv"), b"touched").unwrap();
        let only_e1 = |c: &HistoryChanges| !c.changed.is_empty();
        let (changes, _) = replay_until(&next, only_e1);
        let paths: Vec<&Path> = changes.changed.iter().map(|e| e.path.as_path()).collect();
        assert_eq!(paths, vec![root.join("Show/S01/e1.mkv").as_path()]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_replay_the_journal_cannot_answer_asks_for_a_walk() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("library");
        std::fs::create_dir(&root).unwrap();
        let token = HistoryToken::capture(&root).unwrap();
        for f in ["a.mkv", "b.mkv", "c.mkv"] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        let rescan = |t: &HistoryToken, options: &ReplayOptions| match replay_since(t, options)
            .unwrap()
            .replay
        {
            Replay::Rescan(reason) => Some(reason),
            Replay::Changes(_) => None,
        };
        let defaults = ReplayOptions::default();
        let other_volume = HistoryToken {
            volume_uuid: "00000000-0000-0000-0000-000000000000".into(),
            ..token.clone()
        };
        assert_eq!(
            rescan(&other_volume, &defaults),
            Some(RescanReason::VolumeChanged)
        );
        let ahead = HistoryToken {
            event_id: u64::MAX - 1,
            ..token.clone()
        };
        assert_eq!(rescan(&ahead, &defaults), Some(RescanReason::IdAhead));
        let replaced = HistoryToken {
            root_ino: token.root_ino + 1,
            ..token.clone()
        };
        assert_eq!(
            rescan(&replaced, &defaults),
            Some(RescanReason::RootReplaced)
        );
        let old = HistoryToken {
            saved_at_ms: token.saved_at_ms - 31 * 24 * 3600 * 1000,
            ..token.clone()
        };
        assert_eq!(rescan(&old, &defaults), Some(RescanReason::TooOld));
        // Three files, at least three records: past a limit of one.
        let start = Instant::now();
        let tight = ReplayOptions {
            max_events: 1,
            ..ReplayOptions::default()
        };
        while rescan(&token, &tight).is_none() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(rescan(&token, &tight), Some(RescanReason::TooManyEvents));

        // A root that is gone is the caller's "unavailable", not a change set.
        let gone = HistoryToken {
            root: td.path().join("not-there"),
            ..token
        };
        let err = replay_since(&gone, &defaults).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ENOENT);
    }
}
