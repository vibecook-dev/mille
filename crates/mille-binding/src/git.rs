//! `GitEngine` across napi (SPEC mille-git §18). `cfg(feature = "git")`.
//!
//! A thin marshaling layer over `mille_git::GitEngine`: no git logic lives
//! here. Four decisions shape it, and each is easy to undo by accident.
//!
//! **Results cross as JSON text, not `#[napi(object)]` mirrors.** `mille-git`'s
//! public types already derive `Serialize` in Appendix A's camelCase, so one
//! `serde_json::to_string` is the whole conversion and the TypeScript wrapper
//! `JSON.parse`s it. Mirrors would duplicate ~15 types field by field, and
//! napi's own serde path turns every `u64` above `u32::MAX` into a `BigInt`
//! (`generation`, `tookMs`, a large file's `bytes`) where JSON keeps a number.
//! It is also the shape the MessagePort transport (PR 8) frames anyway, and
//! `JSON.parse` of a 10 000-row snapshot beats building it property by
//! property through N-API. Inputs *are* mirrored (`git_types.rs`): they are
//! few, small, and napi validates them for free.
//!
//! **Cancellation is an `operationId`, not an `AbortSignal` parameter.** napi-rs
//! 3.8's `AbortSignal` is `!Send`, overwrites the signal's `onabort`
//! property, and never fires for a signal that was already aborted. The
//! explorer settled this already (`FileExplorer.cancelOperation`); the git
//! class does the same. Every cancellable entry point registers its token
//! **on the JS thread, before the promise is returned**, so a
//! `cancelOperation` issued the instant the call returns always finds it —
//! that is why these are sync functions returning `PromiseRaw` rather than
//! `async fn`s, whose bodies first run on a runtime worker. The TypeScript
//! wrapper turns an `AbortSignal` into exactly this.
//!
//! **The engine keeps its own runtime; napi's is not shared.** `GitEngine::new`
//! starts a two-worker runtime, and `with_runtime` (which would adopt napi's)
//! is deliberately not used. napi-rs exposes no handle to its runtime, drops
//! it when the Node environment exits and builds a *new* one when an Electron
//! renderer reloads (`start_async_runtime`); a `Handle` captured from the old
//! one would leave every queued status silently cancelled. The engine's
//! watchers and queue schedule work long after any JS call returned, so they
//! need a runtime whose lifetime the engine owns. It also keeps gix's blocking
//! work (status, diff) out of the explorer's blocking pool. The cost is two
//! idle worker threads per engine, and playground builds one engine.
//!
//! **Listeners are weak.** A status or warning subscription must never keep a
//! Node process alive on its own; the explorer's channels made the opposite
//! choice for a different reason (its watcher is the process's job). Calling
//! a weak listener after its environment closed is a no-op.
//!
//! `GIT|<code>|<path>|<message>` is the error reason (§18.2), deliberately not
//! `FX|`: the TypeScript `GitError` and `FileSystemError` parse different
//! code unions.
//!
//! Tests live in JavaScript against a `--features git` build
//! (`packages/mille/test/git-*.test.mjs`, §28): this crate is a cdylib against
//! N-API and cannot link a Rust test binary.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use mille_core::{EntryId, EntryStore};
use mille_git::{CancellationToken, GitEngine, GitError, GitErrorKind, OpOptions, StatusBackend};
use napi::bindgen_prelude::{Buffer, Env, Error, PromiseRaw, Result, Status, Unknown};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use crate::events::{Channel, EventBus};
use crate::explorer::FileExplorer;
use crate::git_types::{
    CommitRequestJs, DiffRequestJs, GitEngineOptionsJs, LogRequestJs, ReadObjectRequestJs,
};
use crate::types::{ChangeNoticeJs, FileSystemEventJs};

/// A JSON-text listener: `(json: string) => void`, weak (see module docs).
type JsonListener = ThreadsafeFunction<String, Unknown<'static>, String, Status, false, true>;

/// Encode a `GitError` as `GIT|<code>|<path>|<message>` (§18.2).
pub(crate) fn git_error_to_napi(err: GitError) -> Error {
    let reason = format!(
        "GIT|{}|{}|{}",
        err.code(),
        err.path().unwrap_or(""),
        err.message()
    );
    Error::new(Status::GenericFailure, reason)
}

fn invalid(message: impl Into<String>) -> Error {
    git_error_to_napi(GitError::new(GitErrorKind::InvalidInput, message))
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|err| {
        git_error_to_napi(GitError::new(
            GitErrorKind::Unknown,
            format!("cannot encode a git result as JSON: {err}"),
        ))
    })
}

fn forward<T: Serialize + ?Sized>(listener: &JsonListener, value: &T) {
    if let Ok(text) = serde_json::to_string(value) {
        listener.call(text, ThreadsafeFunctionCallMode::NonBlocking);
    }
}

/// One live subscription, remembered so `off` and `dispose` can undo it.
enum Subscription {
    Status(PathBuf, u64),
    Worktrees(PathBuf, u64),
    Warning(u64),
    /// A `bindExplorer`: two native listeners on that explorer's bus. Held
    /// weakly — a binding must not keep a disposed explorer's state alive.
    Explorer {
        bus: Weak<EventBus>,
        event: u64,
        change_tree: u64,
    },
}

impl Subscription {
    fn cancel(self, engine: Option<&GitEngine>) {
        match (self, engine) {
            (
                Subscription::Explorer {
                    bus,
                    event,
                    change_tree,
                },
                _,
            ) => {
                if let Some(bus) = bus.upgrade() {
                    bus.unsubscribe(event);
                    bus.unsubscribe(change_tree);
                }
            }
            (Subscription::Status(worktree, id), Some(engine)) => {
                engine.unsubscribe_status(&worktree, id)
            }
            (Subscription::Worktrees(worktree, id), Some(engine)) => {
                engine.unsubscribe_worktrees(&worktree, id)
            }
            (Subscription::Warning(id), Some(engine)) => engine.unsubscribe_warnings(id),
            (_, None) => {}
        }
    }
}

/// Run a native explorer listener's body so a bug in it can never unwind the
/// thread that emitted the event — the explorer's watcher thread, whose death
/// would silently end every future watcher event for the explorer.
fn contained(work: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(work));
}

/// §11.2 rule 3 and §11.2.1: one `'event'` → routed worktree-relative hints.
/// Routing (realpath, longest checkout prefix, strip to POSIX, drop `.git`
/// and lock noise) is `GitEngine::hint_os_paths`'s; this only picks the paths.
fn hint_event(engine: &GitEngine, store: &EntryStore, event: &FileSystemEventJs) {
    let Some(path) = event.path.as_deref() else {
        return; // `'error'` carries none
    };
    match event.kind.as_str() {
        // An overflow names a subtree whose events were lost: full status for
        // every session whose checkout contains it — never one merely under it.
        "overflow" => engine.hint_coarse_os_path(Path::new(path)),
        "renamed" => {
            // `path` is `to`; `old_name` is a basename only. Recover `from`
            // through the old parent's path so the deletion side is hinted
            // too. When the old parent is unknown (a lazy, unwalked folder)
            // only `to` is hinted and the §10.6 merge drops the stale row.
            let mut paths = vec![PathBuf::from(path)];
            if let (Some(parent), Some(name)) = (event.old_parent_id, event.old_name.as_deref()) {
                if let Some(dir) = store.path_for_id(EntryId(parent as u64)) {
                    paths.push(dir.join(name));
                }
            }
            engine.hint_os_paths(&paths);
        }
        // created / changed / deleted, and the `'warning'` a degraded rename
        // reconciles from disk: the path changed in some way.
        _ => engine.hint_os_paths(&[PathBuf::from(path)]),
    }
}

/// §11.2 rule 4, the supplement: `ChangeTree` ids → paths (no write lock;
/// `path_for_id` is a map read) → the same routing. Coarse subtrees full-scan
/// the checkouts that contain them.
///
/// `ChangeTree` also fires from JS-thread calls that move no file
/// (projection settings, root changes, lazy hydration), so this stays bounded:
/// past `MAX_HINT_PATHS` ids it maps nothing and hints each explorer root as
/// coarse instead — a full status for the checkouts containing a root. Every
/// change the watcher saw has already arrived as an `'event'`; what this can
/// add is a `resync` that found changes the watcher missed.
fn hint_change(engine: &GitEngine, store: &EntryStore, notice: &ChangeNoticeJs) {
    for id in &notice.coarse_subtrees {
        if let Some(path) = store.path_for_id(EntryId(*id as u64)) {
            engine.hint_coarse_os_path(&path);
        }
    }
    let count = notice.changed_ids.len() + notice.child_set_changed.len();
    if count == 0 {
        return;
    }
    if count > mille_git::MAX_HINT_PATHS {
        let snapshot = store.snapshot();
        for root in snapshot.roots() {
            if let Some(path) = store.path_for_id(*root) {
                engine.hint_coarse_os_path(&path);
            }
        }
        return;
    }
    let paths: Vec<PathBuf> = notice
        .changed_ids
        .iter()
        .chain(&notice.child_set_changed)
        .filter_map(|id| store.path_for_id(EntryId(*id as u64)))
        .collect();
    engine.hint_os_paths(&paths);
}

/// A cancellable call's token, registered under its `operationId` for as long
/// as the call runs. Dropping it (success, failure, or a panic unwinding the
/// future) releases the id, so a retry never inherits a stale token.
struct Operation {
    registry: Arc<Mutex<HashMap<String, CancellationToken>>>,
    id: Option<String>,
    token: CancellationToken,
}

impl Operation {
    fn begin(
        registry: &Arc<Mutex<HashMap<String, CancellationToken>>>,
        id: Option<String>,
    ) -> Result<Self> {
        let token = CancellationToken::new();
        let id = id.filter(|id| !id.is_empty());
        if let Some(id) = &id {
            let mut active = registry.lock();
            if active.contains_key(id) {
                return Err(invalid(format!(
                    "duplicate operationId already in flight: {id}"
                )));
            }
            active.insert(id.clone(), token.clone());
        }
        Ok(Self {
            registry: registry.clone(),
            id,
            token,
        })
    }

    fn options(&self) -> OpOptions {
        OpOptions {
            cancel: Some(self.token.clone()),
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            self.registry.lock().remove(id);
        }
    }
}

/// The napi `GitEngine` class (§18.1). Node / UtilityProcess only: the
/// TypeScript wrapper lives in `@vibecook/mille/git`, never in a renderer
/// barrel (§18.3).
#[napi(js_name = "GitEngine")]
pub struct GitEngineJs {
    /// `None` after `dispose`. In-flight calls hold their own `Arc`, so the
    /// engine (its sessions, watchers and runtime) goes away when the last of
    /// them finishes.
    engine: RwLock<Option<Arc<GitEngine>>>,
    operations: Arc<Mutex<HashMap<String, CancellationToken>>>,
    subscriptions: Mutex<HashMap<u32, Subscription>>,
    next_subscription: AtomicU32,
}

impl GitEngineJs {
    fn engine(&self) -> Result<Arc<GitEngine>> {
        self.engine
            .read()
            .clone()
            .ok_or_else(|| invalid("GitEngine is disposed"))
    }

    fn remember(&self, subscription: Subscription) -> u32 {
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed) + 1;
        self.subscriptions.lock().insert(id, subscription);
        id
    }

    fn begin(&self, operation_id: Option<String>) -> Result<Operation> {
        Operation::begin(&self.operations, operation_id)
    }
}

#[napi]
impl GitEngineJs {
    /// Build an engine. Does no I/O beyond probing for a `git` binary; a
    /// probe that finds none leaves writes failing `ENOGIT` (§32.2).
    #[napi(constructor, catch_unwind)]
    pub fn new(options: GitEngineOptionsJs) -> Result<Self> {
        let engine = GitEngine::try_new(options.into_options()).map_err(git_error_to_napi)?;
        Ok(Self {
            engine: RwLock::new(Some(Arc::new(engine))),
            operations: Arc::new(Mutex::new(HashMap::new())),
            subscriptions: Mutex::new(HashMap::new()),
            next_subscription: AtomicU32::new(0),
        })
    }

    /// The git binary writes use, or `null` when none was found.
    #[napi(js_name = "gitPath", catch_unwind)]
    pub fn git_path(&self) -> Result<Option<String>> {
        Ok(self
            .engine()?
            .git_path()
            .map(|path| path.to_string_lossy().into_owned()))
    }

    /// Open a worktree (any directory under `allowedRoots`). Resolves to the
    /// identity's JSON; a non-repository opens as `kind: "none"`.
    #[napi(catch_unwind)]
    pub fn open<'env>(&self, env: &'env Env, worktree: String) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        env.spawn_future(async move {
            let identity = engine
                .open(Path::new(&worktree))
                .await
                .map_err(git_error_to_napi)?;
            to_json(&identity)
        })
    }

    /// Drop one `open` reference; the last one tears the session down.
    #[napi(catch_unwind)]
    pub fn close<'env>(&self, env: &'env Env, worktree: String) -> Result<PromiseRaw<'env, ()>> {
        let engine = self.engine()?;
        env.spawn_future(async move {
            engine
                .close(Path::new(&worktree))
                .await
                .map_err(git_error_to_napi)
        })
    }

    /// The identity of an open worktree, `HEAD` re-read. JSON.
    #[napi(catch_unwind)]
    pub fn identity(&self, worktree: String) -> Result<String> {
        let identity = self
            .engine()?
            .identity(Path::new(&worktree))
            .map_err(git_error_to_napi)?;
        to_json(&identity)
    }

    /// The settled status snapshot. JSON. Never fails for not-a-repo or a
    /// missing git (§23).
    #[napi(catch_unwind)]
    pub fn status<'env>(
        &self,
        env: &'env Env,
        worktree: String,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        env.spawn_future(async move {
            let snapshot = engine
                .status(Path::new(&worktree))
                .await
                .map_err(git_error_to_napi)?;
            to_json(&*snapshot)
        })
    }

    /// Run a full (`paths` null) or path-limited status now and resolve to
    /// the snapshot that carried it. Additive (README deviation 15).
    #[napi(catch_unwind)]
    pub fn refresh<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        paths: Option<Vec<String>>,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        env.spawn_future(async move {
            let snapshot = engine
                .refresh(Path::new(&worktree), paths)
                .await
                .map_err(git_error_to_napi)?;
            to_json(&*snapshot)
        })
    }

    /// Path hints (§18.1): **worktree-relative POSIX** paths, `null` for a
    /// full status. Synchronous and non-blocking — it only arms the session's
    /// debounce. Unknown worktrees and unsafe paths are ignored.
    #[napi(js_name = "hintPaths", catch_unwind)]
    pub fn hint_paths(&self, worktree: String, paths: Option<Vec<String>>) -> Result<()> {
        self.engine()?.hint_paths(Path::new(&worktree), paths);
        Ok(())
    }

    /// Pin a session's status backend: `"gix"` (default) or `"cli"` (§21.3).
    #[napi(js_name = "setStatusBackend", catch_unwind)]
    pub fn set_status_backend(&self, worktree: String, backend: String) -> Result<()> {
        let backend = match backend.as_str() {
            "gix" => StatusBackend::Gix,
            "cli" => StatusBackend::Cli,
            other => {
                return Err(invalid(format!(
                    "unknown status backend {other:?}; expected gix or cli"
                )))
            }
        };
        self.engine()?
            .set_status_backend(Path::new(&worktree), backend)
            .map_err(git_error_to_napi)
    }

    /// A one-path diff with caps (§13). JSON `DiffResult`.
    #[napi(catch_unwind)]
    pub fn diff<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        request: DiffRequestJs,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        let request = request
            .into_request(operation.token.clone())
            .map_err(git_error_to_napi)?;
        env.spawn_future(async move {
            let result = engine
                .diff(Path::new(&worktree), request)
                .await
                .map_err(git_error_to_napi)?;
            drop(operation);
            to_json(&result)
        })
    }

    /// A page of history (§14.1). JSON `LogPage`.
    #[napi(catch_unwind)]
    pub fn log<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        request: Option<LogRequestJs>,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        let request = request
            .unwrap_or(LogRequestJs {
                path: None,
                limit: None,
                skip: None,
                revision: None,
            })
            .into_request(operation.token.clone());
        env.spawn_future(async move {
            let page = engine
                .log(Path::new(&worktree), request)
                .await
                .map_err(git_error_to_napi)?;
            drop(operation);
            to_json(&page)
        })
    }

    /// The files a commit changed (§14.3). JSON `CommitFile[]`.
    #[napi(js_name = "commitFiles", catch_unwind)]
    pub fn commit_files<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        sha: String,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        env.spawn_future(async move {
            let files = engine
                .commit_files(Path::new(&worktree), &sha, operation.options())
                .await
                .map_err(git_error_to_napi)?;
            to_json(&files)
        })
    }

    /// `rev:path` bytes (§14.2), or `null` when it does not resolve or is at
    /// or above `hardRefuseBytes`.
    #[napi(js_name = "readObject", catch_unwind)]
    pub fn read_object<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        request: ReadObjectRequestJs,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, Option<Buffer>>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        let request = request.into_request(operation.token.clone());
        env.spawn_future(async move {
            let bytes = engine
                .read_object(Path::new(&worktree), request)
                .await
                .map_err(git_error_to_napi)?;
            drop(operation);
            Ok(bytes.map(Buffer::from))
        })
    }

    /// Working-tree bytes, realpath-contained (§14.2), or `null`.
    #[napi(js_name = "readWorktreeFile", catch_unwind)]
    pub fn read_worktree_file<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        path: String,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, Option<Buffer>>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        env.spawn_future(async move {
            let bytes = engine
                .read_worktree_file(Path::new(&worktree), &path, operation.options())
                .await
                .map_err(git_error_to_napi)?;
            Ok(bytes.map(Buffer::from))
        })
    }

    /// `git add` (§15.3).
    #[napi(catch_unwind)]
    pub fn stage<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        paths: Vec<String>,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        env.spawn_future(async move {
            engine
                .stage(Path::new(&worktree), &paths, operation.options())
                .await
                .map_err(git_error_to_napi)
        })
    }

    /// Unstage (§15.3).
    #[napi(catch_unwind)]
    pub fn unstage<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        paths: Vec<String>,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        env.spawn_future(async move {
            engine
                .unstage(Path::new(&worktree), &paths, operation.options())
                .await
                .map_err(git_error_to_napi)
        })
    }

    /// Restore **index** bytes to the worktree (§15.3, AC-017).
    #[napi(catch_unwind)]
    pub fn restore<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        paths: Vec<String>,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        env.spawn_future(async move {
            engine
                .restore(Path::new(&worktree), &paths, operation.options())
                .await
                .map_err(git_error_to_napi)
        })
    }

    /// Commit the include-set (§15.4); resolves to the new commit's hex id.
    #[napi(catch_unwind)]
    pub fn commit<'env>(
        &self,
        env: &'env Env,
        worktree: String,
        request: CommitRequestJs,
        operation_id: Option<String>,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        let operation = self.begin(operation_id)?;
        let request = request.into_request(operation.token.clone());
        env.spawn_future(async move {
            let sha = engine
                .commit(Path::new(&worktree), request)
                .await
                .map_err(git_error_to_napi);
            drop(operation);
            sha
        })
    }

    /// Every worktree of the repository, primary included (§16). JSON.
    #[napi(catch_unwind)]
    pub fn worktrees<'env>(
        &self,
        env: &'env Env,
        worktree: String,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        env.spawn_future(async move {
            let list = engine
                .worktrees(Path::new(&worktree))
                .await
                .map_err(git_error_to_napi)?;
            to_json(&list)
        })
    }

    /// The config that makes status cheap (§21.2). JSON object.
    #[napi(js_name = "advisePerformanceConfig", catch_unwind)]
    pub fn advise_performance_config<'env>(
        &self,
        env: &'env Env,
        worktree: String,
    ) -> Result<PromiseRaw<'env, String>> {
        let engine = self.engine()?;
        env.spawn_future(async move {
            let advice = engine
                .advise_performance_config(Path::new(&worktree))
                .await
                .map_err(git_error_to_napi)?;
            to_json(&advice)
        })
    }

    /// Trip the token of an in-flight call. `true` when one was found.
    #[napi(js_name = "cancelOperation", catch_unwind)]
    pub fn cancel_operation(&self, operation_id: String) -> bool {
        match self.operations.lock().get(&operation_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Subscribe to a worktree's published snapshots (`(json) => void`).
    /// `EINVAL` when the worktree is not open.
    #[napi(js_name = "onStatus", catch_unwind)]
    pub fn on_status(&self, worktree: String, listener: JsonListener) -> Result<u32> {
        let engine = self.engine()?;
        let worktree = PathBuf::from(worktree);
        let id = engine
            .subscribe_status(
                &worktree,
                Arc::new(move |snapshot| forward(&listener, &*snapshot)),
            )
            .map_err(git_error_to_napi)?;
        Ok(self.remember(Subscription::Status(worktree, id)))
    }

    /// Subscribe to a worktree's list of worktrees (`git worktree add/remove`).
    #[napi(js_name = "onWorktrees", catch_unwind)]
    pub fn on_worktrees(&self, worktree: String, listener: JsonListener) -> Result<u32> {
        let engine = self.engine()?;
        let worktree = PathBuf::from(worktree);
        let id = engine
            .subscribe_worktrees(&worktree, Arc::new(move |list| forward(&listener, list)))
            .map_err(git_error_to_napi)?;
        Ok(self.remember(Subscription::Worktrees(worktree, id)))
    }

    /// Subscribe to the engine's `'warning'` channel (§22.2).
    #[napi(js_name = "onWarning", catch_unwind)]
    pub fn on_warning(&self, listener: JsonListener) -> Result<u32> {
        let id = self
            .engine()?
            .subscribe_warnings(Arc::new(move |warning| forward(&listener, warning)));
        Ok(self.remember(Subscription::Warning(id)))
    }

    /// Remove a subscription. `false` for an unknown id; double-off is fine.
    #[napi(catch_unwind)]
    pub fn off(&self, subscription_id: u32) -> bool {
        let Some(subscription) = self.subscriptions.lock().remove(&subscription_id) else {
            return false;
        };
        let engine = self.engine.read().clone();
        subscription.cancel(engine.as_deref());
        true
    }

    /// Cancel every in-flight call, drop every subscription, and release the
    /// engine: its sessions, gitdir watches and runtime stop once the last
    /// in-flight call returns. Every later call is `EINVAL`. Idempotent.
    #[napi(catch_unwind)]
    pub fn dispose(&self) {
        let engine = self.engine.write().take();
        let subscriptions: Vec<Subscription> = self
            .subscriptions
            .lock()
            .drain()
            .map(|(_, subscription)| subscription)
            .collect();
        for token in self.operations.lock().values() {
            token.cancel();
        }
        for subscription in subscriptions {
            subscription.cancel(engine.as_deref());
        }
    }

    /// Hear a same-process `FileExplorer`'s watcher (SPEC mille-git §11.2,
    /// PR 6b). Returns a subscription id; `off(id)` unbinds, as does `dispose`.
    ///
    /// **Same-process only, and optional.** It needs the live explorer, so it
    /// is for a host that runs both in one process (playground's utility).
    /// Without it only the gitdir watch refreshes status — the shell client's
    /// freshness: staging, commits and branch switches, but not a worktree-only
    /// save. A host that cannot bind forwards routed paths through `hintPaths`
    /// instead; F3 needs one of the two (§18.1). `@vibecook/mille/git`'s
    /// `bindExplorer(transport, fx)` is the helper hosts call; it ends here.
    ///
    /// Why it is built the way it is:
    ///
    /// - It takes no `takePendingChanges`: that `ChangeSet` drain belongs to
    ///   the host's delta coalescer, and a second consumer would empty every
    ///   session's deltas (T-DELTA, AC-014). It listens instead, natively, on
    ///   the explorer's `'event'` and `'change:tree'` channels — with no JS
    ///   listener in the path, so a paused event loop cannot drop a hint.
    /// - `'event'` OS paths are the worktree-save signal. `ChangeTree` +
    ///   `pathForId` misses content-only saves and saves under folders the
    ///   walker never visited (T-HINT-LAZY), so it is only the supplement.
    /// - The listeners run on the watcher thread while `process_batch` holds
    ///   `policy_gate`. They only route and arm the session's debounce timer:
    ///   no status, no store `write_lock`, no `policy_gate`, no gix, no
    ///   runtime. The status run happens later, on the engine's own runtime.
    /// - Relatives, never OS paths, reach the queue: routing realpaths each
    ///   path and strips the longest matching checkout root (§11.2.1), so
    ///   `/var` vs `/private/var` and two checkouts side by side both hold.
    #[napi(js_name = "bindExplorer", catch_unwind)]
    pub fn bind_explorer(&self, explorer: &FileExplorer) -> Result<u32> {
        let engine = Arc::downgrade(&self.engine()?);
        let store: Weak<EntryStore> = Arc::downgrade(&explorer.store);

        let (event_engine, event_store) = (engine.clone(), store.clone());
        let event =
            explorer
                .events
                .subscribe_native_event(Arc::new(move |event: &FileSystemEventJs| {
                    let (Some(engine), Some(store)) =
                        (event_engine.upgrade(), event_store.upgrade())
                    else {
                        return;
                    };
                    contained(|| hint_event(&engine, &store, event));
                }));
        let change_tree = explorer.events.subscribe_native_change(
            Channel::ChangeTree,
            Arc::new(move |notice: &ChangeNoticeJs| {
                let (Some(engine), Some(store)) = (engine.upgrade(), store.upgrade()) else {
                    return;
                };
                contained(|| hint_change(&engine, &store, notice));
            }),
        );
        Ok(self.remember(Subscription::Explorer {
            bus: Arc::downgrade(&explorer.events),
            event,
            change_tree,
        }))
    }
}

/// A panic inside `mille-git`'s own code, reached from a sync entry point
/// (§18.4). Only with `--features panic-probe,git`.
#[cfg(feature = "panic-probe")]
#[napi(js_name = "__gitPanicProbeSync", catch_unwind)]
pub fn git_panic_probe_sync() -> Result<u32> {
    let engine = GitEngine::try_new(Default::default()).map_err(git_error_to_napi)?;
    Ok(engine.panic_probe_sync())
}

/// A panic inside a task on the git engine's runtime — where a gix panic in
/// a status or diff would happen — reached from an async entry point.
#[cfg(feature = "panic-probe")]
#[napi(js_name = "__gitPanicProbeAsync", catch_unwind)]
pub async fn git_panic_probe_async() -> Result<u32> {
    let engine = GitEngine::try_new(Default::default()).map_err(git_error_to_napi)?;
    engine
        .panic_probe_async()
        .await
        .map_err(git_error_to_napi)?;
    Ok(0)
}
