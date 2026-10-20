//! Worktree mutations through the git CLI (SPEC mille-git §15): stage,
//! unstage, restore, and the include-set commit.
//!
//! Every mutation runs inside the session's blocking section (`queue.rs`):
//! it waits for in-flight diff / log reads, drops an in-flight status, and
//! queues a full status when it ends. Paths are validated twice — lexically
//! against the checkout root (`assert_path_under_root`) and against
//! `allowedRoots` by realpath of their parent — so a package workspace
//! cannot `restore ../../secret` even though status is whole-repo (§20.2).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::cli::{chunk_paths, CliOutput, RunOptions, MAX_PATHS_ARGV_CHARS};
use crate::containment::assert_path_under_root;
use crate::error::{GitError, GitErrorKind, Result};
use crate::identity::GitRepoKind;
use crate::paths;
use crate::session::Session;
use crate::status::StatusEntry;
use crate::warning::WarningCode;
use crate::Inner;

/// A commit of an include-set (§15.4).
#[derive(Clone, Debug, Default)]
pub struct CommitRequest {
    /// The first line. Required, non-empty after trimming.
    pub summary: String,
    /// The rest of the message, separated from the summary by a blank line.
    pub body: Option<String>,
    /// The include-set: worktree-relative POSIX paths. Required, non-empty.
    pub paths: Vec<String>,
    /// Abort: the git child's process group gets SIGTERM, then SIGKILL.
    pub cancel: Option<CancellationToken>,
}

/// Options shared by the path mutations.
#[derive(Clone, Debug, Default)]
pub struct OpOptions {
    /// Abort the operation (§12.5).
    pub cancel: Option<CancellationToken>,
}

/// The checkout root a mutation runs in. `ENOTAREPO` for a plain directory,
/// `EUNSUPPORTED` for a bare repository (§9.4, §23).
pub(crate) fn require_worktree(session: &Session) -> Result<PathBuf> {
    let identity = session.identity();
    match identity.kind {
        GitRepoKind::Worktree => Ok(identity.worktree_path),
        GitRepoKind::None => Err(GitError::not_a_repo(&identity.worktree_path)),
        GitRepoKind::Bare => Err(GitError::new(
            GitErrorKind::Unsupported,
            "a bare repository has no worktree to change",
        )),
    }
}

/// Validate every path: lexical containment under the checkout root, then
/// `allowedRoots` (the explorer root, not the whole checkout).
pub(crate) fn validate_paths(inner: &Inner, session: &Session, paths: &[String]) -> Result<()> {
    let root = session.identity().worktree_path;
    for path in paths {
        assert_path_under_root(&root, path)?;
        if !inner
            .allow
            .admits_entry(&paths::join_posix(&session.real_root, path))
        {
            return Err(
                GitError::invalid(format!("path is outside allowedRoots: {path}"))
                    .with_path(path.clone()),
            );
        }
    }
    Ok(())
}

fn require_git(inner: &Inner, root: &Path) -> Result<()> {
    inner.git.require().map(|_| ()).inspect_err(|err| {
        inner.warn(WarningCode::NoGit, err.message().to_owned(), root);
    })
}

/// `git <verb...> -- <chunk>` for each argv-sized chunk of `paths`.
async fn for_each_chunk(
    inner: &Inner,
    root: &Path,
    verb: &[&str],
    paths: &[String],
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    for chunk in chunk_paths(paths, MAX_PATHS_ARGV_CHARS) {
        let mut args: Vec<String> = verb.iter().map(|s| (*s).to_owned()).collect();
        args.push("--".to_owned());
        args.extend(chunk);
        let label = verb.first().copied().unwrap_or("git");
        inner
            .git
            .run(root, &args, RunOptions::write().cancel(cancel))
            .await?
            .check(label)?;
    }
    Ok(())
}

/// `git add -- <paths>`.
pub(crate) async fn stage(
    inner: &Arc<Inner>,
    session: &Arc<Session>,
    paths: &[String],
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    validate_paths(inner, session, paths)?;
    let root = require_worktree(session)?;
    if paths.is_empty() {
        return Ok(());
    }
    require_git(inner, &root)?;
    let _blocking = session.blocking(inner).await;
    for_each_chunk(inner, &root, &["add"], paths, cancel).await
}

/// `git restore --staged`, falling back to `git reset -q HEAD`; on an unborn
/// branch (where both need a `HEAD`), `git rm --cached`.
pub(crate) async fn unstage(
    inner: &Arc<Inner>,
    session: &Arc<Session>,
    paths: &[String],
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    validate_paths(inner, session, paths)?;
    let root = require_worktree(session)?;
    if paths.is_empty() {
        return Ok(());
    }
    require_git(inner, &root)?;
    let _blocking = session.blocking(inner).await;
    unstage_locked(inner, session, &root, paths, cancel).await
}

async fn unstage_locked(
    inner: &Inner,
    session: &Session,
    root: &Path,
    paths: &[String],
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    if session.refresh_head().head.is_none() {
        return for_each_chunk(
            inner,
            root,
            &["rm", "--cached", "-r", "-q", "--ignore-unmatch"],
            paths,
            cancel,
        )
        .await;
    }
    match for_each_chunk(inner, root, &["restore", "--staged"], paths, cancel).await {
        Err(err) if err.kind() == GitErrorKind::Unknown => {
            for_each_chunk(inner, root, &["reset", "-q", "HEAD"], paths, cancel).await
        }
        other => other,
    }
}

/// Discard worktree changes: **index → worktree** (`git restore --worktree`),
/// never `--source=HEAD`, which would also throw away staged-but-uncommitted
/// bytes and change what the explorer's "Discard changes" means (§15.3,
/// AC-017). `git checkout HEAD --` is only the fallback for a git without
/// `restore`, as in `createShellScmClient.revert`.
pub(crate) async fn restore(
    inner: &Arc<Inner>,
    session: &Arc<Session>,
    paths: &[String],
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    validate_paths(inner, session, paths)?;
    let root = require_worktree(session)?;
    if paths.is_empty() {
        return Ok(());
    }
    require_git(inner, &root)?;
    let _blocking = session.blocking(inner).await;
    match for_each_chunk(inner, &root, &["restore", "--worktree"], paths, cancel).await {
        Err(err) if err.kind() == GitErrorKind::Unknown => {
            for_each_chunk(inner, &root, &["checkout", "HEAD"], paths, cancel).await
        }
        other => other,
    }
}

/// The include-set commit (§15.4, GitHub Desktop's model):
///
/// 1. reject an empty summary or include-set; contain every path;
/// 2. stage the included paths that have something unstaged;
/// 3. unstage what is staged but not included (a rename row counts as
///    included when either of its names is);
/// 4. `git commit -F -` with the message on stdin — hooks run, because this
///    is git — under `commitTimeout`;
/// 5. return `rev-parse HEAD`; the blocking section's end refreshes status.
pub(crate) async fn commit(
    inner: &Arc<Inner>,
    session: &Arc<Session>,
    request: CommitRequest,
) -> Result<String> {
    if request.summary.trim().is_empty() {
        return Err(GitError::invalid("commit summary must not be empty"));
    }
    if request.paths.is_empty() {
        return Err(GitError::invalid("commit include-set must not be empty"));
    }
    validate_paths(inner, session, &request.paths)?;
    let root = require_worktree(session)?;
    require_git(inner, &root)?;
    let cancel = request.cancel.as_ref();
    let _blocking = session.blocking(inner).await;

    let current = porcelain_now(inner, &root, cancel).await?;
    let (to_add, to_unstage) = plan_include_set(&current, &request.paths);
    if !to_add.is_empty() {
        for_each_chunk(inner, &root, &["add"], &to_add, cancel).await?;
    }
    if !to_unstage.is_empty() {
        unstage_locked(inner, session, &root, &to_unstage, cancel).await?;
    }

    let mut message = request.summary.trim_end().to_owned();
    if let Some(body) = request.body.as_deref().filter(|b| !b.trim().is_empty()) {
        message.push_str("\n\n");
        message.push_str(body.trim_end());
    }
    message.push('\n');
    let options = RunOptions {
        stdin: Some(message.into_bytes()),
        timeout: Some(inner.options.commit_timeout),
        ..RunOptions::write().cancel(cancel)
    };
    let out = inner
        .git
        .run(&root, ["commit", "-F", "-", "--no-status"], options)
        .await?;
    if !out.success() {
        return Err(commit_failure(inner, &root, &out).await);
    }
    let head = inner
        .git
        .run(&root, ["rev-parse", "HEAD"], RunOptions::read())
        .await?
        .check("rev-parse")?;
    let sha = head.stdout_text().trim().to_owned();
    if !(sha.len() == 40 || sha.len() == 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitError::unknown(format!(
            "rev-parse HEAD returned {sha:?}"
        )));
    }
    Ok(sha)
}

async fn porcelain_now(
    inner: &Inner,
    root: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<StatusEntry>> {
    let out = inner
        .git
        .run(
            root,
            ["status", "--porcelain=v2", "-z", "--untracked-files=all"],
            RunOptions::read().cancel(cancel),
        )
        .await?
        .check("status")?;
    Ok(crate::porcelain::parse_porcelain_v2(
        &out.stdout,
        &mut |w| {
            inner.warn(WarningCode::Parse, w, root);
        },
    ))
}

/// `(paths to git add, paths to unstage)` for an include-set.
///
/// Only included paths with something unstaged are added: §15.4's literal
/// `git add -- <included>` fails outright ("pathspec did not match") for an
/// included path whose deletion is already staged, and has nothing to do
/// for one that is fully staged or clean.
pub(crate) fn plan_include_set(
    current: &[StatusEntry],
    include: &[String],
) -> (Vec<String>, Vec<String>) {
    let included: BTreeSet<&str> = include.iter().map(String::as_str).collect();
    let mut to_add = BTreeSet::new();
    let mut to_unstage = BTreeSet::new();
    for entry in current {
        let is_included = included.contains(entry.path.as_str())
            || entry
                .orig_path
                .as_deref()
                .is_some_and(|orig| included.contains(orig));
        if is_included {
            if entry.untracked || entry.conflicted || entry.xy.worktree.is_some() {
                to_add.insert(entry.path.clone());
            }
        } else if entry.staged {
            to_unstage.insert(entry.path.clone());
            if let Some(orig) = &entry.orig_path {
                to_unstage.insert(orig.clone());
            }
        }
    }
    (
        to_add.into_iter().collect(),
        to_unstage.into_iter().collect(),
    )
}

/// Classify a failed `git commit`.
async fn commit_failure(inner: &Inner, root: &Path, out: &CliOutput) -> GitError {
    let stdout = out.stdout_text();
    if stdout.contains("nothing to commit") || stdout.contains("no changes added to commit") {
        return GitError::invalid("nothing to commit: the include-set has no changes");
    }
    let mut error = out.failure("commit", GitErrorKind::Unknown);
    if error.kind() == GitErrorKind::Unknown && commit_hooks_present(inner, root).await {
        error = GitError::new(GitErrorKind::Hook, error.message().to_owned());
    }
    if error.message().starts_with("git commit failed") && !stdout.trim().is_empty() {
        error = GitError::new(error.kind(), stdout.trim().to_owned());
    }
    error
}

/// Does the effective hooks directory (`core.hooksPath` aware) hold a hook
/// that runs on commit?
async fn commit_hooks_present(inner: &Inner, root: &Path) -> bool {
    let Ok(out) = inner
        .git
        .run(
            root,
            ["rev-parse", "--git-path", "hooks"],
            RunOptions::read(),
        )
        .await
    else {
        return false;
    };
    if !out.success() {
        return false;
    }
    let dir = PathBuf::from(out.stdout_text().trim());
    let dir = if dir.is_absolute() {
        dir
    } else {
        root.join(dir)
    };
    ["pre-commit", "prepare-commit-msg", "commit-msg"]
        .iter()
        .any(|hook| dir.join(hook).is_file())
}

/// §21.2: the settings that make status cheap on an ADE-created worktree.
/// Written only when `write_performance_config` is set (default false — the
/// engine does not mutate arbitrary clones).
pub(crate) async fn advise_performance_config(
    inner: &Arc<Inner>,
    session: &Arc<Session>,
) -> Result<BTreeMap<String, String>> {
    let root = require_worktree(session)?;
    let mut advice = BTreeMap::new();
    advice.insert("core.untrackedCache".to_owned(), "true".to_owned());
    // The builtin fsmonitor daemon: git >= 2.37, macOS and Windows only.
    if cfg!(any(target_os = "macos", windows)) && git_version(inner, &root).await >= Some((2, 37)) {
        advice.insert("core.fsmonitor".to_owned(), "true".to_owned());
    }
    if inner.options.write_performance_config {
        require_git(inner, &root)?;
        let _blocking = session.blocking(inner).await;
        for (key, value) in &advice {
            inner
                .git
                .run(
                    &root,
                    ["config", key.as_str(), value.as_str()],
                    RunOptions::write(),
                )
                .await?
                .check("config")?;
        }
    }
    Ok(advice)
}

async fn git_version(inner: &Inner, root: &Path) -> Option<(u32, u32)> {
    let out = inner
        .git
        .run(root, ["--version"], RunOptions::read())
        .await
        .ok()?;
    parse_git_version(&out.stdout_text())
}

/// `git version 2.50.1 (Apple Git-155)` → `(2, 50)`.
pub(crate) fn parse_git_version(text: &str) -> Option<(u32, u32)> {
    let version = text.split_whitespace().nth(2)?;
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{StatusLetter, StatusXy};

    fn entry(
        path: &str,
        index: Option<StatusLetter>,
        worktree: Option<StatusLetter>,
    ) -> StatusEntry {
        StatusEntry::tracked(path.to_owned(), None, StatusXy { index, worktree })
    }

    #[test]
    fn the_include_set_plan_adds_unstaged_and_unstages_excluded() {
        use StatusLetter::*;
        let mut renamed = entry("new.ts", Some(Renamed), None);
        renamed.orig_path = Some("old.ts".into());
        let current = vec![
            entry("a.ts", None, Some(Modified)), // included, unstaged -> add
            entry("b.ts", Some(Modified), None), // included, fully staged -> nothing
            entry("c.ts", None, Some(Modified)), // excluded, unstaged -> nothing
            entry("d.ts", Some(Added), Some(Modified)), // excluded, staged -> unstage
            entry("gone.ts", Some(Deleted), None), // included staged deletion -> nothing to add
            StatusEntry::untracked("u.ts".into()), // included untracked -> add
            renamed.clone(),                     // excluded rename -> unstage both names
        ];
        let include: Vec<String> = ["a.ts", "b.ts", "gone.ts", "u.ts", "clean.ts"]
            .map(String::from)
            .to_vec();
        let (add, unstage) = plan_include_set(&current, &include);
        assert_eq!(add, ["a.ts", "u.ts"]);
        assert_eq!(unstage, ["d.ts", "new.ts", "old.ts"]);

        // Including either name of a rename keeps the pair staged.
        let (_, unstage) = plan_include_set(&[renamed], &["old.ts".to_owned()]);
        assert!(unstage.is_empty());
    }

    #[test]
    fn git_versions_parse() {
        assert_eq!(
            parse_git_version("git version 2.50.1 (Apple Git-155)\n"),
            Some((2, 50))
        );
        assert_eq!(
            parse_git_version("git version 2.37.0.windows.1"),
            Some((2, 37))
        );
        assert_eq!(parse_git_version("nonsense"), None);
        assert!(Some((2, 36)) < Some((2, 37)) && None < Some((2, 37)));
    }
}
