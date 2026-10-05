//! `#[napi(object)]` mirrors for what JavaScript hands the git engine
//! (SPEC mille-git §18.1, Appendix A). `cfg(feature = "git")`.
//!
//! Only the *inputs* are mirrored here. Results — identities, snapshots,
//! diffs, log pages, worktree lists, warnings — cross as JSON text built from
//! `mille-git`'s own `Serialize` derives, which already spell Appendix A's
//! camelCase. Two reasons, both in `git.rs`'s module comment: one wire shape
//! shared with the MessagePort transport, and numbers that stay numbers.
//!
//! Durations are milliseconds and sizes are `u32`: napi has no `usize`, and a
//! cap above 4 GiB is not a cap.

use std::path::PathBuf;
use std::time::Duration;

use mille_git::{CancellationToken, GitError, ReadObjectRequest};
use mille_git::{CommitRequest, DiffRequest, DiffSpec, GitEngineOptions, LogRequest};

/// Appendix A `GitEngineOptions`.
#[napi(object)]
pub struct GitEngineOptionsJs {
    /// Caller-path and mutation containment. The `.git` walk MAY leave them.
    /// Empty fails closed: every `open` is `EINVAL`.
    pub allowed_roots: Vec<String>,
    /// The git binary; absent probes `PATH`, then the well-known paths.
    pub git_path: Option<String>,
    pub status_row_cap: Option<u32>,
    pub debounce_ms: Option<u32>,
    pub max_wait_ms: Option<u32>,
    /// Idle time after a path-limited run before a full rescan (§21.1).
    /// Additive: not in Appendix A (README deviation 18).
    pub idle_rescan_ms: Option<u32>,
    pub auto_render_max_bytes: Option<u32>,
    pub auto_render_max_lines: Option<u32>,
    pub hard_refuse_bytes: Option<u32>,
    pub log_page_default: Option<u32>,
    pub commit_timeout_ms: Option<u32>,
    pub write_performance_config: Option<bool>,
}

impl GitEngineOptionsJs {
    pub(crate) fn into_options(self) -> GitEngineOptions {
        let defaults = GitEngineOptions::default();
        let ms = |value: Option<u32>, default: Duration| {
            value.map_or(default, |ms| Duration::from_millis(u64::from(ms)))
        };
        let size = |value: Option<u32>, default: usize| value.map_or(default, |n| n as usize);
        GitEngineOptions {
            allowed_roots: self.allowed_roots.into_iter().map(PathBuf::from).collect(),
            git_path: self.git_path.filter(|p| !p.is_empty()).map(PathBuf::from),
            status_row_cap: size(self.status_row_cap, defaults.status_row_cap),
            debounce: ms(self.debounce_ms, defaults.debounce),
            max_wait: ms(self.max_wait_ms, defaults.max_wait),
            idle_rescan: ms(self.idle_rescan_ms, defaults.idle_rescan),
            auto_render_max_bytes: size(self.auto_render_max_bytes, defaults.auto_render_max_bytes),
            auto_render_max_lines: size(self.auto_render_max_lines, defaults.auto_render_max_lines),
            hard_refuse_bytes: size(self.hard_refuse_bytes, defaults.hard_refuse_bytes),
            log_page_default: size(self.log_page_default, defaults.log_page_default),
            commit_timeout: ms(self.commit_timeout_ms, defaults.commit_timeout),
            write_performance_config: self
                .write_performance_config
                .unwrap_or(defaults.write_performance_config),
        }
    }
}

/// Appendix A `DiffRequest` without `signal` (that is the call's
/// `operationId`).
#[napi(object)]
pub struct DiffRequestJs {
    pub path: String,
    /// `'worktree' | 'index' | 'commit'`.
    pub spec: String,
    pub commit: Option<String>,
    pub max_bytes: Option<u32>,
    pub max_lines: Option<u32>,
}

impl DiffRequestJs {
    pub(crate) fn into_request(self, cancel: CancellationToken) -> Result<DiffRequest, GitError> {
        let spec = match self.spec.as_str() {
            "worktree" => DiffSpec::Worktree,
            "index" => DiffSpec::Index,
            "commit" => DiffSpec::Commit,
            other => {
                return Err(GitError::new(
                    mille_git::GitErrorKind::InvalidInput,
                    format!("unknown diff spec {other:?}; expected worktree, index or commit"),
                ))
            }
        };
        Ok(DiffRequest {
            path: self.path,
            spec,
            commit: self.commit,
            max_bytes: self.max_bytes.map(|n| n as usize),
            max_lines: self.max_lines.map(|n| n as usize),
            cancel: Some(cancel),
        })
    }
}

/// Appendix A `LogRequest` without `signal`.
#[napi(object)]
pub struct LogRequestJs {
    pub path: Option<String>,
    pub limit: Option<u32>,
    pub skip: Option<u32>,
    pub revision: Option<String>,
}

impl LogRequestJs {
    pub(crate) fn into_request(self, cancel: CancellationToken) -> LogRequest {
        LogRequest {
            path: self.path,
            limit: self.limit.map(|n| n as usize),
            skip: self.skip.map(|n| n as usize),
            revision: self.revision,
            cancel: Some(cancel),
        }
    }
}

/// Appendix A `ReadObjectRequest` without `signal`.
#[napi(object)]
pub struct ReadObjectRequestJs {
    pub path: String,
    pub revision: String,
}

impl ReadObjectRequestJs {
    pub(crate) fn into_request(self, cancel: CancellationToken) -> ReadObjectRequest {
        ReadObjectRequest {
            path: self.path,
            revision: self.revision,
            cancel: Some(cancel),
        }
    }
}

/// Appendix A `CommitRequest` without `signal`.
#[napi(object)]
pub struct CommitRequestJs {
    pub summary: String,
    pub body: Option<String>,
    pub paths: Vec<String>,
}

impl CommitRequestJs {
    pub(crate) fn into_request(self, cancel: CancellationToken) -> CommitRequest {
        CommitRequest {
            summary: self.summary,
            body: self.body,
            paths: self.paths,
            cancel: Some(cancel),
        }
    }
}
