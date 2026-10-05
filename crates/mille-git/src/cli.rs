//! The git CLI seam. Every write in v1 goes through here (SPEC mille-git
//! §15.1), as do the read fallbacks gix cannot serve.
//!
//! Why the CLI for writes at all, when gix reads: hooks, LFS filters,
//! credential helpers and signing are git's behaviour, and reproducing them
//! in-process is how an engine silently diverges from the user's `git commit`.
//! The spawn tax (~5–15 ms on macOS) is paid on writes only.
//!
//! The rules here are security rules, not style:
//!
//! - **No shell.** `Command` is built from an argv vector; nothing is ever
//!   joined into one command line or handed to `sh -c` / `cmd.exe /C`, so a
//!   path like `a;rm -rf ~` is one argument to git and nothing more.
//! - **Commit messages go on stdin** (`git commit -F -`), never `-m <text>`:
//!   multi-line bodies and encodings survive, and the message never sits on
//!   a process listing.
//! - **Every argv starts with `-c alias.<verb>=<verb>` for each verb this
//!   crate spawns.** A repository's config can alias `commit` to
//!   `!touch pwned`. Git does ignore aliases that shadow builtins today, but
//!   that is git's policy, not ours to lean on, and `alias.*=` is not valid
//!   config to blank them all; `GIT_CONFIG_PARAMETERS` quoting is easy to get
//!   wrong. The argv `-c` list is the normative form (§20.4), and adding a
//!   verb means adding its override in the same change. `config` and `rm`
//!   are in the list because this crate spawns them (performance config,
//!   unstaging on an unborn branch) — beyond §15.1's list, same rule.
//! - **`--literal-pathspecs`.** Paths reach git as exact names: a Next.js
//!   route `app/[id]/page.tsx` must not glob-match `app/i/page.tsx`.
//! - **Filtered environment** (§20.3): `PATH`, `HOME`, `USERPROFILE`, the
//!   host's `GIT_*` (so GPG/SSH/LFS keep working) minus the variables that
//!   would point git at a *different* repository, plus the handful of
//!   platform variables a child process needs to function. Never an env map
//!   from a caller, and `GIT_DIR` is never set: the working directory selects
//!   the repository.
//! - **Cancellation is graceful.** The child runs in its own process group
//!   (Unix); an abort sends SIGTERM to the group — git removes its own
//!   `index.lock` on SIGTERM, and a hook's `sleep 30` dies with it — then
//!   SIGKILL after two seconds. A bare SIGKILL would strand `index.lock` and
//!   wedge every later write.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::error::{GitError, GitErrorKind, Result};

/// Every git verb this crate spawns. The argv prefix neutralizes a user
/// alias for each one (§20.4).
pub const ALIAS_GUARDED_VERBS: &[&str] = &[
    "add",
    "restore",
    "commit",
    "status",
    "reset",
    "checkout",
    "show",
    "log",
    "diff",
    "rev-parse",
    "worktree",
    "config",
    "rm",
];

/// The global options that precede every verb, in order.
pub fn argv_prefix() -> Vec<String> {
    let mut argv = vec![
        "-c".to_owned(),
        "core.quotepath=false".to_owned(),
        "-c".to_owned(),
        "color.ui=false".to_owned(),
    ];
    for verb in ALIAS_GUARDED_VERBS {
        argv.push("-c".to_owned());
        argv.push(format!("alias.{verb}={verb}"));
    }
    argv.push("--literal-pathspecs".to_owned());
    argv
}

/// Whether the command may take git's optional locks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum LockMode {
    /// status / diff / log / show: `--no-optional-locks` + `GIT_OPTIONAL_LOCKS=0`,
    /// so a stuck `index.lock` left by an agent cannot deadlock a read.
    Read,
}

/// Knobs for one invocation.
#[derive(Clone, Debug)]
pub(crate) struct RunOptions {
    pub(crate) mode: LockMode,
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) cancel: Option<CancellationToken>,
    /// Stop reading (and terminate the child) once stdout exceeds this.
    pub(crate) max_stdout: Option<usize>,
}

impl RunOptions {
    pub(crate) fn read() -> Self {
        Self {
            mode: LockMode::Read,
            stdin: None,
            timeout: None,
            cancel: None,
            max_stdout: None,
        }
    }
}

/// What a finished child produced.
#[derive(Debug)]
pub(crate) struct CliOutput {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// stdout passed `max_stdout`; `stdout` holds the first bytes only.
    pub(crate) stdout_overflow: bool,
}

impl CliOutput {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0) && !self.stdout_overflow
    }

    pub(crate) fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }

    pub(crate) fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Turn a non-zero exit into a typed error.
    pub(crate) fn check(self, what: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        Err(self.failure(what, GitErrorKind::Unknown))
    }

    /// The error a failed run maps to; `fallback` when nothing more specific
    /// can be read from stderr.
    pub(crate) fn failure(&self, what: &str, fallback: GitErrorKind) -> GitError {
        let stderr = self.stderr_text();
        let kind = if stderr.contains("index.lock") && stderr.contains("exists") {
            GitErrorKind::Locked
        } else {
            fallback
        };
        let message = if stderr.is_empty() {
            format!(
                "git {what} failed (exit {})",
                self.code.map_or("signal".to_owned(), |c| c.to_string())
            )
        } else {
            stderr
        };
        GitError::new(kind, message)
    }
}

/// A resolved git binary, or the knowledge that there is none.
#[derive(Clone, Debug)]
pub(crate) struct GitCli {
    binary: Option<PathBuf>,
}

impl GitCli {
    /// SPEC §32.2: `PATH` first, then the well-known install locations. An
    /// explicit path is used as given (if it exists); an explicit bare name
    /// is looked up on `PATH`.
    pub(crate) fn probe(explicit: Option<&Path>) -> Self {
        let binary = match explicit {
            Some(path) if path.is_absolute() || path.components().count() > 1 => {
                is_executable(path).then(|| path.to_path_buf())
            }
            Some(name) if name != Path::new("git") => search_path(name.as_os_str()),
            _ => search_path(OsStr::new("git")).or_else(well_known_git),
        };
        Self { binary }
    }

    #[cfg(test)]
    pub(crate) fn missing() -> Self {
        Self { binary: None }
    }

    pub(crate) fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }

    pub(crate) fn require(&self) -> Result<&Path> {
        self.binary()
            .ok_or_else(|| GitError::new(GitErrorKind::NoGit, "no git binary found on PATH"))
    }

    fn command(&self, cwd: &Path, mode: LockMode) -> Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(self.require()?);
        // `current_dir` rather than `-C`: one less argument to quote on
        // Windows, and git resolves the repository from the cwd either way.
        cmd.current_dir(cwd);
        cmd.env_clear();
        cmd.envs(filtered_env());
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        if mode == LockMode::Read {
            cmd.env("GIT_OPTIONAL_LOCKS", "0");
        }
        cmd.args(argv_prefix());
        if mode == LockMode::Read {
            cmd.arg("--no-optional-locks");
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        Ok(cmd)
    }

    /// Run `git <args>` in `cwd` and collect its output.
    pub(crate) async fn run<I, S>(&self, cwd: &Path, args: I, opts: RunOptions) -> Result<CliOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        if opts
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(GitError::canceled());
        }
        let mut cmd = self.command(cwd, opts.mode)?;
        cmd.args(args);
        cmd.stdin(if opts.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = cmd.spawn().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                GitError::new(GitErrorKind::NoGit, format!("cannot spawn git: {err}"))
            }
            _ => GitError::io(&err, "spawn git"),
        })?;

        if let (Some(bytes), Some(mut stdin)) = (opts.stdin, child.stdin.take()) {
            tokio::spawn(async move {
                // A child that exits without reading stdin is the child's
                // business; its exit status says what happened.
                let _ = stdin.write_all(&bytes).await;
                let _ = stdin.shutdown().await;
            });
        }

        let overflow = CancellationToken::new();
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let stdout_task = child.stdout.take().map(|pipe| {
            tokio::spawn(drain(
                pipe,
                stdout_buf.clone(),
                opts.max_stdout,
                Some(overflow.clone()),
            ))
        });
        let stderr_task = child
            .stderr
            .take()
            .map(|pipe| tokio::spawn(drain(pipe, stderr_buf.clone(), Some(1 << 20), None)));

        let cancel = opts.cancel.unwrap_or_default();
        let timeout = opts.timeout.unwrap_or(Duration::MAX);
        enum End {
            Exited(std::io::Result<std::process::ExitStatus>),
            Canceled,
            TimedOut,
            Overflow,
        }
        let end = tokio::select! {
            status = child.wait() => End::Exited(status),
            _ = cancel.cancelled() => End::Canceled,
            _ = overflow.cancelled() => End::Overflow,
            _ = sleep_for(timeout) => End::TimedOut,
        };
        let status = match end {
            End::Exited(status) => Some(status.map_err(|err| GitError::io(&err, "wait for git"))?),
            End::Canceled => {
                terminate(&mut child).await;
                return Err(GitError::canceled());
            }
            End::TimedOut => {
                terminate(&mut child).await;
                return Err(GitError::new(
                    GitErrorKind::Timeout,
                    format!(
                        "git did not finish within {} ms and was terminated",
                        timeout.as_millis()
                    ),
                ));
            }
            End::Overflow => {
                terminate(&mut child).await;
                None
            }
        };

        // The child has exited, but a hook's grandchild can still hold the
        // pipes open. Give the readers a bounded grace period, then take
        // whatever arrived.
        for task in [stdout_task, stderr_task].into_iter().flatten() {
            if tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .is_err()
            {
                tracing::debug!("git output pipe still open after exit; using what was read");
            }
        }
        let stdout = std::mem::take(&mut *stdout_buf.lock());
        let stderr = std::mem::take(&mut *stderr_buf.lock());
        Ok(CliOutput {
            code: status.and_then(|s| s.code()),
            stdout,
            stderr,
            stdout_overflow: status.is_none(),
        })
    }
}

async fn sleep_for(duration: Duration) {
    if duration == Duration::MAX {
        std::future::pending::<()>().await;
    } else {
        tokio::time::sleep(duration).await;
    }
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(
    mut pipe: R,
    sink: Arc<Mutex<Vec<u8>>>,
    cap: Option<usize>,
    overflow: Option<CancellationToken>,
) {
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut sink = sink.lock();
                let room = cap.map_or(n, |cap| cap.saturating_sub(sink.len()).min(n));
                sink.extend_from_slice(&chunk[..room]);
                if room < n {
                    drop(sink);
                    if let Some(token) = &overflow {
                        token.cancel();
                        return;
                    }
                    // stderr past its cap: keep draining so the child never
                    // blocks on a full pipe, but stop storing.
                }
            }
        }
    }
}

/// SIGTERM the child's process group, give it two seconds to clean up its
/// lock files, then SIGKILL.
async fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let group = -(pid as i32);
        // SAFETY: `kill` has no memory-safety preconditions; the group id is
        // our own child's (spawned with `process_group(0)`, still unreaped).
        unsafe { libc::kill(group, libc::SIGTERM) };
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_ok()
        {
            return;
        }
        // SAFETY: as above; the child has not been reaped yet.
        unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = child.wait().await;
        return;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// `GIT_*` variables that would redirect git to a different repository than
/// the one the working directory names — inherited from a host that is
/// itself running inside a git hook, they would make every engine call act
/// on the wrong repo.
const REPO_LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_OPTIONAL_LOCKS",
    "GIT_TERMINAL_PROMPT",
];

/// Non-`GIT_*` variables a git child (and its hooks, GPG, SSH, LFS) needs.
const PASSTHROUGH_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    // POSIX process basics
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "USER",
    "LOGNAME",
    "SHELL",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    // signing and transport agents
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GNUPGHOME",
    "GPG_AGENT_INFO",
    "GPG_TTY",
    // Windows: without these, child processes cannot find the system
    // directory, a temp dir, or their own config
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "TEMP",
    "TMP",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "HOMEDRIVE",
    "HOMEPATH",
    "COMSPEC",
    "PATHEXT",
    "USERNAME",
];

pub(crate) fn filtered_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            // Windows environment names are case-insensitive.
            let upper = key.to_ascii_uppercase();
            if upper.starts_with("GIT_") {
                return !REPO_LOCATION_VARS.contains(&upper.as_str());
            }
            PASSTHROUGH_VARS.contains(&upper.as_str())
        })
        .collect()
}

fn executable_names(name: &OsStr) -> Vec<OsString> {
    if cfg!(windows) && Path::new(name).extension().is_none() {
        let mut exe = name.to_os_string();
        exe.push(".exe");
        vec![exe]
    } else {
        vec![name.to_os_string()]
    }
}

fn search_path(name: &OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| executable_names(name).into_iter().map(move |n| dir.join(n)))
        .find(|candidate| is_executable(candidate))
}

fn well_known_git() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(base) = std::env::var_os(var) {
                candidates.push(PathBuf::from(&base).join("Git").join("cmd").join("git.exe"));
                candidates.push(PathBuf::from(&base).join("Git").join("bin").join("git.exe"));
            }
        }
    } else {
        candidates.extend(
            [
                "/opt/homebrew/bin/git",
                "/usr/local/bin/git",
                "/usr/bin/git",
            ]
            .map(PathBuf::from),
        );
    }
    candidates
        .into_iter()
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_guarded_verb_is_neutralized_before_the_verb() {
        let argv = argv_prefix();
        for verb in ALIAS_GUARDED_VERBS {
            let pair = format!("alias.{verb}={verb}");
            let at = argv
                .iter()
                .position(|a| a == &pair)
                .expect("alias override present");
            assert_eq!(argv[at - 1], "-c");
        }
        assert!(argv.contains(&"--literal-pathspecs".to_owned()));
    }

    #[test]
    fn env_filter_drops_repo_redirects_and_keeps_path() {
        let env = filtered_env();
        let keys: Vec<String> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().to_ascii_uppercase())
            .collect();
        assert!(keys
            .iter()
            .all(|k| !REPO_LOCATION_VARS.contains(&k.as_str())));
        assert!(keys
            .iter()
            .all(|k| k.starts_with("GIT_") || PASSTHROUGH_VARS.contains(&k.as_str())));
        if std::env::var_os("PATH").is_some() {
            assert!(keys.contains(&"PATH".to_owned()));
        }
    }

    #[test]
    fn a_missing_explicit_binary_is_no_git() {
        let cli = GitCli::probe(Some(Path::new("/definitely/not/a/git")));
        assert!(cli.binary().is_none());
        assert_eq!(cli.require().unwrap_err().code(), "ENOGIT");
        assert!(GitCli::missing().binary().is_none());
    }
}
