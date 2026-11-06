//! One `Drive` per root: an exclusive lock on `<root>/LOCK`, held for the
//! drive's life and released by the OS when the process ends, however it ends.
//!
//! Unix takes `flock(LOCK_EX | LOCK_NB)`, which conflicts between two opens
//! even inside one process. Windows opens the file with no sharing at all, so a
//! second open fails with a sharing violation. Neither needs a newer toolchain
//! than this workspace's `rust-version` (std's `File::try_lock` is newer).

use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::error::{DriveError, DriveErrorCode, Result};

#[derive(Debug)]
pub(crate) struct DriveLock {
    _file: File,
}

fn busy(path: &Path) -> DriveError {
    DriveError::new(
        DriveErrorCode::Busy,
        format!("{} is open in another Drive", path.display()),
    )
    .with_path(path)
}

impl DriveLock {
    #[cfg(unix)]
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|err| DriveError::io(err, path))?;
        // SAFETY: `flock` reads only the descriptor, which `file` owns and
        // keeps open for as long as the lock is held.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(busy(path));
            }
            return Err(DriveError::io(err, path));
        }
        Ok(Self { _file: file })
    }

    #[cfg(windows)]
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;

        // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33).
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => Ok(Self { _file: file }),
            Err(err) if matches!(err.raw_os_error(), Some(32 | 33)) => Err(busy(path)),
            Err(err) => Err(DriveError::io(err, path)),
        }
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|err| DriveError::io(err, path))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_on_one_root_is_ebusy_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("LOCK");
        let first = DriveLock::acquire(&path).unwrap();
        let second = DriveLock::acquire(&path).unwrap_err();
        assert_eq!(second.code(), DriveErrorCode::Busy);
        drop(first);
        DriveLock::acquire(&path).unwrap();
    }
}
