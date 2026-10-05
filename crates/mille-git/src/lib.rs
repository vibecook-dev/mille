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
mod error;
mod gitdir;
mod identity;
mod paths;
mod porcelain;
mod session;
mod status;
mod warning;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::runtime::{Handle, Runtime};

pub use cli::{argv_prefix, ALIAS_GUARDED_VERBS};
pub use containment::{
    assert_path_under_root, assert_real_path_under_root, assert_safe_revision,
    is_safe_workspace_relative_path,
};
pub use error::{GitError, GitErrorKind, Result};
pub use gitdir::{find_dot_git, resolve_git_dir, MAX_DOT_GIT_WALK};
pub use identity::{GitIdentity, GitRepoKind};
pub use porcelain::parse_porcelain_v2;
pub use status::{
    apply_row_cap, merge_status, sort_entries, status_thread_limit, StatusBackend, StatusEntry,
    StatusLetter, StatusSnapshot, StatusXy, MAX_HINT_PATHS,
};
pub use warning::{GitWarning, WarningCallback, WarningCode};

use cli::GitCli;
use containment::AllowList;
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
    pub async fn close(&self, worktree: &Path) -> Result<()> {
        let removed = self.inner.registry.lock().release(worktree);
        if let Some(session) = removed {
            session.shutdown();
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
    pub async fn status(&self, worktree: &Path) -> Result<Arc<StatusSnapshot>> {
        self.refresh(worktree, None).await
    }

    /// Run status now — full when `paths` is `None`, else limited to those
    /// worktree-relative POSIX paths and merged per §10.6 — and return the
    /// snapshot it published. Unsafe paths are dropped from the hint set.
    pub async fn refresh(
        &self,
        worktree: &Path,
        paths: Option<Vec<String>>,
    ) -> Result<Arc<StatusSnapshot>> {
        let session = self.inner.session(worktree)?;
        let inner = self.inner.clone();
        let hints = paths.map(|p| {
            p.into_iter()
                .filter(|p| is_safe_workspace_relative_path(p))
                .collect()
        });
        self.spawn(async move {
            Ok(session
                .run_status(&inner, hints, Arc::new(AtomicBool::new(false)))
                .await)
        })
        .await
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
        let (session, _created) = self.registry.lock().insert_or_join(discovered, &caller);
        Ok(session.identity())
    }
}
