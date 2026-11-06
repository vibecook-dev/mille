//! `Drive` across napi (`crates/mille-drive`). `cfg(feature = "drive")`.
//!
//! A thin marshaling layer: no drive logic lives here. It follows the git
//! class (`git.rs`) on every point that class settled.
//!
//! **Results cross as JSON text**, parsed by `@vibecook/mille/drive`, so
//! sizes stay numbers and the TypeScript types are the serde shapes. The one
//! exception is sync actions, whose frames are bytes: they cross as
//! `{json, frames}`, the actions in JSON naming frames by index into a
//! `Buffer[]`, so a frame is never base64.
//!
//! **Errors are `DRIVE|<code>|<path>|<message>`**, a prefix of their own, as
//! `GIT|` is: the TypeScript `DriveError` parses its own code union.
//!
//! **Slow work leaves the JavaScript thread.** Putting a file (hashing it,
//! perhaps copying it), building, GC and admitting a pull run on tokio's
//! blocking pool through `env.spawn_future`. Document reads and writes, frames
//! and actions are quick and stay synchronous; a write saves the document's
//! snapshot before it returns.
//!
//! **Listeners are weak**, as git's are: a change subscription never keeps a
//! Node process alive on its own.
//!
//! **Every entry point is `catch_unwind`**, and a panic on the blocking pool
//! comes back as an `EIO` rejection, never an abort.
//!
//! Tests live in JavaScript (`packages/mille/test/drive*.test.mjs`): this
//! crate is a cdylib against N-API and cannot link a Rust test binary. The
//! engine's own tests are `cargo test -p mille-drive`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use mille_drive::{
    Blake3, Drive, DriveChange, DriveError, DriveErrorCode, FileEntry, GcOptions, ObjectRef,
    SyncAction, VersionKey,
};
use napi::bindgen_prelude::{Buffer, Env, Error, PromiseRaw, Result, Status, Unknown};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use parking_lot::{Mutex, RwLock};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::drive_types::{
    put_mode, BuildIntoOptionsJs, DriveOptionsJs, DriveSyncActionsJs, PutOptionsJs,
};

/// A JSON-text listener: `(json: string) => void`, weak (see module docs).
type JsonListener = ThreadsafeFunction<String, Unknown<'static>, String, Status, false, true>;

/// Encode a `DriveError` as `DRIVE|<code>|<path>|<message>`.
pub(crate) fn drive_error_to_napi(err: DriveError) -> Error {
    let path = match err.path() {
        Some(path) => path.to_string_lossy().into_owned(),
        None => String::new(),
    };
    let reason = format!("DRIVE|{}|{}|{}", err.code().as_str(), path, err.message());
    Error::new(Status::GenericFailure, reason)
}

fn invalid(message: impl Into<String>) -> Error {
    drive_error_to_napi(DriveError::new(DriveErrorCode::Invalid, message))
}

/// A mille-drive result as a napi one.
fn js<T>(result: mille_drive::Result<T>) -> Result<T> {
    result.map_err(drive_error_to_napi)
}

fn encode_failed(err: serde_json::Error) -> Error {
    invalid(format!("cannot encode a drive result as JSON: {err}"))
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(encode_failed)
}

fn parse_json<T: DeserializeOwned>(what: &str, text: &str) -> Result<T> {
    match serde_json::from_str::<T>(text) {
        Ok(value) => Ok(value),
        Err(err) => Err(invalid(format!(
            "{what} is not what the drive expects: {err}"
        ))),
    }
}

fn parse_digest(hex: &str) -> Result<Blake3> {
    js(hex.parse::<Blake3>())
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Run `work` on tokio's blocking pool. A panic in it is an `EIO` rejection.
async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> mille_drive::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => js(result),
        Err(err) => {
            let message = format!("a drive task did not finish: {err}");
            Err(drive_error_to_napi(DriveError::new(
                DriveErrorCode::Io,
                message,
            )))
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ActionJson {
    Send {
        peer: String,
        frame: usize,
    },
    Pull {
        peer: String,
        blake3: String,
        into: String,
    },
}

fn actions_to_js(actions: Vec<SyncAction>) -> Result<DriveSyncActionsJs> {
    let mut list = Vec::with_capacity(actions.len());
    let mut frames = Vec::new();
    for action in actions {
        match action {
            SyncAction::Send { to, frame } => {
                list.push(ActionJson::Send {
                    peer: to,
                    frame: frames.len(),
                });
                frames.push(Buffer::from(frame));
            }
            SyncAction::Pull { from, digest, into } => {
                list.push(ActionJson::Pull {
                    peer: from,
                    blake3: digest.to_hex(),
                    into: path_text(&into),
                });
            }
        }
    }
    let json = to_json(&list)?;
    Ok(DriveSyncActionsJs { json, frames })
}

/// The napi `Drive` class. Node / Electron UtilityProcess only: the
/// TypeScript wrapper is `@vibecook/mille/drive`, never the root barrel.
#[napi(js_name = "Drive")]
pub struct DriveJs {
    /// `None` after `dispose`. In-flight calls hold their own `Arc`, so the
    /// drive (and its root's lock) goes away when the last of them finishes.
    drive: RwLock<Option<Arc<Drive>>>,
    /// The JavaScript subscription id → the drive's listener id.
    subscriptions: Mutex<BTreeMap<u32, u64>>,
    next_subscription: AtomicU32,
}

impl DriveJs {
    fn drive(&self) -> Result<Arc<Drive>> {
        let drive = self.drive.read().clone();
        drive.ok_or_else(|| invalid("Drive is disposed"))
    }
}

#[napi]
impl DriveJs {
    /// Open (or create) the drive at `options.root`. `EBUSY` while another
    /// `Drive`, in this process or another, holds it.
    #[napi(constructor, catch_unwind)]
    pub fn new(options: DriveOptionsJs) -> Result<Self> {
        let (root, options) = js(options.into_options())?;
        let drive = js(Drive::open(&root, options))?;
        Ok(Self {
            drive: RwLock::new(Some(Arc::new(drive))),
            subscriptions: Mutex::new(BTreeMap::new()),
            next_subscription: AtomicU32::new(0),
        })
    }

    #[napi(catch_unwind)]
    pub fn root(&self) -> Result<String> {
        Ok(path_text(self.drive()?.root()))
    }

    /// The one folder a host registers as a pull root.
    #[napi(js_name = "objectsRoot", catch_unwind)]
    pub fn objects_root(&self) -> Result<String> {
        Ok(path_text(self.drive()?.objects_root()))
    }

    #[napi(catch_unwind)]
    pub fn has(&self, blake3: String) -> Result<bool> {
        let digest = parse_digest(&blake3)?;
        Ok(self.drive()?.has(&digest))
    }

    /// The held object's path, or `null`.
    #[napi(js_name = "objectPath", catch_unwind)]
    pub fn object_path(&self, blake3: String) -> Result<Option<String>> {
        let digest = parse_digest(&blake3)?;
        let path = self.drive()?.object_path(&digest);
        Ok(path.as_deref().map(path_text))
    }

    /// Bring a file in. Resolves to the JSON `PutOutcome`.
    #[napi(js_name = "putFile", catch_unwind)]
    pub fn put_file<'env>(
        &self,
        env: &'env Env,
        src: String,
        options: Option<PutOptionsJs>,
    ) -> Result<PromiseRaw<'env, String>> {
        let drive = self.drive()?;
        let (mode, expect) = match options {
            Some(options) => (options.mode, options.expect),
            None => (None, None),
        };
        let mode = js(put_mode(mode.as_deref()))?;
        let expect = match expect {
            Some(hex) => Some(parse_digest(&hex)?),
            None => None,
        };
        env.spawn_future(async move {
            let work = move || drive.put_file(Path::new(&src), mode, expect.as_ref());
            let outcome = blocking(work).await?;
            to_json(&outcome)
        })
    }

    /// Bring bytes in. Resolves to the JSON `PutOutcome`.
    #[napi(js_name = "putBytes", catch_unwind)]
    pub fn put_bytes<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
    ) -> Result<PromiseRaw<'env, String>> {
        let drive = self.drive()?;
        let bytes = data.to_vec();
        env.spawn_future(async move {
            let outcome = blocking(move || drive.put_bytes(&bytes)).await?;
            to_json(&outcome)
        })
    }

    /// Re-hash a held object: `false` when its bytes no longer match.
    #[napi(catch_unwind)]
    pub fn verify<'env>(&self, env: &'env Env, blake3: String) -> Result<PromiseRaw<'env, bool>> {
        let drive = self.drive()?;
        let digest = parse_digest(&blake3)?;
        env.spawn_future(async move {
            let work = move || drive.verify(&digest);
            blocking(work).await
        })
    }

    /// Remove unheld objects. `keepJson` is a JSON `[{item, version}]` of the
    /// host's own views. Resolves to the JSON `GcReport`.
    #[napi(catch_unwind)]
    pub fn gc<'env>(
        &self,
        env: &'env Env,
        dry_run: Option<bool>,
        keep_json: Option<String>,
    ) -> Result<PromiseRaw<'env, String>> {
        let drive = self.drive()?;
        let keep: Vec<VersionKey> = match keep_json {
            Some(text) => parse_json("keep", &text)?,
            None => Vec::new(),
        };
        let options = GcOptions {
            dry_run: dry_run.unwrap_or(false),
            keep,
        };
        env.spawn_future(async move {
            let report = blocking(move || drive.gc(&options)).await?;
            to_json(&report)
        })
    }

    /// Record a version from a JSON `[{path, blake3, bytes}]`. Returns its id.
    #[napi(js_name = "addVersion", catch_unwind)]
    pub fn add_version(
        &self,
        item: String,
        files_json: String,
        make_current: Option<bool>,
    ) -> Result<String> {
        let files: Vec<FileEntry> = parse_json("files", &files_json)?;
        let make_current = make_current.unwrap_or(true);
        js(self.drive()?.add_version(&item, files, make_current))
    }

    #[napi(js_name = "setCurrent", catch_unwind)]
    pub fn set_current(&self, item: String, version: String) -> Result<()> {
        js(self.drive()?.set_current(&item, &version))
    }

    #[napi(js_name = "setKept", catch_unwind)]
    pub fn set_kept(&self, item: String, kept: bool) -> Result<()> {
        js(self.drive()?.set_kept(&item, kept))
    }

    #[napi(js_name = "setHidden", catch_unwind)]
    pub fn set_hidden(&self, item: String, hidden: bool) -> Result<()> {
        js(self.drive()?.set_hidden(&item, hidden))
    }

    /// `value: null` removes the key.
    #[napi(js_name = "setMeta", catch_unwind)]
    pub fn set_meta(&self, item: String, key: String, value: Option<String>) -> Result<()> {
        js(self.drive()?.set_meta(&item, &key, value.as_deref()))
    }

    /// `objectJson` is a JSON `{blake3, bytes}`, or `null` to remove.
    #[napi(js_name = "setAttachment", catch_unwind)]
    pub fn set_attachment(
        &self,
        item: String,
        version: String,
        name: String,
        object_json: Option<String>,
    ) -> Result<()> {
        let object = match object_json {
            Some(text) => Some(parse_json::<ObjectRef>("object", &text)?),
            None => None,
        };
        js(self.drive()?.set_attachment(&item, &version, &name, object))
    }

    /// Every item, as a JSON `ItemView[]`.
    #[napi(catch_unwind)]
    pub fn items(&self) -> Result<String> {
        to_json(&self.drive()?.items())
    }

    /// One item as a JSON `ItemView`, or `null`.
    #[napi(catch_unwind)]
    pub fn item(&self, id: String) -> Result<Option<String>> {
        match self.drive()?.item(&id) {
            Some(item) => Ok(Some(to_json(&item)?)),
            None => Ok(None),
        }
    }

    /// Objects the hold policy names and the drive does not hold yet.
    #[napi(catch_unwind)]
    pub fn wants(&self) -> Result<Vec<String>> {
        let wants = self.drive()?.wants();
        Ok(wants.iter().map(Blake3::to_hex).collect())
    }

    /// Build a version under `built/`. Resolves to its folder.
    #[napi(catch_unwind)]
    pub fn build<'env>(
        &self,
        env: &'env Env,
        item: String,
        version: String,
    ) -> Result<PromiseRaw<'env, String>> {
        let drive = self.drive()?;
        env.spawn_future(async move {
            let built = blocking(move || drive.build(&item, &version)).await?;
            Ok(path_text(&built))
        })
    }

    /// Build a version into a host's own folder.
    #[napi(js_name = "buildInto", catch_unwind)]
    pub fn build_into<'env>(
        &self,
        env: &'env Env,
        item: String,
        version: String,
        dest: String,
        options: Option<BuildIntoOptionsJs>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let drive = self.drive()?;
        let options = match options {
            Some(options) => js(options.into_options())?,
            None => Default::default(),
        };
        env.spawn_future(async move {
            let work = move || drive.build_into(&item, &version, Path::new(&dest), &options);
            blocking(work).await
        })
    }

    #[napi(catch_unwind)]
    pub fn unbuild<'env>(
        &self,
        env: &'env Env,
        item: String,
        version: String,
    ) -> Result<PromiseRaw<'env, ()>> {
        let drive = self.drive()?;
        env.spawn_future(async move {
            let work = move || drive.unbuild(&item, &version);
            blocking(work).await
        })
    }

    /// The drive's read-only export, as JSON.
    #[napi(js_name = "exportDescriptor", catch_unwind)]
    pub fn export_descriptor(&self) -> Result<String> {
        to_json(&self.drive()?.export_descriptor())
    }

    /// This drive's version vector, encoded.
    #[napi(js_name = "versionVector", catch_unwind)]
    pub fn version_vector(&self) -> Result<Buffer> {
        Ok(Buffer::from(self.drive()?.version_vector()))
    }

    #[napi(js_name = "syncOpen", catch_unwind)]
    pub fn sync_open(&self, peer: String) -> Result<DriveSyncActionsJs> {
        actions_to_js(js(self.drive()?.sync_open(&peer))?)
    }

    /// A frame from `peer`. Throws `EPROTO`/`EFORMAT` for one the drive
    /// refuses; the peer stays open.
    #[napi(js_name = "syncHandle", catch_unwind)]
    pub fn sync_handle(&self, peer: String, frame: Buffer) -> Result<DriveSyncActionsJs> {
        actions_to_js(js(self.drive()?.sync_handle(&peer, &frame))?)
    }

    #[napi(js_name = "syncLocalChanged", catch_unwind)]
    pub fn sync_local_changed(&self) -> Result<DriveSyncActionsJs> {
        actions_to_js(js(self.drive()?.sync_local_changed())?)
    }

    /// A pull landed at `path`: verify and admit it (the file is consumed).
    /// Rejects `EMISMATCH` for a damaged copy; then call `syncPullFailed`.
    #[napi(js_name = "syncPulled", catch_unwind)]
    pub fn sync_pulled<'env>(
        &self,
        env: &'env Env,
        peer: String,
        blake3: String,
        path: String,
    ) -> Result<PromiseRaw<'env, DriveSyncActionsJs>> {
        let drive = self.drive()?;
        let digest = parse_digest(&blake3)?;
        env.spawn_future(async move {
            let work = move || drive.sync_pulled(&peer, &digest, Path::new(&path));
            actions_to_js(blocking(work).await?)
        })
    }

    #[napi(js_name = "syncPullFailed", catch_unwind)]
    pub fn sync_pull_failed(&self, peer: String, blake3: String) -> Result<DriveSyncActionsJs> {
        let digest = parse_digest(&blake3)?;
        actions_to_js(js(self.drive()?.sync_pull_failed(&peer, &digest))?)
    }

    #[napi(js_name = "syncTick", catch_unwind)]
    pub fn sync_tick(&self) -> Result<DriveSyncActionsJs> {
        actions_to_js(js(self.drive()?.sync_tick())?)
    }

    #[napi(js_name = "syncClose", catch_unwind)]
    pub fn sync_close(&self, peer: String) -> Result<()> {
        self.drive()?.sync_close(&peer);
        Ok(())
    }

    /// Where sync stands, as JSON `{peers, pulling, wants, unsourced}`.
    #[napi(js_name = "syncStatus", catch_unwind)]
    pub fn sync_status(&self) -> Result<String> {
        to_json(&self.drive()?.sync_status())
    }

    /// Hear every change (`(json) => void`, a JSON `{kind, items}`). Returns
    /// a subscription id for `off`.
    #[napi(js_name = "onChange", catch_unwind)]
    pub fn on_change(&self, listener: JsonListener) -> Result<u32> {
        let drive = self.drive()?;
        let forward = move |change: &DriveChange| {
            if let Ok(text) = serde_json::to_string(change) {
                listener.call(text, ThreadsafeFunctionCallMode::NonBlocking);
            }
        };
        let native = drive.subscribe(Arc::new(forward));
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed) + 1;
        self.subscriptions.lock().insert(id, native);
        Ok(id)
    }

    /// Remove a subscription. `false` for an unknown id; double-off is fine.
    #[napi(catch_unwind)]
    pub fn off(&self, subscription_id: u32) -> bool {
        let Some(native) = self.subscriptions.lock().remove(&subscription_id) else {
            return false;
        };
        if let Some(drive) = self.drive.read().clone() {
            drive.unsubscribe(native);
        }
        true
    }

    /// Drop every subscription and release the drive: its root's lock is
    /// released once the last in-flight call returns. Every later call is
    /// `EINVAL`. Idempotent.
    #[napi(catch_unwind)]
    pub fn dispose(&self) {
        let drive = self.drive.write().take();
        let subscriptions = std::mem::take(&mut *self.subscriptions.lock());
        if let Some(drive) = drive {
            for native in subscriptions.into_values() {
                drive.unsubscribe(native);
            }
        }
    }
}
