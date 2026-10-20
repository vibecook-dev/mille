//! Error taxonomy (SPEC §4.7). ErrorCode mirrors api.d.ts; FxError is the
//! internal Rust error. The NAPI binding stamps `__fx_code` on thrown
//! exceptions so the TS side can reconstruct a FileSystemError.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable error code mirroring `api.d.ts` ErrorCode union.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum ErrorCode {
    EACCES,
    ENOENT,
    EEXIST,
    EISDIR,
    ENOTDIR,
    ELOOP,
    ENOSPC,
    EROFS,
    EBUSY,
    EINVAL,
    ECANCELED,
    EUNSUPPORTED,
    EUNKNOWN,
    // Appended, not alphabetized: the derive numbers variants by position, so
    // a mid-list insert would renumber every code a binary format recorded.
    /// A native read made no progress within the explorer's read deadline,
    /// or a read into the same path is still stuck (see `read_boundary`).
    ETIMEDOUT,
    /// The directory is a cloud placeholder whose contents are not on this
    /// machine (macOS `SF_DATALESS`), and the explorer does not materialize.
    EDATALESS,
}

impl ErrorCode {
    /// String variant name — the payload shipped across the NAPI boundary
    /// as `__fx_code` (SPEC §4.7).
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::EACCES => "EACCES",
            ErrorCode::ENOENT => "ENOENT",
            ErrorCode::EEXIST => "EEXIST",
            ErrorCode::EISDIR => "EISDIR",
            ErrorCode::ENOTDIR => "ENOTDIR",
            ErrorCode::ELOOP => "ELOOP",
            ErrorCode::ENOSPC => "ENOSPC",
            ErrorCode::EROFS => "EROFS",
            ErrorCode::EBUSY => "EBUSY",
            ErrorCode::EINVAL => "EINVAL",
            ErrorCode::ECANCELED => "ECANCELED",
            ErrorCode::EUNSUPPORTED => "EUNSUPPORTED",
            ErrorCode::EUNKNOWN => "EUNKNOWN",
            ErrorCode::ETIMEDOUT => "ETIMEDOUT",
            ErrorCode::EDATALESS => "EDATALESS",
        }
    }

    /// Build an `FxError::Io` from a `std::io::Error` + the path we were
    /// operating on. Use this at every call site that knows the path; the
    /// bare `From<io::Error>` fallback uses an empty path.
    pub fn from_io_error(err: &std::io::Error, path: PathBuf) -> FxError {
        let code = io_error_code(err);
        // Clone the io::Error through its raw_os_error / kind since io::Error
        // itself isn't Clone.
        let source = clone_io_error(err);
        FxError::Io { code, path, source }
    }
}

/// Internal error type. All variants have an `ErrorCode` available via `code()`.
#[derive(Error, Debug)]
pub enum FxError {
    #[error("io error ({code:?}) at {path:?}: {source}")]
    Io {
        code: ErrorCode,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("cancelled")]
    Cancelled,

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("internal bug: {0}")]
    InternalBug(String),

    #[error("EntryId space exhausted (2^53 cap)")]
    EntryIdExhausted,

    /// A walk finished, but some directories below its root never answered
    /// (stalled, refused while stuck, dataless) or answered with an OS error.
    /// Their children are unknown — not absent — so a caller that wanted the
    /// whole tree must not treat the entries it did get as complete. Returned
    /// by the walk forms that have no report to put `unread` in.
    #[error("{}", incomplete_message(unread))]
    Incomplete {
        unread: Vec<crate::walker::UnreadDirectory>,
    },
}

fn incomplete_message(unread: &[crate::walker::UnreadDirectory]) -> String {
    match unread.first() {
        Some(first) => format!(
            "walk incomplete: {} director{} could not be read (first: {:?}, {:?})",
            unread.len(),
            if unread.len() == 1 { "y" } else { "ies" },
            first.path,
            first.failure.code(),
        ),
        None => "walk incomplete".to_string(),
    }
}

impl FxError {
    /// The ErrorCode that crosses the NAPI boundary for this error.
    pub fn code(&self) -> ErrorCode {
        match self {
            FxError::Io { code, .. } => *code,
            FxError::Unsupported(_) => ErrorCode::EUNSUPPORTED,
            FxError::Cancelled => ErrorCode::ECANCELED,
            FxError::InvalidInput(_) => ErrorCode::EINVAL,
            FxError::InternalBug(_) => ErrorCode::EUNKNOWN,
            FxError::EntryIdExhausted => ErrorCode::EUNKNOWN,
            // The first directory's own reason: ETIMEDOUT for a stall,
            // EDATALESS for a placeholder, EACCES for a permission error.
            FxError::Incomplete { unread } => unread
                .first()
                .map_or(ErrorCode::ETIMEDOUT, |first| first.failure.code()),
        }
    }
}

impl From<std::io::Error> for FxError {
    /// Path-less fallback; prefer `ErrorCode::from_io_error(err, path)` when
    /// the path is known at the call site.
    fn from(err: std::io::Error) -> Self {
        ErrorCode::from_io_error(&err, PathBuf::new())
    }
}

#[cfg(unix)]
fn io_error_code(err: &std::io::Error) -> ErrorCode {
    if let Some(errno) = err.raw_os_error() {
        return match errno {
            13 => ErrorCode::EACCES,
            2 => ErrorCode::ENOENT,
            17 => ErrorCode::EEXIST,
            21 => ErrorCode::EISDIR,
            20 => ErrorCode::ENOTDIR,
            // ELOOP is 40 on Linux, 62 on macOS/BSD.
            40 | 62 => ErrorCode::ELOOP,
            28 => ErrorCode::ENOSPC,
            // EROFS is 30 on Linux, 30 on macOS too.
            30 => ErrorCode::EROFS,
            16 => ErrorCode::EBUSY,
            22 => ErrorCode::EINVAL,
            125 => ErrorCode::ECANCELED,
            _ => io_kind_code(err),
        };
    }
    io_kind_code(err)
}

#[cfg(windows)]
fn io_error_code(err: &std::io::Error) -> ErrorCode {
    // Win32 status codes, not errno values. Leaning on `ErrorKind` alone was
    // losing the most common Windows failure outright: a file held by another
    // process fails with ERROR_SHARING_VIOLATION, which `std` reports as
    // `ErrorKind::Uncategorized` — so it arrived as EUNKNOWN even though
    // EMBEDDING.md documents EBUSY for exactly that case.
    //
    // Only unambiguous codes are listed. Anything else keeps the `ErrorKind`
    // fallback, which already covers ERROR_ACCESS_DENIED and friends.
    if let Some(code) = err.raw_os_error() {
        return match code {
            // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND / ERROR_INVALID_DRIVE
            2 | 3 | 15 => ErrorCode::ENOENT,
            // ERROR_ACCESS_DENIED
            5 => ErrorCode::EACCES,
            // ERROR_WRITE_PROTECT
            19 => ErrorCode::EROFS,
            // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION
            32 | 33 => ErrorCode::EBUSY,
            // ERROR_HANDLE_DISK_FULL / ERROR_DISK_FULL
            39 | 112 => ErrorCode::ENOSPC,
            // ERROR_FILE_EXISTS / ERROR_ALREADY_EXISTS
            80 | 183 => ErrorCode::EEXIST,
            // ERROR_INVALID_PARAMETER / ERROR_INVALID_NAME /
            // ERROR_FILENAME_EXCED_RANGE
            87 | 123 | 206 => ErrorCode::EINVAL,
            // ERROR_DIRECTORY — "the directory name is invalid", raised when a
            // file is used where a directory is required.
            267 => ErrorCode::ENOTDIR,
            // ERROR_OPERATION_ABORTED
            995 => ErrorCode::ECANCELED,
            // ERROR_USER_MAPPED_FILE — a mapped file cannot be replaced.
            1224 => ErrorCode::EBUSY,
            // ERROR_CANT_RESOLVE_FILENAME — reparse-point resolution gave up,
            // the Windows analogue of a symlink loop.
            1921 => ErrorCode::ELOOP,
            _ => io_kind_code(err),
        };
    }
    io_kind_code(err)
}

/// Fallback: map `io::ErrorKind` to an ErrorCode. Used on Windows and when
/// `raw_os_error()` is unrecognized on Unix.
fn io_kind_code(err: &std::io::Error) -> ErrorCode {
    use std::io::ErrorKind as K;
    match err.kind() {
        K::PermissionDenied => ErrorCode::EACCES,
        K::NotFound => ErrorCode::ENOENT,
        K::AlreadyExists => ErrorCode::EEXIST,
        K::NotADirectory => ErrorCode::ENOTDIR,
        K::IsADirectory => ErrorCode::EISDIR,
        K::InvalidInput | K::InvalidData => ErrorCode::EINVAL,
        K::TimedOut | K::Interrupted => ErrorCode::EBUSY,
        K::Unsupported => ErrorCode::EUNSUPPORTED,
        _ => ErrorCode::EUNKNOWN,
    }
}

/// `std::io::Error` isn't Clone; reconstruct one that preserves whichever
/// signal we actually use (raw_os_error or ErrorKind + message).
fn clone_io_error(err: &std::io::Error) -> std::io::Error {
    if let Some(errno) = err.raw_os_error() {
        std::io::Error::from_raw_os_error(errno)
    } else {
        std::io::Error::new(err.kind(), err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_as_str_roundtrip() {
        let all = [
            (ErrorCode::EACCES, "EACCES"),
            (ErrorCode::ENOENT, "ENOENT"),
            (ErrorCode::EEXIST, "EEXIST"),
            (ErrorCode::EISDIR, "EISDIR"),
            (ErrorCode::ENOTDIR, "ENOTDIR"),
            (ErrorCode::ELOOP, "ELOOP"),
            (ErrorCode::ENOSPC, "ENOSPC"),
            (ErrorCode::EROFS, "EROFS"),
            (ErrorCode::EBUSY, "EBUSY"),
            (ErrorCode::EINVAL, "EINVAL"),
            (ErrorCode::ECANCELED, "ECANCELED"),
            (ErrorCode::EUNSUPPORTED, "EUNSUPPORTED"),
            (ErrorCode::EUNKNOWN, "EUNKNOWN"),
            (ErrorCode::ETIMEDOUT, "ETIMEDOUT"),
            (ErrorCode::EDATALESS, "EDATALESS"),
        ];
        for (code, name) in all {
            assert_eq!(code.as_str(), name);
        }
    }

    /// "Permission denied" as the host OS numbers it: errno 13 on Unix,
    /// ERROR_ACCESS_DENIED (5) on Windows. Passing the Unix value on Windows
    /// selects ERROR_INVALID_DATA, which is a different error entirely.
    const ACCESS_DENIED: i32 = if cfg!(windows) { 5 } else { 13 };

    #[test]
    fn fx_error_code_per_variant() {
        let io = ErrorCode::from_io_error(
            &std::io::Error::from_raw_os_error(ACCESS_DENIED),
            PathBuf::from("x"),
        );
        assert_eq!(io.code(), ErrorCode::EACCES);

        assert_eq!(
            FxError::Unsupported("x".into()).code(),
            ErrorCode::EUNSUPPORTED
        );
        assert_eq!(FxError::Cancelled.code(), ErrorCode::ECANCELED);
        assert_eq!(FxError::InvalidInput("x".into()).code(), ErrorCode::EINVAL);
        assert_eq!(FxError::InternalBug("x".into()).code(), ErrorCode::EUNKNOWN);
        assert_eq!(FxError::EntryIdExhausted.code(), ErrorCode::EUNKNOWN);

        let incomplete = FxError::Incomplete {
            unread: vec![crate::walker::UnreadDirectory {
                path: PathBuf::from("/media/tv"),
                failure: crate::read_boundary::ReadFailure::Dataless,
            }],
        };
        assert_eq!(incomplete.code(), ErrorCode::EDATALESS);
        assert!(
            incomplete.to_string().contains("1 directory"),
            "{incomplete}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn io_error_errno_mapping_unix() {
        let cases = [
            (13, ErrorCode::EACCES),
            (2, ErrorCode::ENOENT),
            (17, ErrorCode::EEXIST),
            (21, ErrorCode::EISDIR),
            (20, ErrorCode::ENOTDIR),
            (28, ErrorCode::ENOSPC),
            (30, ErrorCode::EROFS),
            (16, ErrorCode::EBUSY),
            (22, ErrorCode::EINVAL),
        ];
        for (errno, expected) in cases {
            let err = std::io::Error::from_raw_os_error(errno);
            let fx = ErrorCode::from_io_error(&err, PathBuf::from("/tmp/x"));
            assert_eq!(fx.code(), expected, "errno {errno}");
        }
    }

    /// The Win32 status codes an explorer actually meets. ERROR_SHARING_VIOLATION
    /// is the important one: `std` reports it as `ErrorKind::Uncategorized`, so
    /// before this table it surfaced as EUNKNOWN rather than the EBUSY that
    /// EMBEDDING.md documents for "file in use by another process".
    #[cfg(windows)]
    #[test]
    fn io_error_status_mapping_windows() {
        let cases = [
            (5, ErrorCode::EACCES),
            (2, ErrorCode::ENOENT),
            (3, ErrorCode::ENOENT),
            (32, ErrorCode::EBUSY),
            (33, ErrorCode::EBUSY),
            (80, ErrorCode::EEXIST),
            (183, ErrorCode::EEXIST),
            (19, ErrorCode::EROFS),
            (112, ErrorCode::ENOSPC),
            (267, ErrorCode::ENOTDIR),
            (995, ErrorCode::ECANCELED),
            (1921, ErrorCode::ELOOP),
        ];
        for (status, expected) in cases {
            let err = std::io::Error::from_raw_os_error(status);
            let fx = ErrorCode::from_io_error(&err, PathBuf::from("C:\\tmp\\x"));
            assert_eq!(fx.code(), expected, "win32 status {status}");
        }
    }

    /// A real sharing violation, not a synthesized status code: hold a file
    /// with an exclusive share mode and confirm the operations an explorer
    /// performs report EBUSY rather than EUNKNOWN.
    #[cfg(windows)]
    #[test]
    fn exclusive_handle_reports_ebusy() {
        use std::os::windows::fs::OpenOptionsExt;

        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("locked.txt");
        std::fs::write(&path, b"x").unwrap();

        // share_mode(0) is what an editor or scanner holding the file does.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();

        for err in [
            std::fs::remove_file(&path).unwrap_err(),
            std::fs::rename(&path, td.path().join("moved.txt")).unwrap_err(),
            std::fs::read(&path).unwrap_err(),
        ] {
            assert_eq!(
                ErrorCode::from_io_error(&err, path.clone()).code(),
                ErrorCode::EBUSY,
                "raw_os_error={:?}",
                err.raw_os_error()
            );
        }
    }

    #[test]
    fn io_error_kind_fallback() {
        // An error without a raw_os_error still maps via ErrorKind.
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let fx = ErrorCode::from_io_error(&err, PathBuf::from("/x"));
        assert_eq!(fx.code(), ErrorCode::EACCES);

        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let fx = ErrorCode::from_io_error(&err, PathBuf::from("/x"));
        assert_eq!(fx.code(), ErrorCode::ENOENT);

        let err = std::io::Error::new(std::io::ErrorKind::AlreadyExists, "boom");
        let fx = ErrorCode::from_io_error(&err, PathBuf::from("/x"));
        assert_eq!(fx.code(), ErrorCode::EEXIST);

        let err = std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory");
        let fx = ErrorCode::from_io_error(&err, PathBuf::from("/x"));
        assert_eq!(fx.code(), ErrorCode::ENOTDIR);

        let err = std::io::Error::new(std::io::ErrorKind::IsADirectory, "is a directory");
        let fx = ErrorCode::from_io_error(&err, PathBuf::from("/x"));
        assert_eq!(fx.code(), ErrorCode::EISDIR);
    }

    #[test]
    fn from_io_error_fallback_has_empty_path() {
        let err = std::io::Error::from_raw_os_error(2);
        let fx: FxError = err.into();
        match fx {
            FxError::Io { code, path, .. } => {
                assert_eq!(code, ErrorCode::ENOENT);
                assert_eq!(path, PathBuf::new());
            }
            _ => panic!("expected Io variant"),
        }
    }
}
