//! The drive's folder, and the few file operations every part of it shares.
//!
//! ```text
//! <root>/
//!   drive.json      {format, device}: what wrote this folder
//!   drive.loro      the document's full snapshot, replaced atomically
//!   LOCK            held exclusively by the one open Drive
//!   objects/ab/ab…  objects by blake3, read-only (object_relpath)
//!   built/<item>/<version>/   versions built as folders (hard links)
//!   staging/        temporaries, all named `mdrive-…`, swept at open
//! ```
//!
//! A host's own `staging_dir` holds `LOCK.mdrive` while the drive is open.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::digest::{object_relpath, Blake3};

/// The on-disk format `drive.json` records. A reader refuses a newer one.
pub(crate) const FORMAT: u32 = 1;

/// Every temporary mille-drive makes in `staging/` starts with this, so the
/// sweep at open never touches a file it did not make — even when a host
/// points `staging_dir` at a folder it shares.
pub(crate) const TEMP_PREFIX: &str = "mdrive-";

/// Temporaries made beside a destination (a build) start with this.
pub(crate) const SIBLING_TEMP_PREFIX: &str = ".mdrive-";

/// The lock a drive takes in a host's own staging folder. Not `mdrive-…`, so
/// no sweep ever removes it.
pub(crate) const STAGING_LOCK: &str = "LOCK.mdrive";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name no other temporary of this process or another has: the pid, a
/// process-wide counter and the clock.
pub(crate) fn temp_name(kind: &str) -> String {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{TEMP_PREFIX}{kind}-{}-{n}-{nanos}", std::process::id())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub(crate) root: PathBuf,
    pub(crate) objects: PathBuf,
    pub(crate) built: PathBuf,
    pub(crate) staging: PathBuf,
    pub(crate) doc: PathBuf,
    pub(crate) meta: PathBuf,
    pub(crate) lock: PathBuf,
}

impl Layout {
    pub(crate) fn new(root: &Path, staging: Option<&Path>) -> Self {
        Self {
            root: root.to_path_buf(),
            objects: root.join("objects"),
            built: root.join("built"),
            staging: staging.map_or_else(|| root.join("staging"), Path::to_path_buf),
            doc: root.join("drive.loro"),
            meta: root.join("drive.json"),
            lock: root.join("LOCK"),
        }
    }

    pub(crate) fn object_path(&self, digest: &Blake3) -> PathBuf {
        self.objects.join(object_relpath(digest))
    }

    pub(crate) fn built_version(&self, item: &str, version: &str) -> PathBuf {
        self.built.join(item).join(version)
    }
}

/// Make `path` read-only (every write bit off on Unix, the attribute on
/// Windows). An object shares its inode with every hard link to it, so this
/// protects each name at once.
pub(crate) fn set_readonly(path: &Path) -> io::Result<()> {
    let mut perms = fs::symlink_metadata(path)?.permissions();
    if !perms.readonly() {
        perms.set_readonly(true);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Remove a file, read-only or not. Unix unlinks a read-only file freely;
/// Windows refuses until its attribute is cleared.
pub(crate) fn remove_file_force(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => retry_remove_writable(path, err),
    }
}

/// Remove a folder and everything in it. Missing is fine.
pub(crate) fn remove_tree(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => retry_remove_tree_writable(dir, err),
    }
}

#[cfg(windows)]
fn retry_remove_writable(path: &Path, err: io::Error) -> io::Result<()> {
    if err.kind() != io::ErrorKind::PermissionDenied {
        return Err(err);
    }
    clear_readonly(path)?;
    fs::remove_file(path)
}

#[cfg(not(windows))]
fn retry_remove_writable(_path: &Path, err: io::Error) -> io::Result<()> {
    Err(err)
}

#[cfg(windows)]
fn retry_remove_tree_writable(dir: &Path, err: io::Error) -> io::Result<()> {
    if err.kind() != io::ErrorKind::PermissionDenied {
        return Err(err);
    }
    clear_readonly_tree(dir)?;
    fs::remove_dir_all(dir)
}

#[cfg(not(windows))]
fn retry_remove_tree_writable(_dir: &Path, err: io::Error) -> io::Result<()> {
    Err(err)
}

/// Clear the read-only attribute — Windows only, where it is what blocks a
/// delete. On a hard-linked object this clears it for every name; `gc` puts it
/// back on the objects it keeps.
#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly(path: &Path) -> io::Result<()> {
    let mut perms = fs::symlink_metadata(path)?.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(windows)]
fn clear_readonly_tree(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            clear_readonly_tree(&path)?;
        } else {
            clear_readonly(&path)?;
        }
    }
    Ok(())
}

/// Replace `path` with `bytes` atomically: write a sibling, flush it to disk,
/// rename it over. One writer per root (the LOCK), so the sibling's fixed name
/// cannot collide.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(tmp, path)
}

/// Remove what a crash left: `staging/mdrive-*`, `objects/mdrive-*` (the
/// probe `open` renames into a host's staging check) and
/// `built/<item>/.mdrive-*`. Shards under `objects/` are two hex characters,
/// never `mdrive-…`.
pub(crate) fn sweep(layout: &Layout) -> io::Result<()> {
    sweep_prefixed(&layout.staging, TEMP_PREFIX)?;
    sweep_prefixed(&layout.objects, TEMP_PREFIX)?;
    let items = match fs::read_dir(&layout.built) {
        Ok(items) => items,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for item in items {
        let item = item?;
        if item.file_type()?.is_dir() {
            sweep_prefixed(&item.path(), SIBLING_TEMP_PREFIX)?;
        }
    }
    Ok(())
}

/// Remove every entry of `dir` whose name starts with `prefix`. A missing
/// `dir` is fine.
pub(crate) fn sweep_prefixed(dir: &Path, prefix: &str) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            remove_tree(&entry.path())?;
        } else {
            remove_file_force(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_names_are_prefixed_and_distinct() {
        let a = temp_name("put");
        let b = temp_name("put");
        assert!(a.starts_with("mdrive-put-"), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn the_sweep_removes_only_its_own_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path(), None);
        fs::create_dir_all(&layout.staging).unwrap();
        fs::create_dir_all(layout.built.join("item")).unwrap();
        fs::create_dir_all(layout.objects.join("ab")).unwrap();
        fs::write(layout.staging.join("mdrive-put-1"), b"x").unwrap();
        fs::create_dir_all(layout.staging.join("mdrive-build-2/a")).unwrap();
        fs::write(layout.staging.join("someone-elses"), b"y").unwrap();
        fs::write(layout.staging.join(STAGING_LOCK), b"").unwrap();
        fs::create_dir_all(layout.built.join("item/.mdrive-build-3")).unwrap();
        fs::create_dir_all(layout.built.join("item/0123")).unwrap();
        fs::write(layout.objects.join("mdrive-probe-4"), b"").unwrap();

        sweep(&layout).unwrap();

        assert!(!layout.staging.join("mdrive-put-1").exists());
        assert!(!layout.staging.join("mdrive-build-2").exists());
        assert!(layout.staging.join("someone-elses").exists());
        assert!(layout.staging.join(STAGING_LOCK).exists());
        assert!(!layout.built.join("item/.mdrive-build-3").exists());
        assert!(layout.built.join("item/0123").exists());
        assert!(!layout.objects.join("mdrive-probe-4").exists());
        assert!(layout.objects.join("ab").is_dir());
    }

    #[test]
    fn a_read_only_file_and_tree_are_removable() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("t");
        fs::create_dir_all(tree.join("sub")).unwrap();
        let file = tree.join("sub/f");
        fs::write(&file, b"z").unwrap();
        set_readonly(&file).unwrap();
        assert!(fs::metadata(&file).unwrap().permissions().readonly());
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());

        let lone = dir.path().join("lone");
        fs::write(&lone, b"z").unwrap();
        set_readonly(&lone).unwrap();
        remove_file_force(&lone).unwrap();
        assert!(!lone.exists());
        remove_file_force(&lone).unwrap();
    }

    #[test]
    fn write_atomic_replaces_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drive.loro");
        write_atomic(&path, b"first, and longer").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert!(!dir.path().join("drive.loro.tmp").exists());
    }
}
