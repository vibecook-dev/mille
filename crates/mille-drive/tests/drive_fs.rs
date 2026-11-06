//! One drive on a real file system: the root's lock, a host's staging folder,
//! persistence (and a change that cannot be saved), the rules `add_version`
//! holds, builds by hard link, host views and where they may not go, GC by
//! references, the export, a forged version record, and change events.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mille_drive::{
    Blake3, BuildIntoOptions, ChangeKind, Drive, DriveChange, DriveErrorCode, DriveOptions,
    FileEntry, GcOptions, PutMode, VersionKey,
};

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("drive");
    Fixture { dir, root }
}

fn open(root: &Path) -> Drive {
    Drive::open(root, DriveOptions::new("desk")).unwrap()
}

fn entry(drive: &Drive, scratch: &Path, path: &str, data: &str) -> FileEntry {
    let src = scratch.join(format!("src-{}", Blake3::of(path.as_bytes())));
    fs::write(&src, data).unwrap();
    let put = drive.put_file(&src, PutMode::Copy, None).unwrap();
    FileEntry {
        path: path.to_owned(),
        blake3: put.object.blake3,
        bytes: put.object.bytes,
    }
}

fn version(drive: &Drive, scratch: &Path, item: &str, files: &[(&str, &str)]) -> String {
    let entries = files
        .iter()
        .map(|(path, data)| entry(drive, scratch, path, data))
        .collect();
    drive.add_version(item, entries, true).unwrap()
}

#[allow(clippy::permissions_set_readonly_false)]
fn delete_object(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_readonly(false);
    fs::set_permissions(path, perms).unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn a_second_open_of_a_root_is_ebusy() {
    let f = fixture();
    let first = open(&f.root);
    let err = Drive::open(&f.root, DriveOptions::new("desk"))
        .err()
        .unwrap();
    assert_eq!(err.code(), DriveErrorCode::Busy);
    drop(first);
    open(&f.root);
}

#[test]
fn bad_options_are_refused() {
    let f = fixture();
    let mut options = DriveOptions::new("");
    assert!(Drive::open(&f.root, options.clone()).is_err());
    options.device = "desk".to_owned();
    options.max_frame_bytes = 100;
    let err = Drive::open(&f.root, options).err().unwrap();
    assert_eq!(err.code(), DriveErrorCode::Invalid);
}

#[test]
fn the_document_survives_a_reopen() {
    let f = fixture();
    let scratch = f.dir.path();
    let drive = open(&f.root);
    let id = version(&drive, scratch, "app", &[("index.html", "<p>")]);
    drive.set_kept("app", true).unwrap();
    drive.set_meta("app", "title", Some("An app")).unwrap();
    let before = drive.items();
    drop(drive);

    let drive = open(&f.root);
    assert_eq!(drive.items(), before);
    let item = drive.item("app").unwrap();
    assert_eq!(item.current.as_deref(), Some(id.as_str()));
    assert!(item.kept);
    assert_eq!(item.meta.get("title").map(String::as_str), Some("An app"));
}

#[test]
fn add_version_needs_every_object_held_at_its_size() {
    let f = fixture();
    let drive = open(&f.root);
    let held = entry(&drive, f.dir.path(), "a.txt", "alpha");

    let missing = FileEntry {
        path: "b.txt".to_owned(),
        blake3: Blake3::of(b"never put"),
        bytes: 9,
    };
    let files = vec![held.clone(), missing];
    let err = drive.add_version("app", files, true).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::NotFound);

    let wrong_size = FileEntry {
        bytes: 99,
        ..held.clone()
    };
    let err = drive
        .add_version("app", vec![wrong_size], true)
        .unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Mismatch);

    let files = vec![held.clone()];
    let err = drive.add_version("Not:An/Id", files, true).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Invalid);
    assert!(drive.items().is_empty());

    let files = vec![held.clone()];
    let id = drive.add_version("app", files, false).unwrap();
    assert_eq!(drive.item("app").unwrap().current, None);
    // The same tree again is the same version.
    assert_eq!(drive.add_version("app", vec![held], true).unwrap(), id);
    assert_eq!(drive.item("app").unwrap().versions.len(), 1);
}

#[test]
fn intents_name_known_items_and_versions() {
    let f = fixture();
    let drive = open(&f.root);
    let unknown = "0".repeat(64);
    let err = drive.set_kept("nothing", true).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::UnknownItem);
    version(&drive, f.dir.path(), "app", &[("index.html", "x")]);
    let err = drive.set_current("app", &unknown).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::UnknownVersion);
    let err = drive.set_current("app", "not-hex").unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Invalid);
    // Meta may name an item before its first version.
    drive
        .set_meta("later", "source", Some("a fold key"))
        .unwrap();
}

#[test]
fn a_build_is_hard_links_and_idempotent() {
    let f = fixture();
    let drive = open(&f.root);
    let files = [("index.html", "<p>"), ("assets/js/app.js", "go()")];
    let id = version(&drive, f.dir.path(), "app", &files);

    let built = drive.build("app", &id).unwrap();
    assert_eq!(built, f.root.join("built").join("app").join(&id));
    let app_js = built.join("assets").join("js").join("app.js");
    assert_eq!(fs::read_to_string(app_js.as_path()).unwrap(), "go()");
    assert_eq!(drive.build("app", &id).unwrap(), built);

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let object = drive.object_path(&Blake3::of(b"go()")).unwrap();
        let built_ino = fs::metadata(&app_js).unwrap().ino();
        assert_eq!(built_ino, fs::metadata(object).unwrap().ino());
    }

    drive.unbuild("app", &id).unwrap();
    assert!(!built.exists());
    let object = drive.object_path(&Blake3::of(b"go()")).unwrap();
    assert!(fs::metadata(object).unwrap().permissions().readonly());
}

#[test]
fn a_build_missing_an_object_is_enoent_and_leaves_nothing() {
    let f = fixture();
    let drive = open(&f.root);
    let id = version(&drive, f.dir.path(), "app", &[("a", "1"), ("b", "2")]);
    delete_object(&drive.object_path(&Blake3::of(b"2")).unwrap());

    let err = drive.build("app", &id).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::NotFound);
    let item_dir = f.root.join("built").join("app");
    let leftovers: Vec<_> = fs::read_dir(item_dir).unwrap().collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_eq!(drive.wants(), vec![Blake3::of(b"2")]);
}

#[test]
fn build_into_makes_a_host_view_with_exceptions() {
    let f = fixture();
    let drive = open(&f.root);
    let files = [("index.html", "<p>"), ("app.js", "go()")];
    let id = version(&drive, f.dir.path(), "app", &files);
    let view = f.dir.path().join("view");
    let options = BuildIntoOptions {
        mode: PutMode::Copy,
        except: vec!["index.html".to_owned()],
        replace: false,
    };
    drive.build_into("app", &id, &view, &options).unwrap();
    assert!(!view.join("index.html").exists());
    assert_eq!(fs::read_to_string(view.join("app.js")).unwrap(), "go()");

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let object = drive.object_path(&Blake3::of(b"go()")).unwrap();
        let view_ino = fs::metadata(view.join("app.js")).unwrap().ino();
        assert_ne!(view_ino, fs::metadata(object).unwrap().ino());
    }

    let err = drive.build_into("app", &id, &view, &options).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Exists);
    let replace = BuildIntoOptions {
        replace: true,
        ..BuildIntoOptions::default()
    };
    drive.build_into("app", &id, &view, &replace).unwrap();
    assert_eq!(fs::read_to_string(view.join("index.html")).unwrap(), "<p>");

    let inside = f.root.join("mine");
    let err = drive.build_into("app", &id, &inside, &replace).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Invalid);
}

#[test]
fn build_into_never_replaces_the_drive_or_a_folder_above_it() {
    let f = fixture();
    let drive = open(&f.root);
    let id = version(&drive, f.dir.path(), "app", &[("index.html", "<p>")]);
    let replace = BuildIntoOptions {
        replace: true,
        ..BuildIntoOptions::default()
    };
    let above = f.dir.path().to_path_buf();
    for dest in [f.root.clone(), above, f.root.join("..")] {
        let err = drive.build_into("app", &id, &dest, &replace).unwrap_err();
        assert_eq!(err.code(), DriveErrorCode::Invalid, "{}", dest.display());
    }
    assert!(f.root.join("drive.loro").is_file());
    assert!(drive.verify(&Blake3::of(b"<p>")).unwrap());
    assert_eq!(drive.items().len(), 1);
}

#[test]
fn gc_keeps_held_built_and_kept_versions_and_drops_the_rest() {
    let f = fixture();
    let scratch = f.dir.path();
    let drive = open(&f.root);
    let old = version(&drive, scratch, "app", &[("index.html", "old")]);
    version(&drive, scratch, "app", &[("index.html", "new")]);
    let stray = drive.put_bytes(b"stray").unwrap().object.blake3;

    let dry = drive.gc(&GcOptions {
        dry_run: true,
        ..GcOptions::default()
    });
    let dry = dry.unwrap();
    let removed: Vec<Blake3> = dry.removed.iter().map(|o| o.blake3).collect();
    assert_eq!(removed.len(), 2);
    assert!(removed.contains(&stray) && removed.contains(&Blake3::of(b"old")));
    assert!(drive.has(&stray));

    // A built version is held by its build.
    drive.build("app", &old).unwrap();
    let report = drive.gc(&GcOptions::default()).unwrap();
    let removed: Vec<Blake3> = report.removed.iter().map(|o| o.blake3).collect();
    assert_eq!(removed, vec![stray]);
    assert!(drive.has(&Blake3::of(b"old")));
    assert!(drive.has(&Blake3::of(b"new")));

    // A host's own view is held when it says so.
    drive.unbuild("app", &old).unwrap();
    let keep = GcOptions {
        dry_run: false,
        keep: vec![VersionKey {
            item: "app".to_owned(),
            version: old.clone(),
        }],
    };
    assert!(drive.gc(&keep).unwrap().removed.is_empty());

    // Kept, with every version held, holds the old one too.
    drop(drive);
    let mut options = DriveOptions::new("desk");
    options.hold.kept_versions = mille_drive::KeptScope::All;
    let drive = Drive::open(&f.root, options).unwrap();
    drive.set_kept("app", true).unwrap();
    assert!(drive.gc(&GcOptions::default()).unwrap().removed.is_empty());
    assert!(drive.has(&Blake3::of(b"old")));
}

#[test]
fn the_export_is_built_read_only_with_no_symlinks() {
    let f = fixture();
    let drive = open(&f.root);
    let export = drive.export_descriptor();
    assert_eq!(export.kind, "drive");
    assert_eq!(export.access, "read-only");
    assert!(!export.follow_symlinks);
    let built = fs::canonicalize(f.root.join("built")).unwrap();
    assert_eq!(export.roots, vec![built]);
    let json = serde_json::to_value(&export).unwrap();
    assert_eq!(json["followSymlinks"], serde_json::json!(false));
}

#[test]
fn a_forged_version_record_is_quarantined_not_wanted_or_built() {
    let f = fixture();
    let drive = open(&f.root);
    let claimed = "0".repeat(64);
    let record = serde_json::json!({
        "files": [{ "path": "index.html", "blake3": Blake3::of(b"x").to_hex(), "bytes": 1 }],
        "createdAt": 1,
        "device": "elsewhere",
    });
    let forged = loro::LoroDoc::new();
    let key = format!("app/{claimed}");
    let versions = forged.get_map("versions");
    versions.insert(&key, record.to_string()).unwrap();
    let current = forged.get_map("current");
    current.insert("app", claimed.clone()).unwrap();
    forged.commit();
    let update = forged.export(loro::ExportMode::all_updates()).unwrap();

    let report = drive.import_updates(&update).unwrap();
    let expected = VersionKey {
        item: "app".to_owned(),
        version: claimed.clone(),
    };
    assert_eq!(report.quarantined, vec![expected]);
    assert!(report.new_versions.is_empty());
    let item = drive.item("app").unwrap();
    assert!(item.versions[0].quarantined);
    assert!(drive.wants().is_empty());
    let err = drive.build("app", &claimed).unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Mismatch);
}

#[test]
fn garbage_updates_are_refused_and_the_drive_keeps_working() {
    let f = fixture();
    let drive = open(&f.root);
    version(&drive, f.dir.path(), "app", &[("index.html", "x")]);
    let err = drive.import_updates(b"definitely not loro").unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Format);
    assert_eq!(drive.items().len(), 1);
    version(&drive, f.dir.path(), "two", &[("index.html", "y")]);
    assert_eq!(drive.items().len(), 2);
}

#[test]
fn local_writes_tell_the_listeners() {
    let f = fixture();
    let drive = open(&f.root);
    let seen: Arc<Mutex<Vec<DriveChange>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let id = drive.subscribe(Arc::new(move |change: &DriveChange| {
        sink.lock().unwrap().push(change.clone());
    }));
    version(&drive, f.dir.path(), "app", &[("index.html", "x")]);
    drive.set_kept("app", true).unwrap();
    assert!(drive.unsubscribe(id));
    drive.set_kept("app", false).unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|change| change.kind == ChangeKind::Local));
    assert!(seen
        .iter()
        .all(|change| change.items == vec!["app".to_owned()]));
}

#[cfg(unix)]
#[test]
fn a_change_that_cannot_be_saved_is_not_kept_or_heard() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let drive = open(&f.root);
    version(&drive, f.dir.path(), "app", &[("index.html", "x")]);
    let heard: Arc<Mutex<Vec<DriveChange>>> = Arc::default();
    let sink = Arc::clone(&heard);
    drive.subscribe(Arc::new(move |change: &DriveChange| {
        sink.lock().unwrap().push(change.clone());
    }));

    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o555)).unwrap();
    let writable = fs::write(f.root.join("probe"), b"").is_ok();
    let refused = drive.set_kept("app", true);
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        // Run as root, the mode refuses nothing: no save can fail here.
        return;
    }

    let err = refused.unwrap_err();
    assert_eq!(err.code(), DriveErrorCode::Io, "{err}");
    assert!(
        !drive.item("app").unwrap().kept,
        "the refused Keep is not in memory"
    );
    assert!(heard.lock().unwrap().is_empty());

    drive.set_kept("app", true).unwrap();
    assert!(drive.item("app").unwrap().kept);
    drop(drive);
    assert!(open(&f.root).item("app").unwrap().kept);
}

#[test]
fn a_hosts_staging_folder_is_this_drives_alone() {
    let f = fixture();
    let staging = f.dir.path().join("host-staging");
    let mut options = DriveOptions::new("desk");
    options.staging_dir = Some(staging.clone());
    let drive = Drive::open(&f.root, options.clone()).unwrap();
    let id = version(&drive, f.dir.path(), "app", &[("index.html", "staged")]);
    drive.build("app", &id).unwrap();
    assert!(drive.has(&Blake3::of(b"staged")));
    assert!(!f.root.join("staging").exists());
    let left: Vec<String> = fs::read_dir(&staging)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec!["LOCK.mdrive".to_owned()]);

    // Another drive would sweep this one's temporaries at its open.
    let other = f.dir.path().join("other");
    let err = Drive::open(&other, options.clone()).err().unwrap();
    assert_eq!(err.code(), DriveErrorCode::Busy);
    drop(drive);
    Drive::open(&other, options).unwrap();
}

#[test]
fn the_staging_sweep_runs_at_open() {
    let f = fixture();
    drop(open(&f.root));
    let leftover = f.root.join("staging").join("mdrive-put-1-2-3");
    fs::write(&leftover, b"half a put").unwrap();
    drop(open(&f.root));
    assert!(!leftover.exists());
}
