//! Binding-owned live watcher pipeline.
//!
//! OS events are hints, not authoritative mutations: every batch is
//! coalesced/rename-paired and then reconciled against a bounded disk walk.
//! That makes duplicate echoes, atomic-save event shapes, and dropped
//! intermediate notifications idempotent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mille_core::{
    coalesce_events, walk, walk_with_ignore, Entry, EntryId, EntryKind, EntryStore, FsChangeEvent,
    FxError, IgnoreMatcher, IntentCache, IntentKind, RenamePairer, SymlinkPolicy, WalkOptions,
    WalkedEntry, Watcher, WatcherOptions,
};
use parking_lot::Mutex;

use crate::events::{clone_event, Channel, EventBus};
use crate::types::{ChangeNoticeJs, EntryJs, ErrorPayloadJs, FileSystemEventJs, WarningPayloadJs};

#[derive(Clone)]
pub(crate) struct WatchConfig {
    pub roots: Arc<parking_lot::RwLock<Vec<PathBuf>>>,
    pub respect_ignore: bool,
    pub exclude_globs: Arc<parking_lot::RwLock<Vec<String>>>,
    pub policy_gate: Arc<Mutex<()>>,
    pub follow_symlinks: SymlinkPolicy,
    pub walker_concurrency: usize,
    pub debounce_ms: u64,
}

#[derive(Default)]
pub(crate) struct ReconcileOutcome {
    pub(crate) changed_ids: HashSet<EntryId>,
    pub(crate) child_set_changed: HashSet<EntryId>,
    pub(crate) coarse_ids: HashSet<EntryId>,
}

impl ReconcileOutcome {
    pub(crate) fn merge(&mut self, other: Self) {
        self.changed_ids.extend(other.changed_ids);
        self.child_set_changed.extend(other.child_set_changed);
        self.coarse_ids.extend(other.coarse_ids);
    }
}

pub(crate) fn create_watcher(
    store: Arc<EntryStore>,
    events: Arc<EventBus>,
    intents: Arc<Mutex<IntentCache>>,
    config: WatchConfig,
) -> Result<Watcher, FxError> {
    let pairer = Arc::new(Mutex::new(RenamePairer::new(Duration::from_secs(1))));
    let callback_store = Arc::clone(&store);
    let callback_events = Arc::clone(&events);
    let callback_config = config.clone();

    let watcher = Watcher::new_batch_with_options(
        move |raw| {
            process_batch(
                &callback_store,
                &callback_events,
                &intents,
                &pairer,
                &callback_config,
                raw,
            );
        },
        WatcherOptions {
            recursive: true,
            debounce_ms: Some(config.debounce_ms),
        },
    )?;

    let configured_roots = config.roots.read().clone();
    for root in &configured_roots {
        if let Err(err) = watcher.watch(
            root,
            WatcherOptions {
                recursive: true,
                debounce_ms: Some(config.debounce_ms),
            },
        ) {
            events.emit_warning(WarningPayloadJs {
                code: "WNOWATCH".into(),
                detail: Some(format!("failed to watch {}: {err}", root.display())),
            });
        }
    }

    Ok(watcher)
}

fn process_batch(
    store: &Arc<EntryStore>,
    events: &Arc<EventBus>,
    intents: &Arc<Mutex<IntentCache>>,
    pairer: &Arc<Mutex<RenamePairer>>,
    config: &WatchConfig,
    raw: Vec<mille_core::RawEvent>,
) {
    let _policy_guard = config.policy_gate.lock();
    let now = Instant::now();
    let configured_roots = config.roots.read().clone();
    let raw: Vec<_> = raw
        .into_iter()
        .map(|event| normalize_raw_event(event, &configured_roots))
        .collect();
    // Never leave an unmatched rename half waiting indefinitely for another
    // OS callback. Pair halves from this debounce batch, then degrade any
    // remainder into an explicit reconciliation hint.
    let high_level = {
        let mut pairer = pairer.lock();
        let mut events = pairer.feed(coalesce_events(&raw), now);
        events.extend(pairer.flush());
        events
    };
    let filtered: Vec<_> = high_level
        .into_iter()
        .filter(|event| !consume_intent(event, intents, now))
        .collect();
    if filtered.is_empty() {
        return;
    }

    let before_version = store.tree_version();
    let mut outcome = ReconcileOutcome::default();
    let mut public_events = Vec::with_capacity(filtered.len());

    for event in &filtered {
        if let FsChangeEvent::Error { message } = event {
            events.emit_error(ErrorPayloadJs {
                code: "EUNKNOWN".into(),
                message: message.clone(),
                path: None,
            });
        }
        let before = event_paths(event)
            .into_iter()
            .find_map(|path| store.get_by_path(path));
        match reconcile_event(store, config, event) {
            Ok(changes) => outcome.merge(changes),
            Err(err) => {
                events.emit_error(ErrorPayloadJs {
                    code: err.code().as_str().into(),
                    message: err.to_string(),
                    path: event_paths(event)
                        .first()
                        .map(|path| path.to_string_lossy().into_owned()),
                });
            }
        }
        public_events.push(to_public_event(store, event, before.as_deref()));
    }

    for event in &public_events {
        events.emit_event(clone_event(event));
    }
    events.emit_batch(public_events);

    let after_version = store.tree_version();
    if after_version != before_version || !outcome.coarse_ids.is_empty() {
        let notice = || ChangeNoticeJs {
            tree_version: after_version as u32,
            decoration_version: 0,
            tree_changed: after_version != before_version,
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
        events.emit_change(Channel::Change, notice());
        events.emit_change(Channel::ChangeTree, notice());
    }
}

fn normalize_raw_event(event: mille_core::RawEvent, roots: &[PathBuf]) -> mille_core::RawEvent {
    use mille_core::RawEvent;
    match event {
        RawEvent::Created(path) => RawEvent::Created(normalize_path(path, roots)),
        RawEvent::Modified(path) => RawEvent::Modified(normalize_path(path, roots)),
        RawEvent::Deleted(path) => RawEvent::Deleted(normalize_path(path, roots)),
        RawEvent::Any(path) => RawEvent::Any(normalize_path(path, roots)),
        RawEvent::Overflow { root } => RawEvent::Overflow {
            root: normalize_path(root, roots),
        },
        RawEvent::Error(message) => RawEvent::Error(message),
    }
}

/// macOS commonly canonicalizes `/var` to `/private/var` in FSEvents while
/// callers and the initial walk retain the spelling they supplied. Translate
/// watcher paths back into that configured-root namespace so the path index
/// remains stable across platforms and symlinked temp roots.
fn normalize_path(path: PathBuf, roots: &[PathBuf]) -> PathBuf {
    for root in roots {
        if path == *root || path.starts_with(root) {
            return path;
        }
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if path == canonical {
            return root.clone();
        }
        if let Ok(relative) = path.strip_prefix(&canonical) {
            return root.join(relative);
        }
    }
    path
}

fn consume_intent(event: &FsChangeEvent, intents: &Arc<Mutex<IntentCache>>, now: Instant) -> bool {
    let mut cache = intents.lock();
    match event {
        FsChangeEvent::Created { path } => cache.consume(path, IntentKind::Create, now),
        FsChangeEvent::Modified { path } => cache.consume(path, IntentKind::Modify, now),
        FsChangeEvent::Deleted { path } => cache.consume(path, IntentKind::Delete, now),
        FsChangeEvent::Renamed { from, to } => {
            let from_hit = cache.consume(from, IntentKind::Rename, now);
            let to_hit = cache.consume(to, IntentKind::Rename, now);
            from_hit && to_hit
        }
        _ => false,
    }
}

fn reconcile_event(
    store: &EntryStore,
    config: &WatchConfig,
    event: &FsChangeEvent,
) -> Result<ReconcileOutcome, FxError> {
    match event {
        FsChangeEvent::Created { path } => {
            if path.is_dir() && store.get_by_path(path).is_none() {
                // FSEvents may represent a directory rename/move as an
                // independent delete + create rather than a paired rename. A
                // depth-1 parent reconciliation would add only the directory
                // row and silently drop its already-present descendants. Walk
                // the created directory itself so the final snapshot matches
                // disk without rescanning the entire workspace root.
                let mut out = reconcile_directory(store, config, path, None)?;
                // A create-only rename shape has no explicit old path. The
                // authoritative direct-child comparison at the destination's
                // parent removes any sibling directory that disappeared in
                // the same filesystem transaction, including its known
                // descendants. Without this pass the new subtree is correct
                // but the old alias remains visible indefinitely.
                if let Some(parent) = path.parent() {
                    out.merge(reconcile_directory(store, config, parent, Some(1))?);
                }
                Ok(out)
            } else {
                reconcile_nearest_parent(store, config, path, Some(1))
            }
        }
        FsChangeEvent::Modified { path } => reconcile_nearest_parent(store, config, path, Some(1)),
        FsChangeEvent::Deleted { path } => {
            let mut out = ReconcileOutcome::default();
            if let Some(entry) = store.get_by_path(path) {
                if entry.parent_id.is_none() && config.roots.read().iter().any(|root| root == path)
                {
                    let (_, removed) = store.mark_root_unavailable(entry.id)?;
                    out.changed_ids.insert(entry.id);
                    out.changed_ids.extend(removed);
                    out.child_set_changed.insert(entry.id);
                } else {
                    let parent = entry.parent_id;
                    let removed = store.remove_subtree(entry.id);
                    out.changed_ids.extend(removed.iter().map(|entry| entry.id));
                    if let Some(parent) = parent {
                        out.child_set_changed.insert(parent);
                    }
                }
            } else {
                out.merge(reconcile_nearest_parent(store, config, path, Some(1))?);
            }
            Ok(out)
        }
        FsChangeEvent::Renamed { from, to } => {
            let mut out = ReconcileOutcome::default();
            let mut preserved_known_subtree = false;
            if let Some(entry) = store.get_by_path(from) {
                let parent = entry.parent_id;
                match store.rename(entry.id, to.clone()) {
                    Ok(()) => {
                        // Same-parent file and directory renames preserve EntryId
                        // and every known descendant. The store rewrites the
                        // reverse path index for the known subtree, avoiding an
                        // eager disk walk and preventing expanded children from
                        // disappearing until the next manual expansion.
                        preserved_known_subtree = true;
                        out.changed_ids.insert(entry.id);
                        if let Some(parent) = parent {
                            out.child_set_changed.insert(parent);
                        }
                    }
                    Err(_) => {
                        // Cross-parent moves are not yet an atomic store
                        // primitive. Fall back to authoritative removal + disk
                        // reconciliation below.
                        let removed = store.remove_subtree(entry.id);
                        out.changed_ids.extend(removed.iter().map(|entry| entry.id));
                        if let Some(parent) = parent {
                            out.child_set_changed.insert(parent);
                        }
                    }
                }
            }
            let depth = if preserved_known_subtree || !to.is_dir() {
                Some(1)
            } else {
                // A cross-parent directory move lost the old in-memory
                // structure. Rebuild the destination subtree now so known
                // descendants do not silently disappear.
                None
            };
            if depth.is_none() && to.is_dir() {
                out.merge(reconcile_directory(store, config, to, None)?);
            } else {
                out.merge(reconcile_nearest_parent(store, config, to, depth)?);
            }
            Ok(out)
        }
        FsChangeEvent::Unknown { path }
        | FsChangeEvent::RenameDegraded {
            missing_half: path, ..
        } => reconcile_nearest_parent(store, config, path, Some(1)),
        FsChangeEvent::Coarse { root } => {
            let scope = nearest_known_directory(store, root)
                .or_else(|| containing_root(&config.roots.read(), root))
                .unwrap_or_else(|| root.clone());
            let mut out = reconcile_directory(store, config, &scope, None)?;
            if let Some(entry) = store.get_by_path(&scope) {
                out.coarse_ids.insert(entry.id);
            }
            Ok(out)
        }
        FsChangeEvent::Error { .. } => Ok(ReconcileOutcome::default()),
    }
}

fn reconcile_nearest_parent(
    store: &EntryStore,
    config: &WatchConfig,
    path: &Path,
    depth: Option<usize>,
) -> Result<ReconcileOutcome, FxError> {
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    let scope = nearest_known_directory(store, &start)
        .or_else(|| containing_root(&config.roots.read(), path))
        .ok_or_else(|| {
            FxError::InvalidInput(format!("watch path is outside configured roots: {path:?}"))
        })?;
    reconcile_directory(store, config, &scope, depth)
}

fn nearest_known_directory(store: &EntryStore, path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if store.get_by_path(candidate).is_some_and(|entry| {
            entry.kind == EntryKind::Directory || entry.symlink_target_is_dir == Some(true)
        }) {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

fn containing_root(roots: &[PathBuf], path: &Path) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path == root.as_path() || path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .cloned()
}

/// Reconcile one directory against disk. `depth=Some(1)` updates the
/// directory and its direct children; `None` performs a complete subtree
/// reconciliation for overflow recovery.
pub(crate) fn reconcile_directory(
    store: &EntryStore,
    config: &WatchConfig,
    directory: &Path,
    depth: Option<usize>,
) -> Result<ReconcileOutcome, FxError> {
    let mut out = ReconcileOutcome::default();
    if !directory.exists() {
        if let Some(entry) = store.get_by_path(directory) {
            if entry.parent_id.is_none() && config.roots.read().iter().any(|root| root == directory)
            {
                let (_, removed) = store.mark_root_unavailable(entry.id)?;
                out.changed_ids.insert(entry.id);
                out.changed_ids.extend(removed);
                out.child_set_changed.insert(entry.id);
            } else {
                let parent = entry.parent_id;
                let removed = store.remove_subtree(entry.id);
                out.changed_ids.extend(removed.iter().map(|entry| entry.id));
                if let Some(parent) = parent {
                    out.child_set_changed.insert(parent);
                }
            }
        }
        return Ok(out);
    }

    let walk_options = WalkOptions {
        max_depth: depth,
        follow_symlinks: config.follow_symlinks,
        include_hidden: true,
        include_root: true,
        parallelism: config.walker_concurrency,
    };
    let (mut walked, repository_ignore, excludes) =
        walk_for_reconcile(directory, walk_options, config)?;
    walked.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.path.cmp(&b.path)));
    let disk_paths: HashSet<PathBuf> = walked.iter().map(|entry| entry.path.clone()).collect();

    let known = store.paths_under(directory);
    let mut missing: Vec<_> = known
        .into_iter()
        .filter(|(path, _)| in_depth(directory, path, depth) && !disk_paths.contains(path))
        .collect();
    missing.sort_by_key(|(path, _)| path.components().count());
    for (path, id) in missing {
        if store.get_by_path(&path).is_none() {
            continue;
        }
        let parent = store.get_by_id(id).and_then(|entry| entry.parent_id);
        let removed = store.remove_subtree(id);
        out.changed_ids.extend(removed.iter().map(|entry| entry.id));
        if let Some(parent) = parent {
            out.child_set_changed.insert(parent);
        }
    }

    for walked_entry in &walked {
        let existing = store.get_by_path(&walked_entry.path);
        let parent_id = existing
            .as_ref()
            .and_then(|entry| entry.parent_id)
            .or_else(|| {
                walked_entry
                    .parent_path
                    .as_ref()
                    .and_then(|parent| store.get_by_path(parent))
                    .map(|entry| entry.id)
            })
            .or_else(|| {
                // A walk rooted at a newly-created/moved directory reports no
                // parent_path for the walk root itself. Recover its real tree
                // parent from the absolute path; otherwise every create-only
                // directory rename is inserted as an extra workspace root and
                // survives deletion as a dangling subtree.
                (walked_entry.path == directory)
                    .then(|| walked_entry.path.parent())
                    .flatten()
                    .and_then(|parent| store.get_by_path(parent))
                    .map(|entry| entry.id)
            });
        let entry = entry_from_walked(
            walked_entry,
            parent_id,
            repository_ignore.as_ref(),
            excludes.as_ref(),
        );
        if let Some(existing) = existing {
            if existing.kind == EntryKind::Directory && entry.kind != EntryKind::Directory {
                let removed = store.remove_subtree(existing.id);
                out.changed_ids.extend(removed.iter().map(|entry| entry.id));
                let id = store.insert(walked_entry.path.clone(), entry)?;
                out.changed_ids.insert(id);
                if let Some(parent) = parent_id {
                    out.child_set_changed.insert(parent);
                }
            } else if store.update(existing.id, entry)? {
                out.changed_ids.insert(existing.id);
            }
        } else {
            let id = store.insert(walked_entry.path.clone(), entry)?;
            out.changed_ids.insert(id);
            if let Some(parent) = parent_id {
                out.child_set_changed.insert(parent);
            }
        }
    }

    // Publish listing completion independently of child insertion. An empty
    // directory produces no child records, and a previously URI-hydrated
    // directory may already contain only a partial chain. Without an explicit
    // completion marker both cases remain indistinguishable from "loading".
    let completed_ids: Vec<EntryId> = walked
        .iter()
        .filter(|entry| {
            let directory_like =
                entry.kind == EntryKind::Directory || entry.symlink_target_is_dir == Some(true);
            let listing_covered = entry.kind == EntryKind::Directory
                || (entry.path == directory && entry.symlink_target_is_dir == Some(true));
            let below_frontier = depth.is_none_or(|limit| (entry.depth as usize) < limit);
            let traversal_complete = entry.path == directory
                || (!repository_ignore
                    .as_ref()
                    .is_some_and(|matcher| matcher.is_ignored(&entry.path, directory_like))
                    && !excludes
                        .as_ref()
                        .is_some_and(|matcher| matcher.is_ignored(&entry.path, directory_like)));
            listing_covered && below_frontier && traversal_complete
        })
        .filter_map(|entry| store.get_by_path(&entry.path).map(|stored| stored.id))
        .collect();
    let newly_loaded = store.mark_directories_children_loaded(&completed_ids)?;
    out.child_set_changed.extend(newly_loaded);

    Ok(out)
}

fn in_depth(root: &Path, path: &Path, depth: Option<usize>) -> bool {
    match depth {
        None => true,
        Some(limit) => path
            .strip_prefix(root)
            .map(|relative| relative.components().count() <= limit)
            .unwrap_or(false),
    }
}

// The walk hands back its entries plus the two matchers it resolved; naming
// the tuple would not make the signature easier to read at the call site.
#[allow(clippy::type_complexity)]
fn walk_for_reconcile(
    directory: &Path,
    options: WalkOptions,
    config: &WatchConfig,
) -> Result<
    (
        Vec<WalkedEntry>,
        Option<IgnoreMatcher>,
        Option<IgnoreMatcher>,
    ),
    FxError,
> {
    let exclude_globs = config.exclude_globs.read().clone();
    if !config.respect_ignore && exclude_globs.is_empty() {
        return Ok((walk(directory, options)?, None, None));
    }

    let root =
        containing_root(&config.roots.read(), directory).unwrap_or_else(|| directory.to_path_buf());
    let mut traversal = IgnoreMatcher::new();
    let mut repository_ignore = IgnoreMatcher::new();
    let mut excludes = IgnoreMatcher::new();
    if !exclude_globs.is_empty() {
        traversal.add_from_string(&root, &exclude_globs.join("\n"))?;
        excludes.add_from_string(&root, &exclude_globs.join("\n"))?;
    }
    let mut anchor = root.clone();
    for segment in directory
        .strip_prefix(&root)
        .ok()
        .into_iter()
        .flat_map(|path| path.iter())
    {
        if config.respect_ignore {
            add_ignore_files(&mut traversal, &anchor);
            add_ignore_files(&mut repository_ignore, &anchor);
        }
        anchor = anchor.join(segment);
    }
    if config.respect_ignore {
        add_ignore_files(&mut traversal, directory);
        add_ignore_files(&mut repository_ignore, directory);
    }
    let walked = walk_with_ignore(directory, options, &traversal)?;
    // Keep ancestor rules that were seeded above the bounded walk, while
    // adding nested ignore files discovered during this reconciliation.
    if config.respect_ignore {
        for entry in &walked {
            if entry.path.file_name().is_some_and(|name| {
                mille_core::IGNORE_FILE_NAMES.contains(&name.to_string_lossy().as_ref())
            }) {
                let _ = repository_ignore.add_from_file(&entry.path);
            }
        }
    }
    Ok((
        walked,
        config.respect_ignore.then_some(repository_ignore),
        (!exclude_globs.is_empty()).then_some(excludes),
    ))
}

fn add_ignore_files(matcher: &mut IgnoreMatcher, directory: &Path) {
    for name in mille_core::IGNORE_FILE_NAMES {
        let candidate = directory.join(name);
        if candidate.is_file() {
            let _ = matcher.add_from_file(&candidate);
        }
    }
}

fn entry_from_walked(
    walked: &WalkedEntry,
    parent_id: Option<EntryId>,
    repository_ignore: Option<&IgnoreMatcher>,
    excludes: Option<&IgnoreMatcher>,
) -> Entry {
    let directory_like =
        walked.kind == EntryKind::Directory || walked.symlink_target_is_dir == Some(true);
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
        is_ignored: repository_ignore
            .map(|matcher| matcher.is_ignored(&walked.path, directory_like))
            .unwrap_or(false),
        is_excluded: excludes
            .map(|matcher| matcher.is_ignored(&walked.path, directory_like))
            .unwrap_or(false),
        is_readonly: walked.is_readonly,
        is_hidden: walked.is_hidden,
    }
}

fn event_paths(event: &FsChangeEvent) -> Vec<&Path> {
    match event {
        FsChangeEvent::Created { path }
        | FsChangeEvent::Modified { path }
        | FsChangeEvent::Deleted { path }
        | FsChangeEvent::Unknown { path }
        | FsChangeEvent::RenameDegraded {
            missing_half: path, ..
        } => vec![path],
        FsChangeEvent::Renamed { from, to } => vec![from, to],
        FsChangeEvent::Coarse { root } => vec![root],
        FsChangeEvent::Error { .. } => Vec::new(),
    }
}

fn to_public_event(
    store: &EntryStore,
    event: &FsChangeEvent,
    before: Option<&Entry>,
) -> FileSystemEventJs {
    let blank = |kind: &str| FileSystemEventJs {
        kind: kind.into(),
        id: None,
        parent_id: None,
        old_parent_id: None,
        new_parent_id: None,
        old_name: None,
        new_name: None,
        entry: None,
        path: None,
        code: None,
        message: None,
        detail: None,
    };
    match event {
        FsChangeEvent::Created { path } | FsChangeEvent::Modified { path } => {
            let current = store.get_by_path(path);
            let mut public = blank(if matches!(event, FsChangeEvent::Created { .. }) {
                "created"
            } else {
                "changed"
            });
            public.path = Some(path.to_string_lossy().into_owned());
            if let Some(entry) = current {
                public.id = Some(entry.id.raw() as i64);
                public.parent_id = entry.parent_id.map(|id| id.raw() as i64);
                public.entry = Some(EntryJs::from_core(&entry));
            }
            public
        }
        FsChangeEvent::Deleted { path } => {
            let mut public = blank("deleted");
            public.path = Some(path.to_string_lossy().into_owned());
            if let Some(entry) = before {
                public.id = Some(entry.id.raw() as i64);
                public.parent_id = entry.parent_id.map(|id| id.raw() as i64);
            }
            public
        }
        FsChangeEvent::Renamed { from, to } => {
            let current = store.get_by_path(to);
            let mut public = blank("renamed");
            public.path = Some(to.to_string_lossy().into_owned());
            public.old_name = from
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            public.new_name = to
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            public.old_parent_id = before
                .and_then(|entry| entry.parent_id)
                .map(|id| id.raw() as i64);
            if let Some(entry) = current {
                public.id = Some(entry.id.raw() as i64);
                public.new_parent_id = entry.parent_id.map(|id| id.raw() as i64);
                public.entry = Some(EntryJs::from_core(&entry));
            }
            public
        }
        FsChangeEvent::Unknown { path }
        | FsChangeEvent::RenameDegraded {
            missing_half: path, ..
        } => {
            let mut public = blank("warning");
            public.path = Some(path.to_string_lossy().into_owned());
            public.code = Some("WRENAMEDEGRADED".into());
            public.detail = Some("ambiguous watcher event reconciled from disk".into());
            public
        }
        FsChangeEvent::Coarse { root } => {
            let mut public = blank("overflow");
            public.path = Some(root.to_string_lossy().into_owned());
            if let Some(entry) =
                nearest_known_directory(store, root).and_then(|path| store.get_by_path(&path))
            {
                public.id = Some(entry.id.raw() as i64);
            }
            public
        }
        FsChangeEvent::Error { message } => {
            let mut public = blank("error");
            public.code = Some("EUNKNOWN".into());
            public.message = Some(message.clone());
            public
        }
    }
}
