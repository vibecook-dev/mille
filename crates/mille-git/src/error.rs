//! Typed errors for every fallible `GitEngine` call.
//!
//! The wire codes (`EINVAL`, `ENOTAREPO`, …) are the TypeScript
//! `GitErrorCode` union from SPEC mille-git Appendix A. The binding (PR 6a)
//! serializes them as `GIT|<code>|<message>`; nothing in this crate knows
//! about napi.

use std::fmt;

/// What went wrong, independent of the human-readable message.
///
/// `Access` and `NotFound` are not in Appendix B's sketch; they exist so the
/// `EACCES` / `ENOENT` codes the TypeScript union already carries have a Rust
/// spelling instead of collapsing into `Io`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum GitErrorKind {
    /// Bad argument: a path escaping the root, an unsafe revision, an empty
    /// commit include-set, a worktree outside `allowedRoots`.
    InvalidInput,
    /// Permission denied by the OS.
    Access,
    /// A file or object the caller named does not exist.
    NotFound,
    /// The worktree is not a git repository (`kind: "none"`).
    NotARepo,
    /// No usable `git` binary — writes need one; gix reads do not.
    NoGit,
    /// The caller's cancellation token fired.
    Canceled,
    /// The operation is meaningless here (a worktree mutation on a bare repo).
    Unsupported,
    /// A git hook rejected the operation; the message carries git's stderr.
    Hook,
    /// The git child outlived `commitTimeout` and was terminated.
    Timeout,
    /// Another process holds `index.lock`.
    Locked,
    /// Any other I/O failure. Crosses the wire as `EUNKNOWN`.
    Io,
    /// Everything else, including a panic caught at a task boundary.
    Unknown,
}

impl GitErrorKind {
    /// The `GitErrorCode` this kind crosses the napi / transport boundary as.
    pub fn code(self) -> &'static str {
        match self {
            GitErrorKind::InvalidInput => "EINVAL",
            GitErrorKind::Access => "EACCES",
            GitErrorKind::NotFound => "ENOENT",
            GitErrorKind::NotARepo => "ENOTAREPO",
            GitErrorKind::NoGit => "ENOGIT",
            GitErrorKind::Canceled => "ECANCELED",
            GitErrorKind::Unsupported => "EUNSUPPORTED",
            GitErrorKind::Hook => "EHOOK",
            GitErrorKind::Timeout => "ETIMEOUT",
            GitErrorKind::Locked => "ELOCKED",
            GitErrorKind::Io | GitErrorKind::Unknown => "EUNKNOWN",
        }
    }
}

impl fmt::Display for GitErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The one error type every public `mille-git` function returns.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct GitError {
    kind: GitErrorKind,
    message: String,
    path: Option<String>,
}

impl GitError {
    /// Build an error of `kind` with a human-readable `message`.
    pub fn new(kind: GitErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            path: None,
        }
    }

    /// Attach the (workspace-relative or absolute) path the error is about.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// The kind, for matching.
    pub fn kind(&self) -> GitErrorKind {
        self.kind
    }

    /// The wire code (`EINVAL`, …).
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// The message without the code prefix.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The path this error is about, when there is one.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(GitErrorKind::InvalidInput, message)
    }

    pub(crate) fn canceled() -> Self {
        Self::new(GitErrorKind::Canceled, "git operation aborted")
    }

    pub(crate) fn unknown(message: impl Into<String>) -> Self {
        Self::new(GitErrorKind::Unknown, message)
    }

    pub(crate) fn io(err: &std::io::Error, context: impl fmt::Display) -> Self {
        let kind = match err.kind() {
            std::io::ErrorKind::NotFound => GitErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => GitErrorKind::Access,
            _ => GitErrorKind::Io,
        };
        Self::new(kind, format!("{context}: {err}"))
    }
}

/// Crate-wide result alias.
pub type Result<T, E = GitError> = std::result::Result<T, E>;
