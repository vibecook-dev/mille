//! FileExplorer `#[napi]` class.
//!
//! Owns the entry store, live filesystem watcher, mutations, snapshots,
//! typed event fan-out, and deterministic shutdown for local mode.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

use napi::bindgen_prelude::{Buffer, Result, Status, Unknown};
use napi::threadsafe_function::ThreadsafeFunction;
use napi::Error;
use napi_derive::napi;

use mille_core::{
    Entry, EntryId, EntryKind, EntryStore, FxError, IntentCache, IntentKind, Watcher,
};

use crate::error::{fx_error_to_napi, io_to_fx};

/// Build a store entry from a walked entry.
///
/// The walker already stat'd the path, so re-reading it with `stat_to_entry`
/// costs a second syscall for data we hold — and, because that helper is
/// async, doing so forces the caller to await while holding `policy_gate`.
/// This is the synchronous equivalent; `is_ignored` / `is_excluded` are left
/// to the caller, which classifies under the gate.
fn entry_from_walked(walked: &mille_core::WalkedEntry, parent_id: Option<EntryId>) -> Entry {
    Entry {
        id: EntryId(0),
        parent_id,
        name: walked.name.clone(),
        kind: walked.kind,
        size: walked.size,
        mtime_ms: walked.mtime_ms,
        ctime_ms: walked.ctime_ms,
        symlink_target_is_dir: walked.symlink_target_is_dir,
        path_segments: None,
        is_ignored: false,
        is_excluded: false,
        is_readonly: walked.is_readonly,
        is_hidden: walked.is_hidden,
    }
}

/// Publish every directory whose immediate listing was actually covered by a
/// completed walk. Directories exactly on a bounded walk's depth frontier are
/// entries only—their children were not read—and ignored/excluded descendants
/// may have had traversal pruned. The walk root is the exception: expanding an
/// ignored folder explicitly is allowed and does read its direct children.
fn mark_walked_directory_listings(
    store: &EntryStore,
    walked: &[mille_core::WalkedEntry],
    walk_root: &Path,
    max_depth: Option<usize>,
    repository_ignore: Option<&mille_core::IgnoreMatcher>,
    excludes: Option<&mille_core::IgnoreMatcher>,
) -> std::result::Result<Vec<EntryId>, FxError> {
    let ids: Vec<EntryId> = walked
        .iter()
        .filter(|entry| {
            let directory_like =
                entry.kind == EntryKind::Directory || entry.symlink_target_is_dir == Some(true);
            // Smart/Never traversal records descendant directory symlinks but
            // does not read through them. Only an explicitly walked symlink
            // root can claim listing completion.
            let listing_covered = entry.kind == EntryKind::Directory
                || (entry.path == walk_root && entry.symlink_target_is_dir == Some(true));
            let below_frontier = max_depth.is_none_or(|limit| (entry.depth as usize) < limit);
            let traversal_complete = entry.path == walk_root
                || (!repository_ignore
                    .is_some_and(|matcher| matcher.is_ignored(&entry.path, directory_like))
                    && !excludes
                        .is_some_and(|matcher| matcher.is_ignored(&entry.path, directory_like)));
            listing_covered && below_frontier && traversal_complete
        })
        .filter_map(|entry| store.get_by_path(&entry.path).map(|stored| stored.id))
        .collect();
    store.mark_directories_children_loaded(&ids)
}
use crate::events::{Channel, EventBus};
use crate::journal::{
    capture_fs_identity, directory_is_empty, ensure_managed_recycle_base, path_is_under,
    pin_or_record_id, CreateIdentity, FsIdentity, JournalKind, OperationJournal, UndoDescriptorJs,
    UndoResultJs,
};
use crate::mutations::{
    kind_from_u8, resolve_entry_path, stat_to_entry, DeleteOptionsJs, TransferOptionsJs,
};
use crate::snapshot::MirrorSnapshot;
use crate::types::{
    ChangeNoticeJs, ChangeSetJs, EntryJs, ErrorPayloadJs, FileSystemEventJs, SearchHitJs,
    SearchOptionsJs, WarningPayloadJs,
};

#[napi(object)]
pub struct DestinationProbeJs {
    /// `"free"`, `"exists"`, or `"case_conflict"`.
    pub status: String,
    pub existing_name: Option<String>,
    pub path: Option<String>,
}

#[napi(object)]
pub struct FileNestingRuleJs {
    pub parent_pattern: String,
    pub child_patterns: Vec<String>,
}

#[napi(object)]
pub struct ProjectionSettingsJs {
    pub sort_by: String,
    pub case_sensitive: bool,
    pub locale: Option<String>,
    pub folders_on_top: bool,
    pub show_hidden_files: bool,
    pub show_ignored_files: bool,
    pub compact_folders: bool,
    pub exclude_globs: Vec<String>,
    pub file_nesting_rules: Vec<FileNestingRuleJs>,
}

/// Local-mode capability bitmask.
/// ReadWrite (1) | CaseSensitive (2) | Trash (8) | Watch (32) = 43.
/// Keep in sync with api.d.ts `Capability`.
const LOCAL_CAPABILITIES: u32 = 0b10_1011;

/// Options passed from JS when constructing FileExplorer. Mirrors
/// `ExplorerOptions` in api.d.ts. Optional fields are `Option<T>`.
#[napi(object)]
pub struct ExplorerOptionsJs {
    /// Workspace roots. Each is a `file://` URI — for Phase 5 we accept
    /// absolute filesystem paths as strings and resolve `file://` in TS.
    pub roots: Vec<String>,
    pub respect_ignore: Option<bool>,
    /// "smart" | "true" | "false". Defaults to "smart" when unset.
    pub follow_symlinks: Option<String>,
    pub walker_concurrency: Option<u32>,
    pub watch_debounce_ms: Option<u32>,
    pub compact_folders: Option<bool>,
    pub exclude_globs: Option<Vec<String>>,
    pub snapshot_path: Option<String>,
    pub max_cached_entries: Option<u32>,
    pub sort_by: Option<String>,
    pub case_sensitive: Option<bool>,
    pub locale: Option<String>,
    pub folders_on_top: Option<bool>,
    pub show_hidden_files: Option<bool>,
    pub show_ignored_files: Option<bool>,
    pub file_nesting_rules: Option<Vec<FileNestingRuleJs>>,
}

/// Resolved form of ExplorerOptionsJs with defaults applied.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct ResolvedOptions {
    pub respect_ignore: bool,
    pub follow_symlinks: mille_core::SymlinkPolicy,
    pub walker_concurrency: usize,
    pub watch_debounce_ms: u64,
    pub compact_folders: bool,
    pub snapshot_path: Option<PathBuf>,
    pub max_cached_entries: usize,
}

/// The in-process FileExplorer. Phase 5 builds it out method by method.
#[napi]
pub struct FileExplorer {
    pub(crate) store: Arc<EntryStore>,
    pub(crate) watcher: Arc<std::sync::Mutex<Option<Watcher>>>,
    pub(crate) intents: Arc<parking_lot::Mutex<IntentCache>>,
    disposed: AtomicBool,
    pub(crate) roots: Arc<parking_lot::RwLock<Vec<PathBuf>>>,
    pub(crate) options: ResolvedOptions,
    /// Runtime-configurable exclude rules shared by initial/lazy walks and
    /// watcher reconciliation.
    pub(crate) exclude_globs: Arc<parking_lot::RwLock<Vec<String>>>,
    /// Serializes classification against settings changes so a watcher batch
    /// using an old matcher cannot overwrite freshly reclassified entries.
    ///
    /// Mutation paths hold this across their filesystem `await`s on purpose:
    /// what must be atomic against a watcher batch is "touch the disk, then
    /// update the store", so releasing it in between would reopen the race
    /// this gate exists to close. Those sites therefore carry
    /// `#[allow(clippy::await_holding_lock)]`.
    ///
    /// It stays a `parking_lot::Mutex` rather than `tokio::sync::Mutex`
    /// because `watch_runtime` and `update_projection_settings` take it from
    /// synchronous contexts, where an async mutex would need `blocking_lock`
    /// and panic inside the runtime.
    ///
    /// The cost is that a worker thread blocks while another mutation
    /// finishes, rather than yielding. Mutations are serialized by design, so
    /// this bounds throughput rather than risking a deadlock on the
    /// multi-threaded runtime — but it is why the lint fires, and splitting
    /// the gate into a store-side and a disk-side lock would remove it.
    pub(crate) policy_gate: Arc<parking_lot::Mutex<()>>,
    /// Event fan-out for on('change' | 'event' | 'batch' | ...). Shared
    /// via Arc so background-thread emitters can clone one reference per
    /// producer.
    pub(crate) events: Arc<EventBus>,
    /// In-flight long operations keyed by host-supplied operation id.
    /// `cancelOperation` trips the matching token between recursive steps.
    pub(crate) operations: Arc<parking_lot::Mutex<HashMap<String, CancellationToken>>>,
    /// Undo stack for create / rename / move / soft-delete.
    pub(crate) journal: Arc<parking_lot::Mutex<OperationJournal>>,
}

#[napi]
impl FileExplorer {
    #[napi(constructor, catch_unwind)]
    pub fn new(mut options: ExplorerOptionsJs) -> Result<Self> {
        let roots: Vec<PathBuf> = options.roots.iter().map(PathBuf::from).collect();
        if roots.is_empty() {
            return Err(Error::from_reason(
                "FileExplorer requires at least one root",
            ));
        }
        for r in &roots {
            if !r.is_absolute() {
                return Err(Error::from_reason(format!(
                    "root must be absolute: {:?}",
                    r
                )));
            }
        }

        let case_sensitive = options.case_sensitive.unwrap_or(false);
        let file_nesting = mille_core::FileNestingPolicy::new(
            options
                .file_nesting_rules
                .take()
                .unwrap_or_default()
                .into_iter()
                .map(|rule| (rule.parent_pattern, rule.child_patterns)),
            case_sensitive,
        );
        let exclude_globs = options.exclude_globs.take().unwrap_or_default();
        let resolved = ResolvedOptions {
            respect_ignore: options.respect_ignore.unwrap_or(true),
            follow_symlinks: match options.follow_symlinks.as_deref() {
                Some("true") => mille_core::SymlinkPolicy::Always,
                Some("false") => mille_core::SymlinkPolicy::Never,
                _ => mille_core::SymlinkPolicy::Smart,
            },
            walker_concurrency: options
                .walker_concurrency
                .map(|n| n as usize)
                .unwrap_or_else(num_cpus_or_8),
            watch_debounce_ms: options.watch_debounce_ms.unwrap_or(75) as u64,
            compact_folders: options.compact_folders.unwrap_or(true),
            snapshot_path: options.snapshot_path.map(PathBuf::from),
            max_cached_entries: options
                .max_cached_entries
                .map(|n| n as usize)
                .unwrap_or(500_000),
        };

        let sibling_order = mille_core::sort::SiblingOrder::try_new(
            match options.sort_by.as_deref() {
                Some("type") => mille_core::sort::SortBy::Type,
                Some("modified") => mille_core::sort::SortBy::Modified,
                _ => mille_core::sort::SortBy::Name,
            },
            case_sensitive,
            options.folders_on_top.unwrap_or(true),
            options.locale.as_deref(),
        )
        .map_err(fx_error_to_napi)?;
        let visibility = mille_core::VisibilityPolicy {
            show_hidden_files: options.show_hidden_files.unwrap_or(true),
            show_ignored_files: options.show_ignored_files.unwrap_or(true),
        };

        Ok(Self {
            store: Arc::new(EntryStore::with_projection_settings(
                sibling_order,
                visibility,
                resolved.compact_folders,
                file_nesting,
            )),
            watcher: Arc::new(std::sync::Mutex::new(None)),
            intents: Arc::new(parking_lot::Mutex::new(IntentCache::new())),
            disposed: AtomicBool::new(false),
            roots: Arc::new(parking_lot::RwLock::new(roots)),
            options: resolved,
            exclude_globs: Arc::new(parking_lot::RwLock::new(exclude_globs)),
            policy_gate: Arc::new(parking_lot::Mutex::new(())),
            events: Arc::new(EventBus::new()),
            operations: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            journal: Arc::new(parking_lot::Mutex::new(OperationJournal::new())),
        })
    }

    /// Cancel an in-flight transfer identified by `operation_id`. Returns
    /// true when a matching operation was found and signalled.
    #[napi(js_name = "cancelOperation", catch_unwind)]
    pub fn cancel_operation(&self, operation_id: String) -> bool {
        let map = self.operations.lock();
        if let Some(token) = map.get(&operation_id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    fn begin_copy_progress(
        &self,
        options: Option<&TransferOptionsJs>,
    ) -> std::result::Result<Option<CopyProgressCtx>, FxError> {
        let Some(operation_id) = options.and_then(|o| o.operation_id.clone()) else {
            return Ok(None);
        };
        if operation_id.is_empty() {
            return Ok(None);
        }
        let report = options.and_then(|o| o.report_progress).unwrap_or(true);
        let token = CancellationToken::new();
        {
            let mut map = self.operations.lock();
            if map.contains_key(&operation_id) {
                return Err(FxError::InvalidInput(format!(
                    "duplicate operationId already in flight: {operation_id}"
                )));
            }
            map.insert(operation_id.clone(), token.clone());
        }
        Ok(Some(CopyProgressCtx {
            operation_id,
            token,
            events: Arc::clone(&self.events),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            report_every: 16,
            report_progress: report,
        }))
    }

    fn end_copy_progress(&self, progress: Option<&CopyProgressCtx>, status: &str) {
        let Some(progress) = progress else {
            return;
        };
        progress.emit_complete(status);
        let mut map = self.operations.lock();
        // Duplicate operationIds are rejected at begin, so removing by id is safe.
        map.remove(&progress.operation_id);
    }

    /// Local-mode baseline capabilities. Phase 5 wave 3+ will recompute
    /// this from provider registration once the dispatcher lands.
    #[napi(getter, catch_unwind)]
    pub fn capabilities(&self) -> u32 {
        LOCAL_CAPABILITIES
    }

    /// Phase 5.3 replaces with ChangeSet drain + TSFN invocation.
    /// For now: returns the current store tree-version.
    #[napi(js_name = "getTreeVersion", catch_unwind)]
    pub fn get_tree_version(&self) -> u32 {
        self.store.tree_version() as u32
    }

    /// Resolve an indexed entry identity to its exact absolute path. This is
    /// an internal fast path used by the TypeScript wrapper for lazy prefetch;
    /// unlike basename reconstruction it is unambiguous across workspace roots.
    #[napi(js_name = "pathForId", catch_unwind)]
    pub fn path_for_id(&self, id: i64) -> Option<String> {
        self.store
            .path_for_id(EntryId(id as u64))
            .map(|path| path.to_string_lossy().into_owned())
    }

    /// Resolve an absolute or workspace-relative path through the store's
    /// reverse path index. A path that exists below a configured root but is
    /// not indexed yet is hydrated one ancestor at a time, keeping lazy-mode
    /// reveal work proportional to path depth rather than workspace size.
    /// Relative paths are tested below every configured root; a leading root
    /// folder name is accepted as well.
    #[napi(js_name = "resolvePath", catch_unwind)]
    pub async fn resolve_path(&self, path: String) -> Result<Option<i64>> {
        let input = PathBuf::from(path);
        if input
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            return Ok(None);
        }

        let roots = self.roots.read().clone();
        let mut candidates: Vec<(PathBuf, PathBuf)> = Vec::new();
        for root in &roots {
            let candidate = if input.is_absolute() {
                if !input.starts_with(root) {
                    continue;
                }
                input.clone()
            } else if input.components().next().map(|part| part.as_os_str()) == root.file_name() {
                match root.parent() {
                    Some(parent) => parent.join(&input),
                    None => continue,
                }
            } else if input.as_os_str().is_empty() {
                root.clone()
            } else {
                root.join(&input)
            };
            if candidate.starts_with(root) {
                candidates.push((root.clone(), candidate));
            }
        }

        for (root, candidate) in candidates {
            if let Some(entry) = self.store.get_by_path(&candidate) {
                return Ok(Some(entry.id.raw() as i64));
            }

            match tokio::fs::symlink_metadata(&candidate).await {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(fx_error_to_napi(io_to_fx(error, candidate))),
            }

            let mut current = root.clone();
            let mut inserted_ids: Vec<i64> = Vec::new();
            let mut child_set_changed: Vec<i64> = Vec::new();
            let relative = candidate.strip_prefix(&root).map_err(|_| {
                Error::from_reason("resolved path escaped its configured workspace root")
            })?;

            // The root itself may not have been seeded yet (`initialWalk:
            // 'none'`). Insert it first, then hydrate exactly the requested
            // descendant chain.
            let root_name = root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.to_string_lossy().into_owned());
            let root_id = match self.store.get_by_path(&root) {
                Some(entry) => entry.id,
                None => {
                    let entry = stat_to_entry(&root, None, root_name)
                        .await
                        .map_err(fx_error_to_napi)?;
                    self.store
                        .insert(root.clone(), entry)
                        .map_err(fx_error_to_napi)
                        .inspect(|id| {
                            inserted_ids.push(id.raw() as i64);
                        })?
                }
            };
            let mut parent_id = Some(root_id);

            for part in relative.components() {
                let Component::Normal(name) = part else {
                    continue;
                };
                current.push(name);
                let id = match self.store.get_by_path(&current) {
                    Some(entry) => entry.id,
                    None => {
                        let entry =
                            stat_to_entry(&current, parent_id, name.to_string_lossy().into_owned())
                                .await
                                .map_err(fx_error_to_napi)?;
                        self.store
                            .insert(current.clone(), entry)
                            .map_err(fx_error_to_napi)
                            .inspect(|id| {
                                inserted_ids.push(id.raw() as i64);
                                if let Some(parent) = parent_id {
                                    child_set_changed.push(parent.raw() as i64);
                                }
                            })?
                    }
                };
                parent_id = Some(id);
            }

            if let Some(entry) = self.store.get_by_path(&candidate) {
                if inserted_ids.is_empty() {
                    return Ok(Some(entry.id.raw() as i64));
                }
                let _policy_guard = self.policy_gate.lock();
                self.reclassify_current_excludes()
                    .map_err(fx_error_to_napi)?;
                let version = self.store.tree_version() as u32;
                let notice = || ChangeNoticeJs {
                    tree_version: version,
                    decoration_version: 0,
                    tree_changed: true,
                    decorations_changed: false,
                    changed_ids: inserted_ids.clone(),
                    child_set_changed: child_set_changed.clone(),
                    decoration_changed_ids: Vec::new(),
                    coarse_subtrees: Vec::new(),
                };
                self.events.emit_change(Channel::Change, notice());
                self.events.emit_change(Channel::ChangeTree, notice());
                return Ok(Some(entry.id.raw() as i64));
            }
        }
        Ok(None)
    }

    /// Walk every configured root via `mille_core::walk` + `populate_store`
    /// and seed the EntryStore with the resulting entries. Returns the
    /// total number of entries inserted across all roots.
    ///
    /// `include_root: true` is required so the walk root itself lands in the
    /// store and receives an exact path-index identity. Without the
    /// root-as-entry, descendants cannot be linked to a configured workspace
    /// root and mutations fail with EINVAL.
    ///
    /// Deliberately NOT called in `new()` — construction stays cheap so
    /// consumers can attach sessions before scanning begins. Tests and
    /// host harnesses that need a populated tree (Phase 7.10's two-
    /// client mutation-ordering integration test, for example) call
    /// this explicitly after construction.
    ///
    /// The watcher starts before the walk, and path-idempotent insertion
    /// closes the race between initial scan results and live events.
    #[napi(js_name = "populateFromRoots", catch_unwind)]
    pub async fn populate_from_roots(&self) -> Result<u32> {
        use mille_core::{
            populate_store_with_provenance, walk, walk_with_ignore, IgnoreMatcher, WalkOptions,
        };

        self.ensure_watcher()?;
        let _policy_guard = self.policy_gate.lock();
        let exclude_globs = self.exclude_globs.read().clone();
        let roots = self.roots.read().clone();
        let mut total: u32 = 0;
        for root in &roots {
            let options = WalkOptions {
                max_depth: None,
                follow_symlinks: self.options.follow_symlinks,
                include_hidden: true,
                include_root: true,
                parallelism: self.options.walker_concurrency,
            };
            // v0.2 B3: when respect_ignore is on, apply gitignore rules
            // during the walk via process_read_dir. We seed the matcher
            // with the root's own .gitignore (read directly, no walk)
            // so pnpm-style `node_modules/` symlinks are blocked on the
            // very first read_dir call. Nested ignore files are added
            // dynamically inside walk_with_ignore as subdirectories are
            // streamed.
            let use_matcher = self.options.respect_ignore || !exclude_globs.is_empty();
            let walk_result: std::result::Result<_, FxError> = (|| {
                if use_matcher {
                    let mut traversal = IgnoreMatcher::new();
                    let mut repository_ignore = IgnoreMatcher::new();
                    let mut excludes = IgnoreMatcher::new();
                    if self.options.respect_ignore {
                        for name in mille_core::IGNORE_FILE_NAMES {
                            let candidate = root.join(name);
                            if candidate.is_file() {
                                let _ = traversal.add_from_file(&candidate);
                                let _ = repository_ignore.add_from_file(&candidate);
                            }
                        }
                    }
                    add_exclude_globs(&mut traversal, root, &exclude_globs)?;
                    add_exclude_globs(&mut excludes, root, &exclude_globs)?;
                    let w = walk_with_ignore(root, options, &traversal)?;
                    if self.options.respect_ignore {
                        for entry in &w {
                            if entry.path.file_name().is_some_and(|name| {
                                mille_core::IGNORE_FILE_NAMES
                                    .contains(&name.to_string_lossy().as_ref())
                            }) {
                                let _ = repository_ignore.add_from_file(&entry.path);
                            }
                        }
                    }
                    Ok((
                        w,
                        self.options.respect_ignore.then_some(repository_ignore),
                        (!exclude_globs.is_empty()).then_some(excludes),
                    ))
                } else {
                    Ok((walk(root, options)?, None, None))
                }
            })();
            let (walked, repository_ignore, excludes) = match walk_result {
                Ok(result) => result,
                Err(error) if matches!(&error, FxError::Io { .. }) => {
                    self.mark_configured_root_unavailable(root)
                        .map_err(fx_error_to_napi)?;
                    continue;
                }
                Err(error) => return Err(fx_error_to_napi(error)),
            };
            if let Some(walked_root) = walked.iter().find(|entry| entry.path == *root) {
                if let Some(existing) = self.store.get_by_path(root) {
                    if existing.kind == EntryKind::Unavailable {
                        let mut restored = entry_from_walked(walked_root, None);
                        restored.is_ignored = repository_ignore
                            .as_ref()
                            .is_some_and(|matcher| matcher.is_ignored(root, true));
                        restored.is_excluded = excludes
                            .as_ref()
                            .is_some_and(|matcher| matcher.is_ignored(root, true));
                        self.store
                            .update(existing.id, restored)
                            .map_err(fx_error_to_napi)?;
                    }
                }
            }
            let new_entries: Vec<_> = walked
                .iter()
                .filter(|entry| self.store.get_by_path(&entry.path).is_none())
                .cloned()
                .collect();
            let ids = populate_store_with_provenance(
                &self.store,
                root,
                &new_entries,
                repository_ignore.as_ref(),
                excludes.as_ref(),
            )
            .map_err(fx_error_to_napi)?;
            mark_walked_directory_listings(
                &self.store,
                &walked,
                root,
                None,
                repository_ignore.as_ref(),
                excludes.as_ref(),
            )
            .map_err(fx_error_to_napi)?;
            total = total.saturating_add(ids.len() as u32);
        }
        Ok(total)
    }

    /// Seed configured workspace roots as lazy directory placeholders without
    /// touching the filesystem. This is the first-paint contract for remote,
    /// unavailable, and very large workspaces: a host can handshake with stable
    /// root identities immediately, then refresh metadata and hydrate children
    /// independently.
    #[napi(js_name = "seedWorkspaceRoots", catch_unwind)]
    pub fn seed_workspace_roots(&self) -> Result<u32> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(Error::from_reason("FileExplorer is disposed"));
        }
        let roots = self.roots.read().clone();
        let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
        let prepared: Vec<(PathBuf, Entry)> = roots
            .iter()
            .map(|root| {
                let name = root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.to_string_lossy().into_owned());
                let mut entry = Entry {
                    id: EntryId(0),
                    parent_id: None,
                    is_hidden: name.starts_with('.'),
                    name,
                    kind: EntryKind::Directory,
                    size: 0,
                    mtime_ms: 0,
                    ctime_ms: 0,
                    symlink_target_is_dir: None,
                    path_segments: None,
                    is_ignored: false,
                    is_excluded: false,
                    is_readonly: false,
                };
                entry.is_excluded = Self::path_is_excluded(root, &entry, &exclude_matchers);
                (root.clone(), entry)
            })
            .collect();

        let _policy_guard = self.policy_gate.lock();
        let previous_version = self.store.tree_version();
        let (version, added_ids, removed_ids) = self
            .store
            .replace_roots(prepared)
            .map_err(fx_error_to_napi)?;
        if version != previous_version {
            let root_ids: Vec<EntryId> = self.store.snapshot().roots().to_vec();
            let mut changed_ids: Vec<i64> = removed_ids
                .iter()
                .chain(added_ids.iter())
                .chain(root_ids.iter())
                .map(|id| id.raw() as i64)
                .collect();
            changed_ids.sort_unstable();
            changed_ids.dedup();
            let notice = || ChangeNoticeJs {
                tree_version: version as u32,
                decoration_version: 0,
                tree_changed: true,
                decorations_changed: false,
                changed_ids: changed_ids.clone(),
                child_set_changed: Vec::new(),
                decoration_changed_ids: Vec::new(),
                coarse_subtrees: Vec::new(),
            };
            self.events.emit_change(Channel::Change, notice());
            self.events.emit_change(Channel::ChangeTree, notice());
        }
        Ok(version as u32)
    }

    /// Register recursive OS watches without walking workspace descendants.
    /// Host mode starts this independently from root seeding and expansion.
    #[napi(js_name = "startWatching", catch_unwind)]
    pub async fn start_watching(&self) -> Result<u32> {
        self.ensure_watcher()?;
        Ok(self.store.tree_version() as u32)
    }

    /// Re-stat configured roots without walking descendants.
    ///
    /// Missing or inaccessible roots remain visible as `Unavailable` with
    /// stable identity and no stale children. Restored roots keep that id and
    /// become lazy directories again. One public change notice covers the
    /// complete refresh, and the returned version is a synchronization point.
    #[napi(js_name = "refreshWorkspaceRoots", catch_unwind)]
    pub async fn refresh_workspace_roots(&self) -> Result<u32> {
        // Preserve the local FileExplorer contract: filesystem-facing calls
        // lazily enable live updates. Registration is root-bounded (NoCache),
        // so this no longer scans descendants or delays the host handshake.
        self.ensure_watcher()?;
        let roots = self.roots.read().clone();
        let mut observed = Vec::with_capacity(roots.len());
        for root in &roots {
            let name = root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.to_string_lossy().into_owned());
            let observation = match stat_to_entry(root, None, name).await {
                Ok(entry)
                    if entry.kind == EntryKind::Directory
                        || entry.symlink_target_is_dir == Some(true) =>
                {
                    match tokio::fs::read_dir(root).await {
                        Ok(_) => Ok(entry),
                        Err(error) => Err(io_to_fx(error, root.clone())),
                    }
                }
                other => other,
            };
            observed.push(observation);
        }

        let _policy_guard = self.policy_gate.lock();
        let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
        let previous_version = self.store.tree_version();
        let mut changed_ids = Vec::new();
        let mut child_set_changed = Vec::new();
        let mut became_unavailable = Vec::new();
        let mut became_available = Vec::new();
        for (root, observation) in roots.iter().zip(observed) {
            let existing = self.store.get_by_path(root);
            let available = observation.as_ref().is_ok_and(|entry| {
                entry.kind == EntryKind::Directory || entry.symlink_target_is_dir == Some(true)
            });
            if available {
                let mut entry = observation.expect("availability checked above");
                entry.is_excluded = Self::path_is_excluded(root, &entry, &exclude_matchers);
                if let Some(existing) = existing {
                    let was_unavailable = existing.kind == EntryKind::Unavailable;
                    if self
                        .store
                        .update(existing.id, entry)
                        .map_err(fx_error_to_napi)?
                    {
                        changed_ids.push(existing.id);
                    }
                    if was_unavailable {
                        became_available.push(root.clone());
                    }
                } else {
                    let id = self
                        .store
                        .insert(root.clone(), entry)
                        .map_err(fx_error_to_napi)?;
                    changed_ids.push(id);
                    became_available.push(root.clone());
                }
            } else if let Some(existing) = existing {
                let was_available = existing.kind != EntryKind::Unavailable;
                let (_, removed) = self
                    .store
                    .mark_root_unavailable(existing.id)
                    .map_err(fx_error_to_napi)?;
                if was_available {
                    changed_ids.push(existing.id);
                    changed_ids.extend(removed);
                    child_set_changed.push(existing.id);
                    became_unavailable.push(root.clone());
                }
            } else {
                self.mark_configured_root_unavailable(root)
                    .map_err(fx_error_to_napi)?;
                if let Some(inserted) = self.store.get_by_path(root) {
                    changed_ids.push(inserted.id);
                }
                became_unavailable.push(root.clone());
            }
        }

        let ordered_ids: Vec<EntryId> = roots
            .iter()
            .filter_map(|root| self.store.get_by_path(root).map(|entry| entry.id))
            .collect();
        if ordered_ids.len() == roots.len() {
            self.store
                .reorder_roots(&ordered_ids)
                .map_err(fx_error_to_napi)?;
        }

        let watcher_guard = match self.watcher.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(watcher) = watcher_guard.as_ref() {
            let options = mille_core::WatcherOptions {
                recursive: true,
                debounce_ms: Some(self.options.watch_debounce_ms),
            };
            for root in became_unavailable {
                let _ = watcher.unwatch(&root);
            }
            for root in became_available {
                if let Err(error) = watcher.watch(&root, options.clone()) {
                    self.events.emit_warning(WarningPayloadJs {
                        code: "WNOWATCH".into(),
                        detail: Some(format!("failed to watch {}: {error}", root.display())),
                    });
                }
            }
        }
        drop(watcher_guard);

        let version = self.store.tree_version();
        if version != previous_version {
            changed_ids.extend(ordered_ids);
            changed_ids.sort_unstable();
            changed_ids.dedup();
            let notice = || ChangeNoticeJs {
                tree_version: version as u32,
                decoration_version: 0,
                tree_changed: true,
                decorations_changed: false,
                changed_ids: changed_ids.iter().map(|id| id.raw() as i64).collect(),
                child_set_changed: child_set_changed.iter().map(|id| id.raw() as i64).collect(),
                decoration_changed_ids: Vec::new(),
                coarse_subtrees: Vec::new(),
            };
            self.events.emit_change(Channel::Change, notice());
            self.events.emit_change(Channel::ChangeTree, notice());
        }
        Ok(version as u32)
    }

    /// Authoritatively reconcile one known entry against disk.
    ///
    /// Directories reconcile their direct children by default or their complete
    /// known subtree when `recursive` is true. Files reconcile through their
    /// containing directory so atomic replacement, deletion, and metadata
    /// changes all use the same watcher-tested path.
    #[napi(js_name = "resync", catch_unwind)]
    pub async fn resync(&self, id: i64, recursive: Option<bool>) -> Result<u32> {
        self.ensure_watcher()?;
        let entry_id = EntryId(id as u64);
        let entry = self.store.get_by_id(entry_id).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} not found in snapshot",
                id
            )))
        })?;
        let path = self.resolve_path_for_id(id)?;
        let directory_like = entry.kind == EntryKind::Directory
            || entry.symlink_target_is_dir == Some(true)
            || entry.parent_id.is_none();
        let (scope, depth) = if directory_like {
            (
                path,
                if recursive.unwrap_or(false) {
                    None
                } else {
                    Some(1)
                },
            )
        } else {
            entry.parent_id.ok_or_else(|| {
                fx_error_to_napi(FxError::InvalidInput(format!(
                    "file id {} has no containing directory",
                    id
                )))
            })?;
            let parent_path = path.parent().map(Path::to_path_buf).ok_or_else(|| {
                fx_error_to_napi(FxError::InvalidInput(format!(
                    "file id {} has no containing directory path",
                    id
                )))
            })?;
            (parent_path, Some(1))
        };

        let _policy_guard = self.policy_gate.lock();
        let previous_version = self.store.tree_version();
        let outcome = crate::watch_runtime::reconcile_directory(
            &self.store,
            &self.watch_config(),
            &scope,
            depth,
        )
        .map_err(fx_error_to_napi)?;
        self.emit_reconcile_notice(previous_version, &outcome);
        Ok(self.store.tree_version() as u32)
    }

    /// Progressively reconcile one directory in bounded publication batches.
    /// The operation remains provisional until EOF; cancellation leaves any
    /// useful prefix cached but does not mark the directory complete.
    #[napi(js_name = "resyncProgressive", catch_unwind)]
    pub async fn resync_progressive(
        &self,
        id: i64,
        operation_id: String,
        batch_size: Option<u32>,
    ) -> Result<u32> {
        self.ensure_watcher()?;
        if operation_id.is_empty() {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "resyncProgressive requires a non-empty operationId".into(),
            )));
        }
        let entry = self.store.get_by_id(EntryId(id as u64)).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} not found in snapshot",
                id
            )))
        })?;
        if entry.kind != EntryKind::Directory && entry.symlink_target_is_dir != Some(true) {
            return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} is not a directory",
                id
            ))));
        }
        let path = self.resolve_path_for_id(id)?;
        let token = CancellationToken::new();
        {
            let mut operations = self.operations.lock();
            if operations.contains_key(&operation_id) {
                return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                    "duplicate operationId already in flight: {operation_id}"
                ))));
            }
            operations.insert(operation_id.clone(), token.clone());
        }

        let result: std::result::Result<u32, FxError> = (|| {
            let _policy_guard = self.policy_gate.lock();
            let mut previous_version = self.store.tree_version();
            crate::watch_runtime::reconcile_directory_progressive(
                &self.store,
                &self.watch_config(),
                &path,
                batch_size.unwrap_or(256).clamp(16, 4096) as usize,
                &token,
                |outcome| {
                    self.emit_reconcile_notice(previous_version, outcome);
                    previous_version = self.store.tree_version();
                },
            )?;
            Ok(self.store.tree_version() as u32)
        })();
        self.operations.lock().remove(&operation_id);
        result.map_err(fx_error_to_napi)
    }

    /// Authoritatively reconcile every configured root and all descendants.
    /// One change notice covers the complete operation.
    #[napi(js_name = "resyncWorkspace", catch_unwind)]
    pub async fn resync_workspace(&self) -> Result<u32> {
        self.ensure_watcher()?;
        let roots = self.roots.read().clone();
        let _policy_guard = self.policy_gate.lock();
        let previous_version = self.store.tree_version();
        let config = self.watch_config();
        let mut outcome = crate::watch_runtime::ReconcileOutcome::default();
        for root in roots {
            let root_outcome =
                crate::watch_runtime::reconcile_directory(&self.store, &config, &root, None)
                    .map_err(fx_error_to_napi)?;
            outcome.merge(root_outcome);
        }
        self.emit_reconcile_notice(previous_version, &outcome);
        Ok(self.store.tree_version() as u32)
    }

    /// Bounded-depth walk starting at `path`. We filter the walk output
    /// against the current snapshot before calling `populate_store` to avoid
    /// unnecessary updates; the store also enforces path idempotence. This
    /// keeps the method idempotent, which is what the host-side
    /// `setExpanded` handler needs when a client re-expands a folder the
    /// mirror already covers.
    ///
    /// `max_depth: 0` returns only the walk root; `max_depth: 1` returns
    /// the root plus direct children. `None` is unlimited (same as
    /// `populateFromRoots`) but is only reachable from callers that pass
    /// `null` explicitly — the TS wrapper always supplies a bound.
    ///
    /// `include_root: true` surfaces the folder itself as an Entry when
    /// the store doesn't already hold it; the `roots-only` initial-walk
    /// mode in commit B2.2 relies on this to seed each root with a real
    /// Entry record before any children are walked.
    #[napi(js_name = "populateFromPath", catch_unwind)]
    pub async fn populate_from_path(
        &self,
        path: String,
        max_depth: Option<u32>,
        include_root: Option<bool>,
    ) -> Result<u32> {
        use mille_core::{
            populate_store_with_provenance, walk, walk_with_ignore, IgnoreMatcher, WalkOptions,
        };

        self.ensure_watcher()?;
        let _policy_guard = self.policy_gate.lock();
        let exclude_globs = self.exclude_globs.read().clone();
        let p = PathBuf::from(&path);
        if !p.is_absolute() {
            return Err(Error::from_reason(format!(
                "path must be absolute: {}",
                path
            )));
        }

        // Only walk inside one of the configured roots. This both protects
        // the store from accidentally picking up out-of-workspace entries
        // and gives `populate_store` a predictable root context.
        let roots = self.roots.read().clone();
        let root = roots
            .iter()
            .find(|r| p == **r || p.starts_with(r))
            .cloned()
            .ok_or_else(|| {
                Error::from_reason(format!("path {:?} is not under any configured root", p))
            })?;

        let options = WalkOptions {
            max_depth: max_depth.map(|n| n as usize),
            follow_symlinks: self.options.follow_symlinks,
            include_hidden: true,
            include_root: include_root.unwrap_or(true),
            parallelism: self.options.walker_concurrency,
        };

        // v0.2 B3: respect_ignore now uses the symlink-aware walker so
        // lazy `setExpanded` walks on a pnpm monorepo don't accidentally
        // descend into the central store.
        let use_matcher = self.options.respect_ignore || !exclude_globs.is_empty();
        let walk_result: std::result::Result<_, FxError> = (|| {
            if use_matcher {
                let mut traversal = IgnoreMatcher::new();
                let mut repository_ignore = IgnoreMatcher::new();
                let mut excludes = IgnoreMatcher::new();
                add_exclude_globs(&mut traversal, &root, &exclude_globs)?;
                add_exclude_globs(&mut excludes, &root, &exclude_globs)?;
                // Seed with every ignore file on the path from the workspace
                // root down to the target directory — a lazy expand at
                // `repo/packages/foo` should still honor `repo/.gitignore`.
                let mut anchor = root.clone();
                for seg in p
                    .strip_prefix(&root)
                    .ok()
                    .into_iter()
                    .flat_map(|r| r.iter())
                {
                    if self.options.respect_ignore {
                        for name in mille_core::IGNORE_FILE_NAMES {
                            let candidate = anchor.join(name);
                            if candidate.is_file() {
                                let _ = traversal.add_from_file(&candidate);
                                let _ = repository_ignore.add_from_file(&candidate);
                            }
                        }
                    }
                    anchor = anchor.join(seg);
                }
                if self.options.respect_ignore {
                    for name in mille_core::IGNORE_FILE_NAMES {
                        let candidate = p.join(name);
                        if candidate.is_file() {
                            let _ = traversal.add_from_file(&candidate);
                            let _ = repository_ignore.add_from_file(&candidate);
                        }
                    }
                }
                let w = walk_with_ignore(&p, options, &traversal)?;
                if self.options.respect_ignore {
                    for entry in &w {
                        if entry.path.file_name().is_some_and(|name| {
                            mille_core::IGNORE_FILE_NAMES.contains(&name.to_string_lossy().as_ref())
                        }) {
                            let _ = repository_ignore.add_from_file(&entry.path);
                        }
                    }
                }
                Ok((
                    w,
                    self.options.respect_ignore.then_some(repository_ignore),
                    (!exclude_globs.is_empty()).then_some(excludes),
                ))
            } else {
                Ok((walk(&p, options)?, None, None))
            }
        })();
        let (walked, repository_ignore, excludes) = match walk_result {
            Ok(result) => result,
            Err(error) if p == root && matches!(&error, FxError::Io { .. }) => {
                self.mark_configured_root_unavailable(&root)
                    .map_err(fx_error_to_napi)?;
                return Ok(0);
            }
            Err(error) => return Err(fx_error_to_napi(error)),
        };
        if p == root {
            if let Some(walked_root) = walked.iter().find(|entry| entry.path == root) {
                if let Some(existing) = self.store.get_by_path(&root) {
                    if existing.kind == EntryKind::Unavailable {
                        let mut restored = entry_from_walked(walked_root, None);
                        restored.is_ignored = repository_ignore
                            .as_ref()
                            .is_some_and(|matcher| matcher.is_ignored(&root, true));
                        restored.is_excluded = excludes
                            .as_ref()
                            .is_some_and(|matcher| matcher.is_ignored(&root, true));
                        self.store
                            .update(existing.id, restored)
                            .map_err(fx_error_to_napi)?;
                    }
                }
            }
        }

        // Avoid rebuilding entries the store already knows about. `insert`
        // independently enforces path idempotence for watcher/walker races.
        let filtered: Vec<_> = walked
            .iter()
            .filter(|w| self.store.get_by_path(&w.path).is_none())
            .cloned()
            .collect();

        let ids = populate_store_with_provenance(
            &self.store,
            &root,
            &filtered,
            repository_ignore.as_ref(),
            excludes.as_ref(),
        )
        .map_err(fx_error_to_napi)?;
        mark_walked_directory_listings(
            &self.store,
            &walked,
            &p,
            max_depth.map(|depth| depth as usize),
            repository_ignore.as_ref(),
            excludes.as_ref(),
        )
        .map_err(fx_error_to_napi)?;
        Ok(ids.len() as u32)
    }

    /// Capture an immutable view of the tree. The inner Arc is stable
    /// between deltas, so identity comparison holds on the JS side.
    #[napi(js_name = "getSnapshot", catch_unwind)]
    pub fn get_snapshot(&self) -> MirrorSnapshot {
        MirrorSnapshot {
            inner: self.store.snapshot(),
        }
    }

    /// Atomically replace display-only settings and reclassify configured
    /// excludes without rebuilding or walking the explorer.
    #[napi(js_name = "updateProjectionSettings", catch_unwind)]
    pub fn update_projection_settings(&self, settings: ProjectionSettingsJs) -> Result<u32> {
        let exclude_globs = settings.exclude_globs;
        let _policy_guard = self.policy_gate.lock();
        let excludes_changed = self.exclude_globs.read().as_slice() != exclude_globs.as_slice();
        let exclude_matchers = if excludes_changed {
            let roots = self.roots.read().clone();
            Some(compile_exclude_matchers(&roots, &exclude_globs).map_err(fx_error_to_napi)?)
        } else {
            None
        };
        let previous_version = self.store.tree_version() as u32;
        let sibling_order = mille_core::sort::SiblingOrder::try_new(
            match settings.sort_by.as_str() {
                "type" => mille_core::sort::SortBy::Type,
                "modified" => mille_core::sort::SortBy::Modified,
                _ => mille_core::sort::SortBy::Name,
            },
            settings.case_sensitive,
            settings.folders_on_top,
            settings.locale.as_deref(),
        )
        .map_err(fx_error_to_napi)?;
        let visibility = mille_core::VisibilityPolicy {
            show_hidden_files: settings.show_hidden_files,
            show_ignored_files: settings.show_ignored_files,
        };
        let nesting = mille_core::FileNestingPolicy::new(
            settings
                .file_nesting_rules
                .into_iter()
                .map(|rule| (rule.parent_pattern, rule.child_patterns)),
            settings.case_sensitive,
        );
        let version = if let Some(exclude_matchers) = exclude_matchers.as_ref() {
            self.store.reconfigure_projection_with_exclusions(
                sibling_order,
                visibility,
                settings.compact_folders,
                nesting,
                |path, entry| {
                    exclude_matchers
                        .iter()
                        .filter(|(root, _)| path.starts_with(root))
                        .max_by_key(|(root, _)| root.components().count())
                        .is_some_and(|(_, matcher)| {
                            let directory_like = entry.kind == EntryKind::Directory
                                || entry.symlink_target_is_dir == Some(true);
                            matcher.is_ignored(path, directory_like)
                        })
                },
            )
        } else {
            self.store.reconfigure_projection(
                sibling_order,
                visibility,
                settings.compact_folders,
                nesting,
            )
        } as u32;
        *self.exclude_globs.write() = exclude_globs;
        if version == previous_version {
            return Ok(version);
        }
        let notice = || ChangeNoticeJs {
            tree_version: version,
            decoration_version: 0,
            tree_changed: true,
            decorations_changed: false,
            changed_ids: Vec::new(),
            child_set_changed: Vec::new(),
            decoration_changed_ids: Vec::new(),
            coarse_subtrees: Vec::new(),
        };
        self.events.emit_change(Channel::Change, notice());
        self.events.emit_change(Channel::ChangeTree, notice());
        Ok(version)
    }

    /// Atomically reorder the current workspace roots by stable EntryId.
    ///
    /// The input must be an exact permutation. No filesystem paths or entry
    /// records change; this publishes a new immutable snapshot solely so
    /// local subscribers and remote mirrors observe the new display order.
    #[napi(js_name = "reorderRoots", catch_unwind)]
    pub fn reorder_roots(&self, ids: Vec<i64>) -> Result<u32> {
        let roots: Vec<EntryId> = ids.into_iter().map(|id| EntryId(id as u64)).collect();
        let previous_version = self.store.tree_version() as u32;
        let version = self.store.reorder_roots(&roots).map_err(fx_error_to_napi)? as u32;
        if version == previous_version {
            return Ok(version);
        }
        let ordered_paths: Vec<PathBuf> = roots
            .iter()
            .filter_map(|id| self.store.path_for_id(*id))
            .collect();
        if ordered_paths.len() == roots.len() {
            *self.roots.write() = ordered_paths;
        }
        let changed_ids: Vec<i64> = roots.iter().map(|id| id.0 as i64).collect();
        let notice = || ChangeNoticeJs {
            tree_version: version,
            decoration_version: 0,
            tree_changed: true,
            decorations_changed: false,
            changed_ids: changed_ids.clone(),
            child_set_changed: Vec::new(),
            decoration_changed_ids: Vec::new(),
            coarse_subtrees: Vec::new(),
        };
        self.events.emit_change(Channel::Change, notice());
        self.events.emit_change(Channel::ChangeTree, notice());
        Ok(version)
    }

    /// Atomically replace the configured workspace roots in display order.
    ///
    /// New roots are seeded as entries only; descendants hydrate lazily on
    /// expansion. Removed roots lose their complete known subtrees. Inputs are
    /// validated and statted before watcher/store/config state changes.
    #[napi(js_name = "updateWorkspaceRoots", catch_unwind)]
    pub async fn update_workspace_roots(&self, roots: Vec<String>) -> Result<u32> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(Error::from_reason("FileExplorer is disposed"));
        }
        let paths: Vec<PathBuf> = roots.into_iter().map(PathBuf::from).collect();
        for path in &paths {
            if !path.is_absolute() {
                return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                    "workspace root must be absolute: {path:?}"
                ))));
            }
        }
        for (index, path) in paths.iter().enumerate() {
            if paths.iter().skip(index + 1).any(|other| {
                path == other
                    || path.starts_with(other.as_path())
                    || other.starts_with(path.as_path())
            }) {
                return Err(fx_error_to_napi(FxError::InvalidInput(
                    "workspace roots must be unique and non-overlapping".into(),
                )));
            }
        }

        let configured_before = self.roots.read().clone();
        let snapshot_before = self.store.snapshot();
        let displayed_before: Vec<PathBuf> = snapshot_before
            .roots()
            .iter()
            .filter_map(|id| self.store.path_for_id(*id))
            .collect();
        if paths == configured_before && paths == displayed_before {
            return Ok(snapshot_before.tree_version() as u32);
        }

        // Filesystem I/O finishes before the policy gate blocks watcher
        // reconciliation. This is also the failure-atomic validation pass.
        let mut prepared = Vec::with_capacity(paths.len());
        for path in &paths {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            let mut entry = stat_to_entry(path, None, name)
                .await
                .map_err(fx_error_to_napi)?;
            if entry.kind != EntryKind::Directory && entry.symlink_target_is_dir != Some(true) {
                let error = std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "workspace root is not a directory",
                );
                return Err(fx_error_to_napi(io_to_fx(error, path.clone())));
            }
            entry.parent_id = None;
            prepared.push((path.clone(), entry));
        }

        // An already-indexed child cannot also become a root without
        // reparenting the outer workspace. Overlapping roots are intentionally
        // rejected in this slice so identity and navigation paths stay exact.
        for path in &paths {
            if let Some(existing) = self.store.get_by_path(path) {
                if existing.parent_id.is_some() || !snapshot_before.roots().contains(&existing.id) {
                    return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                        "workspace root overlaps an indexed descendant: {path:?}"
                    ))));
                }
            }
        }

        let exclude_globs = self.exclude_globs.read().clone();
        let exclude_matchers =
            compile_exclude_matchers(&paths, &exclude_globs).map_err(fx_error_to_napi)?;
        for (path, entry) in &mut prepared {
            entry.is_excluded = Self::path_is_excluded(path, entry, &exclude_matchers);
        }

        let added_paths: Vec<PathBuf> = paths
            .iter()
            .filter(|path| !configured_before.contains(path))
            .cloned()
            .collect();
        let removed_paths: Vec<PathBuf> = configured_before
            .iter()
            .filter(|path| !paths.contains(path))
            .cloned()
            .collect();
        let watch_options = mille_core::WatcherOptions {
            recursive: true,
            debounce_ms: Some(self.options.watch_debounce_ms),
        };

        let _policy_guard = self.policy_gate.lock();
        let watcher_guard = match self.watcher.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut watched_added: Vec<PathBuf> = Vec::new();
        let mut watched_removed: Vec<PathBuf> = Vec::new();
        if let Some(watcher) = watcher_guard.as_ref() {
            for path in &added_paths {
                if let Err(error) = watcher.watch(path, watch_options.clone()) {
                    for added in &watched_added {
                        let _ = watcher.unwatch(added);
                    }
                    return Err(fx_error_to_napi(error));
                }
                watched_added.push(path.clone());
            }
            for path in &removed_paths {
                if let Err(error) = watcher.unwatch(path) {
                    for removed in &watched_removed {
                        let _ = watcher.watch(removed, watch_options.clone());
                    }
                    for added in &watched_added {
                        let _ = watcher.unwatch(added);
                    }
                    return Err(fx_error_to_napi(error));
                }
                watched_removed.push(path.clone());
            }
        }

        let previous_version = self.store.tree_version() as u32;
        let (version, added_ids, removed_ids) = match self.store.replace_roots(prepared) {
            Ok(result) => result,
            Err(error) => {
                if let Some(watcher) = watcher_guard.as_ref() {
                    for removed in &watched_removed {
                        let _ = watcher.watch(removed, watch_options.clone());
                    }
                    for added in &watched_added {
                        let _ = watcher.unwatch(added);
                    }
                }
                return Err(fx_error_to_napi(error));
            }
        };
        *self.roots.write() = paths;
        drop(watcher_guard);

        let version = version as u32;
        if version == previous_version {
            return Ok(version);
        }
        let current_root_ids: Vec<i64> = self
            .store
            .snapshot()
            .roots()
            .iter()
            .map(|id| id.raw() as i64)
            .collect();
        let mut changed_ids: Vec<i64> = removed_ids
            .iter()
            .chain(added_ids.iter())
            .map(|id| id.raw() as i64)
            .collect();
        changed_ids.extend(current_root_ids);
        changed_ids.sort_unstable();
        changed_ids.dedup();
        let notice = || ChangeNoticeJs {
            tree_version: version,
            decoration_version: 0,
            tree_changed: true,
            decorations_changed: false,
            changed_ids: changed_ids.clone(),
            child_set_changed: Vec::new(),
            decoration_changed_ids: Vec::new(),
            coarse_subtrees: Vec::new(),
        };
        self.events.emit_change(Channel::Change, notice());
        self.events.emit_change(Channel::ChangeTree, notice());
        Ok(version)
    }

    /// Drain the store's pending ChangeSet, atomically resetting it. Called
    /// once per coalescer tick (Phase 7.6) to feed the per-session delta
    /// diff. Empty ChangeSets are cheap — fields are zero-length vecs and
    /// `to_version == from_version`.
    #[napi(js_name = "takePendingChanges", catch_unwind)]
    pub fn take_pending_changes(&self) -> ChangeSetJs {
        let cs = self.store.take_pending_changes();
        ChangeSetJs::from_core(&cs)
    }

    /// Create a file or directory under `parent_id`. Phase 5 scope: leaf only.
    /// Fails with EEXIST when the destination already exists (never truncates).
    #[napi(catch_unwind)]
    pub async fn create(&self, parent_id: i64, name: String, kind: u8) -> Result<EntryJs> {
        let kind_enum = kind_from_u8(kind).map_err(fx_error_to_napi)?;
        let parent_eid = EntryId(parent_id as u64);

        let parent_path = resolve_entry_path(&self.store, parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "parent id {} not found in snapshot",
                parent_id
            )))
        })?;

        validate_entry_name(&name).map_err(fx_error_to_napi)?;
        let new_path = parent_path.join(&name);

        // Refuse to clobber an existing path (including case-only siblings).
        match tokio::fs::symlink_metadata(&new_path).await {
            Ok(_) => {
                return Err(fx_error_to_napi(io_to_fx(
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "destination already exists",
                    ),
                    new_path,
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(fx_error_to_napi(io_to_fx(error, new_path))),
        }
        if find_case_conflict(&parent_path, &name)
            .await
            .map_err(fx_error_to_napi)?
            .is_some()
        {
            return Err(fx_error_to_napi(io_to_fx(
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "destination already exists with different case",
                ),
                new_path,
            )));
        }

        match kind_enum {
            EntryKind::Directory => tokio::fs::create_dir(&new_path)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, new_path.clone())))?,
            EntryKind::File => {
                // create_new refuses to truncate an existing file.
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&new_path)
                    .await
                    .map_err(|e| fx_error_to_napi(io_to_fx(e, new_path.clone())))?;
            }
            _ => unreachable!("kind_from_u8 already rejected non-File/Directory"),
        }
        self.record_intent(new_path.clone(), IntentKind::Create);

        // Stat and insert into the store. Capture filesystem identity for
        // undo-create: device+inode (or Windows file index) plus size /
        // timestamps so same-size replacements cannot be mistaken for the
        // original empty create.
        let mut entry = stat_to_entry(&new_path, Some(parent_eid), name)
            .await
            .map_err(fx_error_to_napi)?;
        let kind_u8 = if kind_enum == EntryKind::Directory {
            1
        } else {
            0
        };
        let fs = capture_fs_identity(&new_path, kind_u8)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, new_path.clone())))?;
        // Pin the created file so its inode cannot be recycled while the undo
        // entry lives; see `CreateIdentity::pin`. Directories rely on the
        // emptiness check instead, and a failure to open leaves `None`, which
        // makes undo refuse rather than trust a possibly-reused inode.
        let (pin, pinned_id) = if kind_u8 == 0 {
            match tokio::fs::File::open(&new_path).await {
                Ok(file) => pin_or_record_id(file.into_std().await),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };
        let identity = CreateIdentity {
            entry_id: EntryId(0), // filled after insert
            path: new_path.clone(),
            fs,
            pin,
            pinned_id,
        };
        let _policy_guard = self.policy_gate.lock();
        let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
        entry.is_excluded = Self::path_is_excluded(&new_path, &entry, &exclude_matchers);
        let new_id = self
            .store
            .insert(new_path, entry)
            .map_err(fx_error_to_napi)?;

        // Re-read the freshly inserted Entry so id/summary state is current.
        let arc = self
            .store
            .get_by_id(new_id)
            .ok_or_else(|| Error::from_reason("insert succeeded but id vanished"))?;
        let mut identity = identity;
        identity.entry_id = new_id;
        self.journal.lock().push_create(identity);
        Ok(EntryJs::from_core(arc.as_ref()))
    }

    /// Rename an entry in place while preserving its identity and any known
    /// descendant identities.
    #[napi(catch_unwind)]
    pub async fn rename(&self, id: i64, new_name: String) -> Result<EntryJs> {
        let eid = EntryId(id as u64);
        let old_path = resolve_entry_path(&self.store, eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} not found in snapshot",
                id
            )))
        })?;
        let parent_path = old_path.parent().map(|p| p.to_path_buf()).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "cannot rename a root: id {}",
                id
            )))
        })?;
        validate_entry_name(&new_name).map_err(fx_error_to_napi)?;
        let new_path = parent_path.join(&new_name);
        if new_path == old_path {
            let arc = self
                .store
                .get_by_id(eid)
                .ok_or_else(|| Error::from_reason("rename target vanished"))?;
            return Ok(EntryJs::from_core(arc.as_ref()));
        }
        // Never overwrite an existing destination.
        match tokio::fs::symlink_metadata(&new_path).await {
            Ok(_) => {
                return Err(fx_error_to_napi(io_to_fx(
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "rename destination already exists",
                    ),
                    new_path,
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(fx_error_to_napi(io_to_fx(error, new_path))),
        }
        if find_case_conflict(&parent_path, &new_name)
            .await
            .map_err(fx_error_to_napi)?
            .is_some()
        {
            return Err(fx_error_to_napi(io_to_fx(
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "rename destination already exists with different case",
                ),
                new_path,
            )));
        }

        tokio::fs::rename(&old_path, &new_path)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, old_path.clone())))?;
        self.record_intent(old_path.clone(), IntentKind::Rename);
        self.record_intent(new_path.clone(), IntentKind::Rename);

        // Only the store move and reclassification need the gate; the journal
        // capture below is filesystem I/O that no matcher looks at.
        {
            let _policy_guard = self.policy_gate.lock();
            self.store
                .rename(eid, new_path.clone())
                .map_err(fx_error_to_napi)?;
            self.reclassify_current_excludes()
                .map_err(fx_error_to_napi)?;
        }

        let kind_u8 = self
            .store
            .get_by_id(eid)
            .map(|e| {
                if e.kind == EntryKind::Directory || e.symlink_target_is_dir == Some(true) {
                    1u8
                } else {
                    0u8
                }
            })
            .unwrap_or(0);
        let fs = capture_fs_identity(&new_path, kind_u8)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, new_path.clone())))?;
        self.journal.lock().push_rename(eid, old_path, new_path, fs);
        let arc = self
            .store
            .get_by_id(eid)
            .ok_or_else(|| Error::from_reason("rename succeeded but id vanished"))?;
        Ok(EntryJs::from_core(arc.as_ref()))
    }

    /// Move an entry under a new parent, optionally renaming in flight.
    #[napi(js_name = "move", catch_unwind)]
    pub async fn move_entry(
        &self,
        id: i64,
        new_parent_id: i64,
        new_name: Option<String>,
        options: Option<TransferOptionsJs>,
    ) -> Result<EntryJs> {
        let eid = EntryId(id as u64);
        let new_parent_eid = EntryId(new_parent_id as u64);
        let (allow_cross_root, collision) =
            transfer_policy(options.as_ref()).map_err(fx_error_to_napi)?;

        let old_path = resolve_entry_path(&self.store, eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} not found in snapshot",
                id
            )))
        })?;
        let new_parent_path = resolve_entry_path(&self.store, new_parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "new_parent id {} not found in snapshot",
                new_parent_id
            )))
        })?;
        let snapshot = self.store.snapshot();
        let source = snapshot.get(eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} vanished mid-move",
                id
            )))
        })?;
        if source.parent_id.is_none() {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "workspace roots cannot be moved".into(),
            )));
        }
        let destination_parent = snapshot.get(new_parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "new_parent id {} vanished mid-move",
                new_parent_id
            )))
        })?;
        if destination_parent.kind != EntryKind::Directory
            && destination_parent.symlink_target_is_dir != Some(true)
        {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "move destination is not a directory".into(),
            )));
        }
        if source.kind == EntryKind::Directory && new_parent_path.starts_with(&old_path) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot move a directory into its own subtree".into(),
            )));
        }
        let roots = self.roots.read().clone();
        let source_root = configured_root_for_path(&roots, &old_path);
        let destination_root = configured_root_for_path(&roots, &new_parent_path);
        if source_root != destination_root && !allow_cross_root {
            return Err(fx_error_to_napi(FxError::Unsupported(
                "cross-root move requires { crossRoot: true }".into(),
            )));
        }

        let desired_name = new_name.unwrap_or_else(|| {
            old_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
                .unwrap_or_default()
        });
        let destination_root = destination_root.ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(
                "move destination is not under a configured root".into(),
            ))
        })?;
        // Containment: parent must resolve inside the workspace root, and the
        // destination name must be a single non-traversing component.
        let (contained_parent, root_canon) =
            assert_parent_contained(&new_parent_path, &destination_root)
                .await
                .map_err(fx_error_to_napi)?;
        let desired_path = join_under_root(
            &new_parent_path,
            &contained_parent,
            &desired_name,
            &root_canon,
        )
        .map_err(fx_error_to_napi)?;
        if paths_equal(&desired_path, &old_path) || new_parent_path.join(&desired_name) == old_path
        {
            return Ok(EntryJs::from_core(source.as_ref()));
        }
        let dest = resolve_transfer_destination(&new_parent_path, &desired_name, collision)
            .await
            .map_err(fx_error_to_napi)?;
        // Re-check containment after collision renaming.
        join_under_root(&new_parent_path, &contained_parent, &dest.name, &root_canon)
            .map_err(fx_error_to_napi)?;
        if paths_equal(&dest.path, &old_path) {
            return Ok(EntryJs::from_core(source.as_ref()));
        }
        if matches!(dest.action, DestAction::Skip) {
            // Destination already holds the name; treat as successful no-op.
            return Ok(EntryJs::from_core(source.as_ref()));
        }

        let progress = self
            .begin_copy_progress(options.as_ref())
            .map_err(fx_error_to_napi)?;
        if let Some(ref prog) = progress {
            // Single-step renames still expose a cooperative cancel point.
            if let Err(error) = prog.check() {
                self.end_copy_progress(progress.as_ref(), "cancelled");
                return Err(fx_error_to_napi(error));
            }
            prog.total.store(1, Ordering::Relaxed);
        }

        // Merge for directories preserves destination-only children.
        if matches!(dest.action, DestAction::Merge) && source.kind == EntryKind::Directory {
            let dest_meta = tokio::fs::symlink_metadata(&dest.path)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, dest.path.clone())))?;
            if !dest_meta.is_dir() || dest_meta.file_type().is_symlink() {
                self.end_copy_progress(progress.as_ref(), "failed");
                return Err(fx_error_to_napi(FxError::InvalidInput(
                    "collision: merge requires a real directory destination".into(),
                )));
            }
            if let Err(error) =
                merge_move_tree_on_disk_with_progress(&old_path, &dest.path, progress.as_ref())
                    .await
            {
                let status = if matches!(error, FxError::Cancelled) {
                    "cancelled"
                } else {
                    "failed"
                };
                self.end_copy_progress(progress.as_ref(), status);
                // Partial merge-move: reindex both trees so the store matches disk.
                let config = self.watch_config();
                let _policy_guard = self.policy_gate.lock();
                let _ = crate::watch_runtime::reconcile_directory(
                    &self.store,
                    &config,
                    &dest.path,
                    None,
                );
                if old_path.exists() {
                    let _ = crate::watch_runtime::reconcile_directory(
                        &self.store,
                        &config,
                        &old_path,
                        None,
                    );
                } else if let Some(parent) = old_path.parent() {
                    let _ = crate::watch_runtime::reconcile_directory(
                        &self.store,
                        &config,
                        parent,
                        Some(1),
                    );
                }
                return Err(fx_error_to_napi(error));
            }
            self.end_copy_progress(progress.as_ref(), "completed");
            self.record_intent(old_path.clone(), IntentKind::Delete);
            self.record_intent(dest.path.clone(), IntentKind::Rename);
            let _policy_guard = self.policy_gate.lock();
            // Source identity is gone; drop it and reconcile the merged tree.
            let _ = self.store.remove_subtree(eid);
            let config = self.watch_config();
            crate::watch_runtime::reconcile_directory(&self.store, &config, &dest.path, None)
                .map_err(fx_error_to_napi)?;
            self.reclassify_current_excludes()
                .map_err(fx_error_to_napi)?;
            self.journal.lock().record_non_undoable(
                "move",
                "Move (merge)".into(),
                "directory merge-move cannot be fully reversed",
            );
            let arc = self.store.get_by_path(&dest.path).ok_or_else(|| {
                Error::from_reason("merge-move succeeded but destination vanished")
            })?;
            return Ok(EntryJs::from_core(arc.as_ref()));
        }

        let mut destroyed_destination = false;
        if matches!(dest.action, DestAction::Overwrite | DestAction::Merge) {
            // File merge falls back to overwrite. Never delete the source.
            if paths_equal(&dest.path, &old_path) {
                self.end_copy_progress(progress.as_ref(), "completed");
                return Ok(EntryJs::from_core(source.as_ref()));
            }
            if let Some(existing) = self.store.get_by_path(&dest.path) {
                let existing_id = existing.id;
                if existing_id == eid {
                    self.end_copy_progress(progress.as_ref(), "completed");
                    return Ok(EntryJs::from_core(source.as_ref()));
                }
                let existing_kind = existing.kind;
                let _policy_guard = self.policy_gate.lock();
                if existing_kind == EntryKind::Directory {
                    let _ = self.store.remove_subtree(existing_id);
                } else {
                    let _ = self.store.remove(existing_id);
                }
            }
            if tokio::fs::symlink_metadata(&dest.path).await.is_ok() {
                destroyed_destination = true;
                remove_path_best_effort(&dest.path).await;
            }
        }
        let new_path = dest.path;

        if let Some(ref prog) = progress {
            if let Err(error) = prog.check() {
                self.end_copy_progress(progress.as_ref(), "cancelled");
                return Err(fx_error_to_napi(error));
            }
        }

        if let Err(error) = tokio::fs::rename(&old_path, &new_path).await {
            self.end_copy_progress(progress.as_ref(), "failed");
            if error.raw_os_error() == Some(18) {
                return Err(fx_error_to_napi(FxError::Unsupported(
                    "cross-device move requires copy/delete fallback".into(),
                )));
            }
            return Err(fx_error_to_napi(io_to_fx(error, old_path.clone())));
        }
        self.record_intent(old_path.clone(), IntentKind::Rename);
        self.record_intent(new_path.clone(), IntentKind::Rename);

        // The gate covers the store move; the rollback below is filesystem
        // I/O and must not run while holding it.
        let moved = {
            let _policy_guard = self.policy_gate.lock();
            self.store.rename(eid, new_path.clone())
        };
        if let Err(error) = moved {
            let _ = tokio::fs::rename(&new_path, &old_path).await;
            self.end_copy_progress(progress.as_ref(), "failed");
            return Err(fx_error_to_napi(error));
        }
        if let Some(ref prog) = progress {
            prog.bump(&new_path);
        }
        self.end_copy_progress(progress.as_ref(), "completed");
        {
            let _policy_guard = self.policy_gate.lock();
            self.reclassify_current_excludes()
                .map_err(fx_error_to_napi)?;
        }

        // Capture identity before taking the journal lock: the stat is I/O and
        // nothing else may be blocked on the journal while it runs.
        let moved_identity = if destroyed_destination {
            None
        } else {
            let kind_u8 = if source.kind == EntryKind::Directory
                || source.symlink_target_is_dir == Some(true)
            {
                1u8
            } else {
                0u8
            };
            // Capture after rename so identity matches the object at new_path.
            Some(match capture_fs_identity(&new_path, kind_u8).await {
                Ok(fs) => fs,
                Err(e) => {
                    // Journal still records a best-effort identity from store.
                    let _ = e;
                    FsIdentity {
                        size: source.size,
                        mtime_ms: source.mtime_ms,
                        ctime_ms: source.ctime_ms,
                        kind: kind_u8,
                        dev: 0,
                        ino: 0,
                    }
                }
            })
        };
        {
            let mut journal = self.journal.lock();
            match moved_identity {
                None => {
                    // Destination content was permanently removed — not reverseable.
                    journal.record_non_undoable(
                        "move",
                        format!(
                            "Move {} (overwrite)",
                            old_path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ),
                        "destination was permanently overwritten and cannot be restored",
                    );
                }
                Some(fs) => {
                    journal.push_move(eid, old_path, new_path.clone(), fs);
                }
            }
        }
        let arc = self
            .store
            .get_by_id(eid)
            .ok_or_else(|| Error::from_reason("move succeeded but id vanished"))?;
        Ok(EntryJs::from_core(arc.as_ref()))
    }

    /// Delete an entry. Directories with children require recursive: true.
    ///
    /// By default (`trash: true`) the path is soft-deleted into a managed
    /// recycle directory **outside** the workspace (`$TMPDIR/mille-recycle/…`)
    /// so `undo()` can restore it without polluting the tree. Pass
    /// `trash: false` for a permanent delete (reported as non-undoable).
    #[napi(catch_unwind)]
    pub async fn delete(&self, id: i64, options: Option<DeleteOptionsJs>) -> Result<()> {
        let eid = EntryId(id as u64);
        let recursive = options.as_ref().and_then(|o| o.recursive).unwrap_or(false);
        let use_trash = options.as_ref().and_then(|o| o.trash).unwrap_or(true);

        let snap = self.store.snapshot();
        let path = resolve_entry_path(&self.store, eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} not found in snapshot",
                id
            )))
        })?;
        let entry = snap.get(eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} vanished mid-delete",
                id
            )))
        })?;
        if entry.parent_id.is_none() {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "workspace roots cannot be deleted".into(),
            )));
        }
        let parent_id = entry.parent_id.unwrap();
        let name = entry.name.clone();
        let was_dir =
            entry.kind == EntryKind::Directory || entry.symlink_target_is_dir == Some(true);
        let size = entry.size;
        let mtime_ms = entry.mtime_ms;
        let ctime_ms = entry.ctime_ms;

        if was_dir && snap.has_children(eid) && !recursive {
            return Err(fx_error_to_napi(FxError::Unsupported(format!(
                "delete of non-empty directory {:?} requires recursive: true",
                path
            ))));
        }

        if use_trash {
            let roots = self.roots.read().clone();
            let root = configured_root_for_path(&roots, &path).ok_or_else(|| {
                fx_error_to_napi(FxError::InvalidInput(
                    "delete path is not under a configured root".into(),
                ))
            })?;
            // Recycle lives outside the workspace so it never appears in the tree.
            // ensure_managed_recycle_base refuses symlink-hijacked bases, creates
            // real directories without following links, canonicalizes, and
            // permission-restricts (0o700 on Unix).
            let base = ensure_managed_recycle_base(&root, &roots)
                .map_err(|msg| fx_error_to_napi(FxError::InternalBug(msg)))?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let recycle_dir = base.join(format!("{stamp}"));
            // Create the stamp bucket as a real directory (not create_dir_all
            // through a possible symlink race on base — base is already verified).
            tokio::fs::create_dir(&recycle_dir)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, recycle_dir.clone())))?;
            // Containment: recycle_dir must stay under canonical base.
            if !path_is_under(&recycle_dir, &base) {
                let _ = tokio::fs::remove_dir(&recycle_dir).await;
                return Err(fx_error_to_napi(FxError::InvalidInput(
                    "recycle path escaped managed base".into(),
                )));
            }
            let recycle_path = recycle_dir.join(&name);
            if !path_is_under(&recycle_path, &base) {
                let _ = tokio::fs::remove_dir(&recycle_dir).await;
                return Err(fx_error_to_napi(FxError::InvalidInput(
                    "recycle destination escaped managed base".into(),
                )));
            }
            tokio::fs::rename(&path, &recycle_path)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
            self.record_intent(path.clone(), IntentKind::Delete);
            self.record_intent(recycle_path.clone(), IntentKind::Create);
            // Capture identity of the recycled object so undo can refuse a
            // replaced payload (e.g. someone swaps the recycle file with an
            // impostor before undo).
            let kind_u8 = if was_dir { 1u8 } else { 0u8 };
            let fs = capture_fs_identity(&recycle_path, kind_u8)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, recycle_path.clone())))?;
            if recursive || was_dir {
                let _ = self.store.remove_subtree(eid);
            } else {
                let _ = self.store.remove(eid);
            }
            self.journal.lock().push_soft_delete(
                path,
                recycle_path,
                parent_id,
                name,
                was_dir,
                recursive,
                fs,
            );
            let _ = (size, mtime_ms, ctime_ms);
            return Ok(());
        }

        // Permanent delete — not undoable.
        if was_dir {
            if recursive {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
            } else {
                tokio::fs::remove_dir(&path)
                    .await
                    .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
            }
        } else {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
        }
        self.record_intent(path.clone(), IntentKind::Delete);
        if recursive || was_dir {
            let _ = self.store.remove_subtree(eid);
        } else {
            let _ = self.store.remove(eid);
        }
        self.journal.lock().record_non_undoable(
            "delete",
            format!("Delete {name}"),
            "permanent delete cannot be undone",
        );
        Ok(())
    }

    /// True when at least one undoable operation is on the journal stack.
    #[napi(js_name = "canUndo", catch_unwind)]
    pub fn can_undo(&self) -> bool {
        self.journal.lock().can_undo()
    }

    /// Describe the next **undoable** operation without applying it.
    #[napi(js_name = "peekUndo", catch_unwind)]
    pub fn peek_undo(&self) -> Option<UndoDescriptorJs> {
        self.journal.lock().peek().map(|e| e.descriptor())
    }

    /// Most recent mutation descriptor, including non-undoable permanent deletes
    /// and overwrite-moves (`undoable: false` with a reason).
    #[napi(js_name = "lastMutation", catch_unwind)]
    pub fn last_mutation(&self) -> Option<UndoDescriptorJs> {
        self.journal.lock().last_mutation()
    }

    /// Reverse the most recent undoable mutation. The journal entry is removed
    /// only after a fully successful reverse mutation.
    #[napi(catch_unwind)]
    pub async fn undo(&self) -> Result<UndoResultJs> {
        let entry = self
            .journal
            .lock()
            .peek_owned()
            .ok_or_else(|| fx_error_to_napi(FxError::InvalidInput("nothing to undo".into())))?;
        let descriptor = entry.descriptor();
        let result = match &entry.kind {
            JournalKind::Create { identity } => {
                let mut r = self.undo_create(identity).await?;
                r.id = descriptor.id;
                r.kind = descriptor.kind.clone();
                r.label = descriptor.label.clone();
                r
            }
            JournalKind::Rename {
                entry_id,
                old_path,
                new_path,
                fs,
            }
            | JournalKind::Move {
                entry_id,
                old_path,
                new_path,
                fs,
            } => {
                self.undo_rename_or_move(*entry_id, old_path, new_path, fs, &descriptor)
                    .await?
            }
            JournalKind::SoftDelete {
                original_path,
                recycle_path,
                parent_id,
                name,
                was_dir,
                recursive,
                fs,
            } => {
                self.undo_soft_delete(
                    original_path,
                    recycle_path,
                    *parent_id,
                    name,
                    *was_dir,
                    *recursive,
                    fs,
                    &descriptor,
                )
                .await?
            }
        };
        // Only pop after full success.
        let _ = self.journal.lock().pop();
        Ok(result)
    }

    async fn undo_create(&self, identity: &CreateIdentity) -> Result<UndoResultJs> {
        let path = &identity.path;
        // Identity gate: store must still track this entry_id at path, and
        // on-disk filesystem identity must still match the journaled create.
        let store_ok = self
            .store
            .get_by_id(identity.entry_id)
            .map(|_| {
                resolve_entry_path(&self.store, identity.entry_id)
                    .map(|p| p == *path)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !store_ok {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot undo create: path no longer matches the original entry".into(),
            )));
        }
        if !path.exists() {
            // Path already gone — clear store identity only.
            let _ = self.store.remove_subtree(identity.entry_id);
            let _ = self.store.remove(identity.entry_id);
            return Ok(UndoResultJs {
                id: 0,
                kind: "create".into(),
                label: String::new(),
                entry_id: None,
            });
        }
        let meta = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
        let is_dir = meta.is_dir() && !meta.file_type().is_symlink();
        if is_dir != (identity.fs.kind == 1) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot undo create: entry kind changed".into(),
            )));
        }
        let disk = capture_fs_identity(path, identity.fs.kind)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
        if !identity.pinned_matches_disk(&disk) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot undo create: on-disk identity no longer matches the original entry".into(),
            )));
        }
        if is_dir {
            // Refuse to recursively destroy descendant content added after create.
            let empty = directory_is_empty(path)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
            if !empty {
                return Err(fx_error_to_napi(FxError::InvalidInput(
                    "cannot undo create: directory is no longer empty".into(),
                )));
            }
            tokio::fs::remove_dir(path)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
        } else {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, path.clone())))?;
        }
        self.record_intent(path.clone(), IntentKind::Delete);
        let _ = self.store.remove_subtree(identity.entry_id);
        let _ = self.store.remove(identity.entry_id);
        Ok(UndoResultJs {
            id: 0,
            kind: "create".into(),
            label: String::new(),
            entry_id: None,
        })
    }

    async fn undo_rename_or_move(
        &self,
        entry_id: EntryId,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        expected_fs: &FsIdentity,
        descriptor: &UndoDescriptorJs,
    ) -> Result<UndoResultJs> {
        if !new_path.exists() {
            return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                "cannot undo: {:?} no longer exists",
                new_path
            ))));
        }
        if old_path.exists() {
            return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                "cannot undo: {:?} already exists",
                old_path
            ))));
        }
        // Require the store still tracks this identity at new_path.
        let current = resolve_entry_path(&self.store, entry_id);
        if current.as_deref() != Some(new_path) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot undo: entry identity no longer matches journaled path".into(),
            )));
        }
        // Require the on-disk object at new_path is still the same file/dir we
        // renamed/moved — not an unrelated same-name replacement.
        let disk = capture_fs_identity(new_path, expected_fs.kind)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, new_path.to_path_buf())))?;
        if !expected_fs.matches_disk(&disk) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot undo: on-disk identity at destination no longer matches the original entry"
                    .into(),
            )));
        }
        if let Some(parent) = old_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, parent.to_path_buf())))?;
        }
        tokio::fs::rename(new_path, old_path)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, new_path.to_path_buf())))?;
        self.record_intent(new_path.to_path_buf(), IntentKind::Rename);
        self.record_intent(old_path.to_path_buf(), IntentKind::Rename);
        let _policy_guard = self.policy_gate.lock();
        self.store
            .rename(entry_id, old_path.to_path_buf())
            .map_err(fx_error_to_napi)?;
        self.reclassify_current_excludes()
            .map_err(fx_error_to_napi)?;
        Ok(UndoResultJs {
            id: descriptor.id,
            kind: descriptor.kind.clone(),
            label: descriptor.label.clone(),
            entry_id: Some(entry_id.raw() as i64),
        })
    }

    // Restoring a soft-delete needs the whole journaled tuple; bundling it
    // into a struct would just move the same fields behind another name.
    #[allow(clippy::too_many_arguments)]
    async fn undo_soft_delete(
        &self,
        original_path: &std::path::Path,
        recycle_path: &std::path::Path,
        parent_id: EntryId,
        name: &str,
        was_dir: bool,
        _recursive: bool,
        expected_fs: &FsIdentity,
        descriptor: &UndoDescriptorJs,
    ) -> Result<UndoResultJs> {
        let _ = name;
        if !recycle_path.exists() {
            return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                "cannot undo delete: recycle path missing {:?}",
                recycle_path
            ))));
        }
        if original_path.exists() {
            return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                "cannot undo delete: {:?} already exists",
                original_path
            ))));
        }
        // Refuse to restore a replaced recycle payload (e.g. impostor file
        // written over the soft-deleted object before undo).
        let kind_u8 = if was_dir { 1u8 } else { 0u8 };
        let disk = capture_fs_identity(recycle_path, kind_u8)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, recycle_path.to_path_buf())))?;
        if disk.kind != expected_fs.kind {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot undo delete: recycle payload kind no longer matches".into(),
            )));
        }
        if !expected_fs.matches_disk(&disk) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot undo delete: recycle payload identity no longer matches the deleted entry"
                    .into(),
            )));
        }
        if let Some(parent) = original_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| fx_error_to_napi(io_to_fx(e, parent.to_path_buf())))?;
        }
        tokio::fs::rename(recycle_path, original_path)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, recycle_path.to_path_buf())))?;
        self.record_intent(recycle_path.to_path_buf(), IntentKind::Delete);
        self.record_intent(original_path.to_path_buf(), IntentKind::Create);
        if let Some(bucket) = recycle_path.parent() {
            let _ = tokio::fs::remove_dir(bucket).await;
        }
        let mut entry = stat_to_entry(
            original_path,
            Some(parent_id),
            original_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("restored")
                .to_string(),
        )
        .await
        .map_err(fx_error_to_napi)?;
        // Classify and insert under the gate, as `create` does: the stat above
        // is I/O, and only the matcher read plus the insert must be atomic
        // against a settings change.
        let new_id = {
            let _policy_guard = self.policy_gate.lock();
            let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
            entry.is_excluded = Self::path_is_excluded(original_path, &entry, &exclude_matchers);
            self.store
                .insert(original_path.to_path_buf(), entry)
                .map_err(fx_error_to_napi)?
        };
        if tokio::fs::metadata(original_path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            let config = self.watch_config();
            let _ = crate::watch_runtime::reconcile_directory(
                &self.store,
                &config,
                original_path,
                None,
            );
        }
        self.reclassify_current_excludes()
            .map_err(fx_error_to_napi)?;
        Ok(UndoResultJs {
            id: descriptor.id,
            kind: descriptor.kind.clone(),
            label: descriptor.label.clone(),
            entry_id: Some(new_id.raw() as i64),
        })
    }

    /// Copy a file or directory under a new parent. Directories copy
    /// recursively with content preserved.
    #[napi(catch_unwind)]
    pub async fn copy(
        &self,
        id: i64,
        new_parent_id: i64,
        new_name: Option<String>,
        options: Option<TransferOptionsJs>,
    ) -> Result<EntryJs> {
        let eid = EntryId(id as u64);
        let new_parent_eid = EntryId(new_parent_id as u64);
        let (allow_cross_root, collision) =
            transfer_policy(options.as_ref()).map_err(fx_error_to_napi)?;
        let snap = self.store.snapshot();

        let src_path = resolve_entry_path(&self.store, eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} not found in snapshot",
                id
            )))
        })?;
        let new_parent_path = resolve_entry_path(&self.store, new_parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "new_parent id {} not found in snapshot",
                new_parent_id
            )))
        })?;

        let src_entry = snap.get(eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} vanished mid-copy",
                id
            )))
        })?;
        let destination_parent = snap.get(new_parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "new_parent id {} vanished mid-copy",
                new_parent_id
            )))
        })?;
        if destination_parent.kind != EntryKind::Directory
            && destination_parent.symlink_target_is_dir != Some(true)
        {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "copy destination is not a directory".into(),
            )));
        }
        let roots = self.roots.read().clone();
        let source_root = configured_root_for_path(&roots, &src_path);
        let destination_root = configured_root_for_path(&roots, &new_parent_path);
        if source_root != destination_root && !allow_cross_root {
            return Err(fx_error_to_napi(FxError::Unsupported(
                "cross-root copy requires { crossRoot: true }".into(),
            )));
        }

        let desired_name = new_name.unwrap_or_else(|| src_entry.name.clone());
        let workspace_root = destination_root.clone().ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(
                "copy destination is not under a configured root".into(),
            ))
        })?;
        let (contained_parent, root_canon) =
            assert_parent_contained(&new_parent_path, &workspace_root)
                .await
                .map_err(fx_error_to_napi)?;
        join_under_root(
            &new_parent_path,
            &contained_parent,
            &desired_name,
            &root_canon,
        )
        .map_err(fx_error_to_napi)?;
        let dest = resolve_transfer_destination(&new_parent_path, &desired_name, collision)
            .await
            .map_err(fx_error_to_napi)?;
        join_under_root(&new_parent_path, &contained_parent, &dest.name, &root_canon)
            .map_err(fx_error_to_napi)?;
        let effective_name = dest.name.clone();
        let dst_path = dest.path.clone();

        // Never overwrite/remove the source itself (same path / hardlink).
        if paths_equal(&dst_path, &src_path) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot copy a path onto itself".into(),
            )));
        }

        if matches!(dest.action, DestAction::Skip) {
            if let Some(existing) = self.store.get_by_path(&dst_path) {
                return Ok(EntryJs::from_core(existing.as_ref()));
            }
            // Destination exists on disk but is not indexed yet — index it.
            let mut entry = stat_to_entry(&dst_path, Some(new_parent_eid), effective_name)
                .await
                .map_err(fx_error_to_napi)?;
            let _policy_guard = self.policy_gate.lock();
            let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
            entry.is_excluded = Self::path_is_excluded(&dst_path, &entry, &exclude_matchers);
            let id = self
                .store
                .insert(dst_path, entry)
                .map_err(fx_error_to_napi)?;
            let arc = self
                .store
                .get_by_id(id)
                .ok_or_else(|| Error::from_reason("skip target vanished"))?;
            return Ok(EntryJs::from_core(arc.as_ref()));
        }

        // Directory self/descendant guard before any destructive collision action.
        if src_entry.kind == EntryKind::Directory
            && (path_is_self_or_descendant(&dst_path, &src_path))
        {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot copy a directory into itself or a descendant".into(),
            )));
        }

        if matches!(dest.action, DestAction::Overwrite)
            && (paths_equal(&dst_path, &src_path)
                || self
                    .store
                    .get_by_path(&dst_path)
                    .map(|e| e.id == eid)
                    .unwrap_or(false))
        {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot overwrite source with itself".into(),
            )));
        }

        if src_entry.kind == EntryKind::Directory {
            let progress = self
                .begin_copy_progress(options.as_ref())
                .map_err(fx_error_to_napi)?;
            if let Some(ref prog) = progress {
                match count_copy_entries(&src_path, Some(prog)).await {
                    Ok(total) => prog.total.store(total.max(1), Ordering::Relaxed),
                    Err(FxError::Cancelled) => {
                        self.end_copy_progress(progress.as_ref(), "cancelled");
                        return Err(fx_error_to_napi(FxError::Cancelled));
                    }
                    Err(_) => prog.total.store(1, Ordering::Relaxed),
                }
            }
            let copy_result = if matches!(dest.action, DestAction::Merge) {
                merge_tree_on_disk_with_progress(&src_path, &dst_path, progress.as_ref()).await
            } else if matches!(dest.action, DestAction::Overwrite) {
                // Staging keeps the original destination until swap succeeds.
                copy_via_staging(&src_path, &dst_path, progress.as_ref()).await
            } else {
                copy_tree_on_disk_with_progress(&src_path, &dst_path, progress.as_ref()).await
            };
            if let Err(error) = copy_result {
                let status = if matches!(error, FxError::Cancelled) {
                    "cancelled"
                } else {
                    "failed"
                };
                self.end_copy_progress(progress.as_ref(), status);
                // Create (non-overwrite, non-merge) may leave a partial tree.
                if matches!(dest.action, DestAction::Create) {
                    let _ = remove_path_best_effort(&dst_path).await;
                }
                // Overwrite/merge failures leave the original destination intact
                // (staging cleaned by copy_via_staging / merge is non-destructive).
                return Err(fx_error_to_napi(error));
            }
            self.end_copy_progress(progress.as_ref(), "completed");
            // Overwrite: drop any prior store identity before reindex/reconcile.
            if matches!(dest.action, DestAction::Overwrite) {
                if let Some(existing) = self.store.get_by_path(&dst_path) {
                    let existing_id = existing.id;
                    let existing_kind = existing.kind;
                    let _policy_guard = self.policy_gate.lock();
                    if existing_kind == EntryKind::Directory {
                        let _ = self.store.remove_subtree(existing_id);
                    } else {
                        let _ = self.store.remove(existing_id);
                    }
                }
            }
            self.record_intent(dst_path.clone(), IntentKind::Create);
            // Authoritatively reconcile the destination subtree so merge
            // updates existing entries (size/mtime/kind) and drops stale ones.
            // Stat before taking the gate; the insert re-checks under it, so a
            // concurrent insert of the same path still cannot be clobbered.
            let fresh_root = if self.store.get_by_path(&dst_path).is_none() {
                Some(
                    stat_to_entry(&dst_path, Some(new_parent_eid), effective_name.clone())
                        .await
                        .map_err(fx_error_to_napi)?,
                )
            } else {
                None
            };
            let _policy_guard = self.policy_gate.lock();
            if let Some(mut root_entry) = fresh_root {
                if self.store.get_by_path(&dst_path).is_none() {
                    let exclude_matchers =
                        self.current_exclude_matchers().map_err(fx_error_to_napi)?;
                    root_entry.is_excluded =
                        Self::path_is_excluded(&dst_path, &root_entry, &exclude_matchers);
                    self.store
                        .insert(dst_path.clone(), root_entry)
                        .map_err(fx_error_to_napi)?;
                }
            }
            let config = self.watch_config();
            crate::watch_runtime::reconcile_directory(&self.store, &config, &dst_path, None)
                .map_err(fx_error_to_napi)?;
            self.reclassify_current_excludes()
                .map_err(fx_error_to_napi)?;
            let arc = self.store.get_by_path(&dst_path).ok_or_else(|| {
                Error::from_reason("directory copy succeeded but path not indexed")
            })?;
            return Ok(EntryJs::from_core(arc.as_ref()));
        }

        let progress = self
            .begin_copy_progress(options.as_ref())
            .map_err(fx_error_to_napi)?;
        if let Some(ref progress) = progress {
            progress.total.store(1, Ordering::Relaxed);
        }
        let file_result = if matches!(dest.action, DestAction::Overwrite) {
            copy_via_staging(&src_path, &dst_path, progress.as_ref()).await
        } else {
            copy_tree_on_disk_with_progress(&src_path, &dst_path, progress.as_ref()).await
        };
        if let Err(error) = file_result {
            let status = if matches!(error, FxError::Cancelled) {
                "cancelled"
            } else {
                "failed"
            };
            self.end_copy_progress(progress.as_ref(), status);
            if matches!(dest.action, DestAction::Create) {
                let _ = remove_path_best_effort(&dst_path).await;
            }
            return Err(fx_error_to_napi(error));
        }
        self.end_copy_progress(progress.as_ref(), "completed");
        if matches!(dest.action, DestAction::Overwrite) {
            if let Some(existing) = self.store.get_by_path(&dst_path) {
                let _policy_guard = self.policy_gate.lock();
                let _ = self.store.remove(existing.id);
            }
        }
        self.record_intent(dst_path.clone(), IntentKind::Create);

        let mut entry = stat_to_entry(&dst_path, Some(new_parent_eid), effective_name)
            .await
            .map_err(fx_error_to_napi)?;
        let _policy_guard = self.policy_gate.lock();
        let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
        entry.is_excluded = Self::path_is_excluded(&dst_path, &entry, &exclude_matchers);
        if let Some(existing) = self.store.get_by_path(&dst_path) {
            let _ = self.store.update(existing.id, entry);
            let arc = self
                .store
                .get_by_id(existing.id)
                .ok_or_else(|| Error::from_reason("copy update succeeded but id vanished"))?;
            return Ok(EntryJs::from_core(arc.as_ref()));
        }
        let new_id = self
            .store
            .insert(dst_path, entry)
            .map_err(fx_error_to_napi)?;

        let arc = self
            .store
            .get_by_id(new_id)
            .ok_or_else(|| Error::from_reason("copy succeeded but id vanished"))?;
        Ok(EntryJs::from_core(arc.as_ref()))
    }

    /// Copy a filesystem path that may live outside the workspace into a
    /// destination folder under a configured root. Files keep content and
    /// metadata-friendly mode bits via the OS copy; directories copy
    /// recursively. Existing destinations follow `TransferOptions.collision`
    /// (`error` default, or `rename` for a free suffix). Per-item failures
    /// surface as structured `FileSystemError`s without creating empty
    /// placeholder files.
    #[napi(js_name = "copyFromPath", catch_unwind)]
    pub async fn copy_from_path(
        &self,
        source_path: String,
        new_parent_id: i64,
        new_name: Option<String>,
        options: Option<TransferOptionsJs>,
    ) -> Result<EntryJs> {
        let src_path = PathBuf::from(&source_path);
        if !src_path.is_absolute() {
            return Err(fx_error_to_napi(FxError::InvalidInput(format!(
                "copyFromPath source must be absolute: {source_path}"
            ))));
        }
        let new_parent_eid = EntryId(new_parent_id as u64);
        let (_, collision) = transfer_policy(options.as_ref()).map_err(fx_error_to_napi)?;

        let new_parent_path = resolve_entry_path(&self.store, new_parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "new_parent id {new_parent_id} not found in snapshot"
            )))
        })?;
        let snap = self.store.snapshot();
        let destination_parent = snap.get(new_parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "new_parent id {new_parent_id} vanished mid-copyFromPath"
            )))
        })?;
        if destination_parent.kind != EntryKind::Directory
            && destination_parent.symlink_target_is_dir != Some(true)
        {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "copyFromPath destination is not a directory".into(),
            )));
        }

        let roots = self.roots.read().clone();
        let destination_root =
            configured_root_for_path(&roots, &new_parent_path).ok_or_else(|| {
                fx_error_to_napi(FxError::InvalidInput(
                    "copyFromPath destination is not under a configured root".into(),
                ))
            })?;

        let src_meta = tokio::fs::symlink_metadata(&src_path)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, src_path.clone())))?;
        let desired_name = new_name.unwrap_or_else(|| {
            src_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "imported".to_string())
        });
        validate_entry_name(&desired_name).map_err(fx_error_to_napi)?;
        let (contained_parent, root_canon) =
            assert_parent_contained(&new_parent_path, &destination_root)
                .await
                .map_err(fx_error_to_napi)?;
        join_under_root(
            &new_parent_path,
            &contained_parent,
            &desired_name,
            &root_canon,
        )
        .map_err(fx_error_to_napi)?;

        let dest = resolve_transfer_destination(&new_parent_path, &desired_name, collision)
            .await
            .map_err(fx_error_to_napi)?;
        join_under_root(&new_parent_path, &contained_parent, &dest.name, &root_canon)
            .map_err(fx_error_to_napi)?;
        let effective_name = dest.name.clone();
        let dst_path = dest.path.clone();

        if paths_equal(&dst_path, &src_path) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot copy a path onto itself".into(),
            )));
        }

        // Reject self/descendant cycles for directory sources.
        if src_meta.is_dir() || src_meta.file_type().is_symlink() {
            if let Ok(true) = tokio::fs::metadata(&src_path).await.map(|m| m.is_dir()) {
                if path_is_self_or_descendant(&dst_path, &src_path) {
                    return Err(fx_error_to_napi(FxError::InvalidInput(
                        "cannot copy a directory into itself or a descendant".into(),
                    )));
                }
            }
        }

        if matches!(dest.action, DestAction::Skip) {
            if let Some(existing) = self.store.get_by_path(&dst_path) {
                return Ok(EntryJs::from_core(existing.as_ref()));
            }
            let mut entry = stat_to_entry(&dst_path, Some(new_parent_eid), effective_name)
                .await
                .map_err(fx_error_to_napi)?;
            let _policy_guard = self.policy_gate.lock();
            let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
            entry.is_excluded = Self::path_is_excluded(&dst_path, &entry, &exclude_matchers);
            let id = self
                .store
                .insert(dst_path, entry)
                .map_err(fx_error_to_napi)?;
            let arc = self
                .store
                .get_by_id(id)
                .ok_or_else(|| Error::from_reason("skip target vanished"))?;
            return Ok(EntryJs::from_core(arc.as_ref()));
        }

        if matches!(dest.action, DestAction::Overwrite) && paths_equal(&dst_path, &src_path) {
            return Err(fx_error_to_napi(FxError::InvalidInput(
                "cannot overwrite source with itself".into(),
            )));
        }

        // On mid-copy failure or cancel: create paths drop partial trees;
        // overwrite uses staging so the original destination survives.
        let progress = self
            .begin_copy_progress(options.as_ref())
            .map_err(fx_error_to_napi)?;
        if let Some(ref prog) = progress {
            match count_copy_entries(&src_path, Some(prog)).await {
                Ok(total) => prog.total.store(total.max(1), Ordering::Relaxed),
                Err(FxError::Cancelled) => {
                    self.end_copy_progress(progress.as_ref(), "cancelled");
                    return Err(fx_error_to_napi(FxError::Cancelled));
                }
                Err(_) => prog.total.store(1, Ordering::Relaxed),
            }
        }
        let copy_result = if matches!(dest.action, DestAction::Merge) {
            merge_tree_on_disk_with_progress(&src_path, &dst_path, progress.as_ref()).await
        } else if matches!(dest.action, DestAction::Overwrite) {
            copy_via_staging(&src_path, &dst_path, progress.as_ref()).await
        } else {
            copy_tree_on_disk_with_progress(&src_path, &dst_path, progress.as_ref()).await
        };
        if let Err(error) = copy_result {
            let status = if matches!(error, FxError::Cancelled) {
                "cancelled"
            } else {
                "failed"
            };
            self.end_copy_progress(progress.as_ref(), status);
            if matches!(dest.action, DestAction::Create) {
                let _ = remove_path_best_effort(&dst_path).await;
            }
            return Err(fx_error_to_napi(error));
        }
        self.end_copy_progress(progress.as_ref(), "completed");
        if matches!(dest.action, DestAction::Overwrite) {
            if let Some(existing) = self.store.get_by_path(&dst_path) {
                let existing_id = existing.id;
                let existing_kind = existing.kind;
                let _policy_guard = self.policy_gate.lock();
                if existing_kind == EntryKind::Directory {
                    let _ = self.store.remove_subtree(existing_id);
                } else {
                    let _ = self.store.remove(existing_id);
                }
            }
        }
        self.record_intent(dst_path.clone(), IntentKind::Create);

        // Index the new material. Files insert directly; directories walk
        // and populate so nested content is immediately visible.
        let dst_meta = tokio::fs::symlink_metadata(&dst_path)
            .await
            .map_err(|e| fx_error_to_napi(io_to_fx(e, dst_path.clone())))?;
        let is_dir = dst_meta.is_dir()
            || (dst_meta.file_type().is_symlink()
                && tokio::fs::metadata(&dst_path)
                    .await
                    .map(|m| m.is_dir())
                    .unwrap_or(false));

        if !is_dir {
            let mut entry = stat_to_entry(&dst_path, Some(new_parent_eid), effective_name)
                .await
                .map_err(fx_error_to_napi)?;
            let _policy_guard = self.policy_gate.lock();
            let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
            entry.is_excluded = Self::path_is_excluded(&dst_path, &entry, &exclude_matchers);
            if let Some(existing) = self.store.get_by_path(&dst_path) {
                let _ = self.store.update(existing.id, entry);
                let arc = self.store.get_by_id(existing.id).ok_or_else(|| {
                    Error::from_reason("copyFromPath update succeeded but id vanished")
                })?;
                return Ok(EntryJs::from_core(arc.as_ref()));
            }
            let new_id = self
                .store
                .insert(dst_path.clone(), entry)
                .map_err(fx_error_to_napi)?;
            let arc = self
                .store
                .get_by_id(new_id)
                .ok_or_else(|| Error::from_reason("copyFromPath succeeded but id vanished"))?;
            return Ok(EntryJs::from_core(arc.as_ref()));
        }

        // Directory: ensure root is indexed, then authoritatively reconcile
        // so merge updates existing metadata and drops stale descendants.
        // Stat before taking the gate; the insert re-checks under it, so a
        // concurrent insert of the same path still cannot be clobbered.
        let fresh_root = if self.store.get_by_path(&dst_path).is_none() {
            Some(
                stat_to_entry(&dst_path, Some(new_parent_eid), effective_name)
                    .await
                    .map_err(fx_error_to_napi)?,
            )
        } else {
            None
        };
        let _policy_guard = self.policy_gate.lock();
        if let Some(mut root_entry) = fresh_root {
            if self.store.get_by_path(&dst_path).is_none() {
                let exclude_matchers = self.current_exclude_matchers().map_err(fx_error_to_napi)?;
                root_entry.is_excluded =
                    Self::path_is_excluded(&dst_path, &root_entry, &exclude_matchers);
                self.store
                    .insert(dst_path.clone(), root_entry)
                    .map_err(fx_error_to_napi)?;
            }
        }
        let config = self.watch_config();
        crate::watch_runtime::reconcile_directory(&self.store, &config, &dst_path, None)
            .map_err(fx_error_to_napi)?;
        self.reclassify_current_excludes()
            .map_err(fx_error_to_napi)?;
        let arc = self.store.get_by_path(&dst_path).ok_or_else(|| {
            Error::from_reason("copyFromPath directory copy succeeded but path not indexed")
        })?;
        Ok(EntryJs::from_core(arc.as_ref()))
    }

    /// Preflight a transfer destination without mutating disk or the store.
    /// Used by hosts/UI to prompt only when a real collision exists.
    #[napi(js_name = "probeDestination", catch_unwind)]
    pub async fn probe_destination(
        &self,
        parent_id: i64,
        name: String,
    ) -> Result<DestinationProbeJs> {
        let parent_eid = EntryId(parent_id as u64);
        let parent_path = resolve_entry_path(&self.store, parent_eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "parent id {parent_id} not found in snapshot"
            )))
        })?;
        let roots = self.roots.read().clone();
        let root = configured_root_for_path(&roots, &parent_path).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(
                "probeDestination parent is not under a configured root".into(),
            ))
        })?;
        validate_entry_name(&name).map_err(fx_error_to_napi)?;
        let (contained_parent, root_canon) = assert_parent_contained(&parent_path, &root)
            .await
            .map_err(fx_error_to_napi)?;
        let desired = join_under_root(&parent_path, &contained_parent, &name, &root_canon)
            .map_err(fx_error_to_napi)?;

        let case_conflict = find_case_conflict(&contained_parent, &name)
            .await
            .map_err(fx_error_to_napi)?;
        match tokio::fs::symlink_metadata(&desired).await {
            Ok(_) => Ok(DestinationProbeJs {
                status: "exists".into(),
                existing_name: Some(name),
                path: Some(desired.to_string_lossy().into_owned()),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(actual) = case_conflict {
                    Ok(DestinationProbeJs {
                        status: "case_conflict".into(),
                        existing_name: Some(actual),
                        path: Some(contained_parent.join(&name).to_string_lossy().into_owned()),
                    })
                } else {
                    Ok(DestinationProbeJs {
                        status: "free".into(),
                        existing_name: None,
                        path: Some(desired.to_string_lossy().into_owned()),
                    })
                }
            }
            Err(error) => Err(fx_error_to_napi(io_to_fx(error, desired))),
        }
    }

    // ---- File I/O ----------------------------------------------------
    //
    // Each I/O method plumbs a `CancellationToken` through `crate::io`
    // even though the JS-visible surface doesn't yet accept an
    // `AbortSignal` parameter. Reason: napi-rs 3.8's `AbortSignal` is
    // backed by `Rc<RefCell<..>>` and is therefore `!Send`; the
    // `#[napi]` macro wraps `async fn` bodies into `tokio::spawn`-able
    // futures that must be `Send`, so AbortSignal can't appear as a
    // direct parameter. The supported pattern is `AsyncTask` +
    // `Task::with_signal`, which means restructuring these methods
    // around a `Task` impl — that belongs in Phase 6 when the TS
    // wrapper lands and can front a unified signal-aware surface.
    //
    // In the meantime `signal_to_token(None)` yields a never-fires
    // token, so the token checks inside `crate::io` are effectively
    // no-ops but ready to bite once the signal path is wired.
    // SPEC §4.6 flags AbortSignal as advisory in Phase 5.

    /// Read a file's contents by id. Returns a `Buffer` — JS receives a
    /// zero-copy view over the bytes (standard Node builds).
    #[napi(js_name = "readFile", catch_unwind)]
    pub async fn read_file(&self, id: i64) -> Result<Buffer> {
        // TODO(phase-6): accept `Option<AbortSignal>` once the wrapper
        // restructures I/O onto `AsyncTask` (napi-rs 3.8 limitation).
        let token = crate::cancel::signal_to_token(None);
        let path = self.resolve_path_for_id(id)?;
        crate::io::read_file(path, token).await
    }

    /// Read a file as text. Only UTF-8 is supported in v0.1; pass
    /// `encoding: "utf-8"` (or omit) — anything else returns EUNSUPPORTED.
    #[napi(js_name = "readText", catch_unwind)]
    pub async fn read_text(&self, id: i64, encoding: Option<String>) -> Result<String> {
        let token = crate::cancel::signal_to_token(None);
        let path = self.resolve_path_for_id(id)?;
        crate::io::read_text(path, encoding, token).await
    }

    /// Write `data` to a file by id. When `options.atomic` is true, writes
    /// through a sibling `.mille.tmp` file + rename — safe against partial
    /// writes on same-filesystem targets.
    #[napi(js_name = "writeFile", catch_unwind)]
    pub async fn write_file(
        &self,
        id: i64,
        data: Buffer,
        options: Option<WriteFileOptionsJs>,
    ) -> Result<()> {
        let token = crate::cancel::signal_to_token(None);
        let path = self.resolve_path_for_id(id)?;
        let existing = self
            .store
            .get_by_id(EntryId(id as u64))
            .ok_or_else(|| Error::from_reason(format!("id {id} vanished mid-write")))?;
        let atomic = options.and_then(|o| o.atomic).unwrap_or(false);
        crate::io::write_file(path.clone(), data.to_vec(), atomic, token).await?;
        let mut updated = stat_to_entry(&path, existing.parent_id, existing.name.clone())
            .await
            .map_err(fx_error_to_napi)?;
        updated.is_ignored = existing.is_ignored;
        updated.is_excluded = existing.is_excluded;
        updated.path_segments = existing.path_segments.clone();
        self.store
            .update(existing.id, updated)
            .map_err(fx_error_to_napi)?;
        self.record_intent(path, IntentKind::Modify);
        Ok(())
    }

    /// Open a streaming reader. Consumers call `.next()` repeatedly until
    /// it returns null; `.cancel()` tears down early. The Phase 6 TS
    /// wrapper presents this as `AsyncIterable<Uint8Array>`.
    ///
    /// TODO(phase-6 / PLAN 13.x): accept `Option<AbortSignal>` once the
    /// wrapper restructures onto `AsyncTask` — same `!Send` constraint
    /// that defers it on read_file/write_file.
    #[napi(js_name = "readFileStream", catch_unwind)]
    pub fn read_file_stream(&self, id: i64) -> Result<crate::stream::FileReadStream> {
        let path = self.resolve_path_for_id(id)?;
        Ok(crate::stream::FileReadStream::open(path))
    }

    // ---- Event subscription ------------------------------------------
    //
    // api.d.ts exposes a single `on(event, listener)` overload set, but
    // napi-rs 3.x requires the ThreadsafeFunction payload type to be
    // known at the FFI boundary (const-generic `CalleeHandled`, typed
    // arg). A single Rust method handling all 8 channels would have to
    // re-create the TSFN via match-on-string from a raw `Function`,
    // which drags in `Env` and leaks the napi-internal builder types.
    //
    // Per-channel `onXxx` methods are simpler and keep payloads typed
    // end-to-end. The Phase 6 TS wrapper unifies them behind the single
    // `on(event, listener)` contract from api.d.ts so JS consumers never
    // see the split surface.

    /// Subscribe to the coalesced `'change'` channel — fires once per
    /// coalescer flush regardless of whether the delta was tree-only,
    /// decoration-only, or both.
    #[napi(js_name = "onChange", catch_unwind)]
    pub fn on_change(
        &self,
        listener: ThreadsafeFunction<
            ChangeNoticeJs,
            Unknown<'static>,
            ChangeNoticeJs,
            Status,
            false,
        >,
    ) -> u64 {
        self.events.subscribe_change(Channel::Change, listener)
    }

    /// Subscribe to the `'change:tree'` channel — tree-structural changes only.
    #[napi(js_name = "onChangeTree", catch_unwind)]
    pub fn on_change_tree(
        &self,
        listener: ThreadsafeFunction<
            ChangeNoticeJs,
            Unknown<'static>,
            ChangeNoticeJs,
            Status,
            false,
        >,
    ) -> u64 {
        self.events.subscribe_change(Channel::ChangeTree, listener)
    }

    /// Subscribe to the `'change:decorations'` channel — decoration
    /// bumps that don't touch the tree structure.
    #[napi(js_name = "onChangeDecorations", catch_unwind)]
    pub fn on_change_decorations(
        &self,
        listener: ThreadsafeFunction<
            ChangeNoticeJs,
            Unknown<'static>,
            ChangeNoticeJs,
            Status,
            false,
        >,
    ) -> u64 {
        self.events
            .subscribe_change(Channel::ChangeDecorations, listener)
    }

    /// Subscribe to the raw single-event stream fed by the live watcher.
    #[napi(js_name = "onEvent", catch_unwind)]
    pub fn on_event(
        &self,
        listener: ThreadsafeFunction<
            FileSystemEventJs,
            Unknown<'static>,
            FileSystemEventJs,
            Status,
            false,
        >,
    ) -> u64 {
        self.events.subscribe_event(listener)
    }

    /// Subscribe to the batched event stream. Each emission is a Vec
    /// of events coalesced within one debounce window.
    #[napi(js_name = "onBatch", catch_unwind)]
    pub fn on_batch(
        &self,
        listener: ThreadsafeFunction<
            Vec<FileSystemEventJs>,
            Unknown<'static>,
            Vec<FileSystemEventJs>,
            Status,
            false,
        >,
    ) -> u64 {
        self.events.subscribe_batch(listener)
    }

    /// Subscribe to soft warnings — inotify budget advisories, dropped
    /// events, platform quirks. Non-fatal; distinct from `'error'`.
    #[napi(js_name = "onWarning", catch_unwind)]
    pub fn on_warning(
        &self,
        listener: ThreadsafeFunction<
            WarningPayloadJs,
            Unknown<'static>,
            WarningPayloadJs,
            Status,
            false,
        >,
    ) -> u64 {
        self.events.subscribe_warning(listener)
    }

    /// Subscribe to the error channel. Engine-side failures (watcher
    /// crash, walker panic) surface here with an ErrorCode + message.
    #[napi(js_name = "onError", catch_unwind)]
    pub fn on_error(
        &self,
        listener: ThreadsafeFunction<
            ErrorPayloadJs,
            Unknown<'static>,
            ErrorPayloadJs,
            Status,
            false,
        >,
    ) -> u64 {
        self.events.subscribe_error(listener)
    }

    /// Subscribe to the `'ready'` channel. Fires once when the initial
    /// scan settles; later phases may re-fire on root re-add.
    #[napi(js_name = "onReady", catch_unwind)]
    pub fn on_ready(
        &self,
        listener: ThreadsafeFunction<(), Unknown<'static>, (), Status, false>,
    ) -> u64 {
        self.events.subscribe_ready(listener)
    }

    /// Remove the listener registered under `subscription_id`. Returns
    /// true if a listener was actually removed; double-off is idempotent.
    #[napi(js_name = "off", catch_unwind)]
    pub fn off(&self, subscription_id: i64) -> bool {
        // `on*` returns u64 but napi widens unsigned 64-bit to JS bigint;
        // the TS wrapper re-narrows to number (we stay < 2^53 in practice
        // since subscription churn is human-scale). Accept i64 here for
        // parity with the rest of the id surface.
        self.events.unsubscribe(subscription_id as u64)
    }

    /// Test-only: emit a synthetic 'ready' so the Wave 7 Node harness can
    /// verify end-to-end delivery without spinning up a real watcher.
    /// Gated under `#[cfg(feature = "test-hooks")]` would be cleaner but
    /// the binding crate is cdylib-only; `pub(crate)` + a thin `#[napi]`
    /// wrapper is enough for the integration tests to reach it.
    #[napi(js_name = "emitReadyForTests", catch_unwind)]
    pub fn emit_ready_for_tests(&self) {
        self.events.emit_ready();
    }

    /// Filename fuzzy search via nucleo (Phase 10). Runs synchronously
    /// against a fresh in-memory snapshot. Returns hits sorted by score
    /// descending; ties break on ascending entry id.
    ///
    /// The client-port path (search-over-the-wire) is Phase 10+ and
    /// is not wired here — this method is local-mode only.
    #[napi(catch_unwind)]
    pub fn search(&self, query: String, options: Option<SearchOptionsJs>) -> Vec<SearchHitJs> {
        let snap = self.store.snapshot();
        let opts = options
            .map(|o| mille_core::SearchOptions {
                limit: o.limit.map(|n| n as usize),
                include_ignored: o.include_ignored.unwrap_or(false),
                case_sensitive: o.case_sensitive.unwrap_or(false),
            })
            .unwrap_or_default();

        let hits = mille_core::search(snap.as_ref(), &query, &opts);
        hits.into_iter()
            .filter_map(|h| {
                // Search only emits ids present in the snapshot's entries,
                // so get() returning None here is a logic bug — skip rather
                // than panic to keep the JS boundary robust on races.
                let entry = snap.get(h.id)?;
                Some(SearchHitJs {
                    entry: EntryJs::from_core(entry.as_ref()),
                    score: h.score as f64,
                    matched_indices: h.matched_indices,
                })
            })
            .collect()
    }

    /// Idempotently stop the watcher and release its forwarding thread.
    #[napi(catch_unwind)]
    pub async fn dispose(&self) -> Result<()> {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let watcher = match self.watcher.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        // Drop outside the mutex: Watcher::drop joins its forwarding thread.
        drop(watcher);
        Ok(())
    }
}

impl FileExplorer {
    /// Caller holds `policy_gate`.
    fn mark_configured_root_unavailable(
        &self,
        root: &std::path::Path,
    ) -> std::result::Result<u64, FxError> {
        if let Some(existing) = self.store.get_by_path(root) {
            return self
                .store
                .mark_root_unavailable(existing.id)
                .map(|(version, _)| version);
        }
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        let id = self.store.insert(
            root.to_path_buf(),
            Entry {
                id: EntryId(0),
                parent_id: None,
                name,
                kind: EntryKind::Unavailable,
                size: 0,
                mtime_ms: 0,
                ctime_ms: 0,
                symlink_target_is_dir: None,
                path_segments: None,
                is_ignored: false,
                is_excluded: false,
                is_readonly: true,
                is_hidden: false,
            },
        )?;
        Ok(self
            .store
            .get_by_id(id)
            .map_or(0, |_| self.store.tree_version()))
    }

    fn current_exclude_matchers(
        &self,
    ) -> std::result::Result<Vec<(PathBuf, mille_core::IgnoreMatcher)>, FxError> {
        compile_exclude_matchers(&self.roots.read(), &self.exclude_globs.read())
    }

    fn path_is_excluded(
        path: &std::path::Path,
        entry: &mille_core::Entry,
        matchers: &[(PathBuf, mille_core::IgnoreMatcher)],
    ) -> bool {
        matchers
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .is_some_and(|(_, matcher)| {
                let directory_like =
                    entry.kind == EntryKind::Directory || entry.symlink_target_is_dir == Some(true);
                matcher.is_ignored(path, directory_like)
            })
    }

    /// Caller holds `policy_gate`.
    fn reclassify_current_excludes(&self) -> std::result::Result<u64, FxError> {
        let matchers = self.current_exclude_matchers()?;
        Ok(self
            .store
            .reconfigure_exclusions(|path, entry| Self::path_is_excluded(path, entry, &matchers)))
    }

    fn ensure_watcher(&self) -> Result<()> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(Error::from_reason("FileExplorer is disposed"));
        }
        let mut guard = match self.watcher.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_some() {
            return Ok(());
        }
        let watcher = crate::watch_runtime::create_watcher(
            Arc::clone(&self.store),
            Arc::clone(&self.events),
            Arc::clone(&self.intents),
            self.watch_config(),
        )
        .map_err(fx_error_to_napi)?;
        *guard = Some(watcher);
        Ok(())
    }

    fn watch_config(&self) -> crate::watch_runtime::WatchConfig {
        crate::watch_runtime::WatchConfig {
            roots: Arc::clone(&self.roots),
            respect_ignore: self.options.respect_ignore,
            exclude_globs: Arc::clone(&self.exclude_globs),
            policy_gate: Arc::clone(&self.policy_gate),
            follow_symlinks: self.options.follow_symlinks,
            walker_concurrency: self.options.walker_concurrency,
            debounce_ms: self.options.watch_debounce_ms,
        }
    }

    fn emit_reconcile_notice(
        &self,
        previous_version: u64,
        outcome: &crate::watch_runtime::ReconcileOutcome,
    ) {
        let version = self.store.tree_version();
        if version == previous_version && outcome.coarse_ids.is_empty() {
            return;
        }
        let notice = || ChangeNoticeJs {
            tree_version: version as u32,
            decoration_version: 0,
            tree_changed: version != previous_version,
            decorations_changed: false,
            changed_ids: outcome
                .changed_ids
                .iter()
                .map(|id| id.raw() as i64)
                .collect(),
            child_set_changed: outcome
                .child_set_changed
                .iter()
                .map(|id| id.raw() as i64)
                .collect(),
            decoration_changed_ids: Vec::new(),
            coarse_subtrees: outcome
                .coarse_ids
                .iter()
                .map(|id| id.raw() as i64)
                .collect(),
        };
        self.events.emit_change(Channel::Change, notice());
        self.events.emit_change(Channel::ChangeTree, notice());
    }

    fn record_intent(&self, path: PathBuf, kind: IntentKind) {
        self.intents.lock().record(path, kind, Instant::now());
    }

    /// Shared path-resolution helper for read/write and mutation methods.
    /// Returns a ready-to-fx_error EINVAL when the id isn't in the current
    /// snapshot — same shape mutations use, so the TS wrapper can key off
    /// `FX|EINVAL|...` uniformly.
    pub(crate) fn resolve_path_for_id(&self, id: i64) -> Result<std::path::PathBuf> {
        let eid = EntryId(id as u64);
        resolve_entry_path(&self.store, eid).ok_or_else(|| {
            fx_error_to_napi(FxError::InvalidInput(format!(
                "id {} not found in snapshot",
                id
            )))
        })
    }
}

fn add_exclude_globs(
    matcher: &mut mille_core::IgnoreMatcher,
    root: &std::path::Path,
    globs: &[String],
) -> std::result::Result<(), FxError> {
    if globs.is_empty() {
        return Ok(());
    }
    matcher.add_from_string(root, &globs.join("\n"))
}

fn compile_exclude_matchers(
    roots: &[PathBuf],
    globs: &[String],
) -> std::result::Result<Vec<(PathBuf, mille_core::IgnoreMatcher)>, FxError> {
    roots
        .iter()
        .map(|root| {
            let mut matcher = mille_core::IgnoreMatcher::new();
            add_exclude_globs(&mut matcher, root, globs)?;
            Ok((root.clone(), matcher))
        })
        .collect()
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum CollisionPolicy {
    Error,
    Rename,
    Overwrite,
    Skip,
    Merge,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum DestAction {
    /// Destination path is free; create it.
    Create,
    /// Destination exists; caller should replace it.
    Overwrite,
    /// Destination directory exists; caller should merge into it.
    Merge,
    /// Destination exists; caller should no-op successfully.
    Skip,
}

struct DestResolution {
    name: String,
    path: PathBuf,
    action: DestAction,
}

fn transfer_policy(
    options: Option<&TransferOptionsJs>,
) -> std::result::Result<(bool, CollisionPolicy), FxError> {
    let cross_root = options
        .and_then(|options| options.cross_root)
        .unwrap_or(false);
    let collision = match options.and_then(|options| options.collision.as_deref()) {
        None | Some("error") => CollisionPolicy::Error,
        Some("rename") => CollisionPolicy::Rename,
        Some("overwrite") => CollisionPolicy::Overwrite,
        Some("skip") => CollisionPolicy::Skip,
        Some("merge") => CollisionPolicy::Merge,
        Some(other) => {
            return Err(FxError::InvalidInput(format!(
                "unsupported collision policy: {other}"
            )));
        }
    };
    Ok((cross_root, collision))
}

fn configured_root_for_path(roots: &[PathBuf], path: &Path) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path == root.as_path() || path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .cloned()
}

/// Reject path components that would escape a single directory entry name.
fn validate_entry_name(name: &str) -> std::result::Result<(), FxError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(FxError::InvalidInput(format!(
            "invalid destination name: {name}"
        )));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(FxError::InvalidInput(format!(
            "destination name must be a single path component: {name}"
        )));
    }
    // Reject Windows drive-relative and reserved separators.
    if name.contains(':') {
        return Err(FxError::InvalidInput(format!(
            "destination name must be a single path component: {name}"
        )));
    }
    Ok(())
}

/// Join `store_parent/name` for store-stable path spelling, while checking
/// physical containment against canonical `parent_canon` / `root_canon`.
/// Returning the store spelling keeps EntryStore path indices coherent on
/// platforms where `/var` and `/private/var` are the same directory.
fn join_under_root(
    store_parent: &Path,
    parent_canon: &Path,
    name: &str,
    root_canon: &Path,
) -> std::result::Result<PathBuf, FxError> {
    validate_entry_name(name)?;
    // Reject traversal in the name itself (already covered by validate) and
    // ensure the store join cannot climb out via odd components.
    let store_dest = store_parent.join(name);
    let mut normalized = PathBuf::new();
    for component in store_dest.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(FxError::InvalidInput(
                        "destination path escapes its parent".into(),
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    // Physical containment uses canonical parent + name.
    let physical = parent_canon.join(name);
    if !(physical == *root_canon || physical.starts_with(root_canon)) {
        return Err(FxError::InvalidInput(format!(
            "destination {:?} is not under workspace root {:?}",
            physical, root_canon
        )));
    }
    // Prefer normalized store spelling when it still lives under the
    // store parent; otherwise fall back to store_parent/name.
    if normalized.starts_with(store_parent) || normalized == store_parent.join(name) {
        Ok(if normalized.as_os_str().is_empty() {
            store_dest
        } else {
            // Keep original store_parent prefix spelling.
            store_dest
        })
    } else {
        Err(FxError::InvalidInput(
            "destination path escapes its parent".into(),
        ))
    }
}

/// Ensure a parent directory is a real path contained in a configured root
/// (symlink destinations that resolve outside the workspace are rejected).
/// Returns `(parent_canonical, root_canonical)` so callers compare with the
/// same spelling (important on macOS where `/var` → `/private/var`).
async fn assert_parent_contained(
    parent: &Path,
    root: &Path,
) -> std::result::Result<(PathBuf, PathBuf), FxError> {
    let parent_canon = tokio::fs::canonicalize(parent)
        .await
        .map_err(|e| io_to_fx(e, parent.to_path_buf()))?;
    let root_canon = tokio::fs::canonicalize(root)
        .await
        .map_err(|e| io_to_fx(e, root.to_path_buf()))?;
    if !(parent_canon == root_canon || parent_canon.starts_with(&root_canon)) {
        return Err(FxError::InvalidInput(format!(
            "destination parent {:?} escapes workspace root {:?}",
            parent_canon, root_canon
        )));
    }
    Ok((parent_canon, root_canon))
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    // Best-effort physical identity when both exist.
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// True when `child` is `ancestor` or a path beneath it, using canonical
/// forms when available so macOS `/var` vs `/private/var` still matches.
fn path_is_self_or_descendant(child: &Path, ancestor: &Path) -> bool {
    if child == ancestor || child.starts_with(ancestor) {
        return true;
    }
    match (
        std::fs::canonicalize(child),
        std::fs::canonicalize(ancestor),
    ) {
        (Ok(c), Ok(a)) => c == a || c.starts_with(&a),
        (Err(_), Ok(a)) => {
            // Destination may not exist yet: canonicalize parent and rejoin.
            match child.parent().and_then(|p| std::fs::canonicalize(p).ok()) {
                Some(parent) => {
                    let candidate = parent.join(child.file_name().unwrap_or_default());
                    candidate == a || candidate.starts_with(&a)
                }
                None => false,
            }
        }
        _ => false,
    }
}

/// Progress + cancellation context for long recursive copies.
struct CopyProgressCtx {
    operation_id: String,
    token: CancellationToken,
    events: Arc<EventBus>,
    done: AtomicU64,
    total: AtomicU64,
    /// Emit at most once per this many completed items.
    report_every: u64,
    /// When false, still track done/total for completion payloads but skip
    /// intermediate OP_PROGRESS spam.
    report_progress: bool,
}

impl CopyProgressCtx {
    fn check(&self) -> std::result::Result<(), FxError> {
        if self.token.is_cancelled() {
            Err(FxError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn bump(&self, path: &Path) {
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.total.load(Ordering::Relaxed).max(done);
        if !self.report_progress {
            return;
        }
        if done == 1 || done == total || done % self.report_every == 0 {
            self.emit_progress(done, total, path);
        }
    }

    fn emit_progress(&self, done: u64, total: u64, path: &Path) {
        if !self.report_progress {
            return;
        }
        let detail = format!(
            r#"{{"operationId":{},"phase":"copy","done":{},"total":{},"path":{}}}"#,
            serde_json_string(&self.operation_id),
            done,
            total,
            serde_json_string(&path.to_string_lossy()),
        );
        self.events.emit_warning(crate::types::WarningPayloadJs {
            code: "OP_PROGRESS".into(),
            detail: Some(detail),
        });
    }

    fn emit_complete(&self, status: &str) {
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed).max(done);
        let detail = format!(
            r#"{{"operationId":{},"status":{},"done":{},"total":{}}}"#,
            serde_json_string(&self.operation_id),
            serde_json_string(status),
            done,
            total,
        );
        let code = if status == "cancelled" {
            "OP_CANCELLED"
        } else {
            "OP_COMPLETE"
        };
        self.events.emit_warning(crate::types::WarningPayloadJs {
            code: code.into(),
            detail: Some(detail),
        });
    }
}

fn serde_json_string(s: &str) -> String {
    // Minimal JSON string escape without pulling serde_json into every call.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

async fn count_copy_entries(
    src: &Path,
    progress: Option<&CopyProgressCtx>,
) -> std::result::Result<u64, FxError> {
    count_copy_entries_guarded(src, &mut Vec::new(), progress).await
}

async fn count_copy_entries_guarded(
    src: &Path,
    stack: &mut Vec<PathBuf>,
    progress: Option<&CopyProgressCtx>,
) -> std::result::Result<u64, FxError> {
    if let Some(p) = progress {
        p.check()?;
    }
    let meta = tokio::fs::symlink_metadata(src)
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?;
    if meta.file_type().is_symlink() {
        // Directory symlinks are not descended (matches copy_tree).
        if tokio::fs::metadata(src)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
            && meta.file_type().is_symlink()
        {
            // Still count the symlink node itself as one entry when we refuse
            // to follow; the copy will error. Count as 1 for progress totals
            // on file symlinks only.
            return Ok(1);
        }
        return Ok(1);
    }
    if meta.is_file() {
        return Ok(1);
    }
    if !meta.is_dir() {
        return Ok(1);
    }
    let src_key = tokio::fs::canonicalize(src)
        .await
        .unwrap_or_else(|_| src.to_path_buf());
    if stack.iter().any(|seen| seen == &src_key) {
        return Ok(0);
    }
    stack.push(src_key);
    let mut total = 1u64; // directory itself
    let mut rd = tokio::fs::read_dir(src)
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?
    {
        total = total.saturating_add(
            Box::pin(count_copy_entries_guarded(&entry.path(), stack, progress)).await?,
        );
    }
    stack.pop();
    Ok(total)
}

/// Recursively copy a file or directory tree on disk.
/// Directory symlinks are *not* followed — they are recreated as symlinks
/// when the platform supports it, otherwise copied as empty placeholders
/// is rejected. File symlinks copy link-target contents once.
/// Sibling staging path for safe overwrite: copy into this path, then swap.
fn staging_path_for(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("entry");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".mille-stage-{stamp}-{name}"))
}

fn backup_path_for(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("entry");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".mille-bak-{stamp}-{name}"))
}

/// Copy `src` into `dst` via a sibling staging path so a cancelled overwrite
/// never destroys the pre-existing destination.
async fn copy_via_staging(
    src: &Path,
    dst: &Path,
    progress: Option<&CopyProgressCtx>,
) -> std::result::Result<(), FxError> {
    let staging = staging_path_for(dst);
    // Ensure we never collide with an existing staging leftover.
    remove_path_best_effort(&staging).await;
    if let Err(error) = copy_tree_on_disk_with_progress(src, &staging, progress).await {
        remove_path_best_effort(&staging).await;
        return Err(error);
    }
    if let Some(p) = progress {
        p.check()?;
    }
    let dest_exists = tokio::fs::symlink_metadata(dst).await.is_ok();
    let backup = if dest_exists {
        let backup = backup_path_for(dst);
        remove_path_best_effort(&backup).await;
        tokio::fs::rename(dst, &backup)
            .await
            .map_err(|e| io_to_fx(e, dst.to_path_buf()))?;
        Some(backup)
    } else {
        None
    };
    if let Err(error) = tokio::fs::rename(&staging, dst).await {
        // Restore backup if we moved the original aside.
        if let Some(backup) = backup.as_ref() {
            let _ = tokio::fs::rename(backup, dst).await;
        }
        remove_path_best_effort(&staging).await;
        return Err(io_to_fx(error, dst.to_path_buf()));
    }
    if let Some(backup) = backup {
        remove_path_best_effort(&backup).await;
    }
    Ok(())
}

async fn copy_tree_on_disk_with_progress(
    src: &Path,
    dst: &Path,
    progress: Option<&CopyProgressCtx>,
) -> std::result::Result<(), FxError> {
    copy_tree_on_disk_guarded(src, dst, &mut Vec::new(), progress).await
}

async fn copy_tree_on_disk_guarded(
    src: &Path,
    dst: &Path,
    stack: &mut Vec<PathBuf>,
    progress: Option<&CopyProgressCtx>,
) -> std::result::Result<(), FxError> {
    if let Some(p) = progress {
        p.check()?;
    }
    let meta = tokio::fs::symlink_metadata(src)
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?;

    if meta.file_type().is_symlink() {
        // Never follow directory symlinks (cycle + containment risk).
        // File symlinks: materialize target content once.
        match tokio::fs::metadata(src).await {
            Ok(target) if target.is_dir() => {
                return Err(FxError::Unsupported(format!(
                    "refusing to recursively follow directory symlink {:?}",
                    src
                )));
            }
            Ok(_) => {
                tokio::fs::copy(src, dst)
                    .await
                    .map_err(|e| io_to_fx(e, dst.to_path_buf()))?;
                if let Some(p) = progress {
                    p.bump(dst);
                }
                return Ok(());
            }
            Err(error) => {
                // Dangling symlink: try to recreate the link shape.
                let target = tokio::fs::read_link(src)
                    .await
                    .map_err(|e| io_to_fx(e, src.to_path_buf()))?;
                create_symlink(&target, dst).await?;
                let _ = error;
                if let Some(p) = progress {
                    p.bump(dst);
                }
                return Ok(());
            }
        }
    }

    if meta.is_file() {
        tokio::fs::copy(src, dst)
            .await
            .map_err(|e| io_to_fx(e, dst.to_path_buf()))?;
        if let Some(p) = progress {
            p.bump(dst);
        }
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(FxError::Unsupported(format!(
            "cannot copy special file {:?}",
            src
        )));
    }

    // Cycle detection via canonical directory identity.
    let src_key = tokio::fs::canonicalize(src)
        .await
        .unwrap_or_else(|_| src.to_path_buf());
    if stack.iter().any(|seen| seen == &src_key) {
        return Err(FxError::InvalidInput(format!(
            "directory cycle detected while copying {:?}",
            src
        )));
    }
    stack.push(src_key);

    tokio::fs::create_dir(dst)
        .await
        .map_err(|e| io_to_fx(e, dst.to_path_buf()))?;
    if let Some(p) = progress {
        p.bump(dst);
    }
    let mut rd = tokio::fs::read_dir(src)
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?
    {
        if let Some(p) = progress {
            p.check()?;
        }
        let name = entry.file_name();
        let child_src = entry.path();
        let child_dst = dst.join(&name);
        Box::pin(copy_tree_on_disk_guarded(
            &child_src, &child_dst, stack, progress,
        ))
        .await?;
    }
    stack.pop();
    Ok(())
}

async fn create_symlink(target: &Path, dst: &Path) -> std::result::Result<(), FxError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        tokio::task::spawn_blocking({
            let target = target.to_path_buf();
            let dst = dst.to_path_buf();
            move || symlink(&target, &dst).map_err(|e| io_to_fx(e, dst))
        })
        .await
        .map_err(|e| FxError::InternalBug(format!("symlink task join failed: {e}")))?
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let target = target.to_path_buf();
        let dst = dst.to_path_buf();
        let is_dir = tokio::fs::metadata(&target)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        tokio::task::spawn_blocking(move || {
            if is_dir {
                symlink_dir(&target, &dst).map_err(|e| io_to_fx(e, dst))
            } else {
                symlink_file(&target, &dst).map_err(|e| io_to_fx(e, dst))
            }
        })
        .await
        .map_err(|e| FxError::InternalBug(format!("symlink task join failed: {e}")))?
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, dst);
        Err(FxError::Unsupported(
            "symlink creation is not supported".into(),
        ))
    }
}

/// Merge `src` into an existing `dst` directory. Files overwrite; missing
/// children are created; nested directories merge recursively. Directory
/// symlinks are not followed. If `dst` does not exist, falls back to a full
/// tree copy.
async fn merge_tree_on_disk_with_progress(
    src: &Path,
    dst: &Path,
    progress: Option<&CopyProgressCtx>,
) -> std::result::Result<(), FxError> {
    let src_meta = tokio::fs::symlink_metadata(src)
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?;
    if src_meta.file_type().is_symlink() {
        // Never follow dir symlinks during merge either.
        if tokio::fs::metadata(src)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return Err(FxError::Unsupported(format!(
                "refusing to recursively follow directory symlink {:?}",
                src
            )));
        }
    }
    let src_is_dir = src_meta.is_dir()
        || (src_meta.file_type().is_symlink()
            && tokio::fs::metadata(src)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false));

    match tokio::fs::symlink_metadata(dst).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return copy_tree_on_disk_with_progress(src, dst, progress).await;
        }
        Err(error) => Err(io_to_fx(error, dst.to_path_buf())),
        Ok(dst_meta) => {
            // Prefer real directories only for merge descent (no symlink follow).
            let dst_is_real_dir = dst_meta.is_dir() && !dst_meta.file_type().is_symlink();
            if src_is_dir && !src_meta.file_type().is_symlink() && dst_is_real_dir {
                // Count the destination directory itself so totals match pre-count
                // (which includes directory nodes).
                if let Some(p) = progress {
                    p.bump(dst);
                }
                let mut rd = tokio::fs::read_dir(src)
                    .await
                    .map_err(|e| io_to_fx(e, src.to_path_buf()))?;
                while let Some(entry) = rd
                    .next_entry()
                    .await
                    .map_err(|e| io_to_fx(e, src.to_path_buf()))?
                {
                    if let Some(p) = progress {
                        p.check()?;
                    }
                    let name = entry.file_name();
                    Box::pin(merge_tree_on_disk_with_progress(
                        &entry.path(),
                        &dst.join(name),
                        progress,
                    ))
                    .await?;
                }
                return Ok(());
            }
            // File-to-file or kind mismatch: replace destination.
            remove_path_best_effort(dst).await;
            return copy_tree_on_disk_with_progress(src, dst, progress).await;
        }
    }
}

/// Move-merge a directory into an existing directory without deleting
/// destination-only children. Source directory is removed when empty.
async fn merge_move_tree_on_disk_with_progress(
    src: &Path,
    dst: &Path,
    progress: Option<&CopyProgressCtx>,
) -> std::result::Result<(), FxError> {
    if let Some(p) = progress {
        p.check()?;
        p.bump(dst);
    }
    let mut rd = tokio::fs::read_dir(src)
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| io_to_fx(e, src.to_path_buf()))?
    {
        if let Some(p) = progress {
            p.check()?;
        }
        let name = entry.file_name();
        let child_src = entry.path();
        let child_dst = dst.join(&name);
        let src_meta = tokio::fs::symlink_metadata(&child_src)
            .await
            .map_err(|e| io_to_fx(e, child_src.clone()))?;
        let src_is_dir = src_meta.is_dir() && !src_meta.file_type().is_symlink();
        match tokio::fs::symlink_metadata(&child_dst).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::rename(&child_src, &child_dst)
                    .await
                    .map_err(|e| io_to_fx(e, child_src.clone()))?;
                if let Some(p) = progress {
                    p.bump(&child_dst);
                }
            }
            Err(error) => return Err(io_to_fx(error, child_dst)),
            Ok(dst_meta) => {
                let dst_is_dir = dst_meta.is_dir() && !dst_meta.file_type().is_symlink();
                if src_is_dir && dst_is_dir {
                    Box::pin(merge_move_tree_on_disk_with_progress(
                        &child_src, &child_dst, progress,
                    ))
                    .await?;
                    let _ = tokio::fs::remove_dir(&child_src).await;
                } else {
                    remove_path_best_effort(&child_dst).await;
                    tokio::fs::rename(&child_src, &child_dst)
                        .await
                        .map_err(|e| io_to_fx(e, child_src.clone()))?;
                    if let Some(p) = progress {
                        p.bump(&child_dst);
                    }
                }
            }
        }
    }
    let _ = tokio::fs::remove_dir(src).await;
    Ok(())
}

async fn remove_path_best_effort(path: &Path) {
    let meta = match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => meta,
        Err(_) => return,
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        let _ = tokio::fs::remove_dir_all(path).await;
    } else {
        let _ = tokio::fs::remove_file(path).await;
    }
}

async fn resolve_transfer_destination(
    parent: &Path,
    desired_name: &str,
    collision: CollisionPolicy,
) -> std::result::Result<DestResolution, FxError> {
    let desired = parent.join(desired_name);

    // Case-only conflict: a sibling differs only by case on a
    // case-insensitive volume. Treat as exists for error/rename policies
    // and surface a clear message; overwrite/merge/skip operate on the
    // on-disk name that actually collides.
    let case_conflict = find_case_conflict(parent, desired_name).await?;
    let existing_path = match tokio::fs::symlink_metadata(&desired).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            case_conflict.map(|actual_name| parent.join(actual_name))
        }
        Err(error) => return Err(io_to_fx(error, desired)),
        Ok(_) => Some(desired.clone()),
    };

    let Some(existing) = existing_path else {
        return Ok(DestResolution {
            name: desired_name.to_string(),
            path: desired,
            action: DestAction::Create,
        });
    };

    match collision {
        CollisionPolicy::Error => {
            let message = if existing != desired {
                format!(
                    "destination already exists with different case ({})",
                    existing.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                )
            } else {
                "destination already exists".into()
            };
            return Err(io_to_fx(
                std::io::Error::new(std::io::ErrorKind::AlreadyExists, message),
                existing,
            ));
        }
        CollisionPolicy::Skip => {
            let name = existing
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(desired_name)
                .to_string();
            return Ok(DestResolution {
                name,
                path: existing,
                action: DestAction::Skip,
            });
        }
        CollisionPolicy::Overwrite => {
            let name = existing
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(desired_name)
                .to_string();
            return Ok(DestResolution {
                name,
                path: existing,
                action: DestAction::Overwrite,
            });
        }
        CollisionPolicy::Merge => {
            let meta = tokio::fs::symlink_metadata(&existing)
                .await
                .map_err(|e| io_to_fx(e, existing.clone()))?;
            let is_dir = meta.is_dir()
                || (meta.file_type().is_symlink()
                    && tokio::fs::metadata(&existing)
                        .await
                        .map(|m| m.is_dir())
                        .unwrap_or(false));
            let name = existing
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(desired_name)
                .to_string();
            return Ok(DestResolution {
                name,
                path: existing,
                action: if is_dir {
                    DestAction::Merge
                } else {
                    // Merge of a file target falls back to overwrite.
                    DestAction::Overwrite
                },
            });
        }
        CollisionPolicy::Rename => {}
    }

    // Rename: allocate a free suffix. Prefer the desired spelling.
    let desired_path = Path::new(desired_name);
    let stem = desired_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(desired_name);
    let extension = desired_path
        .extension()
        .and_then(|extension| extension.to_str());
    for suffix in 1..=10_000u32 {
        let copy_suffix = if suffix == 1 {
            " copy".to_string()
        } else {
            format!(" copy {suffix}")
        };
        let candidate_name = match extension {
            Some(extension) => format!("{stem}{copy_suffix}.{extension}"),
            None => format!("{stem}{copy_suffix}"),
        };
        let candidate = parent.join(&candidate_name);
        match tokio::fs::symlink_metadata(&candidate).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if find_case_conflict(parent, &candidate_name).await?.is_some() {
                    continue;
                }
                return Ok(DestResolution {
                    name: candidate_name,
                    path: candidate,
                    action: DestAction::Create,
                });
            }
            Err(error) => return Err(io_to_fx(error, candidate)),
            Ok(_) => {}
        }
    }
    Err(FxError::InvalidInput(
        "could not allocate a collision-free destination name".into(),
    ))
}

async fn find_case_conflict(
    parent: &Path,
    desired_name: &str,
) -> std::result::Result<Option<String>, FxError> {
    let mut rd = match tokio::fs::read_dir(parent).await {
        Ok(rd) => rd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_to_fx(error, parent.to_path_buf())),
    };
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| io_to_fx(e, parent.to_path_buf()))?
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.as_ref() != desired_name && name.eq_ignore_ascii_case(desired_name) {
            return Ok(Some(name.into_owned()));
        }
    }
    Ok(None)
}

/// Mirror of api.d.ts `WriteFileOptions`.
#[napi(object)]
pub struct WriteFileOptionsJs {
    /// When true, write to a sibling temp file and rename into place.
    pub atomic: Option<bool>,
}

/// SPEC §4.3 caps the walker budget at 8 threads. `std` has no num_cpus,
/// so approximate via `available_parallelism`.
fn num_cpus_or_8() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4)
}
