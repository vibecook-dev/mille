//! Per-checkout sessions and the registry that de-dupes them.
//!
//! One session per checkout root (realpath), however many caller paths open
//! it: `open('/repo/packages/app')` and `open('/repo/packages/other')` share
//! one session, one gitdir watch and one status queue. Each `open` takes a
//! reference; `close` drops one, and only the last `close` tears the session
//! down (SPEC mille-git §9.2).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::error::{GitError, Result};
use crate::identity::{Discovered, GitIdentity, GitRepoKind};
use crate::paths;
use crate::queue::StatusQueue;
use crate::status::{self, StatusBackend, StatusSnapshot};
use crate::warning::WarningCode;
use crate::watch::GitdirWatch;
use crate::worktree_list::{self, GitWorktree};
use crate::Inner;

/// A status subscriber; called on the engine runtime after each publish.
pub type StatusCallback = Arc<dyn Fn(Arc<StatusSnapshot>) + Send + Sync>;

/// A worktree-list subscriber; called when `<common>/worktrees/` changed and
/// the list differs from the last one seen.
pub type WorktreesCallback = Arc<dyn Fn(&[GitWorktree]) + Send + Sync>;

/// The published status of one session.
struct StatusState {
    /// The last merged set, uncapped — what the next path-limited run
    /// merges into, so the row cap never loses rows across merges.
    all: Option<Arc<StatusSnapshot>>,
    /// `all` with the row cap applied: what callers see.
    published: Option<Arc<StatusSnapshot>>,
    generation: u64,
    /// §21.3: gix by default; a session can be pinned to the CLI.
    backend: StatusBackend,
}

#[derive(Default)]
struct WorktreeState {
    refresh_armed: bool,
    last: Option<Vec<GitWorktree>>,
    subscribers: Vec<(u64, WorktreesCallback)>,
}

pub(crate) struct Session {
    /// Realpath of the checkout root (or of the caller path for
    /// `kind: none`). The registry key and the hint-routing prefix (§11.2.1).
    pub(crate) real_root: PathBuf,
    identity: RwLock<GitIdentity>,
    /// `gix::Repository` is `Send` but not `Sync`: sessions keep the
    /// thread-safe handle and call `to_thread_local()` per operation (§9.2).
    pub(crate) repo: Option<gix::ThreadSafeRepository>,
    /// Fired by the last `close` (and by engine drop): in-flight reads stop,
    /// timers stop re-arming.
    pub(crate) cancel: CancellationToken,
    status: Mutex<StatusState>,
    pub(crate) queue: StatusQueue,
    /// The sequence number of the last published run; status waiters watch it.
    published_seq: tokio::sync::watch::Sender<u64>,
    status_subscribers: Mutex<Vec<(u64, StatusCallback)>>,
    next_subscriber: AtomicU64,
    /// Read side: diff / log (PR 5). Write side: blocking mutations.
    pub(crate) ops: Arc<tokio::sync::RwLock<()>>,
    watch: Mutex<Option<GitdirWatch>>,
    worktrees: Mutex<WorktreeState>,
}

impl Session {
    pub(crate) fn new(discovered: Discovered, real_root: PathBuf) -> Self {
        Self {
            real_root,
            identity: RwLock::new(discovered.identity),
            repo: discovered.repo,
            cancel: CancellationToken::new(),
            status: Mutex::new(StatusState {
                all: None,
                published: None,
                generation: 0,
                backend: StatusBackend::Gix,
            }),
            queue: StatusQueue::default(),
            published_seq: tokio::sync::watch::Sender::new(0),
            status_subscribers: Mutex::new(Vec::new()),
            next_subscriber: AtomicU64::new(0),
            ops: Arc::new(tokio::sync::RwLock::new(())),
            watch: Mutex::new(None),
            worktrees: Mutex::new(WorktreeState::default()),
        }
    }

    pub(crate) fn identity(&self) -> GitIdentity {
        self.identity.read().clone()
    }

    pub(crate) fn kind(&self) -> GitRepoKind {
        self.identity.read().kind
    }

    /// Re-read the cheap HEAD fields (branch switch, commit) from the repo.
    pub(crate) fn refresh_head(&self) -> GitIdentity {
        if let Some(repo) = &self.repo {
            let (head, branch, detached) = crate::identity::read_head(&repo.to_thread_local());
            let mut identity = self.identity.write();
            identity.head = head;
            identity.branch = branch;
            identity.detached = detached;
        }
        self.identity()
    }

    pub(crate) fn set_backend(&self, backend: StatusBackend) {
        self.status.lock().backend = backend;
    }

    /// The last published snapshot, if any run has completed.
    pub(crate) fn published(&self) -> Option<Arc<StatusSnapshot>> {
        self.status.lock().published.clone()
    }

    pub(crate) fn set_watch(&self, watch: Option<GitdirWatch>) {
        *self.watch.lock() = watch;
    }

    /// Tear down: stop timers, interrupt the in-flight walk, and drop the
    /// watcher (which joins notify's thread).
    pub(crate) fn shutdown(&self) {
        self.cancel.cancel();
        self.interrupt_status();
        let watch = self.watch.lock().take();
        drop(watch);
    }

    /// Widen a path-limited run's pathspecs with both names of every known
    /// rename row it touches. A pathspec that holds only one side of a
    /// rename cannot see the pair — `git status -- b.ts` reports `A.`, not
    /// `R.` — so without this, editing a staged rename's new file would
    /// downgrade its row to an addition and lose `origPath` until the next
    /// full run. Returns `None` (run full) if widening crosses the
    /// path-limited ceiling, keeping the run and its merge consistent.
    pub(crate) fn widen_with_renames(&self, hints: Vec<String>) -> Option<Vec<String>> {
        let state = self.status.lock();
        let mut widened: std::collections::BTreeSet<String> = hints.into_iter().collect();
        if let Some(all) = &state.all {
            for entry in &all.entries {
                let Some(orig) = &entry.orig_path else {
                    continue;
                };
                if widened.contains(&entry.path) || widened.contains(orig) {
                    widened.insert(entry.path.clone());
                    widened.insert(orig.clone());
                }
            }
        }
        (widened.len() <= status::MAX_HINT_PATHS).then(|| widened.into_iter().collect())
    }

    /// Merge `run` into the session's set (§10.6), cap it, publish it as run
    /// `sequence`.
    pub(crate) fn publish(
        &self,
        mut run: StatusSnapshot,
        hints: Option<&[String]>,
        sequence: u64,
        cap: usize,
    ) -> Arc<StatusSnapshot> {
        let published = {
            let mut state = self.status.lock();
            state.generation += 1;
            run.generation = state.generation;
            let merged = Arc::new(status::merge_status(state.all.as_deref(), run, hints));
            let mut capped = StatusSnapshot::clone(&merged);
            status::apply_row_cap(&mut capped, cap);
            let published = Arc::new(capped);
            state.all = Some(merged);
            state.published = Some(published.clone());
            published
        };
        self.published_seq.send_replace(sequence);
        published
    }

    /// Call every status subscriber (outside all locks).
    pub(crate) fn notify_status(&self, snapshot: &Arc<StatusSnapshot>) {
        let subscribers: Vec<StatusCallback> = self
            .status_subscribers
            .lock()
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for callback in subscribers {
            callback(snapshot.clone());
        }
    }

    pub(crate) fn subscribe_status(&self, callback: StatusCallback) -> u64 {
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed) + 1;
        self.status_subscribers.lock().push((id, callback));
        id
    }

    pub(crate) fn unsubscribe_status(&self, id: u64) {
        self.status_subscribers.lock().retain(|(sub, _)| *sub != id);
    }

    /// Wait until run `target` (or a later one) has published.
    pub(crate) async fn wait_published(&self, target: u64) -> Result<Arc<StatusSnapshot>> {
        let mut seq = self.published_seq.subscribe();
        tokio::select! {
            result = seq.wait_for(|published| *published >= target) => {
                result.map_err(|_| GitError::canceled())?;
            }
            _ = self.cancel.cancelled() => return Err(GitError::canceled()),
        }
        self.published()
            .ok_or_else(|| GitError::unknown("status published nothing"))
    }

    /// Compute one status run (full when `hints` is `None`), unpublished.
    ///
    /// Never fails: not-a-repo and bare yield an empty run, a gix failure
    /// degrades to the CLI, and a CLI failure yields an empty result with a
    /// warning (§23) — decorations must not throw.
    pub(crate) async fn compute_status(
        &self,
        inner: &Inner,
        hints: Option<Vec<String>>,
        interrupt: Arc<AtomicBool>,
    ) -> StatusSnapshot {
        let started = Instant::now();
        let root = self.identity.read().worktree_path.clone();
        if self.kind() != GitRepoKind::Worktree {
            let identity = self.refresh_head();
            return StatusSnapshot::empty(identity, 0);
        }
        let prefer_gix = self.status.lock().backend == StatusBackend::Gix;
        let mut backend = StatusBackend::Gix;
        let mut entries = None;
        if let (true, Some(repo)) = (prefer_gix, self.repo.clone()) {
            let pathspecs = hints.clone();
            let computed = tokio::task::spawn_blocking(move || {
                status::gix_status(&repo.to_thread_local(), pathspecs.as_deref(), interrupt)
            })
            .await;
            match computed {
                Ok(Ok(rows)) => entries = Some(rows),
                Ok(Err(message)) => inner.warn(
                    WarningCode::DegradedCli,
                    format!("gix status failed: {message}"),
                    &root,
                ),
                Err(join) => inner.warn(
                    WarningCode::DegradedCli,
                    format!("gix status panicked: {join}"),
                    &root,
                ),
            }
        }
        let entries = match entries {
            Some(rows) => rows,
            None => {
                backend = StatusBackend::Cli;
                match status::cli_status(&inner.git, &root, hints.as_deref()).await {
                    Ok((rows, parse_warnings)) => {
                        for warning in parse_warnings {
                            inner.warn(WarningCode::Parse, warning, &root);
                        }
                        rows
                    }
                    Err(err) => {
                        let code = if err.kind() == crate::GitErrorKind::NoGit {
                            WarningCode::NoGit
                        } else {
                            WarningCode::DegradedCli
                        };
                        inner.warn(code, format!("git status fallback failed: {err}"), &root);
                        Vec::new()
                    }
                }
            }
        };
        let identity = self.refresh_head();
        StatusSnapshot {
            worktree_path: identity.worktree_path.clone(),
            identity,
            entries,
            truncated: None,
            took_ms: started.elapsed().as_millis() as u64,
            path_limited: false,
            generation: 0,
            status_backend: backend,
        }
    }

    /// The worktree list (§16), read through the CLI.
    pub(crate) async fn list_worktrees(&self, inner: &Inner) -> Result<Vec<GitWorktree>> {
        if self.kind() == GitRepoKind::None {
            return Err(GitError::not_a_repo(&self.identity().worktree_path));
        }
        let root = self.identity().worktree_path;
        worktree_list::list_worktrees(&inner.git, &root, &self.real_root).await
    }

    pub(crate) fn subscribe_worktrees(&self, callback: WorktreesCallback) -> u64 {
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed) + 1;
        self.worktrees.lock().subscribers.push((id, callback));
        id
    }

    pub(crate) fn unsubscribe_worktrees(&self, id: u64) {
        self.worktrees
            .lock()
            .subscribers
            .retain(|(sub, _)| *sub != id);
    }

    /// `<common>/worktrees/` changed: refresh the list after the debounce
    /// and tell subscribers if it differs (§11.4, F5).
    pub(crate) fn worktrees_changed(self: &Arc<Self>, inner: &Arc<Inner>) {
        {
            let mut state = self.worktrees.lock();
            if state.refresh_armed || self.cancel.is_cancelled() {
                return;
            }
            state.refresh_armed = true;
        }
        let (session, engine) = (self.clone(), inner.clone());
        inner.handle.spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(engine.options.debounce) => {}
                _ = session.cancel.cancelled() => return,
            }
            session.worktrees.lock().refresh_armed = false;
            // The first `git worktree add` creates `worktrees/` itself; watch
            // it from now on. (Never from the notify callback: adding a watch
            // from notify's own thread deadlocks inotify.)
            if let Some(common) = session.identity().common_dir {
                let dir = common.join("worktrees");
                if let (true, Some(watch)) = (dir.is_dir(), session.watch.lock().as_mut()) {
                    watch.ensure(&dir);
                }
            }
            let list = match session.list_worktrees(&engine).await {
                Ok(list) => list,
                Err(err) => {
                    engine.warn(
                        WarningCode::Watch,
                        format!("worktree list refresh failed: {err}"),
                        &session.real_root,
                    );
                    return;
                }
            };
            let subscribers: Vec<WorktreesCallback> = {
                let mut state = session.worktrees.lock();
                if state.last.as_ref() == Some(&list) {
                    return;
                }
                state.last = Some(list.clone());
                state.subscribers.iter().map(|(_, cb)| cb.clone()).collect()
            };
            for callback in subscribers {
                callback(&list);
            }
        });
    }
}

struct Entry {
    session: Arc<Session>,
    refcount: usize,
    aliases: Vec<PathBuf>,
}

#[derive(Default)]
pub(crate) struct Registry {
    by_key: HashMap<PathBuf, Entry>,
    aliases: HashMap<PathBuf, PathBuf>,
}

impl Registry {
    /// Resolve any spelling a caller may use — the exact path given to
    /// `open`, the checkout root, or a realpath of either — to its session.
    pub(crate) fn lookup(&self, path: &Path) -> Option<Arc<Session>> {
        self.key_for(path)
            .and_then(|key| self.by_key.get(&key))
            .map(|entry| entry.session.clone())
    }

    fn key_for(&self, path: &Path) -> Option<PathBuf> {
        let lexical = paths::normalize_lexical(path);
        if let Some(key) = self.aliases.get(&lexical) {
            return Some(key.clone());
        }
        let real = paths::canonicalize_lenient(&lexical);
        if self.by_key.contains_key(&real) {
            return Some(real);
        }
        self.aliases.get(&real).cloned()
    }

    /// A second `open` of a caller path already registered: take a reference
    /// without rediscovering.
    pub(crate) fn join_known(&mut self, caller: &Path) -> Option<Arc<Session>> {
        let key = self.aliases.get(caller)?.clone();
        let entry = self.by_key.get_mut(&key)?;
        entry.refcount += 1;
        Some(entry.session.clone())
    }

    /// Insert a freshly discovered session, or join the one already keyed
    /// by the same realpath (a sibling package of an open checkout, or a
    /// concurrent `open` that won the race). Returns `(session, created)`.
    pub(crate) fn insert_or_join(
        &mut self,
        discovered: Discovered,
        caller: &Path,
    ) -> (Arc<Session>, bool) {
        let key = paths::canonicalize_lenient(&discovered.root);
        let mut spellings = vec![
            caller.to_path_buf(),
            paths::normalize_lexical(&discovered.root),
            key.clone(),
        ];
        spellings.push(paths::canonicalize_lenient(caller));
        let created = !self.by_key.contains_key(&key);
        let entry = self.by_key.entry(key.clone()).or_insert_with(|| Entry {
            session: Arc::new(Session::new(discovered, key.clone())),
            refcount: 0,
            aliases: Vec::new(),
        });
        entry.refcount += 1;
        for spelling in spellings {
            if !entry.aliases.contains(&spelling) {
                entry.aliases.push(spelling.clone());
            }
            self.aliases.insert(spelling, key.clone());
        }
        (entry.session.clone(), created)
    }

    /// Drop one reference; returns the session when that was the last one.
    pub(crate) fn release(&mut self, path: &Path) -> Option<Arc<Session>> {
        let key = self.key_for(path)?;
        let entry = self.by_key.get_mut(&key)?;
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount > 0 {
            return None;
        }
        let entry = self.by_key.remove(&key)?;
        for alias in &entry.aliases {
            self.aliases.remove(alias);
        }
        Some(entry.session)
    }

    pub(crate) fn drain(&mut self) -> Vec<Arc<Session>> {
        self.aliases.clear();
        self.by_key
            .drain()
            .map(|(_, entry)| entry.session)
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.by_key.len()
    }

    pub(crate) fn sessions(&self) -> Vec<Arc<Session>> {
        self.by_key
            .values()
            .map(|entry| entry.session.clone())
            .collect()
    }

    /// The realpath key a spelling resolves to. Diagnostic.
    pub(crate) fn real_root_of(&self, path: &Path) -> Option<PathBuf> {
        self.lookup(path).map(|session| session.real_root.clone())
    }
}
