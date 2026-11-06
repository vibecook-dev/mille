//! The object store: files named by their blake3 under `objects/`, written
//! once, read-only after, verified on the way in (DR-L2, DR-L3, DR-L5).
//!
//! Every way in — a local put by hard link or by copy, a pulled file admitted
//! from a transport — stages the bytes under a temporary name, hashes what was
//! staged, and only then publishes it under its digest. Publishing is a rename
//! taken under one gate, after checking the name is free: the first writer of
//! an object wins and every later one learns `Existed` (DR-L5). The gate is
//! process-wide for this root, and the root's LOCK keeps other processes out.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::digest::{Blake3, IO_CHUNK};
use crate::error::{io_err, DriveError, DriveErrorCode, Result};
use crate::layout::{remove_file_force, set_readonly, temp_name, Layout};

/// How `put_file` brings a file in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutMode {
    /// A hard link: the object shares the source's inode, and no bytes are
    /// copied. Fails across volumes.
    Link,
    /// A copy, hashed as it is written.
    Copy,
    /// A hard link when the volume allows one, else a copy. The outcome says
    /// which happened.
    LinkOrCopy,
}

/// What `put_file` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PutHow {
    Linked,
    Copied,
    /// The drive already held the object; nothing was written.
    Existed,
}

/// An object's name and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub blake3: Blake3,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PutOutcome {
    pub object: ObjectRef,
    pub how: PutHow,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GcReport {
    pub dry_run: bool,
    /// Objects removed, or that a dry run would remove, in digest order.
    pub removed: Vec<ObjectRef>,
    /// Objects kept.
    pub retained: usize,
    pub bytes_freed: u64,
}

pub(crate) struct ObjectStore {
    layout: Layout,
    max_object_bytes: u64,
    publish_gate: Mutex<()>,
}

/// A hard link's result: done, or refused by the volume (the caller may copy).
enum LinkAttempt {
    Done(PutOutcome),
    Refused(io::Error),
}

fn too_big(path: &Path, bytes: u64, max: u64) -> DriveError {
    let shown = path.display();
    let message = format!("{shown} is {bytes} bytes, over the drive's {max}-byte object limit");
    DriveError::new(DriveErrorCode::TooBig, message).with_path(path)
}

fn check_expect(expect: Option<&Blake3>, actual: &Blake3, path: &Path) -> Result<()> {
    match expect {
        Some(expect) if expect != actual => {
            let shown = path.display();
            let message = format!("{shown} hashes to {actual}, not the expected {expect}");
            Err(DriveError::mismatch(message).with_path(path))
        }
        _ => Ok(()),
    }
}

/// One read, retried on `EINTR`; `None` at the end of the file.
fn read_chunk(input: &mut File, buf: &mut [u8], path: &Path) -> Result<Option<usize>> {
    loop {
        match input.read(buf) {
            Ok(0) => return Ok(None),
            Ok(n) => return Ok(Some(n)),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(DriveError::io(err, path)),
        }
    }
}

/// The digest and length of `path`, refusing past `limit` bytes.
fn hash_file(path: &Path, limit: u64) -> Result<(Blake3, u64)> {
    let mut input = File::open(path).map_err(io_err(path))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_CHUNK];
    let mut total = 0u64;
    while let Some(n) = read_chunk(&mut input, &mut buf, path)? {
        total += n as u64;
        if total > limit {
            return Err(too_big(path, total, limit));
        }
        hasher.update(&buf[..n]);
    }
    Ok((Blake3::from_hasher(&hasher), total))
}

/// Copy `src` to a new file `dst`, hashing what is written, refusing past
/// `limit` bytes. `dst` is flushed to disk before this returns.
fn copy_hashing(src: &Path, dst: &Path, limit: u64) -> Result<(Blake3, u64)> {
    let mut input = File::open(src).map_err(io_err(src))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut output = options.open(dst).map_err(io_err(dst))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_CHUNK];
    let mut total = 0u64;
    while let Some(n) = read_chunk(&mut input, &mut buf, src)? {
        total += n as u64;
        if total > limit {
            return Err(too_big(src, total, limit));
        }
        hasher.update(&buf[..n]);
        output.write_all(&buf[..n]).map_err(io_err(dst))?;
    }
    output.sync_all().map_err(io_err(dst))?;
    Ok((Blake3::from_hasher(&hasher), total))
}

/// The metadata of a regular file — never a symlink, which a hard link would
/// copy as a link rather than follow.
fn regular_file(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path).map_err(io_err(path))?;
    if !meta.is_file() {
        let message = format!("{} is not a regular file", path.display());
        return Err(DriveError::invalid(message).with_path(path));
    }
    Ok(meta)
}

/// Whether two stats name one file whose content has not visibly changed
/// between them: the same device and inode, length and modification time. A
/// path swapped for another file in between fails the inode check, however
/// alike the two files' sizes and times are.
#[cfg(unix)]
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    let same_inode = (before.dev(), before.ino()) == (after.dev(), after.ino());
    let same_time = before.modified().ok() == after.modified().ok();
    same_inode && same_time && before.len() == after.len()
}

/// Without Unix's inode in std, and with NTFS caching times per directory
/// entry (a second name's can lag), only the length compares here;
/// `put_by_link` hashes the linked name again instead.
#[cfg(not(unix))]
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.len() == after.len()
}

impl ObjectStore {
    pub(crate) fn new(layout: Layout, max_object_bytes: u64) -> Self {
        Self {
            layout,
            max_object_bytes,
            publish_gate: Mutex::new(()),
        }
    }

    pub(crate) fn temp_path(&self, kind: &str) -> PathBuf {
        self.layout.staging.join(temp_name(kind))
    }

    pub(crate) fn object_path(&self, digest: &Blake3) -> PathBuf {
        self.layout.object_path(digest)
    }

    pub(crate) fn has(&self, digest: &Blake3) -> bool {
        match fs::symlink_metadata(self.object_path(digest)) {
            Ok(meta) => meta.is_file(),
            Err(_) => false,
        }
    }

    /// Bring `src` in as an object. `expect` refuses it (`EMISMATCH`, and no
    /// object is left) unless it hashes to that digest.
    pub(crate) fn put_file(
        &self,
        src: &Path,
        mode: PutMode,
        expect: Option<&Blake3>,
    ) -> Result<PutOutcome> {
        let before = regular_file(src)?;
        if before.len() > self.max_object_bytes {
            return Err(too_big(src, before.len(), self.max_object_bytes));
        }
        match mode {
            PutMode::Copy => self.put_by_copy(src, expect),
            PutMode::Link => match self.put_by_link(src, &before, expect)? {
                LinkAttempt::Done(outcome) => Ok(outcome),
                LinkAttempt::Refused(err) => Err(DriveError::io(err, src)),
            },
            PutMode::LinkOrCopy => match self.put_by_link(src, &before, expect)? {
                LinkAttempt::Done(outcome) => Ok(outcome),
                LinkAttempt::Refused(err) if err.kind() == io::ErrorKind::NotFound => {
                    Err(DriveError::io(err, src))
                }
                // Another volume (EXDEV), the link ceiling (EMLINK), a file
                // system without hard links: copy instead.
                LinkAttempt::Refused(_) => self.put_by_copy(src, expect),
            },
        }
    }

    /// Hash `src` where it is, then link it in. Hashing in place means an
    /// object the drive already holds costs no link at all. The stat taken
    /// before hashing is compared with the linked file's (`same_file`), and
    /// where there is no inode to compare the linked name is hashed again, so
    /// a source swapped for another file in between is refused rather than
    /// published under a stale name. A write into the shared inode itself, in
    /// place and at any time, is what `verify` exists to find.
    fn put_by_link(
        &self,
        src: &Path,
        before: &fs::Metadata,
        expect: Option<&Blake3>,
    ) -> Result<LinkAttempt> {
        let (digest, bytes) = hash_file(src, self.max_object_bytes)?;
        check_expect(expect, &digest, src)?;
        let object = ObjectRef {
            blake3: digest,
            bytes,
        };
        if self.has(&digest) {
            let how = PutHow::Existed;
            return Ok(LinkAttempt::Done(PutOutcome { object, how }));
        }
        let tmp = self.temp_path("put");
        if let Err(err) = fs::hard_link(src, &tmp) {
            return Ok(LinkAttempt::Refused(err));
        }
        let unchanged = match fs::symlink_metadata(&tmp) {
            Ok(after) => same_file(before, &after) && after.len() == bytes,
            Err(_) => false,
        };
        let unchanged = unchanged && (cfg!(unix) || self.hashes_to(&tmp, &digest));
        if !unchanged {
            let _ = remove_file_force(&tmp);
            let message = format!("{} changed while it was being put", src.display());
            return Err(DriveError::mismatch(message).with_path(src));
        }
        let how = if self.publish(&tmp, &digest)? {
            PutHow::Linked
        } else {
            PutHow::Existed
        };
        Ok(LinkAttempt::Done(PutOutcome { object, how }))
    }

    /// Whether the file at `path` hashes to `digest`, within the size limit.
    fn hashes_to(&self, path: &Path, digest: &Blake3) -> bool {
        let hashed = hash_file(path, self.max_object_bytes);
        hashed.is_ok_and(|(actual, _)| actual == *digest)
    }

    fn put_by_copy(&self, src: &Path, expect: Option<&Blake3>) -> Result<PutOutcome> {
        let tmp = self.temp_path("put");
        let copied = copy_hashing(src, &tmp, self.max_object_bytes);
        let (digest, bytes) = match copied {
            Ok(done) => done,
            Err(err) => {
                let _ = remove_file_force(&tmp);
                return Err(err);
            }
        };
        if let Err(err) = check_expect(expect, &digest, src) {
            let _ = remove_file_force(&tmp);
            return Err(err);
        }
        let how = if self.publish(&tmp, &digest)? {
            PutHow::Copied
        } else {
            PutHow::Existed
        };
        let object = ObjectRef {
            blake3: digest,
            bytes,
        };
        Ok(PutOutcome { object, how })
    }

    /// Bring `data` in as an object.
    pub(crate) fn put_bytes(&self, data: &[u8]) -> Result<PutOutcome> {
        let bytes = data.len() as u64;
        let max = self.max_object_bytes;
        if bytes > max {
            let message = format!("{bytes} bytes is over the drive's {max}-byte object limit");
            return Err(DriveError::new(DriveErrorCode::TooBig, message));
        }
        let digest = Blake3::of(data);
        let object = ObjectRef {
            blake3: digest,
            bytes,
        };
        if self.has(&digest) {
            let how = PutHow::Existed;
            return Ok(PutOutcome { object, how });
        }
        let tmp = self.temp_path("bytes");
        if let Err(err) = write_new(&tmp, data) {
            let _ = remove_file_force(&tmp);
            return Err(DriveError::io(err, &tmp));
        }
        let how = if self.publish(&tmp, &digest)? {
            PutHow::Copied
        } else {
            PutHow::Existed
        };
        Ok(PutOutcome { object, how })
    }

    /// Admit a file a transport landed at `staged` as the object `expected`.
    /// The file is consumed either way: moved in (or copied, from another
    /// volume) before it is hashed, so no other name can change it after the
    /// check. A digest or size that is not the expected one is `EMISMATCH`,
    /// and nothing is kept.
    pub(crate) fn admit(
        &self,
        expected: &Blake3,
        expected_bytes: Option<u64>,
        staged: &Path,
    ) -> Result<PutOutcome> {
        regular_file(staged)?;
        let tmp = self.temp_path("admit");
        if fs::rename(staged, &tmp).is_err() {
            let copied = fs::copy(staged, &tmp);
            let _ = remove_file_force(staged);
            if let Err(err) = copied {
                let _ = remove_file_force(&tmp);
                return Err(DriveError::io(err, staged));
            }
        }
        let (digest, bytes) = match hash_file(&tmp, self.max_object_bytes) {
            Ok(done) => done,
            Err(err) => {
                let _ = remove_file_force(&tmp);
                return Err(err);
            }
        };
        let wrong_size = expected_bytes.is_some_and(|want| want != bytes);
        if digest != *expected || wrong_size {
            let _ = remove_file_force(&tmp);
            let message =
                format!("a pulled copy of {expected} hashed to {digest} ({bytes} bytes), refused");
            return Err(DriveError::mismatch(message).with_path(staged));
        }
        let how = if self.publish(&tmp, &digest)? {
            PutHow::Copied
        } else {
            PutHow::Existed
        };
        let object = ObjectRef {
            blake3: digest,
            bytes,
        };
        Ok(PutOutcome { object, how })
    }

    /// Publish a staged temporary under `digest`. `true` when this call
    /// published it, `false` when the object already existed (the temporary
    /// is removed either way).
    fn publish(&self, tmp: &Path, digest: &Blake3) -> Result<bool> {
        let target = self.object_path(digest);
        let _gate = self.publish_gate.lock();
        if fs::symlink_metadata(&target).is_ok() {
            let _ = remove_file_force(tmp);
            return Ok(false);
        }
        if let Some(parent) = target.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                let _ = remove_file_force(tmp);
                return Err(DriveError::io(err, parent));
            }
        }
        if let Err(err) = fs::rename(tmp, &target) {
            let _ = remove_file_force(tmp);
            return Err(DriveError::io(err, &target));
        }
        // Best effort: an object that cannot be made read-only is still the
        // right bytes, and `verify` is the scrub either way.
        let _ = set_readonly(&target);
        Ok(true)
    }

    /// Re-hash a held object: `false` when its bytes no longer match its name.
    pub(crate) fn verify(&self, digest: &Blake3) -> Result<bool> {
        let path = self.object_path(digest);
        let (actual, _) = Blake3::of_file(&path).map_err(io_err(&path))?;
        Ok(actual == *digest)
    }

    /// Every object on disk, in digest order. Files under `objects/` that are
    /// not named like objects are not the drive's and are left alone.
    pub(crate) fn list(&self) -> Result<Vec<(ObjectRef, PathBuf)>> {
        let mut out = Vec::new();
        let objects = &self.layout.objects;
        let shards = match fs::read_dir(objects) {
            Ok(shards) => shards,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(DriveError::io(err, objects)),
        };
        for shard in shards {
            let shard = shard.map_err(io_err(objects))?;
            let shard_name = shard.file_name().to_string_lossy().into_owned();
            let shard_path = shard.path();
            if shard_name.len() != 2 || !shard_path.is_dir() {
                continue;
            }
            let files = fs::read_dir(&shard_path).map_err(io_err(&shard_path))?;
            for file in files {
                let file = file.map_err(io_err(&shard_path))?;
                let name = file.file_name().to_string_lossy().into_owned();
                let Ok(digest) = name.parse::<Blake3>() else {
                    continue;
                };
                let path = file.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if name.starts_with(&shard_name) && meta.is_file() {
                    let object = ObjectRef {
                        blake3: digest,
                        bytes: meta.len(),
                    };
                    out.push((object, path));
                }
            }
        }
        out.sort_by_key(|a| a.0.blake3);
        Ok(out)
    }

    /// Remove every object `retained` does not name. A dry run removes
    /// nothing and reports what it would. Kept objects are made read-only
    /// again, which repairs an attribute a Windows unlink of a sibling link
    /// cleared.
    pub(crate) fn gc(&self, retained: &BTreeSet<Blake3>, dry_run: bool) -> Result<GcReport> {
        let mut report = GcReport {
            dry_run,
            ..GcReport::default()
        };
        for (object, path) in self.list()? {
            if retained.contains(&object.blake3) {
                report.retained += 1;
                let _ = set_readonly(&path);
                continue;
            }
            if !dry_run {
                remove_file_force(&path).map_err(io_err(&path))?;
            }
            report.bytes_freed += object.bytes;
            report.removed.push(object);
        }
        Ok(report)
    }
}

/// Write `data` to a new file and flush it to disk.
fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn store(max: u64) -> (tempfile::TempDir, ObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(&dir.path().join("drive"), None);
        fs::create_dir_all(&layout.objects).unwrap();
        fs::create_dir_all(&layout.staging).unwrap();
        (dir, ObjectStore::new(layout, max))
    }

    fn source(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, data).unwrap();
        path
    }

    fn staging_is_empty(store: &ObjectStore) -> bool {
        let mut entries = fs::read_dir(&store.layout.staging).unwrap();
        entries.next().is_none()
    }

    #[test]
    fn a_copy_is_held_read_only_under_its_digest() {
        let (dir, store) = store(1 << 20);
        let src = source(&dir, "a.txt", b"alpha");
        let outcome = store.put_file(&src, PutMode::Copy, None).unwrap();
        assert_eq!(outcome.how, PutHow::Copied);
        assert_eq!(outcome.object.blake3, Blake3::of(b"alpha"));
        assert_eq!(outcome.object.bytes, 5);
        let held = store.object_path(&outcome.object.blake3);
        assert_eq!(fs::read(&held).unwrap(), b"alpha");
        assert!(fs::metadata(&held).unwrap().permissions().readonly());
        assert!(store.has(&outcome.object.blake3));
        assert!(staging_is_empty(&store));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_shares_the_source_inode() {
        use std::os::unix::fs::MetadataExt;

        let (dir, store) = store(1 << 20);
        let src = source(&dir, "b.txt", b"bravo");
        let outcome = store.put_file(&src, PutMode::Link, None).unwrap();
        assert_eq!(outcome.how, PutHow::Linked);
        let held_path = store.object_path(&outcome.object.blake3);
        let held = fs::metadata(held_path).unwrap();
        let original = fs::metadata(&src).unwrap();
        assert_eq!(held.ino(), original.ino());
        assert_eq!(held.nlink(), 2);
        assert!(staging_is_empty(&store));
    }

    #[test]
    fn a_second_put_of_the_same_bytes_existed() {
        let (dir, store) = store(1 << 20);
        let one = source(&dir, "one", b"same");
        let two = source(&dir, "two", b"same");
        store.put_file(&one, PutMode::LinkOrCopy, None).unwrap();
        let again = store.put_file(&two, PutMode::Copy, None).unwrap();
        assert_eq!(again.how, PutHow::Existed);
        let linked = store.put_file(&two, PutMode::Link, None).unwrap();
        assert_eq!(linked.how, PutHow::Existed);
        let bytes = store.put_bytes(b"same").unwrap();
        assert_eq!(bytes.how, PutHow::Existed);
        assert!(staging_is_empty(&store));
    }

    #[test]
    fn a_put_that_misses_its_expected_digest_leaves_nothing() {
        let (dir, store) = store(1 << 20);
        let src = source(&dir, "c", b"charlie");
        let wrong = Blake3::of(b"not charlie");
        for mode in [PutMode::Copy, PutMode::Link, PutMode::LinkOrCopy] {
            let err = store.put_file(&src, mode, Some(&wrong)).unwrap_err();
            assert_eq!(err.code(), DriveErrorCode::Mismatch, "{mode:?}");
        }
        assert!(!store.has(&Blake3::of(b"charlie")));
        assert!(staging_is_empty(&store));
    }

    #[test]
    fn an_object_over_the_limit_is_etoobig() {
        let (dir, store) = store(4);
        let src = source(&dir, "big", b"too big");
        let err = store.put_file(&src, PutMode::Copy, None).unwrap_err();
        assert_eq!(err.code(), DriveErrorCode::TooBig);
        let err = store.put_bytes(b"12345").unwrap_err();
        assert_eq!(err.code(), DriveErrorCode::TooBig);
        assert!(staging_is_empty(&store));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_put() {
        let (dir, store) = store(1 << 20);
        let target = source(&dir, "real", b"r");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(target, &link).unwrap();
        let mode = PutMode::LinkOrCopy;
        let err = store.put_file(&link, mode, None).unwrap_err();
        assert_eq!(err.code(), DriveErrorCode::Invalid);
    }

    #[cfg(unix)]
    #[test]
    fn another_file_of_the_same_length_and_time_is_not_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let one = source(&dir, "one", b"same size");
        let two = source(&dir, "two", b"SAME SIZE");
        let time = fs::metadata(&one).unwrap().modified().unwrap();
        let file = OpenOptions::new().write(true).open(&two).unwrap();
        file.set_modified(time).unwrap();
        drop(file);

        let a = fs::symlink_metadata(&one).unwrap();
        let b = fs::symlink_metadata(&two).unwrap();
        assert_eq!(a.len(), b.len());
        assert_eq!(a.modified().unwrap(), b.modified().unwrap());
        assert!(
            !same_file(&a, &b),
            "a swapped-in file must not pass for the hashed one"
        );
        assert!(same_file(&a, &fs::symlink_metadata(&one).unwrap()));
    }

    #[test]
    fn admit_refuses_a_corrupted_copy_and_consumes_it() {
        let (dir, store) = store(1 << 20);
        let expected = Blake3::of(b"delta");
        let staged = source(&dir, "landed", b"delta, but damaged");
        let err = store.admit(&expected, None, &staged).unwrap_err();
        assert_eq!(err.code(), DriveErrorCode::Mismatch);
        assert!(!staged.exists());
        assert!(!store.has(&expected));
        assert!(staging_is_empty(&store));

        let staged = source(&dir, "landed", b"delta");
        let wrong_size = store.admit(&expected, Some(99), &staged).unwrap_err();
        assert_eq!(wrong_size.code(), DriveErrorCode::Mismatch);

        let staged = source(&dir, "landed", b"delta");
        let admitted = store.admit(&expected, Some(5), &staged).unwrap();
        assert_eq!(admitted.object.blake3, expected);
        assert!(store.has(&expected));
        assert!(!staged.exists());
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn verify_detects_a_tampered_object() {
        let (dir, store) = store(1 << 20);
        let src = source(&dir, "e", b"echo");
        let put = store.put_file(&src, PutMode::Copy, None).unwrap();
        let digest = put.object.blake3;
        assert!(store.verify(&digest).unwrap());
        let held = store.object_path(&digest);
        let mut perms = fs::metadata(&held).unwrap().permissions();
        perms.set_readonly(false);
        fs::set_permissions(&held, perms).unwrap();
        fs::write(&held, b"tampered").unwrap();
        assert!(!store.verify(&digest).unwrap());
        let missing = store.verify(&Blake3::of(b"never")).unwrap_err();
        assert_eq!(missing.code(), DriveErrorCode::NotFound);
    }

    #[test]
    fn gc_keeps_what_is_retained_and_a_dry_run_removes_nothing() {
        let (_dir, store) = store(1 << 20);
        let keep = store.put_bytes(b"keep").unwrap().object;
        let gone = store.put_bytes(b"gone").unwrap().object;
        let retained: BTreeSet<Blake3> = [keep.blake3].into_iter().collect();

        let dry = store.gc(&retained, true).unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.removed, vec![gone]);
        assert_eq!(dry.retained, 1);
        assert!(store.has(&gone.blake3));

        let real = store.gc(&retained, false).unwrap();
        assert_eq!(real.removed, vec![gone]);
        assert_eq!(real.bytes_freed, 4);
        assert!(!store.has(&gone.blake3));
        assert!(store.has(&keep.blake3));
    }

    #[test]
    fn two_writers_of_one_object_publish_it_once() {
        let (dir, store) = store(1 << 20);
        let store = Arc::new(store);
        let data = vec![7u8; 256 * 1024];
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for i in 0..8 {
            let src = source(&dir, &format!("w{i}"), &data);
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store.put_file(&src, PutMode::Copy, None).unwrap().how
            }));
        }
        let hows: Vec<PutHow> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let copied = hows.iter().filter(|how| **how == PutHow::Copied).count();
        let existed = hows.iter().filter(|how| **how == PutHow::Existed).count();
        assert_eq!((copied, existed), (1, 7), "{hows:?}");
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(staging_is_empty(&store));
    }
}
