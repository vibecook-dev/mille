//! Directory listers behind [`walk_scan`](crate::scan::walk_scan).
//!
//! One call lists one directory completely and returns a [`ScanListing`]:
//! every child's name in a single byte arena plus a fixed-size record of
//! the identity and times a media scanner diffs on (kind, size, ns mtime,
//! birth time, `(dev, ino)`, flags, link count). The listers run on
//! `ReadBoundary` helper threads, so they inherit its stall deadline, stuck
//! cap, joined reads and dataless policy exactly as `list_dir` does; the
//! progress ticks below are what keep a large-but-moving listing alive.
//!
//! Three backends, chosen per directory:
//!
//! - **macOS: `getattrlistbulk(2)`.** One syscall returns up to 128 KiB of
//!   packed records — name, type, `(dev, ino)`, birth and modification time,
//!   `st_flags`, link count and size for every child — instead of a
//!   `readdir` plus a full-path `lstat` per child. Research 07 §5 measured
//!   parallel `getattrlistbulk` at 2.6–4.6× the old walker with ~25× less
//!   user CPU. `ENOTSUP`/`EINVAL` on the first call (a filesystem without
//!   the VNOP, or an attribute it refuses) falls back to the portable lister.
//! - **Linux: `getdents64` + `statx(dirfd, name, AT_SYMLINK_NOFOLLOW,
//!   BASIC_STATS | BTIME)`** through rustix: dirfd-relative (no path joins,
//!   no full-path lookups) and with birth time. A kernel without `statx`
//!   (< 4.11, or a seccomp filter answering `EPERM`) falls back.
//! - **Everywhere else, and the fallback: `std::fs::read_dir` +
//!   `DirEntry::metadata`**, which std implements dirfd-relative
//!   (`fstatat`) on Apple and glibc targets.
//!
//! Name rules (hidden, the default junk list, the caller's file filter) run
//! here, on the helper, *before* the per-entry `stat` where the backend has
//! one, so a filtered-out sidecar never costs a syscall on Linux or the
//! fallback path. On the bulk path the attributes arrive with the name, so
//! filtering only saves the record.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{FileType, Metadata};
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::entry::EntryKind;
use crate::read_boundary::ReadProgress;
use crate::scan::{FileFilter, ScanEntry, ScanLister};

/// Gitignore-format text equivalent to the default ignore rules that
/// [`ScanOptions::default_ignores`](crate::scan::ScanOptions::default_ignores)
/// applies: OS, NAS and recycle-bin litter that is never media. The scan
/// matches these names directly (no path, no glob engine) because they are
/// checked for every entry; `IgnoreMatcher::add_from_string` accepts this
/// text unchanged for a caller that wants the same rules as a matcher, and a
/// test keeps the two in agreement. Names match case-sensitively, as git
/// does.
pub const DEFAULT_IGNORE_PATTERNS: &str = "\
# macOS: Finder metadata, AppleDouble resource forks on non-HFS volumes, volume indexes
.DS_Store
._*
.AppleDouble
.Spotlight-V100
.Trashes
.fseventsd
.TemporaryItems
.DocumentRevisions-V100
# Windows
Thumbs.db
desktop.ini
$RECYCLE.BIN
System Volume Information
# Synology (@eaDir thumbnails, #recycle, #snapshot), QNAP, Samba vfs_recycle
@eaDir
\\#recycle
\\#snapshot
@Recycle
.@__thumb
.recycle
# freedesktop trash on removable media, fsck debris
.Trash-*
lost+found
";

/// The exact names in [`DEFAULT_IGNORE_PATTERNS`].
const IGNORED_NAMES: &[&[u8]] = &[
    b".DS_Store",
    b".AppleDouble",
    b".Spotlight-V100",
    b".Trashes",
    b".fseventsd",
    b".TemporaryItems",
    b".DocumentRevisions-V100",
    b"Thumbs.db",
    b"desktop.ini",
    b"$RECYCLE.BIN",
    b"System Volume Information",
    b"@eaDir",
    b"#recycle",
    b"#snapshot",
    b"@Recycle",
    b".@__thumb",
    b".recycle",
    b"lost+found",
];

/// The `prefix*` patterns in [`DEFAULT_IGNORE_PATTERNS`].
const IGNORED_PREFIXES: &[&[u8]] = &[b"._", b".Trash-"];

/// True when `name` matches a default ignore rule.
pub(crate) fn is_default_ignored(name: &[u8]) -> bool {
    // Every rule starts with one of these bytes; most names start with none.
    if !matches!(
        name.first(),
        Some(b'.' | b'@' | b'#' | b'$' | b'T' | b'd' | b'S' | b'l')
    ) {
        return false;
    }
    IGNORED_NAMES.contains(&name) || IGNORED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Which lister produced a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListerKind {
    /// macOS `getattrlistbulk`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Bulk,
    /// Linux `getdents64` + `statx`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Statx,
    /// `std::fs::read_dir` + `DirEntry::metadata`.
    Portable,
}

/// What a scan listing reads and keeps. Part of the boundary's join key:
/// two scans with the same options (the same filter, by identity) share one
/// in-flight read of a directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScanListOptions {
    pub(crate) follow_symlinks: bool,
    pub(crate) include_hidden: bool,
    pub(crate) default_ignores: bool,
    /// Ignore files (gitignore format) whose text the listing returns.
    pub(crate) ignore_file_names: Arc<[OsString]>,
    pub(crate) file_filter: Option<FileFilter>,
    pub(crate) lister: ScanLister,
}

impl ScanListOptions {
    /// Name-only rules, applied before any per-entry `stat`. `hint` is the
    /// directory entry's own type when the backend has it (`d_type`); the
    /// filter runs here only when that type says "regular file".
    fn drops_before_stat(&self, name: &OsStr, hint: Option<EntryKind>) -> bool {
        let bytes = name.as_encoded_bytes();
        if !self.include_hidden && bytes.first() == Some(&b'.') {
            return true;
        }
        if self.default_ignores && is_default_ignored(bytes) {
            return true;
        }
        hint == Some(EntryKind::File) && self.filter_rejects(name)
    }

    /// The filter, for an entry whose type the listing learned by `stat`
    /// (a followed link, or a filesystem without `d_type`). Directories
    /// always pass: the filter selects files, never prunes the tree.
    fn drops_after_stat(&self, name: &OsStr, hint: Option<EntryKind>, kind: EntryKind) -> bool {
        hint != Some(EntryKind::File) && kind != EntryKind::Directory && self.filter_rejects(name)
    }

    fn filter_rejects(&self, name: &OsStr) -> bool {
        self.file_filter
            .as_ref()
            .is_some_and(|filter| !filter.keeps(name))
    }

    fn is_ignore_file(&self, name: &OsStr) -> bool {
        self.ignore_file_names.iter().any(|known| known == name)
    }
}

/// The fixed-size part of a scan entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Record {
    pub(crate) kind: EntryKind,
    pub(crate) is_symlink: bool,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i64,
    pub(crate) btime_ns: Option<i64>,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) flags: u32,
    pub(crate) nlink: u64,
}

impl Record {
    /// Normalise what differs between backends but carries no meaning:
    /// sizes of non-files (a symlink's target length, a directory's
    /// filesystem-specific byte count) and directory link counts (APFS,
    /// HFS+ and ext4 each count differently).
    fn normalised(mut self) -> Self {
        if self.kind != EntryKind::File {
            self.size = 0;
        }
        if self.kind == EntryKind::Directory {
            self.nlink = 1;
        }
        self
    }

    /// From std metadata (the portable lister, and a followed link there).
    pub(crate) fn from_metadata(meta: &Metadata) -> Self {
        let file_type = meta.file_type();
        Record {
            kind: kind_of(file_type),
            is_symlink: file_type.is_symlink(),
            size: meta.len(),
            mtime_ns: platform::mtime_ns(meta),
            btime_ns: meta.created().ok().map(system_time_ns),
            dev: platform::dev(meta),
            ino: platform::ino(meta),
            flags: platform::flags(meta),
            nlink: platform::nlink(meta),
        }
        .normalised()
    }
}

pub(crate) fn kind_of(file_type: FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Unknown
    }
}

/// Nanoseconds since the Unix epoch, negative before it, saturating.
pub(crate) fn system_time_ns(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos())
            .map(|ns| -ns)
            .unwrap_or(i64::MIN),
    }
}

/// `sec * 1e9 + nsec`, saturating.
#[cfg(unix)]
fn timespec_ns(sec: i64, nsec: i64) -> i64 {
    sec.saturating_mul(1_000_000_000).saturating_add(nsec)
}

#[cfg(unix)]
mod platform {
    use std::fs::Metadata;
    use std::os::unix::fs::MetadataExt;

    pub(super) fn mtime_ns(meta: &Metadata) -> i64 {
        super::timespec_ns(meta.mtime(), meta.mtime_nsec())
    }
    pub(super) fn dev(meta: &Metadata) -> u64 {
        meta.dev()
    }
    pub(super) fn ino(meta: &Metadata) -> u64 {
        meta.ino()
    }
    pub(super) fn nlink(meta: &Metadata) -> u64 {
        meta.nlink()
    }
    #[cfg(target_os = "macos")]
    pub(super) fn flags(meta: &Metadata) -> u32 {
        std::os::macos::fs::MetadataExt::st_flags(meta)
    }
    #[cfg(not(target_os = "macos"))]
    pub(super) fn flags(_meta: &Metadata) -> u32 {
        0
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::Metadata;
    use std::os::windows::fs::MetadataExt;

    pub(super) fn mtime_ns(meta: &Metadata) -> i64 {
        meta.modified().map(super::system_time_ns).unwrap_or(0)
    }
    // Stable std exposes no volume serial or file index on Windows.
    pub(super) fn dev(_meta: &Metadata) -> u64 {
        0
    }
    pub(super) fn ino(_meta: &Metadata) -> u64 {
        0
    }
    pub(super) fn nlink(_meta: &Metadata) -> u64 {
        1
    }
    /// `FILE_ATTRIBUTE_*` bits.
    pub(super) fn flags(meta: &Metadata) -> u32 {
        meta.file_attributes()
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::fs::Metadata;

    pub(super) fn mtime_ns(meta: &Metadata) -> i64 {
        meta.modified().map(super::system_time_ns).unwrap_or(0)
    }
    pub(super) fn dev(_meta: &Metadata) -> u64 {
        0
    }
    pub(super) fn ino(_meta: &Metadata) -> u64 {
        0
    }
    pub(super) fn nlink(_meta: &Metadata) -> u64 {
        1
    }
    pub(super) fn flags(_meta: &Metadata) -> u32 {
        0
    }
}

/// Where one name sits in a listing's arena.
#[derive(Clone, Copy, Debug)]
struct NameRef {
    /// Byte offset in the unix arena; the name's index elsewhere.
    start: usize,
    /// Byte length (the unix arena only).
    #[cfg_attr(not(unix), allow(dead_code))]
    len: usize,
}

/// Every name of a listing in one allocation. Unix names are bytes, so any
/// name — including one that is not UTF-8 — is stored and handed back
/// verbatim as an `OsStr`.
#[cfg(unix)]
#[derive(Default)]
struct Names {
    bytes: Vec<u8>,
}

#[cfg(unix)]
impl Names {
    fn push(&mut self, name: &OsStr) -> NameRef {
        use std::os::unix::ffi::OsStrExt;
        self.push_bytes(name.as_bytes())
    }

    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    fn push_bytes(&mut self, name: &[u8]) -> NameRef {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(name);
        NameRef {
            start,
            len: name.len(),
        }
    }

    fn get(&self, name: NameRef) -> &OsStr {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(&self.bytes[name.start..name.start + name.len])
    }
}

/// Elsewhere an `OsStr` is not plain bytes in safe Rust, so each name keeps
/// its own `OsString`; `NameRef::start` is its index.
#[cfg(not(unix))]
#[derive(Default)]
struct Names {
    names: Vec<OsString>,
}

#[cfg(not(unix))]
impl Names {
    fn push(&mut self, name: &OsStr) -> NameRef {
        self.names.push(name.to_os_string());
        NameRef {
            start: self.names.len() - 1,
            len: 0,
        }
    }

    fn get(&self, name: NameRef) -> &OsStr {
        &self.names[name.start]
    }
}

/// One directory, listed by a scan read: what survived the name rules, each
/// child as a name in a shared arena plus a fixed-size record.
pub struct ScanListing {
    names: Names,
    entries: Vec<(NameRef, Record)>,
    /// Ignore files found in the directory, with their text, in listing
    /// order.
    ignore_files: Vec<(OsString, String)>,
    /// Children dropped by name rules or the file filter.
    skipped: u64,
    /// Children the listing saw but could not stat (removed mid-listing, or
    /// a per-entry error).
    vanished: u64,
    lister: ListerKind,
}

impl fmt::Debug for ScanListing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanListing")
            .field("entries", &self.entries.len())
            .field("skipped", &self.skipped)
            .field("vanished", &self.vanished)
            .field("lister", &self.lister)
            .finish()
    }
}

impl ScanListing {
    fn new(lister: ListerKind) -> Self {
        Self {
            names: Names::default(),
            entries: Vec::new(),
            ignore_files: Vec::new(),
            skipped: 0,
            vanished: 0,
            lister,
        }
    }

    fn push(&mut self, name: &OsStr, record: Record) {
        let name = self.names.push(name);
        self.entries.push((name, record));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn push_bytes(&mut self, name: &[u8], record: Record) {
        let name = self.names.push_bytes(name);
        self.entries.push((name, record));
    }

    /// Children kept by the listing.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The children, in the order the filesystem returned them (not sorted).
    pub fn iter(&self) -> impl Iterator<Item = ScanEntry<'_>> + '_ {
        self.entries.iter().map(|(name, record)| ScanEntry {
            name: self.names.get(*name),
            kind: record.kind,
            is_symlink: record.is_symlink,
            size: record.size,
            mtime_ns: record.mtime_ns,
            btime_ns: record.btime_ns,
            dev: record.dev,
            ino: record.ino,
            flags: record.flags,
            nlink: record.nlink,
        })
    }

    pub(crate) fn ignore_files(&self) -> &[(OsString, String)] {
        &self.ignore_files
    }

    pub(crate) fn skipped(&self) -> u64 {
        self.skipped
    }

    pub(crate) fn vanished(&self) -> u64 {
        self.vanished
    }

    pub(crate) fn lister(&self) -> ListerKind {
        self.lister
    }
}

/// List `dir` for a scan, on a boundary helper thread.
pub(crate) fn scan_dir(
    dir: &Path,
    options: &ScanListOptions,
    progress: &ReadProgress,
) -> io::Result<ScanListing> {
    if options.lister == ScanLister::Auto {
        #[cfg(target_os = "macos")]
        if let Some(listing) = bulk::list(dir, options, progress)? {
            return Ok(listing);
        }
        #[cfg(target_os = "linux")]
        if let Some(listing) = linux::list(dir, options, progress)? {
            return Ok(listing);
        }
    }
    list_portable(dir, options, progress)
}

/// `read_dir` + `DirEntry::metadata`: the fallback, and the only lister on
/// platforms without a bulk path.
fn list_portable(
    dir: &Path,
    options: &ScanListOptions,
    progress: &ReadProgress,
) -> io::Result<ScanListing> {
    let mut out = ScanListing::new(ListerKind::Portable);
    for item in std::fs::read_dir(dir)? {
        progress.tick();
        let Ok(item) = item else {
            out.vanished += 1;
            continue;
        };
        let name = item.file_name();
        // `d_type` where the filesystem has it; std falls back to `lstat`.
        let hint = item.file_type().ok().map(kind_of);
        if options.drops_before_stat(&name, hint) {
            out.skipped += 1;
            continue;
        }
        let Ok(meta) = item.metadata() else {
            out.vanished += 1;
            continue;
        };
        let mut record = Record::from_metadata(&meta);
        if record.kind == EntryKind::Symlink && options.follow_symlinks {
            // A link that does not resolve stays a `Symlink` record.
            if let Ok(target) = std::fs::metadata(item.path()) {
                record = Record {
                    is_symlink: true,
                    ..Record::from_metadata(&target)
                };
            }
        }
        if options.drops_after_stat(&name, hint, record.kind) {
            out.skipped += 1;
            continue;
        }
        if record.kind == EntryKind::File && options.is_ignore_file(&name) {
            if let Ok(text) = std::fs::read_to_string(item.path()) {
                progress.tick();
                out.ignore_files.push((name.clone(), text));
            }
        }
        out.push(&name, record);
    }
    Ok(out)
}

/// macOS `getattrlistbulk(2)` lister.
#[cfg(target_os = "macos")]
mod bulk {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::ffi::{c_void, CStr, CString, OsStr};
    use std::hash::{DefaultHasher, Hash, Hasher};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{timespec_ns, ListerKind, Record, ScanListOptions, ScanListing};
    use crate::entry::EntryKind;
    use crate::read_boundary::ReadProgress;

    /// `<sys/attr.h>`; libc does not bind it. Per-entry errno, placed right
    /// after `ATTR_CMN_RETURNED_ATTRS` (man 2 getattrlistbulk).
    const ATTR_CMN_ERROR: u32 = 0x2000_0000;
    /// `<sys/vnode.h>` `enum vtype`.
    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    const VLNK: u32 = 5;
    /// Apple DTS recommends at least 32 KiB per call (forum thread 766035);
    /// 128 KiB holds ~1,000 media-folder entries, so most directories take
    /// one call plus the terminating zero.
    const BUF_LEN: usize = 128 * 1024;

    thread_local! {
        /// Helper threads are long-lived, so each reuses one buffer rather
        /// than allocating (and page-faulting) 128 KiB per directory.
        static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// `Ok(None)` when this directory must be listed another way: the
    /// filesystem rejects the call, or the loop guard tripped.
    pub(super) fn list(
        dir: &Path,
        options: &ScanListOptions,
        progress: &ReadProgress,
    ) -> io::Result<Option<ScanListing>> {
        let path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // SAFETY: `path` is NUL-terminated and outlives the call; the flags
        // are constants. With materialization off, a dataless directory
        // answers EDEADLK here, which the boundary maps to `Dataless`.
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `open` just returned this descriptor; nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        BUF.with(|cell| match cell.try_borrow_mut() {
            Ok(mut buf) => read_all(&fd, &mut buf, dir, options, progress),
            // Not reachable (one listing per helper at a time), but never
            // worth a panic: use a private buffer.
            Err(_) => read_all(&fd, &mut Vec::new(), dir, options, progress),
        })
    }

    /// Network volumes need a loop guard: Apple DTS documents
    /// `getattrlistbulk` returning the same entries forever on SMB
    /// (FB15497909, macOS 15). A local volume is trusted, so it pays nothing.
    fn is_local(fd: &OwnedFd) -> bool {
        // SAFETY: `statfs` is plain old data, so all-zero is a valid value.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open and `stat` is a valid out-pointer.
        let rc = unsafe { libc::fstatfs(fd.as_raw_fd(), &mut stat) };
        rc == 0 && stat.f_flags & (libc::MNT_LOCAL as u32) != 0
    }

    fn read_all(
        fd: &OwnedFd,
        buf: &mut Vec<u8>,
        dir: &Path,
        options: &ScanListOptions,
        progress: &ReadProgress,
    ) -> io::Result<Option<ScanListing>> {
        if buf.len() < BUF_LEN {
            buf.resize(BUF_LEN, 0);
        }
        // SAFETY: `attrlist` is plain old data, so all-zero is valid.
        let mut attrs: libc::attrlist = unsafe { std::mem::zeroed() };
        attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        // Returned in bit order (man 2 getattrlist), ERROR right after
        // RETURNED_ATTRS: name, devid, objtype, crtime, modtime, flags,
        // fileid; then the file group: linkcount, datalength. Directories
        // get no file group; nothing is asked of the directory group.
        attrs.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_CRTIME
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_FLAGS
            | libc::ATTR_CMN_FILEID;
        attrs.fileattr = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH;

        let mut out = ScanListing::new(ListerKind::Bulk);
        let mut seen: Option<HashSet<u64>> = (!is_local(fd)).then(HashSet::new);
        let mut first = true;
        loop {
            // SAFETY: `fd` is an open directory; `attrs` and `buf` are valid
            // for the duration and `buf.len()` is its true length.
            let count = unsafe {
                libc::getattrlistbulk(
                    fd.as_raw_fd(),
                    (&mut attrs as *mut libc::attrlist).cast::<c_void>(),
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len(),
                    0,
                )
            };
            if count < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // The filesystem has no bulk VNOP for these attributes.
                    // Nothing was consumed from `fd`, and the fallback opens
                    // its own, so `readdir` and this call never share one.
                    Some(libc::ENOTSUP | libc::EINVAL) if first => return Ok(None),
                    _ => return Err(err),
                }
            }
            first = false;
            if count == 0 {
                break;
            }
            progress.tick();
            let mut offset = 0usize;
            for _ in 0..count {
                let entry = parse(buf, offset)?;
                offset = offset
                    .checked_add(entry.len)
                    .filter(|end| *end <= buf.len())
                    .ok_or_else(malformed)?;
                if let Some(seen) = seen.as_mut() {
                    let mut hasher = DefaultHasher::new();
                    entry.name.hash(&mut hasher);
                    if !seen.insert(hasher.finish()) {
                        // The same name twice: the iteration is looping (or
                        // two names collide in 64 bits). Either way, start
                        // over with `readdir`.
                        return Ok(None);
                    }
                }
                accept(&mut out, fd, dir, options, &entry);
            }
        }
        Ok(Some(out))
    }

    /// One packed entry, borrowed from the buffer.
    struct Packed<'a> {
        len: usize,
        /// Without the trailing NUL.
        name: &'a [u8],
        /// With it, for `fstatat`.
        name_c: Option<&'a CStr>,
        error: u32,
        record: Record,
    }

    fn malformed() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "getattrlistbulk returned a malformed attribute buffer",
        )
    }

    /// Bounds-checked reads from one entry's bytes.
    struct Reader<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
            let end = self.pos.checked_add(N).ok_or_else(malformed)?;
            let slice = self.bytes.get(self.pos..end).ok_or_else(malformed)?;
            self.pos = end;
            // The slice is exactly N bytes long.
            slice.try_into().map_err(|_| malformed())
        }
        fn u32(&mut self) -> io::Result<u32> {
            self.take::<4>().map(u32::from_ne_bytes)
        }
        fn i32(&mut self) -> io::Result<i32> {
            self.take::<4>().map(i32::from_ne_bytes)
        }
        fn u64(&mut self) -> io::Result<u64> {
            self.take::<8>().map(u64::from_ne_bytes)
        }
        fn i64(&mut self) -> io::Result<i64> {
            self.take::<8>().map(i64::from_ne_bytes)
        }
        /// `struct timespec` on 64-bit Darwin: `time_t` + `long`.
        fn timespec_ns(&mut self) -> io::Result<i64> {
            let sec = self.i64()?;
            let nsec = self.i64()?;
            Ok(timespec_ns(sec, nsec))
        }
    }

    /// Parse the entry at `offset`. Attributes are packed in bit order, each
    /// 4-byte aligned; every one requested here is a multiple of 4 bytes, so
    /// no padding falls between them.
    fn parse(buf: &[u8], offset: usize) -> io::Result<Packed<'_>> {
        let head = buf.get(offset..).ok_or_else(malformed)?;
        let len = Reader {
            bytes: head,
            pos: 0,
        }
        .u32()? as usize;
        let bytes = head.get(..len).ok_or_else(malformed)?;
        let mut r = Reader { bytes, pos: 4 };
        // attribute_set_t: common, vol, dir, file, fork.
        let common = r.u32()?;
        let _vol = r.u32()?;
        let _dir = r.u32()?;
        let file = r.u32()?;
        let _fork = r.u32()?;

        let error = if common & ATTR_CMN_ERROR != 0 {
            r.u32()?
        } else {
            0
        };
        let (name, name_c) = if common & libc::ATTR_CMN_NAME != 0 {
            // attrreference_t: offset from the reference itself, length with NUL.
            let at = r.pos;
            let rel = r.i32()?;
            let with_nul = r.u32()? as usize;
            let start = (at as isize)
                .checked_add(rel as isize)
                .and_then(|s| usize::try_from(s).ok())
                .ok_or_else(malformed)?;
            let end = start.checked_add(with_nul).ok_or_else(malformed)?;
            let raw = bytes.get(start..end).ok_or_else(malformed)?;
            let name_c = CStr::from_bytes_with_nul(raw).ok();
            let name = name_c.map_or(raw, CStr::to_bytes);
            (name, name_c)
        } else {
            return Err(malformed());
        };
        let mut record = Record {
            kind: EntryKind::Unknown,
            is_symlink: false,
            size: 0,
            mtime_ns: 0,
            btime_ns: None,
            dev: 0,
            ino: 0,
            flags: 0,
            nlink: 1,
        };
        if common & libc::ATTR_CMN_DEVID != 0 {
            // dev_t is i32 on Darwin; widen the way std's `MetadataExt::dev`
            // does (`st_dev as u64`), so the two agree bit for bit.
            record.dev = r.i32()? as u64;
        }
        if common & libc::ATTR_CMN_OBJTYPE != 0 {
            record.kind = match r.u32()? {
                VREG => EntryKind::File,
                VDIR => EntryKind::Directory,
                VLNK => EntryKind::Symlink,
                _ => EntryKind::Unknown,
            };
            record.is_symlink = record.kind == EntryKind::Symlink;
        }
        if common & libc::ATTR_CMN_CRTIME != 0 {
            record.btime_ns = Some(r.timespec_ns()?);
        }
        if common & libc::ATTR_CMN_MODTIME != 0 {
            record.mtime_ns = r.timespec_ns()?;
        }
        if common & libc::ATTR_CMN_FLAGS != 0 {
            record.flags = r.u32()?;
        }
        if common & libc::ATTR_CMN_FILEID != 0 {
            record.ino = r.u64()?;
        }
        if file & libc::ATTR_FILE_LINKCOUNT != 0 {
            record.nlink = u64::from(r.u32()?);
        }
        if file & libc::ATTR_FILE_DATALENGTH != 0 {
            record.size = u64::try_from(r.i64()?).unwrap_or(0);
        }
        Ok(Packed {
            len,
            name,
            name_c,
            error,
            record: record.normalised(),
        })
    }

    fn accept(
        out: &mut ScanListing,
        fd: &OwnedFd,
        dir: &Path,
        options: &ScanListOptions,
        entry: &Packed<'_>,
    ) {
        let name = OsStr::from_bytes(entry.name);
        if entry.error != 0 {
            out.vanished += 1;
            return;
        }
        let hint = Some(entry.record.kind);
        if options.drops_before_stat(name, hint) {
            out.skipped += 1;
            return;
        }
        let mut record = entry.record;
        if record.kind == EntryKind::Symlink && options.follow_symlinks {
            if let Some(target) = entry.name_c.and_then(|name| stat_at(fd, name)) {
                record = Record {
                    is_symlink: true,
                    ..target
                };
            }
        }
        if options.drops_after_stat(name, hint, record.kind) {
            out.skipped += 1;
            return;
        }
        if record.kind == EntryKind::File && options.is_ignore_file(name) {
            if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
                out.ignore_files.push((name.to_os_string(), text));
            }
        }
        out.push_bytes(entry.name, record);
    }

    /// `fstatat(fd, name, 0)`: follow a link from its directory.
    fn stat_at(fd: &OwnedFd, name: &CStr) -> Option<Record> {
        // SAFETY: `stat` is plain old data, so all-zero is valid.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open, `name` is NUL-terminated, `st` is a valid
        // out-pointer; flags 0 follows the link.
        let rc = unsafe { libc::fstatat(fd.as_raw_fd(), name.as_ptr(), &mut st, 0) };
        if rc != 0 {
            return None;
        }
        let kind = match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::File,
            libc::S_IFDIR => EntryKind::Directory,
            libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Unknown,
        };
        Some(
            Record {
                kind,
                is_symlink: false,
                size: u64::try_from(st.st_size).unwrap_or(0),
                mtime_ns: timespec_ns(st.st_mtime, st.st_mtime_nsec),
                btime_ns: Some(timespec_ns(st.st_birthtime, st.st_birthtime_nsec)),
                dev: st.st_dev as u64,
                ino: st.st_ino,
                flags: st.st_flags,
                nlink: u64::from(st.st_nlink),
            }
            .normalised(),
        )
    }
}

/// Linux `getdents64` + `statx` lister.
#[cfg(target_os = "linux")]
mod linux {
    use std::cell::RefCell;
    use std::ffi::{CStr, OsStr};
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use rustix::fd::OwnedFd;
    use rustix::fs::{
        openat, statx, AtFlags, FileType, Mode, OFlags, RawDir, Statx, StatxFlags, CWD,
    };
    use rustix::io::Errno;

    use super::{timespec_ns, ListerKind, Record, ScanListOptions, ScanListing};
    use crate::entry::EntryKind;
    use crate::read_boundary::ReadProgress;

    /// `getdents64` buffer; a few hundred entries per call.
    const BUF_LEN: usize = 64 * 1024;

    thread_local! {
        static BUF: RefCell<Vec<MaybeUninit<u8>>> = const { RefCell::new(Vec::new()) };
    }

    /// `Ok(None)` when `statx` is unavailable (kernel < 4.11, or a seccomp
    /// policy that answers `EPERM`): the portable lister takes over.
    pub(super) fn list(
        dir: &Path,
        options: &ScanListOptions,
        progress: &ReadProgress,
    ) -> io::Result<Option<ScanListing>> {
        let fd = openat(
            CWD,
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        BUF.with(|cell| match cell.try_borrow_mut() {
            Ok(mut buf) => read_all(&fd, &mut buf, options, progress),
            Err(_) => read_all(&fd, &mut Vec::new(), options, progress),
        })
    }

    fn read_all(
        fd: &OwnedFd,
        buf: &mut Vec<MaybeUninit<u8>>,
        options: &ScanListOptions,
        progress: &ReadProgress,
    ) -> io::Result<Option<ScanListing>> {
        if buf.len() < BUF_LEN {
            buf.resize(BUF_LEN, MaybeUninit::uninit());
        }
        let mut out = ScanListing::new(ListerKind::Statx);
        let mut stat_ok = false;
        let mut dir = RawDir::new(fd, &mut buf[..]);
        while let Some(entry) = dir.next() {
            let entry = entry?;
            let name_c = entry.file_name();
            let bytes = name_c.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            progress.tick();
            let name = OsStr::from_bytes(bytes);
            let hint = match entry.file_type() {
                FileType::RegularFile => Some(EntryKind::File),
                FileType::Directory => Some(EntryKind::Directory),
                FileType::Symlink => Some(EntryKind::Symlink),
                FileType::Unknown => None,
                _ => Some(EntryKind::Unknown),
            };
            if options.drops_before_stat(name, hint) {
                out.skipped += 1;
                continue;
            }
            let mut record = match stat(fd, name_c, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(record) => record,
                Err(Errno::NOSYS | Errno::PERM) if !stat_ok => return Ok(None),
                Err(_) => {
                    out.vanished += 1;
                    continue;
                }
            };
            stat_ok = true;
            if record.kind == EntryKind::Symlink && options.follow_symlinks {
                if let Ok(target) = stat(fd, name_c, AtFlags::empty()) {
                    record = Record {
                        is_symlink: true,
                        ..target
                    };
                }
            }
            if options.drops_after_stat(name, hint, record.kind) {
                out.skipped += 1;
                continue;
            }
            if record.kind == EntryKind::File && options.is_ignore_file(name) {
                if let Some(text) = read_at(fd, name_c) {
                    out.ignore_files.push((name.to_os_string(), text));
                }
            }
            out.push_bytes(bytes, record);
        }
        Ok(Some(out))
    }

    fn stat(fd: &OwnedFd, name: &CStr, flags: AtFlags) -> Result<Record, Errno> {
        statx(
            fd,
            name,
            flags | AtFlags::STATX_SYNC_AS_STAT,
            StatxFlags::BASIC_STATS | StatxFlags::BTIME,
        )
        .map(|stx| from_statx(&stx))
    }

    fn from_statx(stx: &Statx) -> Record {
        let kind = match FileType::from_raw_mode(u32::from(stx.stx_mode)) {
            FileType::RegularFile => EntryKind::File,
            FileType::Directory => EntryKind::Directory,
            FileType::Symlink => EntryKind::Symlink,
            _ => EntryKind::Unknown,
        };
        let has_btime = StatxFlags::from_bits_retain(stx.stx_mask).contains(StatxFlags::BTIME);
        Record {
            kind,
            is_symlink: kind == EntryKind::Symlink,
            size: stx.stx_size,
            mtime_ns: timespec_ns(stx.stx_mtime.tv_sec, i64::from(stx.stx_mtime.tv_nsec)),
            btime_ns: has_btime
                .then(|| timespec_ns(stx.stx_btime.tv_sec, i64::from(stx.stx_btime.tv_nsec))),
            // The same `makedev` std applies to statx results, so `dev`
            // matches `MetadataExt::dev`.
            dev: rustix::fs::makedev(stx.stx_dev_major, stx.stx_dev_minor),
            ino: stx.stx_ino,
            // STATX_ATTR_* bits (compressed, immutable, encrypted, …); all
            // defined bits fit in 32.
            flags: stx.stx_attributes.bits() as u32,
            nlink: u64::from(stx.stx_nlink),
        }
        .normalised()
    }

    fn read_at(fd: &OwnedFd, name: &CStr) -> Option<String> {
        use std::io::Read;
        let file = openat(fd, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).ok()?;
        let mut text = String::new();
        std::fs::File::from(file).read_to_string(&mut text).ok()?;
        Some(text)
    }
}

impl PartialEq for FileFilter {
    fn eq(&self, other: &Self) -> bool {
        self.same_as(other)
    }
}

impl Eq for FileFilter {}

impl Hash for FileFilter {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore::IgnoreMatcher;

    /// The fast name rules and the documented gitignore text must agree, so
    /// a caller who builds an `IgnoreMatcher` from `DEFAULT_IGNORE_PATTERNS`
    /// gets exactly what the scan does.
    #[test]
    fn default_name_rules_match_the_gitignore_text() {
        let root = Path::new("/lib");
        let mut matcher = IgnoreMatcher::new();
        matcher
            .add_from_string(root, DEFAULT_IGNORE_PATTERNS)
            .unwrap();
        let names = [
            ".DS_Store",
            "._Movie (2001).mkv",
            "._",
            ".AppleDouble",
            ".Spotlight-V100",
            ".Trashes",
            ".fseventsd",
            ".TemporaryItems",
            ".DocumentRevisions-V100",
            "Thumbs.db",
            "desktop.ini",
            "$RECYCLE.BIN",
            "System Volume Information",
            "@eaDir",
            "#recycle",
            "#snapshot",
            "@Recycle",
            ".@__thumb",
            ".recycle",
            ".Trash-1000",
            ".Trash-",
            "lost+found",
            // Near misses that must be kept.
            "DS_Store",
            "x.DS_Store",
            ".DS_Store.mkv",
            "Movie.mkv",
            "Season 01",
            ".Trash",
            "thumbs.db",
            "Thumbs.db.bak",
            "eaDir",
            "recycle",
            "#recycled",
            "lost+found2",
            "System",
            "Desktop.ini",
            ".hidden",
            "_.mkv",
        ];
        for name in names {
            for is_dir in [false, true] {
                assert_eq!(
                    is_default_ignored(name.as_bytes()),
                    matcher.is_ignored(&root.join(name), is_dir),
                    "{name:?} (dir: {is_dir})"
                );
            }
        }
    }

    #[test]
    fn listing_names_round_trip_through_the_arena() {
        let mut listing = ScanListing::new(ListerKind::Portable);
        let record = Record {
            kind: EntryKind::File,
            is_symlink: false,
            size: 1,
            mtime_ns: 2,
            btime_ns: Some(3),
            dev: 4,
            ino: 5,
            flags: 6,
            nlink: 1,
        };
        for name in ["a.mkv", "Séance (1999).mkv", ""] {
            listing.push(OsStr::new(name), record);
        }
        let names: Vec<_> = listing.iter().map(|e| e.name.to_owned()).collect();
        assert_eq!(names, ["a.mkv", "Séance (1999).mkv", ""]);
        assert_eq!(listing.len(), 3);
    }

    /// A name that is not UTF-8 is stored and returned byte for byte.
    #[cfg(unix)]
    #[test]
    fn non_utf8_names_round_trip_through_the_arena() {
        use std::os::unix::ffi::OsStrExt;
        let mut listing = ScanListing::new(ListerKind::Portable);
        let raw: &[u8] = b"caf\xe9 \xff.mkv";
        let record = Record {
            kind: EntryKind::File,
            is_symlink: false,
            size: 0,
            mtime_ns: 0,
            btime_ns: None,
            dev: 0,
            ino: 0,
            flags: 0,
            nlink: 1,
        };
        listing.push(OsStr::from_bytes(raw), record);
        let entry = listing.iter().next().unwrap();
        assert_eq!(entry.name.as_bytes(), raw);
        assert!(entry.name.to_str().is_none());
    }
}
