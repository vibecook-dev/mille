//! The drive's document: a Loro document of flat root maps (DR-D4).
//!
//! | map           | key                          | value                              |
//! |---------------|------------------------------|------------------------------------|
//! | `versions`    | `<item>/<version>`           | JSON `{files, createdAt, device}`  |
//! | `current`     | `<item>`                     | a version id                       |
//! | `kept`        | `<item>`                     | bool                               |
//! | `hidden`      | `<item>`                     | bool                               |
//! | `meta`        | `<item>/<key>`               | string                             |
//! | `attachments` | `<item>/<version>/<name>`    | JSON `{blake3, bytes}`             |
//!
//! Only root maps, and only plain values in them. Loro's ordinary child
//! containers fork when two peers create the same key's child concurrently
//! (loro 1.13.0's changelog), so a version record is one immutable JSON value,
//! never a nested map. Two peers writing one key converge on one value by
//! Loro's last-writer-wins order for maps; a version record is written once
//! (its key is its content address), so its only concurrent writer writes the
//! same files.
//!
//! Every import runs under `catch_unwind`: an update from a peer is untrusted
//! bytes, and a panic inside Loro must refuse the update, not take the host
//! process down.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use loro::{ExportMode, LoroDoc, LoroValue, ValueOrContainer, VersionVector};
use serde::{Deserialize, Serialize};

use crate::error::{DriveError, Result};
use crate::store::ObjectRef;
use crate::version::FileEntry;

pub(crate) const VERSIONS: &str = "versions";
pub(crate) const CURRENT: &str = "current";
pub(crate) const KEPT: &str = "kept";
pub(crate) const HIDDEN: &str = "hidden";
pub(crate) const META: &str = "meta";
pub(crate) const ATTACHMENTS: &str = "attachments";

/// A version's record: an immutable value, keyed by its content address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionRecord {
    pub(crate) files: Vec<FileEntry>,
    pub(crate) created_at: u64,
    pub(crate) device: String,
}

/// Every map, read out as plain values. Keys a reader does not understand are
/// kept here and judged later (quarantined, or ignored), never dropped on read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DocState {
    pub(crate) versions: BTreeMap<String, String>,
    pub(crate) current: BTreeMap<String, String>,
    pub(crate) kept: BTreeMap<String, bool>,
    pub(crate) hidden: BTreeMap<String, bool>,
    pub(crate) meta: BTreeMap<String, String>,
    pub(crate) attachments: BTreeMap<String, String>,
}

/// Why an import did not apply.
#[derive(Debug)]
pub(crate) enum ImportFailure {
    /// Loro refused the bytes.
    Refused(DriveError),
    /// Loro panicked. The document may be inconsistent; reload it from disk.
    Panicked,
}

pub(crate) struct DriveDoc {
    doc: LoroDoc,
}

fn loro_error(what: &str, err: impl std::fmt::Display) -> DriveError {
    DriveError::format(format!("the drive document could not {what}: {err}"))
}

fn as_string(value: &ValueOrContainer) -> Option<String> {
    match value {
        ValueOrContainer::Value(LoroValue::String(text)) => Some(text.as_str().to_owned()),
        _ => None,
    }
}

fn as_bool(value: &ValueOrContainer) -> Option<bool> {
    match value {
        ValueOrContainer::Value(LoroValue::Bool(flag)) => Some(*flag),
        _ => None,
    }
}

impl DriveDoc {
    /// An empty document under a fresh, random peer id. A drive mints a new
    /// peer id every time it opens rather than reusing one: a reused id whose
    /// last changes never reached disk would let two different changes carry
    /// one Loro id, which no replica can untangle.
    pub(crate) fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
        }
    }

    /// A document loaded from a full snapshot.
    pub(crate) fn load(snapshot: &[u8]) -> Result<Self> {
        let doc = Self::new();
        match doc.import(snapshot) {
            Ok(_) => Ok(doc),
            Err(ImportFailure::Refused(err)) => Err(err),
            Err(ImportFailure::Panicked) => Err(DriveError::format(
                "the drive document's snapshot panicked the decoder; it was not loaded",
            )),
        }
    }

    /// Import a peer's update (or a snapshot). `Ok(true)` when it changed the
    /// document.
    pub(crate) fn import(&self, bytes: &[u8]) -> std::result::Result<bool, ImportFailure> {
        let before = self.doc.oplog_vv();
        let doc = &self.doc;
        match catch_unwind(AssertUnwindSafe(|| doc.import(bytes))) {
            Ok(Ok(_status)) => Ok(self.doc.oplog_vv() != before),
            Ok(Err(err)) => Err(ImportFailure::Refused(loro_error("import an update", err))),
            Err(_) => Err(ImportFailure::Panicked),
        }
    }

    pub(crate) fn oplog_vv(&self) -> VersionVector {
        self.doc.oplog_vv()
    }

    pub(crate) fn commit(&self) {
        self.doc.commit();
    }

    /// The full snapshot — never a shallow one (DR §10.2).
    pub(crate) fn export_snapshot(&self) -> Result<Vec<u8>> {
        let exported = self.doc.export(ExportMode::snapshot());
        exported.map_err(|err| loro_error("export a snapshot", err))
    }

    /// Every change this document has that `since` does not.
    pub(crate) fn export_updates_since(&self, since: &VersionVector) -> Result<Vec<u8>> {
        let exported = self.doc.export(ExportMode::updates(since));
        exported.map_err(|err| loro_error("export updates", err))
    }

    fn insert_str(&self, map: &str, key: &str, value: String) -> Result<()> {
        let inserted = self.doc.get_map(map).insert(key, value);
        inserted.map_err(|err| loro_error("write", err))
    }

    fn insert_bool(&self, map: &str, key: &str, value: bool) -> Result<()> {
        let inserted = self.doc.get_map(map).insert(key, value);
        inserted.map_err(|err| loro_error("write", err))
    }

    fn delete(&self, map: &str, key: &str) -> Result<()> {
        let deleted = self.doc.get_map(map).delete(key);
        deleted.map_err(|err| loro_error("delete", err))
    }

    /// Write a version record unless its key is already there. `true` when
    /// this call wrote it.
    pub(crate) fn put_version(&self, item: &str, id: &str, record: &VersionRecord) -> Result<bool> {
        let key = format!("{item}/{id}");
        if self.doc.get_map(VERSIONS).get(&key).is_some() {
            return Ok(false);
        }
        let encoded = serde_json::to_string(record);
        let json = encoded.map_err(|err| loro_error("encode", err))?;
        self.insert_str(VERSIONS, &key, json)?;
        Ok(true)
    }

    pub(crate) fn set_current(&self, item: &str, version: &str) -> Result<()> {
        self.insert_str(CURRENT, item, version.to_owned())
    }

    pub(crate) fn set_flag(&self, map: &str, item: &str, value: bool) -> Result<()> {
        self.insert_bool(map, item, value)
    }

    pub(crate) fn set_meta(&self, item: &str, key: &str, value: Option<&str>) -> Result<()> {
        let key = format!("{item}/{key}");
        match value {
            Some(value) => self.insert_str(META, &key, value.to_owned()),
            None => self.delete(META, &key),
        }
    }

    pub(crate) fn set_attachment(
        &self,
        item: &str,
        version: &str,
        name: &str,
        object: Option<&ObjectRef>,
    ) -> Result<()> {
        let key = format!("{item}/{version}/{name}");
        match object {
            Some(object) => {
                let encoded = serde_json::to_string(object);
                let json = encoded.map_err(|err| loro_error("encode", err))?;
                self.insert_str(ATTACHMENTS, &key, json)
            }
            None => self.delete(ATTACHMENTS, &key),
        }
    }

    fn read_strings(&self, map: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        self.doc.get_map(map).for_each(|key, value| {
            if let Some(text) = as_string(&value) {
                out.insert(key.to_owned(), text);
            }
        });
        out
    }

    fn read_bools(&self, map: &str) -> BTreeMap<String, bool> {
        let mut out = BTreeMap::new();
        self.doc.get_map(map).for_each(|key, value| {
            if let Some(flag) = as_bool(&value) {
                out.insert(key.to_owned(), flag);
            }
        });
        out
    }

    /// Every map, as plain values.
    pub(crate) fn state(&self) -> DocState {
        DocState {
            versions: self.read_strings(VERSIONS),
            current: self.read_strings(CURRENT),
            kept: self.read_bools(KEPT),
            hidden: self.read_bools(HIDDEN),
            meta: self.read_strings(META),
            attachments: self.read_strings(ATTACHMENTS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::Blake3;

    fn record(path: &str, data: &[u8]) -> VersionRecord {
        VersionRecord {
            files: vec![FileEntry {
                path: path.to_owned(),
                blake3: Blake3::of(data),
                bytes: data.len() as u64,
            }],
            created_at: 1,
            device: "test".to_owned(),
        }
    }

    fn sync(a: &DriveDoc, b: &DriveDoc) {
        let to_b = a.export_updates_since(&b.oplog_vv()).unwrap();
        b.import(&to_b).unwrap();
        let to_a = b.export_updates_since(&a.oplog_vv()).unwrap();
        a.import(&to_a).unwrap();
    }

    #[test]
    fn writes_read_back_and_survive_a_snapshot() {
        let doc = DriveDoc::new();
        assert!(doc.put_version("app", "v1", &record("a", b"1")).unwrap());
        assert!(!doc.put_version("app", "v1", &record("a", b"1")).unwrap());
        doc.set_current("app", "v1").unwrap();
        doc.set_flag(KEPT, "app", true).unwrap();
        doc.set_flag(HIDDEN, "app", false).unwrap();
        doc.set_meta("app", "title", Some("An app")).unwrap();
        doc.commit();

        let snapshot = doc.export_snapshot().unwrap();
        let loaded = DriveDoc::load(&snapshot).unwrap();
        let state = loaded.state();
        assert_eq!(state, doc.state());
        assert_eq!(state.current.get("app").map(String::as_str), Some("v1"));
        assert_eq!(state.kept.get("app"), Some(&true));
        assert_eq!(state.hidden.get("app"), Some(&false));
        assert_eq!(
            state.meta.get("app/title").map(String::as_str),
            Some("An app")
        );
        let json = state.versions.get("app/v1").unwrap();
        let back: VersionRecord = serde_json::from_str(json).unwrap();
        assert_eq!(back, record("a", b"1"));

        doc.set_meta("app", "title", None).unwrap();
        doc.commit();
        assert!(!doc.state().meta.contains_key("app/title"));
    }

    #[test]
    fn concurrent_current_writes_converge_on_one_winner() {
        let a = DriveDoc::new();
        let b = DriveDoc::new();
        a.set_current("app", "from-a").unwrap();
        a.commit();
        b.set_current("app", "from-b").unwrap();
        b.commit();
        sync(&a, &b);
        let on_a = a.state().current.get("app").cloned();
        let on_b = b.state().current.get("app").cloned();
        assert_eq!(on_a, on_b);
        assert!(matches!(on_a.as_deref(), Some("from-a" | "from-b")));
    }

    #[test]
    fn garbage_is_refused_without_a_panic_escaping() {
        let doc = DriveDoc::new();
        let cases: [&[u8]; 3] = [b"", b"\x00\x01\x02", b"not a loro update at all"];
        for bytes in cases {
            match doc.import(bytes) {
                Ok(changed) => assert!(!changed, "{bytes:?} changed the document"),
                Err(ImportFailure::Refused(err)) => {
                    assert_eq!(err.code(), crate::error::DriveErrorCode::Format);
                }
                Err(ImportFailure::Panicked) => {}
            }
        }
        assert!(DriveDoc::load(b"\xffnope").is_err());
    }
}
