//! `mille-git` — the git engine behind mille's Changes / History surfaces.
//!
//! gix 0.87 reads (status, diff, log, objects), the git CLI writes (stage,
//! restore, commit — hooks, LFS and credentials stay git's), one operation
//! queue per open worktree, and watches on the resolved gitdir. No napi: the
//! binding exposes this behind its `git` feature (SPEC mille-git §8).
//!
//! The crate owns no explorer state. It never takes `mille-core`'s
//! `EntryStore` write lock, never calls `takePendingChanges`, and never
//! re-watches a worktree the explorer already watches (§11, §12.4).

#![warn(missing_docs)]

mod cli;
mod containment;
mod diff;
mod error;
mod gitdir;
mod identity;
mod log;
mod mutation;
mod paths;
mod porcelain;
mod queue;
mod route;
mod session;
mod status;
mod warning;
mod watch;
mod worktree_list;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::runtime::{Handle, Runtime};

pub use cli::{argv_prefix, chunk_paths, ALIAS_GUARDED_VERBS, MAX_PATHS_ARGV_CHARS};
pub use containment::{
    assert_path_under_root, assert_real_path_under_root, assert_safe_revision,
    is_safe_workspace_relative_path,
};
pub use diff::{
    diff_sides, DiffCaps, DiffHunk, DiffLine, DiffLineKind, DiffRequest, DiffResult, DiffSide,
    DiffSpec, BINARY_SNIFF_BYTES, CONTEXT_LINES, MAX_LINE_CHARS,
};
pub use error::{GitError, GitErrorKind, Result};
pub use gitdir::{find_dot_git, resolve_git_dir, MAX_DOT_GIT_WALK};
pub use identity::{GitIdentity, GitRepoKind};
pub use log::{
    CommitFile, LogCommit, LogPage, LogRequest, ReadObjectRequest, COMMIT_FILES_CAP, MAX_LOG_PAGE,
};
pub use mutation::{CommitRequest, OpOptions};
pub use porcelain::parse_porcelain_v2;
pub use session::{StatusCallback, WorktreesCallback};
pub use status::{
    apply_row_cap, merge_status, sort_entries, status_thread_limit, StatusBackend, StatusEntry,
    StatusLetter, StatusSnapshot, StatusXy, MAX_HINT_PATHS,
};
pub use tokio_util::sync::CancellationToken;
pub use warning::{GitWarning, WarningCallback, WarningCode};
pub use worktree_list::GitWorktree;

/// Raw file bytes from `read_object` / `read_worktree_file`. Text or binary
/// is the caller's call (the TypeScript layer returns valid UTF-8 as a
/// string, as `getContents` does today).
pub type ObjectBytes = Vec<u8>;

use cli::GitCli;
use containment::AllowList;
use queue::{Hints, Urgency};
use session::{Registry, Session};

/// Engine configuration (SPEC mille-git §9.1, Appendix B).
#[derive(Clone, Debug)]
pub struct GitEngineOptions {
    /// Absolute explorer / workspace roots. The `open()` caller path and every
    /// mutation / working-tree read MUST be under one of these. Discovering
    /// `.git` MAY walk above them. Empty → every `open` fails (fail closed).
    pub allowed_roots: Vec<PathBuf>,
    /// The git binary. `None` (or a bare `git`) probes `PATH`, then the
    /// well-known install locations (§32.2).
    pub git_path: Option<PathBuf>,
    /// Status row cap; the rest is reported as `truncated`. Default 10 000.
    pub status_row_cap: usize,
    /// Hint debounce. Default 75 ms (the explorer's default).
    pub debounce: Duration,
    /// Longest a continuous hint stream may defer a status run. Default 500 ms.
    pub max_wait: Duration,
    /// Idle time after a path-limited run before a full rescan (§21.1).
    /// Default 5 s.
    pub idle_rescan: Duration,
    /// Diff auto-render byte cap. Default 256 KiB.
    pub auto_render_max_bytes: usize,
    /// Diff auto-render line cap. Default 5 000.
    pub auto_render_max_lines: usize,
    /// Either side (or the patch) at or above this is refused before load.
    /// Default 1 MiB.
    pub hard_refuse_bytes: usize,
    /// Default log page size. Default 100.
    pub log_page_default: usize,
    /// Kill a commit (hooks included) after this long. Default 60 s.
    pub commit_timeout: Duration,
    /// Let `advise_performance_config` also write the keys it advises.
    /// Default false — the engine does not mutate arbitrary clones.
    pub write_performance_config: bool,
}

impl Default for GitEngineOptions {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            git_path: None,
            status_row_cap: 10_000,
            debounce: Duration::from_millis(75),
            max_wait: Duration::from_millis(500),
            idle_rescan: Duration::from_secs(5),
            auto_render_max_bytes: 262_144,
            auto_render_max_lines: 5_000,
            hard_refuse_bytes: 1_048_576,
            log_page_default: 100,
            commit_timeout: Duration::from_secs(60),
            write_performance_config: false,
        }
    }
}

pub(crate) struct Inner {
    pub(crate) options: GitEngineOptions,
    pub(crate) allow: AllowList,
    pub(crate) git: GitCli,
    pub(crate) handle: Handle,
    pub(crate) registry: Mutex<Registry>,
    pub(crate) warnings: warning::Warnings,
    pub(crate) metrics: queue::Metrics,
}

/// The engine: a registry of per-checkout sessions plus the runtime their
/// work runs on. Construction does no I/O beyond probing for `git` (§9.1).
///
/// Every async method runs its work on the engine's runtime, so callers may
/// await from any executor (napi's runtime, a test's, `block_on`).
pub struct GitEngine {
    inner: Arc<Inner>,
    runtime: Option<Runtime>,
}

impl GitEngine {
    /// Build an engine with its own small multi-threaded runtime.
    ///
    /// # Panics
    ///
    /// When the OS refuses to start the runtime's threads; see [`try_new`](Self::try_new).
    pub fn new(options: GitEngineOptions) -> Self {
        Self::try_new(options).expect("mille-git: cannot start the engine runtime")
    }

    /// [`new`](Self::new), reporting a runtime start failure instead of panicking.
    pub fn try_new(options: GitEngineOptions) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("mille-git")
            .enable_all()
            .build()
            .map_err(|err| GitError::io(&err, "start the mille-git runtime"))?;
        let handle = runtime.handle().clone();
        Ok(Self {
            inner: Self::inner(options, handle),
            runtime: Some(runtime),
        })
    }

    /// Build an engine that schedules its work on an existing runtime (the
    /// binding's). The runtime must have its I/O and time drivers enabled.
    pub fn with_runtime(options: GitEngineOptions, handle: Handle) -> Self {
        Self {
            inner: Self::inner(options, handle),
            runtime: None,
        }
    }

    fn inner(options: GitEngineOptions, handle: Handle) -> Arc<Inner> {
        Arc::new(Inner {
            allow: AllowList::new(&options.allowed_roots),
            git: GitCli::probe(options.git_path.as_deref()),
            options,
            handle,
            registry: Mutex::new(Registry::default()),
            warnings: warning::Warnings::default(),
            metrics: queue::Metrics::default(),
        })
    }

    /// The git binary writes will use, or `None` when none was found (writes
    /// then fail `ENOGIT`; gix reads still work).
    pub fn git_path(&self) -> Option<&Path> {
        self.inner.git.binary()
    }

    /// The options this engine was built with.
    pub fn options(&self) -> &GitEngineOptions {
        &self.inner.options
    }

    /// Open a worktree (or any directory under `allowedRoots`).
    ///
    /// Returns the checkout's identity; `worktree_path` is the discovered
    /// checkout root, which may be an ancestor of `worktree` and of every
    /// allowed root. A directory that is not a repository opens as
    /// `kind: None`. Re-opening a checkout (by any path inside it) shares the
    /// existing session.
    pub async fn open(&self, worktree: &Path) -> Result<GitIdentity> {
        if !worktree.is_absolute() {
            return Err(GitError::invalid(format!(
                "open() needs an absolute path: {}",
                worktree.display()
            )));
        }
        let caller = paths::normalize_lexical(worktree);
        if self.inner.allow.is_empty() {
            return Err(GitError::invalid(
                "no allowedRoots configured; open() fails closed",
            ));
        }
        if !self.inner.allow.admits(&caller) {
            return Err(GitError::invalid(format!(
                "path is outside allowedRoots: {}",
                caller.display()
            ))
            .with_path(caller.display().to_string()));
        }
        let inner = self.inner.clone();
        self.spawn(async move { inner.open(caller).await }).await
    }

    /// Drop one `open` reference; the last one tears the session down
    /// (watchers stop, in-flight reads abort). Idempotent.
    ///
    /// Reads abort at once; a mutation already running is allowed to finish
    /// (waited for up to `commit_timeout`), so a close never strands a
    /// half-made commit.
    pub async fn close(&self, worktree: &Path) -> Result<()> {
        let removed = self.inner.registry.lock().release(worktree);
        if let Some(session) = removed {
            session.shutdown();
            let (ops, timeout) = (session.ops.clone(), self.inner.options.commit_timeout);
            self.spawn(async move {
                let _ = tokio::time::timeout(timeout, ops.write_owned()).await;
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    /// The current identity of an open worktree, with `HEAD` re-read.
    pub fn identity(&self, worktree: &Path) -> Result<GitIdentity> {
        Ok(self.inner.session(worktree)?.refresh_head())
    }

    /// The realpath'd checkout root a spelling resolves to — the session key
    /// and the prefix hint routing matches against. Diagnostic.
    #[doc(hidden)]
    pub fn session_key(&self, worktree: &Path) -> Option<PathBuf> {
        self.inner.registry.lock().real_root_of(worktree)
    }

    /// The current status snapshot: whole-repo, XY preserved, sorted and
    /// capped. Never fails for not-a-repo, bare or a missing git binary — it
    /// returns an empty snapshot (and warns) instead (§23).
    ///
    /// Returns the published snapshot at once when the queue is idle;
    /// otherwise waits for the run that settles what is already queued.
    pub async fn status(&self, worktree: &Path) -> Result<Arc<StatusSnapshot>> {
        let session = self.inner.session(worktree)?;
        let target = match (session.queue.settle_target(), session.published()) {
            (None, Some(snapshot)) => return Ok(snapshot),
            (None, None) => session.hint(&self.inner, Hints::Full, Urgency::Now),
            (Some(target), _) => target,
        };
        self.spawn(async move { session.wait_published(target).await })
            .await
    }

    /// Queue a status run now — full when `paths` is `None`, else limited to
    /// those worktree-relative POSIX paths and merged per §10.6 — and return
    /// the snapshot of the run that carried it. Unsafe paths are dropped.
    pub async fn refresh(
        &self,
        worktree: &Path,
        paths: Option<Vec<String>>,
    ) -> Result<Arc<StatusSnapshot>> {
        let session = self.inner.session(worktree)?;
        let hints = match paths {
            None => Hints::Full,
            Some(paths) => {
                let safe: Vec<String> = paths
                    .into_iter()
                    .filter(|p| is_safe_workspace_relative_path(p))
                    .collect();
                if safe.is_empty() {
                    return self.status(worktree).await;
                }
                Hints::paths(safe)
            }
        };
        let target = session.hint(&self.inner, hints, Urgency::Now);
        self.spawn(async move { session.wait_published(target).await })
            .await
    }

    /// Path hints from a same-process explorer (SPEC §11.2): `paths` are
    /// **worktree-relative POSIX** (`None` = full status). Synchronous,
    /// non-blocking, safe on a notify OS thread with no runtime context —
    /// it only queues work and arms the debounce. An empty list is a full
    /// status; unsafe paths are dropped; an unknown worktree is ignored.
    pub fn hint_paths(&self, worktree: &Path, paths: Option<Vec<String>>) {
        let Some(session) = self.inner.registry.lock().lookup(worktree) else {
            return;
        };
        let hints = match paths {
            None => Hints::Full,
            Some(paths) if paths.is_empty() => Hints::Full,
            Some(paths) => {
                let safe: Vec<String> = paths
                    .into_iter()
                    .filter(|p| is_safe_workspace_relative_path(p))
                    .collect();
                if safe.is_empty() {
                    return;
                }
                Hints::paths(safe)
            }
        };
        session.hint(&self.inner, hints, Urgency::Debounced);
    }

    /// Route explorer `'event'` OS paths to the sessions that own them
    /// (§11.2.1): realpath, longest checkout-prefix match, strip to
    /// worktree-relative POSIX, hint. Paths in no open checkout, inside a
    /// `.git`, or naming lock files are dropped. Synchronous and
    /// non-blocking, like [`hint_paths`](Self::hint_paths).
    pub fn hint_os_paths(&self, paths: &[PathBuf]) {
        let sessions = self.inner.registry.lock().sessions();
        for (session, relative) in route::route(&sessions, paths) {
            session.hint(&self.inner, Hints::paths(relative), Urgency::Debounced);
        }
    }

    /// A coarse or overflow explorer event at `path`: full status for every
    /// session whose checkout root is a prefix of it — never for checkouts
    /// that merely sit *under* it (§11.2 rule 4).
    pub fn hint_coarse_os_path(&self, path: &Path) {
        let sessions = self.inner.registry.lock().sessions();
        for session in route::coarse_owners(&sessions, path) {
            session.hint(&self.inner, Hints::Full, Urgency::Debounced);
        }
    }

    /// Subscribe to a worktree's published snapshots.
    pub fn subscribe_status(&self, worktree: &Path, callback: StatusCallback) -> Result<u64> {
        Ok(self.inner.session(worktree)?.subscribe_status(callback))
    }

    /// Remove a status subscriber. Unknown ids and worktrees are ignored.
    pub fn unsubscribe_status(&self, worktree: &Path, id: u64) {
        if let Ok(session) = self.inner.session(worktree) {
            session.unsubscribe_status(id);
        }
    }

    /// Every worktree of the repository, primary included (§16).
    pub async fn worktrees(&self, worktree: &Path) -> Result<Vec<GitWorktree>> {
        let session = self.inner.session(worktree)?;
        let inner = self.inner.clone();
        self.spawn(async move { session.list_worktrees(&inner).await })
            .await
    }

    /// Subscribe to worktree-list changes (`git worktree add/remove` by
    /// anyone), debounced.
    pub fn subscribe_worktrees(&self, worktree: &Path, callback: WorktreesCallback) -> Result<u64> {
        Ok(self.inner.session(worktree)?.subscribe_worktrees(callback))
    }

    /// Remove a worktree-list subscriber.
    pub fn unsubscribe_worktrees(&self, worktree: &Path, id: u64) {
        if let Ok(session) = self.inner.session(worktree) {
            session.unsubscribe_worktrees(id);
        }
    }

    /// Pin a session's status backend (§21.3): `Cli` when gix measures
    /// slower than CLI + fsmonitor on a repository; `Gix` is the default.
    pub fn set_status_backend(&self, worktree: &Path, backend: StatusBackend) -> Result<()> {
        self.inner.session(worktree)?.set_backend(backend);
        Ok(())
    }

    /// Subscribe to the `'warning'` channel (§22.2). Returns an id for
    /// [`unsubscribe_warnings`](Self::unsubscribe_warnings).
    pub fn subscribe_warnings(&self, callback: WarningCallback) -> u64 {
        self.inner.warnings.subscribe(callback)
    }

    /// Remove a warning subscriber.
    pub fn unsubscribe_warnings(&self, id: u64) {
        self.inner.warnings.unsubscribe(id);
    }

    /// `git add -- <paths>` (worktree-relative POSIX, contained under the
    /// checkout and `allowedRoots`). Empty `paths` is a no-op success.
    pub async fn stage(&self, worktree: &Path, paths: &[String], options: OpOptions) -> Result<()> {
        let (inner, session, paths) = (
            self.inner.clone(),
            self.inner.session(worktree)?,
            paths.to_vec(),
        );
        self.spawn(async move {
            mutation::stage(&inner, &session, &paths, options.cancel.as_ref()).await
        })
        .await
    }

    /// Unstage `paths` (`git restore --staged`, with the `reset` and
    /// unborn-branch fallbacks). Empty `paths` is a no-op success.
    pub async fn unstage(
        &self,
        worktree: &Path,
        paths: &[String],
        options: OpOptions,
    ) -> Result<()> {
        let (inner, session, paths) = (
            self.inner.clone(),
            self.inner.session(worktree)?,
            paths.to_vec(),
        );
        self.spawn(async move {
            mutation::unstage(&inner, &session, &paths, options.cancel.as_ref()).await
        })
        .await
    }

    /// Discard worktree changes, restoring **index** bytes (never
    /// `--source=HEAD`; AC-017). Empty `paths` is a no-op success.
    pub async fn restore(
        &self,
        worktree: &Path,
        paths: &[String],
        options: OpOptions,
    ) -> Result<()> {
        let (inner, session, paths) = (
            self.inner.clone(),
            self.inner.session(worktree)?,
            paths.to_vec(),
        );
        self.spawn(async move {
            mutation::restore(&inner, &session, &paths, options.cancel.as_ref()).await
        })
        .await
    }

    /// Commit the include-set: stage it, unstage everything else, write the
    /// message to `git commit -F -`'s stdin. Hooks run. Returns the new
    /// commit's full hex id (AC-006).
    pub async fn commit(&self, worktree: &Path, request: CommitRequest) -> Result<String> {
        let (inner, session) = (self.inner.clone(), self.inner.session(worktree)?);
        self.spawn(async move { mutation::commit(&inner, &session, request).await })
            .await
    }

    /// The config that makes status cheap on an ADE-created worktree
    /// (§21.2): `core.untrackedCache`, and `core.fsmonitor` where git ≥ 2.37
    /// has the builtin daemon. Written only with `write_performance_config`.
    pub async fn advise_performance_config(
        &self,
        worktree: &Path,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let (inner, session) = (self.inner.clone(), self.inner.session(worktree)?);
        self.spawn(async move { mutation::advise_performance_config(&inner, &session).await })
            .await
    }

    /// A one-path diff with caps (§13): sizes from object headers / `stat`
    /// first, `tooLarge` before any load at `hard_refuse_bytes`, binary
    /// detection, hunks only under the auto-render caps.
    pub async fn diff(&self, worktree: &Path, request: DiffRequest) -> Result<DiffResult> {
        let cancel = request.cancel.clone();
        self.read(
            worktree,
            Permits::Diff,
            cancel,
            move |inner, session| async move {
                let root = session.identity().worktree_path;
                assert_path_under_root(&root, &request.path)?;
                let kind = session.kind();
                if kind == GitRepoKind::None {
                    return Err(GitError::not_a_repo(&root));
                }
                if kind == GitRepoKind::Bare && request.spec != DiffSpec::Commit {
                    return Err(GitError::new(
                        GitErrorKind::Unsupported,
                        "a bare repository has no index or worktree",
                    ));
                }
                let commit = match (request.spec, request.commit.as_deref()) {
                    (DiffSpec::Commit, None) => {
                        return Err(GitError::invalid("a commit diff needs `commit`"))
                    }
                    (DiffSpec::Commit, Some(rev)) => Some(assert_safe_revision(rev)?.to_owned()),
                    _ => None,
                };
                if request.spec == DiffSpec::Worktree
                    && !inner
                        .allow
                        .admits(&paths::join_posix(&session.real_root, &request.path))
                {
                    return Err(GitError::invalid(format!(
                        "path is outside allowedRoots: {}",
                        request.path
                    )));
                }
                let repo = session.repo.clone().ok_or_else(no_gix)?;
                let options = &inner.options;
                let caps = DiffCaps {
                    auto_render_max_bytes: request
                        .max_bytes
                        .unwrap_or(options.auto_render_max_bytes)
                        .min(options.hard_refuse_bytes),
                    auto_render_max_lines: request
                        .max_lines
                        .unwrap_or(options.auto_render_max_lines),
                    hard_refuse_bytes: options.hard_refuse_bytes,
                };
                let real_root = session.real_root.clone();
                blocking(move || {
                    let local = repo.to_thread_local();
                    let path = request.path.as_str();
                    let blob = |id| diff::BlobSide {
                        repo: repo.clone(),
                        id,
                    };
                    match request.spec {
                        DiffSpec::Worktree => {
                            let old = blob(diff::blob_in_index(&local, path)?);
                            let new = diff::WorktreeSide {
                                real_root,
                                relative: path.to_owned(),
                            };
                            diff_sides(path, &old, &new, caps)
                        }
                        DiffSpec::Index => {
                            let old = blob(diff::blob_in_revision(&local, "HEAD", path)?);
                            let new = blob(diff::blob_in_index(&local, path)?);
                            diff_sides(path, &old, &new, caps)
                        }
                        DiffSpec::Commit => {
                            let commit = commit.unwrap_or_default();
                            local.rev_parse_single(commit.as_str()).map_err(|e| {
                                GitError::invalid(format!("unknown revision {commit}: {e}"))
                            })?;
                            let old =
                                blob(diff::blob_in_revision(&local, &format!("{commit}^"), path)?);
                            let new = blob(diff::blob_in_revision(&local, &commit, path)?);
                            diff_sides(path, &old, &new, caps)
                        }
                    }
                })
                .await
            },
        )
        .await
    }

    /// A page of history (§14.1): gix's revision walk, or — with a path
    /// filter — `git log`, whose default history simplification is what
    /// `FileHistoryClient.getHistory` shows today.
    pub async fn log(&self, worktree: &Path, request: LogRequest) -> Result<LogPage> {
        let cancel = request.cancel.clone();
        self.read(
            worktree,
            Permits::Log,
            cancel,
            move |inner, session| async move {
                let root = session.identity().worktree_path;
                if session.kind() == GitRepoKind::None {
                    return Err(GitError::not_a_repo(&root));
                }
                let revision =
                    assert_safe_revision(request.revision.as_deref().unwrap_or("HEAD"))?.to_owned();
                let limit = log::page_limit(request.limit, inner.options.log_page_default);
                let skip = request.skip.unwrap_or(0);
                if revision == "HEAD"
                    && session.repo.is_some()
                    && session.refresh_head().head.is_none()
                {
                    // An unborn branch has no history yet; not an error.
                    return Ok(LogPage {
                        commits: Vec::new(),
                        took_ms: 0,
                        exhausted: true,
                    });
                }
                match (request.path, session.repo.clone()) {
                    (Some(path), _) => {
                        assert_path_under_root(&root, &path)?;
                        log::log_cli(
                            &inner.git,
                            &root,
                            &revision,
                            &path,
                            skip,
                            limit,
                            request.cancel.as_ref(),
                        )
                        .await
                    }
                    (None, Some(repo)) => {
                        blocking(move || {
                            log::log_gix(&repo.to_thread_local(), &revision, skip, limit)
                        })
                        .await
                    }
                    (None, None) => Err(no_gix()),
                }
            },
        )
        .await
    }

    /// The files a commit changed versus its first parent (§14.3), sorted,
    /// capped at [`COMMIT_FILES_CAP`].
    pub async fn commit_files(
        &self,
        worktree: &Path,
        sha: &str,
        options: OpOptions,
    ) -> Result<Vec<CommitFile>> {
        let sha = assert_safe_revision(sha)?.to_owned();
        self.read(
            worktree,
            Permits::Log,
            options.cancel,
            move |_inner, session| async move {
                if session.kind() == GitRepoKind::None {
                    return Err(GitError::not_a_repo(&session.identity().worktree_path));
                }
                let repo = session.repo.clone().ok_or_else(no_gix)?;
                blocking(move || log::commit_files(&repo.to_thread_local(), &sha)).await
            },
        )
        .await
    }

    /// `rev:path` bytes for `getContents` / the revision side of `compare`
    /// (§14.2). `None` when the revision or path does not resolve, or when
    /// the object header is at or above `hard_refuse_bytes`.
    pub async fn read_object(
        &self,
        worktree: &Path,
        request: ReadObjectRequest,
    ) -> Result<Option<ObjectBytes>> {
        let cancel = request.cancel.clone();
        self.read(
            worktree,
            Permits::Diff,
            cancel,
            move |inner, session| async move {
                let root = session.identity().worktree_path;
                assert_path_under_root(&root, &request.path)?;
                let revision = assert_safe_revision(&request.revision)?.to_owned();
                if session.kind() == GitRepoKind::None {
                    return Err(GitError::not_a_repo(&root));
                }
                let repo = session.repo.clone().ok_or_else(no_gix)?;
                let hard_refuse = inner.options.hard_refuse_bytes as u64;
                blocking(move || log::read_object(&repo, &revision, &request.path, hard_refuse))
                    .await
            },
        )
        .await
    }

    /// Working-tree bytes for the working side of `compare` (§14.2): both
    /// ends realpath'd and contained (checkout root and `allowedRoots`), so
    /// a symlink cannot read outside. `None` when missing or too large.
    pub async fn read_worktree_file(
        &self,
        worktree: &Path,
        path: &str,
        options: OpOptions,
    ) -> Result<Option<ObjectBytes>> {
        let path = path.to_owned();
        self.read(
            worktree,
            Permits::Diff,
            options.cancel,
            move |inner, session| async move {
                let root = mutation::require_worktree(&session)?;
                assert_path_under_root(&root, &path)?;
                if !inner
                    .allow
                    .admits(&paths::join_posix(&session.real_root, &path))
                {
                    return Err(
                        GitError::invalid(format!("path escapes allowedRoots: {path}"))
                            .with_path(path),
                    );
                }
                let side = diff::WorktreeSide {
                    real_root: session.real_root.clone(),
                    relative: path,
                };
                let hard_refuse = inner.options.hard_refuse_bytes as u64;
                blocking(move || log::read_worktree_file(&side, hard_refuse)).await
            },
        )
        .await
    }

    /// Run a read on the engine runtime: the read side of the session's ops
    /// lock (mutations exclude reads), a §12.1 concurrency permit, and
    /// cancellation by the caller's token or by `close`.
    async fn read<T, F, Fut>(
        &self,
        worktree: &Path,
        permits: Permits,
        cancel: Option<CancellationToken>,
        op: F,
    ) -> Result<T>
    where
        F: FnOnce(Arc<Inner>, Arc<Session>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (inner, session) = (self.inner.clone(), self.inner.session(worktree)?);
        let caller = cancel.unwrap_or_default();
        if caller.is_cancelled() {
            return Err(GitError::canceled());
        }
        self.spawn(async move {
            let closing = session.cancel.clone();
            let semaphore = match permits {
                Permits::Diff => session.diff_permits.clone(),
                Permits::Log => session.log_permits.clone(),
            };
            let work = async {
                let _read = session.ops.clone().read_owned().await;
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| GitError::canceled())?;
                op(inner, session.clone()).await
            };
            tokio::select! {
                result = work => result,
                _ = caller.cancelled() => Err(GitError::canceled()),
                _ = closing.cancelled() => Err(GitError::canceled()),
            }
        })
        .await
    }

    /// Test hook: every status run sleeps this long before computing, so a
    /// test can hold a run in flight.
    #[doc(hidden)]
    pub fn set_status_delay_for_test(&self, delay: Duration) {
        self.inner.metrics.set_status_delay(delay);
    }

    /// Status runs started by this engine. Diagnostic.
    #[doc(hidden)]
    pub fn status_runs_started(&self) -> u64 {
        self.inner.metrics.runs_started()
    }

    /// The most status runs this engine ever had in flight at once.
    #[doc(hidden)]
    pub fn status_peak_concurrency(&self) -> usize {
        self.inner.metrics.peak_concurrency()
    }

    /// How many sessions are open (after de-dupe). Diagnostic.
    #[doc(hidden)]
    pub fn session_count(&self) -> usize {
        self.inner.registry.lock().len()
    }

    /// Run `fut` on the engine runtime and await it from the caller's.
    async fn spawn<T, F>(&self, fut: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        match self.inner.handle.spawn(fut).await {
            Ok(result) => result,
            Err(err) if err.is_panic() => {
                Err(GitError::unknown(format!("mille-git task panicked: {err}")))
            }
            Err(_) => Err(GitError::canceled()),
        }
    }
}

/// Which §12.1 read limit an operation counts against.
#[derive(Copy, Clone)]
enum Permits {
    Diff,
    Log,
}

/// gix work off the runtime's worker threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| GitError::unknown(format!("mille-git read task failed: {err}")))?
}

fn no_gix() -> GitError {
    GitError::unknown(
        "gix could not open this repository (only the git CLI could); this read needs gix",
    )
}

impl Drop for GitEngine {
    fn drop(&mut self) {
        for session in self.inner.registry.lock().drain() {
            session.shutdown();
        }
        if let Some(runtime) = self.runtime.take() {
            // Never block in drop: the engine may be dropped inside another
            // runtime, where a blocking shutdown panics.
            runtime.shutdown_background();
        }
    }
}

impl Inner {
    pub(crate) fn warn(&self, code: WarningCode, message: impl Into<String>, worktree: &Path) {
        self.warnings
            .emit(code, message, Some(worktree.to_path_buf()));
    }

    pub(crate) fn session(&self, worktree: &Path) -> Result<Arc<Session>> {
        self.registry.lock().lookup(worktree).ok_or_else(|| {
            GitError::invalid(format!("worktree is not open: {}", worktree.display()))
                .with_path(worktree.display().to_string())
        })
    }

    async fn open(self: Arc<Self>, caller: PathBuf) -> Result<GitIdentity> {
        if let Some(session) = self.registry.lock().join_known(&caller) {
            return Ok(session.identity());
        }
        let probe = caller.clone();
        let discovered = tokio::task::spawn_blocking(move || identity::discover_blocking(&probe))
            .await
            .map_err(|err| GitError::unknown(format!("discovery task failed: {err}")))?;
        let discovered = match discovered {
            Ok(found) => found,
            Err(root) => match identity::identity_via_cli(&self.git, &root).await {
                Some(identity) => identity::Discovered {
                    root,
                    identity,
                    repo: None,
                },
                None => identity::Discovered {
                    root: caller.clone(),
                    identity: GitIdentity::none(&caller),
                    repo: None,
                },
            },
        };
        let (session, created) = self.registry.lock().insert_or_join(discovered, &caller);
        if created {
            // Once per checkout: a second open (a sibling package) shares
            // this watch and this queue (§9.2 steps 7–8).
            if session.kind() == GitRepoKind::Worktree {
                session.set_watch(watch::attach(&session, &self));
            }
            session.hint(&self, Hints::Full, Urgency::Now);
        }
        Ok(session.identity())
    }
}
