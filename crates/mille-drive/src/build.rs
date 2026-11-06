//! Versions built as folders: each file a hard link to its object (a copy when
//! the volume refuses a link), assembled under a temporary name beside the
//! destination and renamed into place, so a reader sees no folder or the whole
//! one.
//!
//! A built file shares its object's inode. Writing through it would change
//! the object for every name it has, so objects are read-only and a host that
//! derives a file in a view (a themed `index.html`) replaces the link by
//! writing a new file and renaming it over — never by writing into it.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::error::{io_err, DriveError, DriveErrorCode, Result};
use crate::store::{ObjectStore, PutMode};
use crate::version::FileEntry;

/// Lay `files` out under `dest`, which must not exist yet (its parent must).
/// A file in `except` is left out. Nothing outside `dest` is written, and no
/// symlink is followed: every folder in it is one this call made.
pub(crate) fn materialize(
    store: &ObjectStore,
    files: &[FileEntry],
    dest: &Path,
    mode: PutMode,
    except: &BTreeSet<String>,
) -> Result<()> {
    fs::create_dir(dest).map_err(io_err(dest))?;
    for file in files {
        if except.contains(&file.path) {
            continue;
        }
        let source = store.object_path(&file.blake3);
        if !store.has(&file.blake3) {
            let message = format!("{} needs {}, which is not held", file.path, file.blake3);
            let err = DriveError::new(DriveErrorCode::NotFound, message);
            return Err(err.with_path(source));
        }
        let mut target = dest.to_path_buf();
        for segment in file.path.split('/') {
            target.push(segment);
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        place(&source, &target, mode)?;
    }
    Ok(())
}

fn place(source: &Path, target: &Path, mode: PutMode) -> Result<()> {
    match mode {
        PutMode::Link => fs::hard_link(source, target).map_err(io_err(target)),
        PutMode::Copy => copy(source, target),
        PutMode::LinkOrCopy => match fs::hard_link(source, target) {
            Ok(()) => Ok(()),
            Err(_) => copy(source, target),
        },
    }
}

fn copy(source: &Path, target: &Path) -> Result<()> {
    fs::copy(source, target).map_err(io_err(target))?;
    Ok(())
}
