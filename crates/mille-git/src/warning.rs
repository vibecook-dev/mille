//! The engine's `'warning'` channel (SPEC mille-git §22.2): degraded states
//! are reported, never thrown at a decoration path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

/// Warning codes on the `'warning'` channel.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub enum WarningCode {
    /// No git binary: writes fail `ENOGIT`, the CLI fallback is unavailable.
    /// Emitted once per engine.
    #[serde(rename = "ENOGIT")]
    NoGit,
    /// A malformed gix item or porcelain row was skipped.
    #[serde(rename = "PARSE")]
    Parse,
    /// A gitdir / worktrees watch could not be attached or reported an error.
    #[serde(rename = "WATCH")]
    Watch,
    /// gix status failed and the CLI served the snapshot instead.
    #[serde(rename = "DEGRADED_CLI")]
    DegradedCli,
}

impl WarningCode {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            WarningCode::NoGit => "ENOGIT",
            WarningCode::Parse => "PARSE",
            WarningCode::Watch => "WATCH",
            WarningCode::DegradedCli => "DEGRADED_CLI",
        }
    }
}

/// One warning.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitWarning {
    /// What kind of degradation.
    pub code: WarningCode,
    /// Human-readable detail. Never a commit message or file contents.
    pub message: String,
    /// The session's checkout root, when the warning is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
}

/// A warning subscriber. Called on whatever thread raised the warning; keep
/// it short and non-blocking.
pub type WarningCallback = Arc<dyn Fn(&GitWarning) + Send + Sync>;

#[derive(Default)]
pub(crate) struct Warnings {
    subscribers: Mutex<Vec<(u64, WarningCallback)>>,
    next_id: AtomicU64,
    no_git_reported: AtomicBool,
}

impl Warnings {
    pub(crate) fn subscribe(&self, callback: WarningCallback) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.subscribers.lock().push((id, callback));
        id
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        self.subscribers.lock().retain(|(sub, _)| *sub != id);
    }

    pub(crate) fn emit(
        &self,
        code: WarningCode,
        message: impl Into<String>,
        worktree: Option<PathBuf>,
    ) {
        if code == WarningCode::NoGit && self.no_git_reported.swap(true, Ordering::AcqRel) {
            return; // once per engine (§22.2) — the shell client's every-call spam is the bug
        }
        let warning = GitWarning {
            code,
            message: message.into(),
            worktree,
        };
        tracing::warn!(code = code.as_str(), worktree = ?warning.worktree, "{}", warning.message);
        // Snapshot the list and drop the lock before calling out, so a
        // callback that (un)subscribes cannot deadlock.
        let subscribers: Vec<WarningCallback> = self
            .subscribers
            .lock()
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for callback in subscribers {
            callback(&warning);
        }
    }
}
