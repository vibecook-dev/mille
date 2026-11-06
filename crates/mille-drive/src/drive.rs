//! `Drive`: one drive root, open in one process at a time.
//!
//! The facade over the object store, the document, the builds and the sync
//! core. Every public call is synchronous and does its own file I/O; a host
//! with an event loop runs the heavy ones (put, build, gc, admitting a pull)
//! off it. Internally the document is one mutex and the sync state another;
//! when a call needs both it takes the document first. No listener is called
//! with either held, so a listener may call back into the drive.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use loro::VersionVector;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::build::materialize;
use crate::digest::Blake3;
use crate::doc::{DocState, DriveDoc, ImportFailure, VersionRecord, HIDDEN, KEPT};
use crate::error::{io_err, DriveError, DriveErrorCode, Result};
use crate::export::ExportDescriptor;
use crate::layout::{
    now_ms, remove_tree, set_readonly, sweep, temp_name, write_atomic, Layout, FORMAT,
};
use crate::lock::DriveLock;
use crate::store::{GcReport, ObjectRef, ObjectStore, PutMode, PutOutcome};
use crate::sync::SyncState;
use crate::version::{
    check_item_id, check_name, check_version_id, is_valid_item_id, is_valid_name,
    is_valid_version_id, validate_manifest, version_id, FileEntry, VersionId, MAX_META_BYTES,
};

/// The default object size limit. A host that moves objects over a transport
/// with its own limit passes that one value here too, so neither side admits
/// what the other cannot move.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
/// The default largest frame the sync core hands a transport.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// The smallest `max_frame_bytes` a drive accepts.
pub const MIN_FRAME_BYTES: usize = 4096;
/// The default largest document update a peer may send, across its chunks.
pub const DEFAULT_MAX_UPDATE_BYTES: usize = 256 * 1024 * 1024;
/// The default number of pulls in flight at once.
pub const DEFAULT_MAX_CONCURRENT_PULLS: usize = 4;

/// Which versions of a kept item a drive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptScope {
    /// Its current version only.
    Current,
    /// Every version.
    All,
}

/// What a drive holds eagerly (DR-D5 (a)): every item's current version, a
/// kept item's versions, and the attachments (previews) of what it holds.
/// Everything else is pulled on demand — not in this first cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldPolicy {
    pub current: bool,
    pub kept_versions: KeptScope,
    pub attachments: bool,
}

impl Default for HoldPolicy {
    fn default() -> Self {
        Self {
            current: true,
            kept_versions: KeptScope::Current,
            attachments: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DriveOptions {
    /// This desktop's name for itself, recorded on each version it makes.
    pub device: String,
    /// The export's label.
    pub label: String,
    pub hold: HoldPolicy,
    pub max_object_bytes: u64,
    /// The largest frame the sync core emits; a document update larger than
    /// this travels as chunks, reassembled before import.
    pub max_frame_bytes: usize,
    /// The largest update a peer may send in chunks.
    pub max_update_bytes: usize,
    pub max_concurrent_pulls: usize,
    /// Where temporaries go; `<root>/staging` by default. On the drive's
    /// volume, so a staged file becomes an object by a rename.
    pub staging_dir: Option<PathBuf>,
}

impl DriveOptions {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            label: "Drive".to_owned(),
            hold: HoldPolicy::default(),
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_update_bytes: DEFAULT_MAX_UPDATE_BYTES,
            max_concurrent_pulls: DEFAULT_MAX_CONCURRENT_PULLS,
            staging_dir: None,
        }
    }

    fn validate(&self) -> Result<()> {
        let problem = if self.device.is_empty() || self.device.len() > 256 {
            Some("device must be 1 to 256 bytes".to_owned())
        } else if self.max_object_bytes == 0 {
            Some("max_object_bytes must be positive".to_owned())
        } else if self.max_frame_bytes < MIN_FRAME_BYTES {
            Some(format!(
                "max_frame_bytes must be at least {MIN_FRAME_BYTES}"
            ))
        } else if self.max_update_bytes < self.max_frame_bytes {
            Some("max_update_bytes must be at least max_frame_bytes".to_owned())
        } else if self.max_concurrent_pulls == 0 {
            Some("max_concurrent_pulls must be positive".to_owned())
        } else {
            None
        };
        match problem {
            Some(message) => Err(DriveError::invalid(message)),
            None => Ok(()),
        }
    }
}

/// An item and one of its versions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VersionKey {
    pub item: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionView {
    pub id: VersionId,
    pub files: Vec<FileEntry>,
    pub created_at: u64,
    pub device: String,
    pub attachments: BTreeMap<String, ObjectRef>,
    /// The record does not hash to its id, or its manifest is unsafe. It is
    /// listed so a host can say so, and never built, held or served.
    pub quarantined: bool,
    /// Every file's object is held here.
    pub held: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemView {
    pub id: String,
    pub meta: BTreeMap<String, String>,
    pub current: Option<VersionId>,
    pub kept: bool,
    pub hidden: bool,
    /// Oldest first.
    pub versions: Vec<VersionView>,
}

impl ItemView {
    fn empty(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            meta: BTreeMap::new(),
            current: None,
            kept: false,
            hidden: false,
            versions: Vec::new(),
        }
    }

    fn version(&self, id: &str) -> Option<&VersionView> {
        self.versions.iter().find(|version| version.id == id)
    }

    fn current_version(&self) -> Option<&VersionView> {
        self.current.as_deref().and_then(|id| self.version(id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// A write through this drive.
    Local,
    /// An update from a peer.
    Import,
    /// An object arrived, so a version may now be held.
    Held,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveChange {
    pub kind: ChangeKind,
    pub items: Vec<String>,
}

pub type ChangeListener = Arc<dyn Fn(&DriveChange) + Send + Sync>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub changed: bool,
    pub items: Vec<String>,
    pub new_versions: Vec<VersionKey>,
    pub quarantined: Vec<VersionKey>,
    /// Everything the drive now wants and does not hold.
    pub wants: Vec<Blake3>,
}

/// An import, with the document's version vector either side of it.
pub(crate) struct Imported {
    pub(crate) report: ImportReport,
    pub(crate) before: VersionVector,
    pub(crate) after: VersionVector,
}

#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    pub dry_run: bool,
    /// Versions a host has built into views of its own (`build_into`), whose
    /// objects stay held even when the hold policy would let them go.
    pub keep: Vec<VersionKey>,
}

#[derive(Debug, Clone)]
pub struct BuildIntoOptions {
    pub mode: PutMode,
    /// Paths of the version to leave out, for the host to provide its own.
    pub except: Vec<String>,
    /// Replace `dest` when it exists (an atomic swap); else `EEXIST`.
    pub replace: bool,
}

impl Default for BuildIntoOptions {
    fn default() -> Self {
        Self {
            mode: PutMode::LinkOrCopy,
            except: Vec::new(),
            replace: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DriveMeta {
    format: u32,
    device: String,
}

pub struct Drive {
    pub(crate) layout: Layout,
    pub(crate) options: DriveOptions,
    pub(crate) store: ObjectStore,
    pub(crate) doc: Mutex<DriveDoc>,
    pub(crate) sync: Mutex<SyncState>,
    build_gate: Mutex<()>,
    listeners: Mutex<BTreeMap<u64, ChangeListener>>,
    next_listener: AtomicU64,
    export_root: PathBuf,
    // Last, so it is released after everything else is dropped.
    _lock: DriveLock,
}

fn write_meta(layout: &Layout, device: &str) -> Result<()> {
    match fs::read(&layout.meta) {
        Ok(bytes) => {
            let parsed: std::result::Result<DriveMeta, _> = serde_json::from_slice(&bytes);
            let Ok(meta) = parsed else {
                let message = format!("{} is not a drive's metadata", layout.meta.display());
                return Err(DriveError::format(message).with_path(&layout.meta));
            };
            if meta.format > FORMAT {
                let found = meta.format;
                let message = format!("this drive has format {found}; this build reads {FORMAT}");
                return Err(DriveError::format(message).with_path(&layout.meta));
            }
            if meta.format == FORMAT && meta.device == device {
                return Ok(());
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(DriveError::io(err, &layout.meta)),
    }
    let meta = DriveMeta {
        format: FORMAT,
        device: device.to_owned(),
    };
    let encoded = serde_json::to_vec_pretty(&meta);
    let json = encoded.map_err(|err| DriveError::format(err.to_string()))?;
    write_atomic(&layout.meta, &json).map_err(io_err(&layout.meta))
}

fn load_doc(layout: &Layout) -> Result<DriveDoc> {
    match fs::read(&layout.doc) {
        Ok(bytes) => {
            let loaded = DriveDoc::load(&bytes);
            loaded.map_err(|err| err.with_path(&layout.doc))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(DriveDoc::new()),
        Err(err) => Err(DriveError::io(err, &layout.doc)),
    }
}

fn item_entry<'a>(items: &'a mut BTreeMap<String, ItemView>, id: &str) -> &'a mut ItemView {
    items
        .entry(id.to_owned())
        .or_insert_with(|| ItemView::empty(id))
}

fn version_view(id: &str, json: &str, held: &impl Fn(&Blake3) -> bool) -> VersionView {
    let mut view = VersionView {
        id: id.to_owned(),
        files: Vec::new(),
        created_at: 0,
        device: String::new(),
        attachments: BTreeMap::new(),
        quarantined: true,
        held: false,
    };
    if let Ok(record) = serde_json::from_str::<VersionRecord>(json) {
        let valid = validate_manifest(&record.files).is_ok() && version_id(&record.files) == id;
        view.quarantined = !valid;
        view.held = valid && record.files.iter().all(|file| held(&file.blake3));
        view.files = record.files;
        view.created_at = record.created_at;
        view.device = record.device;
    }
    view
}

/// The document read as items. Keys that name no valid item or version are
/// not anyone's and are skipped; a record that does not match its key is kept
/// and quarantined.
pub(crate) fn catalog(
    state: &DocState,
    held: impl Fn(&Blake3) -> bool,
) -> BTreeMap<String, ItemView> {
    let mut items: BTreeMap<String, ItemView> = BTreeMap::new();
    for (key, json) in &state.versions {
        let Some((item, id)) = key.split_once('/') else {
            continue;
        };
        if is_valid_item_id(item) && is_valid_version_id(id) {
            let view = version_view(id, json, &held);
            item_entry(&mut items, item).versions.push(view);
        }
    }
    for (item, version) in &state.current {
        if is_valid_item_id(item) && is_valid_version_id(version) {
            item_entry(&mut items, item).current = Some(version.clone());
        }
    }
    for (item, kept) in &state.kept {
        if is_valid_item_id(item) {
            item_entry(&mut items, item).kept = *kept;
        }
    }
    for (item, hidden) in &state.hidden {
        if is_valid_item_id(item) {
            item_entry(&mut items, item).hidden = *hidden;
        }
    }
    for (key, value) in &state.meta {
        let Some((item, name)) = key.split_once('/') else {
            continue;
        };
        if is_valid_item_id(item) && is_valid_name(name) {
            let entry = item_entry(&mut items, item);
            entry.meta.insert(name.to_owned(), value.clone());
        }
    }
    for (key, json) in &state.attachments {
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        let [item, version, name] = parts.as_slice() else {
            continue;
        };
        let Ok(object) = serde_json::from_str::<ObjectRef>(json) else {
            continue;
        };
        if !is_valid_name(name) {
            continue;
        }
        let target = items
            .get_mut(*item)
            .and_then(|entry| entry.versions.iter_mut().find(|v| v.id == *version));
        if let Some(view) = target {
            view.attachments.insert((*name).to_owned(), object);
        }
    }
    for item in items.values_mut() {
        item.versions.sort_by_key(|v| (v.created_at, v.id.clone()));
    }
    items
}

/// A version's objects, unless it is quarantined.
fn version_objects(view: &VersionView, attachments: bool, out: &mut BTreeSet<Blake3>) {
    if view.quarantined {
        return;
    }
    out.extend(view.files.iter().map(|file| file.blake3));
    if attachments {
        out.extend(view.attachments.values().map(|object| object.blake3));
    }
}

/// What the hold policy says this drive holds.
pub(crate) fn retained_by_policy(
    items: &BTreeMap<String, ItemView>,
    hold: &HoldPolicy,
) -> BTreeSet<Blake3> {
    let mut out = BTreeSet::new();
    for item in items.values() {
        let current = item.current_version();
        if let (true, Some(version)) = (hold.current, current) {
            version_objects(version, hold.attachments, &mut out);
        }
        if !item.kept {
            continue;
        }
        match hold.kept_versions {
            KeptScope::Current => {
                if let Some(version) = current {
                    version_objects(version, hold.attachments, &mut out);
                }
            }
            KeptScope::All => {
                for version in &item.versions {
                    version_objects(version, hold.attachments, &mut out);
                }
            }
        }
    }
    out
}

/// Per item, a digest of everything the document says about it, so an
/// import can name the items it changed.
fn fingerprints(state: &DocState) -> BTreeMap<String, Blake3> {
    let mut hashers: BTreeMap<String, blake3::Hasher> = BTreeMap::new();
    let mut feed = |item: &str, tag: &str, key: &str, value: &str| {
        let hasher = hashers.entry(item.to_owned()).or_default();
        for part in [tag, key, value] {
            hasher.update(part.as_bytes());
            hasher.update(&[0u8]);
        }
    };
    for (key, value) in &state.versions {
        if let Some((item, _)) = key.split_once('/') {
            feed(item, "v", key, value);
        }
    }
    for (item, value) in &state.current {
        feed(item, "c", item, value);
    }
    for (item, kept) in &state.kept {
        feed(item, "k", item, if *kept { "1" } else { "0" });
    }
    for (item, hidden) in &state.hidden {
        feed(item, "h", item, if *hidden { "1" } else { "0" });
    }
    for (key, value) in &state.meta {
        if let Some((item, _)) = key.split_once('/') {
            feed(item, "m", key, value);
        }
    }
    for (key, value) in &state.attachments {
        if let Some((item, _)) = key.split_once('/') {
            feed(item, "a", key, value);
        }
    }
    let digests = hashers.iter().map(|(item, hasher)| {
        let digest = Blake3::from_hasher(hasher);
        (item.clone(), digest)
    });
    digests.collect()
}

impl Drive {
    /// Open (or create) the drive at `root`. `EBUSY` when another `Drive`
    /// holds it; `EFORMAT` when its metadata or document does not load.
    pub fn open(root: &Path, options: DriveOptions) -> Result<Drive> {
        options.validate()?;
        fs::create_dir_all(root).map_err(io_err(root))?;
        let layout = Layout::new(root, options.staging_dir.as_deref());
        let lock = DriveLock::acquire(&layout.lock)?;
        write_meta(&layout, &options.device)?;
        for dir in [&layout.objects, &layout.built, &layout.staging] {
            fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        sweep(&layout).map_err(io_err(&layout.root))?;
        let doc = load_doc(&layout)?;
        let built = &layout.built;
        let export_root = fs::canonicalize(built).map_err(io_err(built))?;
        let store = ObjectStore::new(layout.clone(), options.max_object_bytes);
        Ok(Drive {
            layout,
            options,
            store,
            doc: Mutex::new(doc),
            sync: Mutex::new(SyncState::default()),
            build_gate: Mutex::new(()),
            listeners: Mutex::new(BTreeMap::new()),
            next_listener: AtomicU64::new(0),
            export_root,
            _lock: lock,
        })
    }

    pub fn root(&self) -> &Path {
        &self.layout.root
    }

    /// The one folder a host may register as a pull root: objects only, never
    /// the document or staging.
    pub fn objects_root(&self) -> &Path {
        &self.layout.objects
    }

    pub fn options(&self) -> &DriveOptions {
        &self.options
    }

    // ── objects (DR-L2, DR-L3, DR-L5) ────────────────────────────────────

    pub fn has(&self, digest: &Blake3) -> bool {
        self.store.has(digest)
    }

    /// The held object's path, or `None` when this drive does not hold it.
    pub fn object_path(&self, digest: &Blake3) -> Option<PathBuf> {
        if self.store.has(digest) {
            Some(self.store.object_path(digest))
        } else {
            None
        }
    }

    /// Bring a file in as an object. With `PutMode::Link` the object shares
    /// the source's inode, and making it read-only makes the source read-only
    /// too.
    pub fn put_file(
        &self,
        src: &Path,
        mode: PutMode,
        expect: Option<&Blake3>,
    ) -> Result<PutOutcome> {
        self.store.put_file(src, mode, expect)
    }

    pub fn put_bytes(&self, data: &[u8]) -> Result<PutOutcome> {
        self.store.put_bytes(data)
    }

    /// Re-hash a held object. `ENOENT` when it is not held.
    pub fn verify(&self, digest: &Blake3) -> Result<bool> {
        self.store.verify(digest)
    }

    /// Remove every object nothing holds: not the hold policy, not a version
    /// built under `built/`, not a version in `options.keep`.
    pub fn gc(&self, options: &GcOptions) -> Result<GcReport> {
        let items = self.catalog();
        let mut retained = retained_by_policy(&items, &self.options.hold);
        let built = self.built_versions()?;
        for key in built.iter().chain(&options.keep) {
            let item = items.get(&key.item);
            if let Some(version) = item.and_then(|item| item.version(&key.version)) {
                version_objects(version, false, &mut retained);
            }
        }
        self.store.gc(&retained, options.dry_run)
    }

    // ── the document (DR-L9: items are generic trees) ────────────────────

    /// Record a version of `item`. Every object it names must be held, at the
    /// size the manifest says. The id is the manifest's content address, so
    /// the same tree added twice, here or anywhere, is one version.
    pub fn add_version(
        &self,
        item: &str,
        files: Vec<FileEntry>,
        make_current: bool,
    ) -> Result<VersionId> {
        check_item_id(item)?;
        validate_manifest(&files)?;
        for file in &files {
            self.check_held(&file.blake3, file.bytes)?;
        }
        let id = version_id(&files);
        let record = VersionRecord {
            files,
            created_at: now_ms(),
            device: self.options.device.clone(),
        };
        self.mutate(item, |doc| {
            doc.put_version(item, &id, &record)?;
            if make_current {
                doc.set_current(item, &id)?;
            }
            Ok(())
        })?;
        Ok(id)
    }

    pub fn set_current(&self, item: &str, version: &str) -> Result<()> {
        check_item_id(item)?;
        check_version_id(version)?;
        self.mutate(item, |doc| {
            let items = catalog(&doc.state(), |_| true);
            let view = find_version(&items, item, version)?;
            if view.quarantined {
                return Err(quarantined(item, version));
            }
            doc.set_current(item, version)
        })
    }

    /// Keep is an intent on the item, merged in the document: Keep on any
    /// desktop is kept on all (DR-L5).
    pub fn set_kept(&self, item: &str, kept: bool) -> Result<()> {
        self.set_intent(KEPT, item, kept)
    }

    pub fn set_hidden(&self, item: &str, hidden: bool) -> Result<()> {
        self.set_intent(HIDDEN, item, hidden)
    }

    fn set_intent(&self, map: &str, item: &str, value: bool) -> Result<()> {
        check_item_id(item)?;
        self.mutate(item, |doc| {
            let items = catalog(&doc.state(), |_| true);
            if !items.get(item).is_some_and(|entry| !entry.versions.is_empty()) {
                return Err(unknown_item(item));
            }
            doc.set_flag(map, item, value)
        })
    }

    /// A string about an item, for the host: its own id for it, a title.
    /// Allowed before the item's first version.
    pub fn set_meta(&self, item: &str, key: &str, value: Option<&str>) -> Result<()> {
        check_item_id(item)?;
        check_name("meta key", key)?;
        if value.is_some_and(|value| value.len() > MAX_META_BYTES) {
            let message = format!("a meta value is at most {MAX_META_BYTES} bytes");
            return Err(DriveError::invalid(message));
        }
        self.mutate(item, |doc| doc.set_meta(item, key, value))
    }

    /// Name an object (a preview) on a version. The object must be held.
    pub fn set_attachment(
        &self,
        item: &str,
        version: &str,
        name: &str,
        object: Option<ObjectRef>,
    ) -> Result<()> {
        check_item_id(item)?;
        check_version_id(version)?;
        check_name("attachment name", name)?;
        if let Some(object) = &object {
            self.check_held(&object.blake3, object.bytes)?;
        }
        self.mutate(item, |doc| {
            let items = catalog(&doc.state(), |_| true);
            find_version(&items, item, version)?;
            doc.set_attachment(item, version, name, object.as_ref())
        })
    }

    pub fn items(&self) -> Vec<ItemView> {
        self.catalog().into_values().collect()
    }

    pub fn item(&self, id: &str) -> Option<ItemView> {
        self.catalog().remove(id)
    }

    /// The objects the hold policy says to hold and that are not held yet,
    /// in digest order.
    pub fn wants(&self) -> Vec<Blake3> {
        self.missing(&self.catalog()).into_iter().collect()
    }

    /// What the hold policy retains in `items` and this drive does not hold.
    pub(crate) fn missing(&self, items: &BTreeMap<String, ItemView>) -> BTreeSet<Blake3> {
        let mut missing = retained_by_policy(items, &self.options.hold);
        missing.retain(|digest| !self.store.has(digest));
        missing
    }

    // ── builds and the export (DR-D8 (a)) ────────────────────────────────

    /// Build a version as `<root>/built/<item>/<version>/`, by hard links.
    /// Idempotent: a built version is returned as it is.
    pub fn build(&self, item: &str, version: &str) -> Result<PathBuf> {
        let files = self.buildable(item, version)?;
        let dest = self.layout.built_version(item, version);
        let _gate = self.build_gate.lock();
        if dest.is_dir() {
            return Ok(dest);
        }
        let parent = self.layout.built.join(item);
        fs::create_dir_all(&parent).map_err(io_err(&parent))?;
        let tmp = parent.join(format!(".{}", temp_name("build")));
        let none = BTreeSet::new();
        let built = materialize(&self.store, &files, &tmp, PutMode::LinkOrCopy, &none);
        if let Err(err) = built {
            let _ = remove_tree(&tmp);
            return Err(err);
        }
        if let Err(err) = fs::rename(&tmp, &dest) {
            let _ = remove_tree(&tmp);
            return Err(DriveError::io(err, &dest));
        }
        Ok(dest)
    }

    /// Build a version into a host's own folder, `dest`, outside the drive.
    /// `except` leaves paths out for the host to provide; `replace` swaps an
    /// existing `dest` for the new tree atomically.
    pub fn build_into(
        &self,
        item: &str,
        version: &str,
        dest: &Path,
        options: &BuildIntoOptions,
    ) -> Result<()> {
        let files = self.buildable(item, version)?;
        let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
            let message = format!("{} cannot be a view folder", dest.display());
            return Err(DriveError::invalid(message).with_path(dest));
        };
        if !dest.is_absolute() {
            let message = format!("{} is not absolute", dest.display());
            return Err(DriveError::invalid(message).with_path(dest));
        }
        let real_parent = fs::canonicalize(parent).map_err(io_err(parent))?;
        let root = &self.layout.root;
        let real_root = fs::canonicalize(root).map_err(io_err(root))?;
        if real_parent.starts_with(&real_root) {
            let message = format!("{} is inside the drive", dest.display());
            return Err(DriveError::invalid(message).with_path(dest));
        }
        let exists = fs::symlink_metadata(dest).is_ok();
        if exists && !options.replace {
            let message = format!("{} exists", dest.display());
            let err = DriveError::new(DriveErrorCode::Exists, message);
            return Err(err.with_path(dest));
        }
        let name = name.to_string_lossy();
        let except: BTreeSet<String> = options.except.iter().cloned().collect();
        let tmp = parent.join(format!(".{name}.{}", temp_name("build")));
        if let Err(err) = materialize(&self.store, &files, &tmp, options.mode, &except) {
            let _ = remove_tree(&tmp);
            return Err(err);
        }
        if !exists {
            return fs::rename(&tmp, dest).map_err(|err| {
                let _ = remove_tree(&tmp);
                DriveError::io(err, dest)
            });
        }
        let old = parent.join(format!(".{name}.{}", temp_name("old")));
        if let Err(err) = fs::rename(dest, &old) {
            let _ = remove_tree(&tmp);
            return Err(DriveError::io(err, dest));
        }
        if let Err(err) = fs::rename(&tmp, dest) {
            let _ = fs::rename(&old, dest);
            let _ = remove_tree(&tmp);
            return Err(DriveError::io(err, dest));
        }
        let _ = remove_tree(&old);
        Ok(())
    }

    /// Remove a version's build under `built/`. Missing is fine.
    pub fn unbuild(&self, item: &str, version: &str) -> Result<()> {
        check_item_id(item)?;
        check_version_id(version)?;
        let dest = self.layout.built_version(item, version);
        {
            let _gate = self.build_gate.lock();
            remove_tree(&dest).map_err(io_err(&dest))?;
            // Only when it is empty now.
            let _ = fs::remove_dir(self.layout.built.join(item));
        }
        // Windows clears a read-only attribute before it unlinks, and the
        // attribute is the object's, shared by every link: put it back.
        let items = self.catalog();
        let view = items.get(item).and_then(|entry| entry.version(version));
        for file in view.map(|view| view.files.as_slice()).unwrap_or_default() {
            if self.store.has(&file.blake3) {
                let _ = set_readonly(&self.store.object_path(&file.blake3));
            }
        }
        Ok(())
    }

    /// The drive as a read-only export of `built/`.
    pub fn export_descriptor(&self) -> ExportDescriptor {
        ExportDescriptor::drive(&self.options.label, &self.export_root)
    }

    // ── listeners ────────────────────────────────────────────────────────

    /// Hear every change: local writes, imports, objects arriving. Called on
    /// whichever thread made the change, with no drive lock held.
    pub fn subscribe(&self, listener: ChangeListener) -> u64 {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed) + 1;
        self.listeners.lock().insert(id, listener);
        id
    }

    pub fn unsubscribe(&self, id: u64) -> bool {
        self.listeners.lock().remove(&id).is_some()
    }

    pub(crate) fn emit(&self, kind: ChangeKind, items: Vec<String>) {
        let listeners: Vec<ChangeListener> = self.listeners.lock().values().cloned().collect();
        if listeners.is_empty() {
            return;
        }
        let change = DriveChange { kind, items };
        for listener in listeners {
            // A listener's bug must not unwind into the drive.
            let _ = catch_unwind(AssertUnwindSafe(|| listener(&change)));
        }
    }

    // ── internals ────────────────────────────────────────────────────────

    pub(crate) fn catalog(&self) -> BTreeMap<String, ItemView> {
        let state = self.doc.lock().state();
        catalog(&state, |digest| self.store.has(digest))
    }

    fn check_held(&self, digest: &Blake3, bytes: u64) -> Result<()> {
        let path = self.store.object_path(digest);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() == bytes => Ok(()),
            Ok(meta) if meta.is_file() => {
                let held = meta.len();
                let message = format!("{digest} is {held} bytes here, not {bytes}");
                Err(DriveError::mismatch(message).with_path(path))
            }
            _ => {
                let message = format!("{digest} is not held; put it first");
                let err = DriveError::new(DriveErrorCode::NotFound, message);
                Err(err.with_path(path))
            }
        }
    }

    fn buildable(&self, item: &str, version: &str) -> Result<Vec<FileEntry>> {
        check_item_id(item)?;
        check_version_id(version)?;
        let items = self.catalog();
        let view = find_version(&items, item, version)?;
        if view.quarantined {
            return Err(quarantined(item, version));
        }
        Ok(view.files.clone())
    }

    fn built_versions(&self) -> Result<Vec<VersionKey>> {
        let mut out = Vec::new();
        let built = &self.layout.built;
        let items = match fs::read_dir(built) {
            Ok(items) => items,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(DriveError::io(err, built)),
        };
        for item in items {
            let item = item.map_err(io_err(built))?;
            let item_name = item.file_name().to_string_lossy().into_owned();
            let item_path = item.path();
            if !is_valid_item_id(&item_name) || !item_path.is_dir() {
                continue;
            }
            let versions = fs::read_dir(&item_path).map_err(io_err(&item_path))?;
            for version in versions {
                let version = version.map_err(io_err(&item_path))?;
                let name = version.file_name().to_string_lossy().into_owned();
                if is_valid_version_id(&name) && version.path().is_dir() {
                    let item = item_name.clone();
                    out.push(VersionKey {
                        item,
                        version: name,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Write the full snapshot (DR §10.2: never a shallow one).
    pub(crate) fn persist(&self, doc: &DriveDoc) -> Result<()> {
        let bytes = doc.export_snapshot()?;
        let path = &self.layout.doc;
        write_atomic(path, &bytes).map_err(io_err(path))
    }

    /// Run `write` against the document, commit, and — when it changed the
    /// document — persist and tell the listeners. `write` checks everything
    /// before it writes anything, so a refusal leaves no half-made change.
    fn mutate(&self, item: &str, write: impl FnOnce(&DriveDoc) -> Result<()>) -> Result<()> {
        let changed = {
            let doc = self.doc.lock();
            let before = doc.oplog_vv();
            write(&doc)?;
            doc.commit();
            let changed = doc.oplog_vv() != before;
            if changed {
                self.persist(&doc)?;
            }
            changed
        };
        if changed {
            self.emit(ChangeKind::Local, vec![item.to_owned()]);
        }
        Ok(())
    }

    /// Import a peer's update and report what it changed.
    pub fn import_updates(&self, bytes: &[u8]) -> Result<ImportReport> {
        let report = self.import_inner(bytes)?.report;
        if report.changed {
            self.emit(ChangeKind::Import, report.items.clone());
        }
        Ok(report)
    }

    /// Import without telling the listeners (the caller does, once it has
    /// released what it holds). The version vectors either side of the
    /// import are read under the same lock as the import itself.
    pub(crate) fn import_inner(&self, bytes: &[u8]) -> Result<Imported> {
        let mut doc = self.doc.lock();
        let before = doc.oplog_vv();
        let old = doc.state();
        match doc.import(bytes) {
            Ok(false) => {
                let after = before.clone();
                let report = ImportReport::default();
                return Ok(Imported {
                    report,
                    before,
                    after,
                });
            }
            Ok(true) => {}
            Err(ImportFailure::Refused(err)) => return Err(err),
            Err(ImportFailure::Panicked) => {
                // The document may be inconsistent after a panic inside it:
                // start again from what is on disk.
                if let Ok(fresh) = load_doc(&self.layout) {
                    *doc = fresh;
                }
                return Err(DriveError::format(
                    "an update panicked the drive document's decoder; it was refused",
                ));
            }
        }
        self.persist(&doc)?;
        let new = doc.state();
        let after = doc.oplog_vv();
        drop(doc);
        let report = self.report(&old, &new);
        Ok(Imported {
            report,
            before,
            after,
        })
    }

    fn report(&self, before: &DocState, after: &DocState) -> ImportReport {
        let old = fingerprints(before);
        let new = fingerprints(after);
        let mut items: BTreeSet<String> = BTreeSet::new();
        for (item, digest) in &new {
            if old.get(item) != Some(digest) {
                items.insert(item.clone());
            }
        }
        for item in old.keys() {
            if !new.contains_key(item) {
                items.insert(item.clone());
            }
        }

        let catalog = catalog(after, |digest| self.store.has(digest));
        let mut report = ImportReport {
            changed: true,
            items: items.into_iter().collect(),
            ..ImportReport::default()
        };
        for key in after.versions.keys() {
            if before.versions.contains_key(key) {
                continue;
            }
            let Some((item, version)) = key.split_once('/') else {
                continue;
            };
            let Some(view) = catalog.get(item).and_then(|entry| entry.version(version)) else {
                continue;
            };
            let key = VersionKey {
                item: item.to_owned(),
                version: version.to_owned(),
            };
            if view.quarantined {
                report.quarantined.push(key);
            } else {
                report.new_versions.push(key);
            }
        }
        report.wants = self.missing(&catalog).into_iter().collect();
        report
    }

    /// The items whose versions or attachments name `digest`.
    pub(crate) fn items_naming(&self, digest: &Blake3) -> Vec<String> {
        let mut out = Vec::new();
        for item in self.catalog().into_values() {
            if item.versions.iter().any(|v| names(v, digest)) {
                out.push(item.id);
            }
        }
        out
    }
}

/// Whether a version's files or attachments name `digest`.
fn names(version: &VersionView, digest: &Blake3) -> bool {
    let in_files = version.files.iter().any(|file| file.blake3 == *digest);
    in_files || version.attachments.values().any(|o| o.blake3 == *digest)
}

fn unknown_item(item: &str) -> DriveError {
    let message = format!("the drive has no item {item:?}");
    DriveError::new(DriveErrorCode::UnknownItem, message)
}

fn quarantined(item: &str, version: &str) -> DriveError {
    let message = format!("{item}/{version} does not match its id; it is quarantined");
    DriveError::mismatch(message)
}

fn find_version<'a>(
    items: &'a BTreeMap<String, ItemView>,
    item: &str,
    version: &str,
) -> Result<&'a VersionView> {
    let Some(entry) = items.get(item) else {
        return Err(unknown_item(item));
    };
    match entry.version(version) {
        Some(view) => Ok(view),
        None => {
            let message = format!("the drive has no version {version} of {item:?}");
            Err(DriveError::new(DriveErrorCode::UnknownVersion, message))
        }
    }
}
