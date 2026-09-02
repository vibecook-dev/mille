//! mille-core — pure-Rust core for the file-explorer library.
//!
//! This crate deliberately has **no** dependency on `napi`. The NAPI surface
//! lives in `mille-binding`. Keeping the engine napi-free lets us:
//!
//! - Unit-test on any target (no Node needed).
//! - Swap bindings later (WASM, CLI, alternate FFI).
//!
//! Module layout mirrors SPEC §4.1. Each module is stubbed and filled out
//! phase-by-phase per PLAN.md.

// TODO: Phase 1 — Entry, EntryKind, EntryId, Capability types
pub mod entry;
pub use entry::{Capability, Entry, EntryId, EntryKind};

// TODO: Phase 1 — Fs trait + InMemoryFs (Phase 2 adds RealFs)
pub mod fs;
pub use fs::{DirEntry, Fs, FsMetadata, InMemoryFs, RealFs};

// TODO: Phase 1–2 — EntryStore (sum-tree, id allocator, path index)
pub mod store;
pub use store::EntryStore;

// Phase 4 — ChangeSet accumulator consumed by the per-session delta diff.
pub mod changes;
pub use changes::ChangeSet;

// TODO: Phase 4 — writeSnapshot / eventsSince (here now so store can use it)
pub mod snapshot;
pub use snapshot::{
    StoreSnapshot, VisibilityPolicy, VisibleRowCount, VisibleRowOut, VisibleRowsQuery,
};

// Phase 4.3 + 4.4 — crash-resume disk IO (write_snapshot / read_snapshot)
// and the resume-diff walker (events_since / ResumeEvent).
pub mod resume;
pub use resume::{
    events_since, read_snapshot, write_snapshot, ResumeEvent, ResumeSnapshot, RootStat,
    CURRENT_FORMAT_VERSION,
};

// TODO: Phase 2 — jwalk-based walker + coalescer
pub mod walker;
pub use walker::{
    build_ignore_matcher_from_walk, populate_store, populate_store_with_provenance, walk,
    walk_batched, walk_with_ignore, walk_with_ignore_batched, SymlinkPolicy, WalkOptions,
    WalkedEntry,
};

// TODO: Phase 2 — ripgrep `ignore` crate wrapper
pub mod ignore;
pub use crate::ignore::{IgnoreMatcher, IGNORE_FILE_NAMES};

// Phase 3 — notify + debouncer + rename pairing + volatile throttling
pub mod watcher;
pub use watcher::{
    coalesce_events, FsChangeEvent, RawEvent, RenamePairer, Watcher, WatcherOptions,
};

// Phase 3.7 — volatile-subtree detection and throttling
pub mod volatile;
pub use volatile::VolatileTracker;

// Phase 3.9 — mutation intent cache (watcher echo suppression)
pub mod intent;
pub use intent::{IntentCache, IntentKind, MutationIntent};

// Phase 3.8 — Linux inotify limit auto-detect + hybrid-fallback advisor
pub mod inotify_limits;
pub use inotify_limits::{advise_budget, current_limits, InotifyLimits, WatchBudget};

// Phase 10 — nucleo fuzzy search adapter over EntryStore snapshots.
pub mod search;
pub mod sort;
pub use search::{search, SearchHit, SearchOptions, SearchScore};

// TODO: Phase 2 — compact-folders computation
pub mod compact;
pub use compact::{compact_chain_for, is_compacted_intermediate};

pub mod file_nesting;
pub use file_nesting::{FileNestingPolicy, FileNestingRule};

// TODO: Phase 1 — FxError + ErrorCode mapping
pub mod error;
pub use error::{ErrorCode, FxError};
