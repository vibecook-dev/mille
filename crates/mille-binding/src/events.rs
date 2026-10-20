//! EventBus — ThreadsafeFunction fan-out for FileExplorer events.
//!
//! SPEC §4.6 requires the engine to emit events to JS from background
//! threads (watcher callback, walker, ChangeSet coalescer). napi-rs's
//! `ThreadsafeFunction` is the only safe way to cross that boundary —
//! a raw `Function` is scope-bound to its originating JS call.
//!
//! Typing strategy: each channel gets its own typed `ThreadsafeFunction`
//! vec so payloads stay statically typed end-to-end. napi-rs 3.x encodes
//! "don't forward Results, just the bare value" as `CalleeHandled = false`
//! (what the SPEC notes called `ErrorStrategy::Fatal`). Using separate
//! typed vecs keeps each `emit_xxx` call zero-cost: no runtime type tag,
//! no serde detour, no `Box<dyn Any>`.
//!
//! The live watcher and store reconciliation pipeline feed this bus. The
//! smoke helper `emit_ready_for_tests` remains available for an isolated
//! end-to-end binding check.
//!
//! Native (Rust) listeners ride the same bus: `subscribe_native_event` /
//! `subscribe_native_change` register an `Arc<dyn Fn>` that runs
//! synchronously on the emitting thread, before any JS listener is queued.
//! The git engine is their one user (`git.rs`, `bind_explorer`, SPEC
//! mille-git §11.2).
//!
//! Concurrency: each channel is an independent `RwLock<Vec<...>>`, so an
//! emit on one channel never blocks subscription changes on another. A
//! snapshot-and-drop pattern in `emit_*` keeps the lock window short —
//! we clone the `Arc<ThreadsafeFunction<_>>`s out, release the read lock,
//! then call. TSFNs are `Send + Sync` and internally ref-counted by
//! napi-rs, so cloning is cheap.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use napi::bindgen_prelude::{JsValuesTupleIntoVec, Status, Unknown};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use parking_lot::RwLock;

use crate::types::{ChangeNoticeJs, ErrorPayloadJs, FileSystemEventJs, WarningPayloadJs};

/// All supported event channels, 1:1 with api.d.ts `on` overloads.
///
/// `Change` / `ChangeTree` / `ChangeDecorations` all carry the same
/// `ChangeNoticeJs` payload — the split is purely a routing hint so JS
/// consumers can subscribe narrowly. Tree-only edits skip the
/// decoration-only listeners, and vice versa.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Change,
    ChangeTree,
    ChangeDecorations,
    Event,
    Batch,
    Warning,
    Error,
    Ready,
}

impl Channel {
    /// Parse an api.d.ts channel name. Unknown names return `None` so the
    /// binding can reject them with EINVAL at the FFI edge.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "change" => Self::Change,
            "change:tree" => Self::ChangeTree,
            "change:decorations" => Self::ChangeDecorations,
            "event" => Self::Event,
            "batch" => Self::Batch,
            "warning" => Self::Warning,
            "error" => Self::Error,
            "ready" => Self::Ready,
            _ => return None,
        })
    }
}

/// Shorthand: Fatal-strategy TSFN (napi 3.x spelling).
///
/// `CalleeHandled = false` means `tsfn.call(T, mode)` — no `Result` wrap.
/// Phase 5 only uses the non-error path; any background-thread error
/// surfaces via an explicit emit on the `'error'` channel instead of
/// being tunneled through every TSFN's error lane.
type Tsfn<T> = ThreadsafeFunction<T, Unknown<'static>, T, Status, false>;

/// A registered listener is a (subscription-id, cloned TSFN) pair.
/// The id is the handle the JS side holds onto for `off(id)`; the TSFN
/// clone keeps the JS callback alive until the last emitter drops it.
type Slot<T> = (u64, Arc<Tsfn<T>>);

/// A Rust listener: called on the emitting thread with a borrow of the payload.
pub type NativeListener<T> = Arc<dyn Fn(&T) + Send + Sync>;

/// A Rust listener on the `'event'` channel (the watcher's thread, inside
/// `process_batch`).
pub type NativeEventListener = NativeListener<FileSystemEventJs>;

/// A Rust listener on one of the change channels.
pub type NativeChangeListener = NativeListener<ChangeNoticeJs>;

type NativeSlot<F> = (u64, F);

/// Internal event-bus. One `RwLock<Vec<Slot<_>>>` per channel so emits
/// on one channel never contend with subscriptions on another.
pub struct EventBus {
    change: RwLock<Vec<Slot<ChangeNoticeJs>>>,
    change_tree: RwLock<Vec<Slot<ChangeNoticeJs>>>,
    change_decorations: RwLock<Vec<Slot<ChangeNoticeJs>>>,
    event: RwLock<Vec<Slot<FileSystemEventJs>>>,
    batch: RwLock<Vec<Slot<Vec<FileSystemEventJs>>>>,
    warning: RwLock<Vec<Slot<WarningPayloadJs>>>,
    error: RwLock<Vec<Slot<ErrorPayloadJs>>>,
    ready: RwLock<Vec<Slot<()>>>,
    native_event: RwLock<Vec<NativeSlot<NativeEventListener>>>,
    native_change: RwLock<Vec<NativeSlot<NativeChangeListener>>>,
    native_change_tree: RwLock<Vec<NativeSlot<NativeChangeListener>>>,
    native_change_decorations: RwLock<Vec<NativeSlot<NativeChangeListener>>>,
    next_id: AtomicU64,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            change: RwLock::new(Vec::new()),
            change_tree: RwLock::new(Vec::new()),
            change_decorations: RwLock::new(Vec::new()),
            event: RwLock::new(Vec::new()),
            batch: RwLock::new(Vec::new()),
            warning: RwLock::new(Vec::new()),
            error: RwLock::new(Vec::new()),
            ready: RwLock::new(Vec::new()),
            native_event: RwLock::new(Vec::new()),
            native_change: RwLock::new(Vec::new()),
            native_change_tree: RwLock::new(Vec::new()),
            native_change_decorations: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ---- subscribe ----------------------------------------------------

    pub fn subscribe_change(&self, channel: Channel, tsfn: Tsfn<ChangeNoticeJs>) -> u64 {
        let id = self.alloc_id();
        let slot = Arc::new(tsfn);
        let list = match channel {
            Channel::Change => &self.change,
            Channel::ChangeTree => &self.change_tree,
            Channel::ChangeDecorations => &self.change_decorations,
            _ => unreachable!("subscribe_change called with non-change channel"),
        };
        list.write().push((id, slot));
        id
    }

    pub fn subscribe_event(&self, tsfn: Tsfn<FileSystemEventJs>) -> u64 {
        let id = self.alloc_id();
        self.event.write().push((id, Arc::new(tsfn)));
        id
    }

    pub fn subscribe_batch(&self, tsfn: Tsfn<Vec<FileSystemEventJs>>) -> u64 {
        let id = self.alloc_id();
        self.batch.write().push((id, Arc::new(tsfn)));
        id
    }

    pub fn subscribe_warning(&self, tsfn: Tsfn<WarningPayloadJs>) -> u64 {
        let id = self.alloc_id();
        self.warning.write().push((id, Arc::new(tsfn)));
        id
    }

    pub fn subscribe_error(&self, tsfn: Tsfn<ErrorPayloadJs>) -> u64 {
        let id = self.alloc_id();
        self.error.write().push((id, Arc::new(tsfn)));
        id
    }

    pub fn subscribe_ready(&self, tsfn: Tsfn<()>) -> u64 {
        let id = self.alloc_id();
        self.ready.write().push((id, Arc::new(tsfn)));
        id
    }

    // ---- native subscribe ---------------------------------------------

    /// Register a Rust listener on the `'event'` channel. Returns an id for
    /// [`unsubscribe`](Self::unsubscribe), from the same space as the TSFN ids.
    ///
    /// Why this exists (SPEC mille-git §11.2): the bus held only JS
    /// threadsafe functions, and cloning `Arc<EventBus>` creates no Rust
    /// listener — so nothing native could hear the watcher. The git engine
    /// needs to, and must not get it any other way:
    ///
    /// - The `ChangeSet` drain (`takePendingChanges`) is the host coalescer's.
    ///   A second consumer would empty the host's deltas.
    /// - `'event'` OS paths are the worktree-save signal. They are emitted for
    ///   every OS change even when the store did not move; `ChangeTree` +
    ///   `pathForId` misses content-only saves (same size and mtime) and saves
    ///   under lazy folders the walker never visited.
    /// - A JS hop (a TSFN, `NonBlocking`) is where a paused event loop drops
    ///   hints; this runs on the emitting thread instead.
    ///
    /// The cost of running there: the watcher's `process_batch` still holds
    /// `policy_gate`. A listener MUST only do cheap, non-blocking work — the
    /// git engine only arms a debounce — and never take `policy_gate`, the
    /// store's write lock, or block on a runtime. A listener that panics
    /// unwinds the emitting thread, so a listener guards itself.
    pub fn subscribe_native_event(&self, listener: NativeEventListener) -> u64 {
        let id = self.alloc_id();
        self.native_event.write().push((id, listener));
        id
    }

    /// Register a Rust listener on `'change'`, `'change:tree'` or
    /// `'change:decorations'`. Same contract as
    /// [`subscribe_native_event`](Self::subscribe_native_event): `ChangeTree`
    /// is emitted from the watcher under `policy_gate` and from JS-thread
    /// calls (`resync`, settings, roots), so the listener must stay cheap.
    pub fn subscribe_native_change(&self, channel: Channel, listener: NativeChangeListener) -> u64 {
        let id = self.alloc_id();
        let list = match channel {
            Channel::Change => &self.native_change,
            Channel::ChangeTree => &self.native_change_tree,
            Channel::ChangeDecorations => &self.native_change_decorations,
            _ => unreachable!("subscribe_native_change called with non-change channel"),
        };
        list.write().push((id, listener));
        id
    }

    // ---- unsubscribe --------------------------------------------------

    /// Remove the listener with the given id. `true` if a slot was
    /// removed — JS can log a stale-id warning when it's `false`, but
    /// double-off is idempotent by design (no error).
    pub fn unsubscribe(&self, id: u64) -> bool {
        fn drop_from<T: 'static + JsValuesTupleIntoVec>(
            list: &RwLock<Vec<Slot<T>>>,
            id: u64,
        ) -> bool {
            let mut guard = list.write();
            let before = guard.len();
            guard.retain(|(slot_id, _)| *slot_id != id);
            guard.len() != before
        }
        fn drop_native<F>(list: &RwLock<Vec<NativeSlot<F>>>, id: u64) -> bool {
            let mut guard = list.write();
            let before = guard.len();
            guard.retain(|(slot_id, _)| *slot_id != id);
            guard.len() != before
        }
        // Try each channel; the id space is global so at most one hits.
        drop_from(&self.change, id)
            || drop_from(&self.change_tree, id)
            || drop_from(&self.change_decorations, id)
            || drop_from(&self.event, id)
            || drop_from(&self.batch, id)
            || drop_from(&self.warning, id)
            || drop_from(&self.error, id)
            || drop_from(&self.ready, id)
            || drop_native(&self.native_event, id)
            || drop_native(&self.native_change, id)
            || drop_native(&self.native_change_tree, id)
            || drop_native(&self.native_change_decorations, id)
    }

    // ---- emit ---------------------------------------------------------
    //
    // Each `emit_*` clones out the TSFNs under a read lock, drops the lock,
    // then calls. Holding the lock across `.call()` would let a misbehaving
    // JS callback that re-enters `on`/`off` deadlock the bus.

    pub fn emit_change(&self, channel: Channel, payload: ChangeNoticeJs) {
        let (list, natives) = match channel {
            Channel::Change => (&self.change, &self.native_change),
            Channel::ChangeTree => (&self.change_tree, &self.native_change_tree),
            Channel::ChangeDecorations => {
                (&self.change_decorations, &self.native_change_decorations)
            }
            _ => return,
        };
        call_native(natives, &payload);
        call_all(list, payload, clone_change);
    }

    pub fn emit_event(&self, payload: FileSystemEventJs) {
        call_native(&self.native_event, &payload);
        call_all(&self.event, payload, clone_event);
    }

    pub fn emit_batch(&self, payload: Vec<FileSystemEventJs>) {
        call_all(&self.batch, payload, clone_batch);
    }

    pub fn emit_warning(&self, payload: WarningPayloadJs) {
        call_all(&self.warning, payload, clone_warning);
    }

    pub fn emit_error(&self, payload: ErrorPayloadJs) {
        call_all(&self.error, payload, clone_error);
    }

    pub fn emit_ready(&self) {
        let snapshot: Vec<Arc<Tsfn<()>>> = {
            let guard = self.ready.read();
            guard.iter().map(|(_, t)| t.clone()).collect()
        };
        for tsfn in snapshot {
            tsfn.call((), ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Native listeners: the same snapshot-and-drop as `call_all`, so a listener
/// that (un)subscribes cannot deadlock the bus. Runs before the TSFNs are
/// queued and regardless of whether any exist.
fn call_native<T>(list: &RwLock<Vec<NativeSlot<NativeListener<T>>>>, payload: &T) {
    let snapshot: Vec<NativeListener<T>> = {
        let guard = list.read();
        if guard.is_empty() {
            return;
        }
        guard.iter().map(|(_, listener)| listener.clone()).collect()
    };
    for listener in snapshot {
        listener(payload);
    }
}

/// Snapshot listeners under a read lock, drop the lock, then `.call()` on
/// each. `clone_payload` duplicates `payload` per-listener because TSFN
/// takes ownership of its argument on each call — and our payload types
/// (`#[napi(object)]` structs) aren't `Clone` by default.
///
/// `T: JsValuesTupleIntoVec` is required by napi-rs's TSFN struct bound
/// (`CallJsBackArgs: 'static + JsValuesTupleIntoVec = T`). Every
/// `#[napi(object)]` type satisfies it via the blanket `ToNapiValue` impl.
fn call_all<T, F>(list: &RwLock<Vec<Slot<T>>>, payload: T, clone_payload: F)
where
    F: Fn(&T) -> T,
    T: 'static + JsValuesTupleIntoVec,
{
    let snapshot: Vec<Arc<Tsfn<T>>> = {
        let guard = list.read();
        guard.iter().map(|(_, t)| t.clone()).collect()
    };
    if snapshot.is_empty() {
        return;
    }
    // Final listener gets the original payload; earlier ones get clones.
    // Micro-optimization so a single-listener emit pays no clone cost.
    let mut iter = snapshot.iter();
    let last = iter.next_back();
    for tsfn in iter {
        tsfn.call(
            clone_payload(&payload),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
    if let Some(tsfn) = last {
        tsfn.call(payload, ThreadsafeFunctionCallMode::NonBlocking);
    }
}

// ---- per-type clone helpers ------------------------------------------
//
// #[napi(object)] types don't derive Clone (napi-rs doesn't require it),
// so we spell out the field-by-field copy here. When the types grow in
// later phases, these stay local.

fn clone_change(c: &ChangeNoticeJs) -> ChangeNoticeJs {
    ChangeNoticeJs {
        tree_version: c.tree_version,
        decoration_version: c.decoration_version,
        tree_changed: c.tree_changed,
        decorations_changed: c.decorations_changed,
        changed_ids: c.changed_ids.clone(),
        child_set_changed: c.child_set_changed.clone(),
        decoration_changed_ids: c.decoration_changed_ids.clone(),
        coarse_subtrees: c.coarse_subtrees.clone(),
    }
}

pub(crate) fn clone_event(e: &FileSystemEventJs) -> FileSystemEventJs {
    FileSystemEventJs {
        kind: e.kind.clone(),
        id: e.id,
        parent_id: e.parent_id,
        old_parent_id: e.old_parent_id,
        new_parent_id: e.new_parent_id,
        old_name: e.old_name.clone(),
        new_name: e.new_name.clone(),
        entry: e.entry.as_ref().map(clone_entry),
        path: e.path.clone(),
        code: e.code.clone(),
        message: e.message.clone(),
        detail: e.detail.clone(),
    }
}

fn clone_entry(entry: &crate::types::EntryJs) -> crate::types::EntryJs {
    crate::types::EntryJs {
        id: entry.id,
        parent_id: entry.parent_id,
        name: entry.name.clone(),
        kind: entry.kind,
        size: entry.size,
        mtime_ms: entry.mtime_ms,
        ctime_ms: entry.ctime_ms,
        symlink_target_is_dir: entry.symlink_target_is_dir,
        path_segments: entry.path_segments.clone(),
        is_ignored: entry.is_ignored,
        is_readonly: entry.is_readonly,
        is_hidden: entry.is_hidden,
    }
}

// Signature matches `call_all`'s `Fn(&T) -> T` with `T = Vec<_>` (not a slice).
#[allow(clippy::ptr_arg)]
fn clone_batch(events: &Vec<FileSystemEventJs>) -> Vec<FileSystemEventJs> {
    events.iter().map(clone_event).collect()
}

fn clone_warning(w: &WarningPayloadJs) -> WarningPayloadJs {
    WarningPayloadJs {
        code: w.code.clone(),
        detail: w.detail.clone(),
    }
}

fn clone_error(e: &ErrorPayloadJs) -> ErrorPayloadJs {
    ErrorPayloadJs {
        code: e.code.clone(),
        message: e.message.clone(),
        path: e.path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_parse_covers_all_names() {
        assert_eq!(Channel::parse("change"), Some(Channel::Change));
        assert_eq!(Channel::parse("change:tree"), Some(Channel::ChangeTree));
        assert_eq!(
            Channel::parse("change:decorations"),
            Some(Channel::ChangeDecorations)
        );
        assert_eq!(Channel::parse("event"), Some(Channel::Event));
        assert_eq!(Channel::parse("batch"), Some(Channel::Batch));
        assert_eq!(Channel::parse("warning"), Some(Channel::Warning));
        assert_eq!(Channel::parse("error"), Some(Channel::Error));
        assert_eq!(Channel::parse("ready"), Some(Channel::Ready));
        assert_eq!(Channel::parse("nope"), None);
        assert_eq!(Channel::parse(""), None);
    }

    #[test]
    fn new_bus_has_no_listeners() {
        let bus = EventBus::new();
        // Unsubscribing an id that was never issued is a no-op.
        assert!(!bus.unsubscribe(1));
        // Emitting with no listeners is a no-op (doesn't panic).
        bus.emit_ready();
    }

    #[test]
    fn id_counter_monotonically_increases() {
        let bus = EventBus::new();
        let a = bus.alloc_id();
        let b = bus.alloc_id();
        let c = bus.alloc_id();
        assert!(a < b && b < c);
    }

    fn blank_event(kind: &str, path: &str) -> FileSystemEventJs {
        FileSystemEventJs {
            kind: kind.into(),
            id: None,
            parent_id: None,
            old_parent_id: None,
            new_parent_id: None,
            old_name: None,
            new_name: None,
            entry: None,
            path: Some(path.into()),
            code: None,
            message: None,
            detail: None,
        }
    }

    fn notice(changed: Vec<i64>) -> ChangeNoticeJs {
        ChangeNoticeJs {
            tree_version: 2,
            decoration_version: 0,
            tree_changed: true,
            decorations_changed: false,
            changed_ids: changed,
            child_set_changed: Vec::new(),
            decoration_changed_ids: Vec::new(),
            coarse_subtrees: Vec::new(),
        }
    }

    // T-NATIVE-SUB (SPEC mille-git §11.2 rule 1): a native subscriber sees
    // `'event'` and `ChangeTree` with zero JS listeners registered, and
    // unsubscribes by the shared id space. Like every test in this crate it
    // is type-checked by clippy but cannot link (cdylib against N-API); the
    // executed twin is `packages/mille/test/git-bind.test.mjs`.
    #[test]
    fn native_subscribers_hear_event_and_change_tree_without_tsfns() {
        use std::sync::Mutex;
        let bus = EventBus::new();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let events = seen.clone();
        let event_id = bus.subscribe_native_event(Arc::new(move |e: &FileSystemEventJs| {
            events.lock().unwrap().push(format!(
                "{}:{}",
                e.kind,
                e.path.clone().unwrap_or_default()
            ));
        }));
        let trees = seen.clone();
        let tree_id = bus.subscribe_native_change(
            Channel::ChangeTree,
            Arc::new(move |n: &ChangeNoticeJs| {
                trees
                    .lock()
                    .unwrap()
                    .push(format!("tree:{:?}", n.changed_ids));
            }),
        );
        bus.emit_event(blank_event("changed", "/r/a.ts"));
        bus.emit_change(Channel::ChangeTree, notice(vec![7]));
        bus.emit_change(Channel::Change, notice(vec![8]));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["changed:/r/a.ts".to_string(), "tree:[7]".to_string()]
        );
        assert!(bus.unsubscribe(event_id));
        assert!(bus.unsubscribe(tree_id));
        assert!(!bus.unsubscribe(tree_id));
        bus.emit_event(blank_event("changed", "/r/b.ts"));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    // TSFN-level subscribe/emit smoke tests require a live JS environment
    // and live in the Wave 7 Node integration harness. The pure-Rust
    // plumbing above is covered by these unit tests plus the binding-level
    // `emit_ready_for_tests` hook.
}
