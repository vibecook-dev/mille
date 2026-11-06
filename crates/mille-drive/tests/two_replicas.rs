//! Two (and three) drives in one process over `MemoryNetwork`: the DR record's
//! M-D1 witnesses — a version made on A is in B's document and B holds its
//! objects; Keep on B is kept on A; a mismatched object is refused — plus
//! attachments, back-off from a holder whose copy is always damaged, an object
//! over the receiver's limit, a pull landed outside staging, catch-up after a
//! partition, convergence of concurrent `current` writes, gossip through a
//! middle drive, and an update cut into chunks.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mille_drive::{
    Blake3, Drive, DriveErrorCode, DriveOptions, FileEntry, MemoryNetwork, PutMode, MIN_FRAME_BYTES,
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

/// Overwrite a held object in place, as rot on the holder's disk would.
#[allow(clippy::permissions_set_readonly_false)]
fn rot(path: &Path, data: &str) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_readonly(false);
    fs::set_permissions(path, perms).unwrap();
    fs::write(path, data).unwrap();
}

/// How many pulls on `peer` were refused as damaged since the last call.
fn refused_on(net: &Net, peer: &str) -> usize {
    let mut refused = 0;
    for (on, err) in net.net.take_errors() {
        if on == peer && err.code() == DriveErrorCode::Mismatch {
            refused += 1;
        }
    }
    refused
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

    // One tick of back-off, then the same holder is tried again.
    net.net.tick("b");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty());
    assert!(b.has(&damaged));
    assert!(b.verify(&damaged).unwrap());
    assert!(b.wants().is_empty());
}

#[test]
fn an_attachment_made_on_a_is_held_on_b() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    let version = publish(a, &net.scratch, "app", &[("index.html", "<p>")]);
    let preview = a.put_bytes(b"a preview's pixels").unwrap().object;
    a.set_attachment("app", &version, "preview", Some(preview))
        .unwrap();
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty());

    let item = b.item("app").unwrap();
    let attached = item.versions[0].attachments.get("preview");
    assert_eq!(attached, Some(&preview));
    assert!(
        b.has(&preview.blake3),
        "B holds what is attached to what it holds"
    );
    assert!(b.wants().is_empty());
}

#[test]
fn a_holder_whose_copy_is_always_damaged_is_tried_less_and_less_often() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    let data = "rots on the holder";
    publish(a, &net.scratch, "app", &[("a.txt", data)]);
    // A still holds the object by name and says Have; every copy is refused.
    rot(
        &a.object_path(&digest_of(data)).unwrap(),
        "ROTS ON THE HOLDER",
    );
    net.net.local_changed("a");
    net.net.run_until_quiet();

    let mut tried = Vec::new();
    for tick in 0..=8 {
        if tick > 0 {
            net.net.tick("b");
            net.net.run_until_quiet();
        }
        if refused_on(&net, "b") > 0 {
            tried.push(tick);
        }
    }
    // At once, then after 1, 2 and 4 ticks: never on every tick.
    assert_eq!(tried, vec![0, 1, 3, 7]);
    assert!(!b.has(&digest_of(data)));
    assert_eq!(b.wants(), vec![digest_of(data)]);
}

#[test]
fn an_object_over_the_receivers_limit_is_not_wanted_and_is_counted() {
    let limits = |name: &str| {
        let mut options = defaults(name);
        if name == "b" {
            options.max_object_bytes = 8;
        }
        options
    };
    let net = network(&["a", "b"], limits);
    net.net.connect("a", "b");
    net.net.run_until_quiet();
    let (a, b) = (net.drive(0), net.drive(1));
    let files = [("small.txt", "small"), ("big.txt", "more than eight bytes")];
    publish(a, &net.scratch, "app", &files);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty(), "no pull of it is tried");

    assert!(b.has(&digest_of("small")));
    assert!(!b.has(&digest_of("more than eight bytes")));
    assert!(b.wants().is_empty());
    let status = b.sync_status();
    assert_eq!((status.wants, status.oversized, status.pulling), (0, 1, 0));
    assert!(!b.item("app").unwrap().versions[0].held);
}

#[test]
fn a_pull_landed_outside_staging_is_admitted_from_there() {
    let net = pair();
    let (a, b) = (net.drive(0), net.drive(1));
    let incoming = net.scratch.join("incoming");
    net.net.land_pulls_in("b", incoming.clone());
    publish(
        a,
        &net.scratch,
        "app",
        &[("index.html", "landed elsewhere")],
    );
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert!(net.net.take_errors().is_empty());

    assert!(b.has(&digest_of("landed elsewhere")));
    assert!(b.verify(&digest_of("landed elsewhere")).unwrap());
    let left: Vec<_> = fs::read_dir(&incoming).unwrap().collect();
    assert!(
        left.is_empty(),
        "admitting a pull consumes the landed file: {left:?}"
    );
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
    let v3 = publish(a, &net.scratch, "app", &[("index.html", "three")]);
    net.net.local_changed("a");
    net.net.run_until_quiet();
    assert_eq!(b.item("app").unwrap().versions.len(), 3);
    assert_eq!(b.item("app").unwrap().current, Some(v3));

    // Each side moves `current` off v3 while they cannot talk, so each
    // records a write the other has not seen: a real conflict.
    net.net.partition("a", "b", true);
    a.set_current("app", &v1).unwrap();
    b.set_current("app", &v2).unwrap();
    assert_eq!(a.item("app").unwrap().current, Some(v1.clone()));
    assert_eq!(b.item("app").unwrap().current, Some(v2.clone()));
    net.net.local_changed("a");
    net.net.local_changed("b");
    net.net.partition("a", "b", false);
    net.net.tick("a");
    net.net.tick("b");
    net.net.run_until_quiet();

    let on_a = a.item("app").unwrap().current;
    let on_b = b.item("app").unwrap().current;
    assert_eq!(on_a, on_b);
    assert!(on_a == Some(v1) || on_a == Some(v2), "{on_a:?}");
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
