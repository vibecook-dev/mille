//! Two (and three) drives in one process over `MemoryNetwork`: the DR record's
//! M-D1 witnesses — a version made on A is in B's document and B holds its
//! objects; Keep on B is kept on A; a mismatched object is refused — plus
//! catch-up after a partition, convergence of concurrent `current` writes,
//! gossip through a middle drive, and an update cut into chunks.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mille_drive::{
    Blake3, Drive, DriveErrorCode, DriveOptions, FileEntry, MemoryNetwork, PutMode,
    MIN_FRAME_BYTES,
};

struct Net {
    _dir: tempfile::TempDir,
    scratch: PathBuf,
    net: MemoryNetwork,
    drives: Vec<Arc<Drive>>,
}

impl Net {
    fn drive(&self, i: usize) -> &Drive {
        &self.drives[i]
    }
}

/// `names.len()` drives, registered and not yet connected.
fn network(names: &[&str], options: impl Fn(&str) -> DriveOptions) -> Net {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let net = MemoryNetwork::new();
    let mut drives = Vec::new();
    for name in names {
        let root = dir.path().join(name);
        let drive = Arc::new(Drive::open(&root, options(name)).unwrap());
        net.register(name, Arc::clone(&drive));
        drives.push(drive);
    }
    Net {
        _dir: dir,
        scratch,
        net,
        drives,
    }
}

fn defaults(name: &str) -> DriveOptions {
    DriveOptions::new(format!("desk-{name}"))
}

/// Two drives, `a` and `b`, connected and quiet.
fn pair() -> Net {
    let net = network(&["a", "b"], defaults);
    net.net.connect("a", "b");
    net.net.run_until_quiet();
    net
}

/// Put each file into `drive` and record them as a new current version.
fn publish(drive: &Drive, scratch: &Path, item: &str, files: &[(&str, &str)]) -> String {
    let mut entries = Vec::new();
    for (path, data) in files {
        let src = scratch.join(format!("{item}-{}", entries.len()));
        fs::write(&src, data).unwrap();
        let put = drive.put_file(&src, PutMode::Copy, None).unwrap();
        entries.push(FileEntry {
            path: (*path).to_owned(),
            blake3: put.object.blake3,
            bytes: put.object.bytes,
        });
    }
    drive.add_version(item, entries, true).unwrap()
}

fn digest_of(data: &str) -> Blake3 {
    Blake3::of(data.as_bytes())
}

#[test]
fn a_version_made_on_a_is_in_bs_document_and_b_holds_its_objects() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    let files = [("index.html", "<p>hi</p>"), ("js/app.js", "go()")];
    let version = publish(a, &net.scratch, "app", &files);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty());

    let item = b.item("app").expect("B has the item");
    assert_eq!(item.current.as_deref(), Some(version.as_str()));
    assert_eq!(item.versions.len(), 1);
    assert!(item.versions[0].held, "B holds every object of A's version");
    assert_eq!(item.versions[0].device, "desk-a");
    assert!(b.wants().is_empty());
    for (_, data) in files {
        let path = b.object_path(&digest_of(data)).expect("held on B");
        assert_eq!(fs::read_to_string(path).unwrap(), data);
    }
    let built = b.build("app", &version).unwrap();
    let app_js = built.join("js").join("app.js");
    assert_eq!(fs::read_to_string(app_js).unwrap(), "go()");
}

#[test]
fn keep_on_b_is_kept_on_a() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    publish(a, &net.scratch, "app", &[("index.html", "v1")]);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(!a.item("app").unwrap().kept);

    b.set_kept("app", true).unwrap();
    net.net.local_changed("b");
    net.net.run_until_quiet();
    assert!(a.item("app").unwrap().kept);

    a.set_hidden("app", true).unwrap();
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(b.item("app").unwrap().hidden);
    assert!(net.net.take_errors().is_empty());
}

#[test]
fn a_mismatched_object_is_refused_still_wanted_and_a_retry_succeeds() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    let damaged = digest_of("the bytes that arrive damaged");
    net.net.corrupt_next_pull(damaged);
    let files = [("a.txt", "the bytes that arrive damaged")];
    publish(a, &net.scratch, "app", &files);
    net.net.local_changed("a");
    net.net.run_until_quiet();

    let errors = net.net.take_errors();
    let refused: Vec<_> = errors
        .iter()
        .filter(|(peer, err)| peer == "b" && err.code() == DriveErrorCode::Mismatch)
        .collect();
    assert_eq!(refused.len(), 1, "{errors:?}");
    assert!(!b.has(&damaged));
    assert_eq!(b.wants(), vec![damaged]);
    assert!(!b.item("app").unwrap().versions[0].held);

    // Every holder failed it once; the tick starts over.
    net.net.tick("b");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty());
    assert!(b.has(&damaged));
    assert!(b.verify(&damaged).unwrap());
    assert!(b.wants().is_empty());
}

#[test]
fn b_catches_up_after_a_partition_by_version_vector() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    net.net.partition("a", "b", true);
    publish(a, &net.scratch, "one", &[("index.html", "first")]);
    net.net.local_changed("a");
    publish(a, &net.scratch, "two", &[("index.html", "second")]);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(b.items().is_empty());

    net.net.partition("a", "b", false);
    net.net.tick("b");
    net.net.run_until_quiet();
    let ids: Vec<String> = b.items().into_iter().map(|item| item.id).collect();
    assert_eq!(ids, vec!["one".to_owned(), "two".to_owned()]);
    assert!(b.wants().is_empty());
    assert!(net.net.take_errors().is_empty());
}

#[test]
fn concurrent_current_writes_converge() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    let v1 = publish(a, &net.scratch, "app", &[("index.html", "one")]);
    let v2 = publish(a, &net.scratch, "app", &[("index.html", "two")]);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert_eq!(b.item("app").unwrap().versions.len(), 2);

    net.net.partition("a", "b", true);
    a.set_current("app", &v1).unwrap();
    b.set_current("app", &v2).unwrap();
    net.net.local_changed("a");
    net.net.local_changed("b");
    net.net.partition("a", "b", false);
    net.net.tick("a");
    net.net.tick("b");
    net.net.run_until_quiet();

    let on_a = a.item("app").unwrap().current;
    let on_b = b.item("app").unwrap().current;
    assert_eq!(on_a, on_b);
    assert!(on_a == Some(v1) || on_a == Some(v2));
}

#[test]
fn a_change_reaches_a_drive_through_a_middle_one() {
    let net = network(&["a", "b", "c"], defaults);
    net.net.connect("a", "b");
    net.net.connect("b", "c");
    net.net.run_until_quiet();
    let files = [("index.html", "relay")];
    let version = publish(net.drive(0), &net.scratch, "app", &files);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty());

    let c = net.drive(2);
    let item = c.item("app").expect("C heard of the version through B");
    assert_eq!(item.current.as_deref(), Some(version.as_str()));
    assert!(c.has(&digest_of("relay")), "C pulled the object from B");
}

#[test]
fn an_update_larger_than_a_frame_arrives_in_chunks() {
    let small = |name: &str| {
        let mut options = defaults(name);
        options.max_frame_bytes = MIN_FRAME_BYTES;
        options
    };
    let net = network(&["a", "b"], small);
    net.net.connect("a", "b");
    net.net.run_until_quiet();
    let mut owned = Vec::new();
    for i in 0..150 {
        let name = format!("pages/section-{}/page-{i}.html", i / 10);
        owned.push((name, format!("file {i}")));
    }
    let files: Vec<(&str, &str)> = owned
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    let version = publish(net.drive(0), &net.scratch, "site", &files);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty());

    let b = net.drive(1);
    let item = b.item("site").expect("the chunked update was reassembled");
    assert_eq!(item.current.as_deref(), Some(version.as_str()));
    assert_eq!(item.versions[0].files.len(), 150);
    assert!(b.wants().is_empty());
}
