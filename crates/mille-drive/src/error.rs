//! `DriveError`: one struct with a stable code, an optional path and a
//! message, so the napi binding can encode it as `DRIVE|<code>|<path>|<message>`
//! and a host can map each code to an honest state (DR-L8) — `ENOSPC` to "this
//! desktop is out of room", `EMISMATCH` to "a copy arrived damaged and was
//! refused", never a blank.

use std::io;
use std::path::{Path, PathBuf};

/// The stable code of a [`DriveError`]. [`DriveErrorCode::as_str`] is the wire
/// spelling, and the TypeScript `DriveErrorCode` union mirrors it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriveErrorCode {
    /// An I/O failure with no more specific code.
    Io,
    /// A file, folder or object that is not there — an object the drive does
    /// not hold, a root that vanished.
    NotFound,
    /// A destination that already exists (`build_into` without `replace`).
    Exists,
    /// Bytes whose blake3 or size is not the one they were named by (DR-L3).
    Mismatch,
    /// An object over `max_object_bytes`.
    TooBig,
    /// The volume is full or over its quota.
    NoSpace,
    /// A malformed argument: an id, a path, a manifest, an option.
    Invalid,
    /// Another `Drive` holds this root's lock.
    Busy,
    /// A document, record or frame that does not decode.
    Format,
    /// A peer that broke the sync protocol (a frame version, a chunk order).
    Proto,
    /// A transport that could not send or pull.
    Transport,
    /// An item id the document does not name.
    UnknownItem,
    /// A version id the document does not name for that item.
    UnknownVersion,
}

impl DriveErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            DriveErrorCode::Io => "EIO",
            DriveErrorCode::NotFound => "ENOENT",
            DriveErrorCode::Exists => "EEXIST",
            DriveErrorCode::Mismatch => "EMISMATCH",
            DriveErrorCode::TooBig => "ETOOBIG",
            DriveErrorCode::NoSpace => "ENOSPC",
            DriveErrorCode::Invalid => "EINVAL",
            DriveErrorCode::Busy => "EBUSY",
            DriveErrorCode::Format => "EFORMAT",
            DriveErrorCode::Proto => "EPROTO",
            DriveErrorCode::Transport => "ETRANSPORT",
            DriveErrorCode::UnknownItem => "EUNKNOWNITEM",
            DriveErrorCode::UnknownVersion => "EUNKNOWNVERSION",
        }
    }
}

/// Every failure mille-drive reports.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DriveError {
    code: DriveErrorCode,
    path: Option<PathBuf>,
    message: String,
}

pub type Result<T> = std::result::Result<T, DriveError>;

impl DriveError {
    pub fn new(code: DriveErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            path: None,
            message: message.into(),
        }
    }

    /// Attach the path the failure concerns.
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// An `io::Error` on `path`, coded by what it means: `ENOENT`, `EEXIST`,
    /// `ENOSPC` (full or over quota), else `EIO`.
    pub fn io(err: io::Error, path: &Path) -> Self {
        let code = if is_out_of_room(&err) {
            DriveErrorCode::NoSpace
        } else {
            match err.kind() {
                io::ErrorKind::NotFound => DriveErrorCode::NotFound,
                io::ErrorKind::AlreadyExists => DriveErrorCode::Exists,
                _ => DriveErrorCode::Io,
            }
        };
        Self::new(code, format!("{}: {err}", path.display())).with_path(path)
    }

    pub fn code(&self) -> DriveErrorCode {
        self.code
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(DriveErrorCode::Invalid, message)
    }

    pub(crate) fn format(message: impl Into<String>) -> Self {
        Self::new(DriveErrorCode::Format, message)
    }

    pub(crate) fn proto(message: impl Into<String>) -> Self {
        Self::new(DriveErrorCode::Proto, message)
    }

    pub(crate) fn mismatch(message: impl Into<String>) -> Self {
        Self::new(DriveErrorCode::Mismatch, message)
    }
}

/// `.map_err(io_err(path))`: an `io::Error` on `path` as a [`DriveError`].
pub(crate) fn io_err(path: &Path) -> impl FnOnce(io::Error) -> DriveError + '_ {
    move |err| DriveError::io(err, path)
}

/// `ENOSPC` and `EDQUOT` by their raw codes, which every supported toolchain
/// reports the same way (the `io::ErrorKind` spellings are newer than this
/// workspace's `rust-version`).
#[cfg(unix)]
fn is_out_of_room(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(code) if code == libc::ENOSPC || code == libc::EDQUOT)
}

/// `ERROR_HANDLE_DISK_FULL` (39) and `ERROR_DISK_FULL` (112).
#[cfg(windows)]
fn is_out_of_room(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(39 | 112))
}

#[cfg(not(any(unix, windows)))]
fn is_out_of_room(_err: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_are_coded_by_meaning() {
        let missing = DriveError::io(io::Error::from(io::ErrorKind::NotFound), Path::new("/a"));
        assert_eq!(missing.code(), DriveErrorCode::NotFound);
        assert_eq!(missing.path(), Some(Path::new("/a")));

        let exists = DriveError::io(
            io::Error::from(io::ErrorKind::AlreadyExists),
            Path::new("/b"),
        );
        assert_eq!(exists.code(), DriveErrorCode::Exists);

        let other = DriveError::io(io::Error::other("boom"), Path::new("/c"));
        assert_eq!(other.code(), DriveErrorCode::Io);
        assert!(other.message().contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn a_full_volume_is_enospc_not_eio() {
        let full = DriveError::io(io::Error::from_raw_os_error(libc::ENOSPC), Path::new("/x"));
        assert_eq!(full.code(), DriveErrorCode::NoSpace);
        assert_eq!(full.code().as_str(), "ENOSPC");
        let quota = DriveError::io(io::Error::from_raw_os_error(libc::EDQUOT), Path::new("/x"));
        assert_eq!(quota.code(), DriveErrorCode::NoSpace);
    }

    #[cfg(windows)]
    #[test]
    fn a_full_volume_is_enospc_not_eio() {
        let full = DriveError::io(io::Error::from_raw_os_error(112), Path::new("C:\\x"));
        assert_eq!(full.code(), DriveErrorCode::NoSpace);
    }

    #[test]
    fn every_code_has_a_distinct_wire_spelling() {
        let codes = [
            DriveErrorCode::Io,
            DriveErrorCode::NotFound,
            DriveErrorCode::Exists,
            DriveErrorCode::Mismatch,
            DriveErrorCode::TooBig,
            DriveErrorCode::NoSpace,
            DriveErrorCode::Invalid,
            DriveErrorCode::Busy,
            DriveErrorCode::Format,
            DriveErrorCode::Proto,
            DriveErrorCode::Transport,
            DriveErrorCode::UnknownItem,
            DriveErrorCode::UnknownVersion,
        ];
        let mut spellings: Vec<&str> = codes.iter().map(|c| c.as_str()).collect();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), codes.len());
        assert!(spellings
            .iter()
            .all(|s| s.starts_with('E') && !s.contains('|')));
    }
}
