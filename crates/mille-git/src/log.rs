//! History reads (SPEC mille-git §14): log pages, the files a commit
//! touched, and object / working-tree bytes for `getContents` / `compare`.
//!
//! `read_object` and `read_worktree_file` are the contents API — `diff()`
//! is capped hunks with no working-tree bytes, and must never be used to
//! reconstruct a file (§14.2). Both refuse at `hardRefuseBytes` by header /
//! `stat` before loading, returning `None` like a missing object.

use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::cli::{GitCli, RunOptions};
use crate::containment::assert_safe_revision;
use crate::diff::{blob_in_revision, DiffSide, WorktreeSide};
use crate::error::{GitError, Result};
use crate::status::StatusLetter;

/// A log page request (§14.1).
#[derive(Clone, Debug, Default)]
pub struct LogRequest {
    /// Only commits touching this worktree-relative POSIX path.
    pub path: Option<String>,
    /// Page size; default `logPageDefault` (100), at most 10 000.
    pub limit: Option<usize>,
    /// Commits to skip.
    pub skip: Option<usize>,
    /// Where to start; default `HEAD`. A safe revision.
    pub revision: Option<String>,
    /// Abort the request.
    pub cancel: Option<CancellationToken>,
}

/// One commit of a page (`FileHistoryRevision` plus parents).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogCommit {
    /// Full hex id.
    pub id: String,
    /// Abbreviated id (`core.abbrev`-aware, unique).
    pub short_id: String,
    /// Author name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Subject line only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Author time, milliseconds since the epoch (`%at` × 1000).
    pub timestamp_ms: i64,
    /// Parent ids, first parent first.
    pub parents: Vec<String>,
}

/// One page of history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    /// Newest first.
    pub commits: Vec<LogCommit>,
    /// Engine time for this page.
    pub took_ms: u64,
    /// No commits remain after this page.
    pub exhausted: bool,
}

/// One file a commit changed versus its first parent (§14.3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitFile {
    /// Worktree-relative POSIX path (the new name for a rename).
    pub path: String,
    /// Rename / copy source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
    /// `A`, `D`, `M`, `R` or `C`.
    pub status: StatusLetter,
}

/// `commitFiles`' answer (§14.3): the changed files sorted by path and capped
/// at [`COMMIT_FILES_CAP`], and whether the cap cut the list short — a
/// History room must be able to say "and more" rather than show a silently
/// partial commit.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitFileList {
    /// At most [`COMMIT_FILES_CAP`] files, sorted by path.
    pub files: Vec<CommitFile>,
    /// The commit changed more files than the cap; `files` is the first
    /// [`COMMIT_FILES_CAP`] of them by path.
    pub truncated: bool,
}

/// `readObject` (§14.2).
#[derive(Clone, Debug, Default)]
pub struct ReadObjectRequest {
    /// Worktree-relative POSIX path.
    pub path: String,
    /// A safe revision (`HEAD`, a sha, `HEAD~2`, a branch). Not `WORKTREE`:
    /// the working tree is [`read_worktree_file`](crate::GitEngine::read_worktree_file).
    pub revision: String,
    /// Abort the request.
    pub cancel: Option<CancellationToken>,
}

/// `commitFiles` cap (§14.3).
pub const COMMIT_FILES_CAP: usize = 10_000;
/// Upper bound on a log page (`safeLimit` in `shell-history.ts`).
pub const MAX_LOG_PAGE: usize = 10_000;

pub(crate) fn page_limit(requested: Option<usize>, default: usize) -> usize {
    match requested {
        Some(0) | None => default.clamp(1, MAX_LOG_PAGE),
        Some(n) => n.min(MAX_LOG_PAGE),
    }
}

/// A log page from gix's revision walk (no path filter).
pub(crate) fn log_gix(
    repo: &gix::Repository,
    revision: &str,
    skip: usize,
    limit: usize,
) -> Result<LogPage> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;

    let started = Instant::now();
    let tip = match repo.rev_parse_single(revision) {
        Ok(id) => id
            .object()
            .map_err(|e| GitError::unknown(e.to_string()))?
            .peel_to_commit(),
        // An unborn HEAD has no history yet; anything else is a bad revision.
        Err(_) if revision == "HEAD" && repo.head().map(|h| h.is_unborn()).unwrap_or(false) => {
            return Ok(LogPage {
                commits: Vec::new(),
                took_ms: 0,
                exhausted: true,
            });
        }
        Err(err) => {
            return Err(GitError::invalid(format!(
                "unknown revision {revision}: {err}"
            )))
        }
    }
    .map_err(|e| GitError::invalid(format!("{revision} is not a commit: {e}")))?;
    let walk = repo
        .rev_walk([tip.id])
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()
        .map_err(|e| GitError::unknown(e.to_string()))?;
    let mut commits = Vec::with_capacity(limit);
    let mut exhausted = true;
    for info in walk.skip(skip) {
        if commits.len() == limit {
            exhausted = false;
            break;
        }
        let info = info.map_err(|e| GitError::unknown(e.to_string()))?;
        let commit = info
            .object()
            .map_err(|e| GitError::unknown(e.to_string()))?;
        let author = commit.author().ok();
        commits.push(LogCommit {
            id: commit.id.to_string(),
            short_id: commit.id().shorten_or_id().to_string(),
            author: author.map(|a| a.name.to_string()).filter(|n| !n.is_empty()),
            message: commit
                .message()
                .ok()
                .map(|m| m.summary().to_string())
                .filter(|s| !s.is_empty()),
            timestamp_ms: author.map_or(0, |a| a.seconds() * 1000),
            parents: commit.parent_ids().map(|p| p.to_string()).collect(),
        });
    }
    Ok(LogPage {
        commits,
        took_ms: started.elapsed().as_millis() as u64,
        exhausted,
    })
}

/// A path-filtered log page through the CLI, which applies git's default
/// history simplification (TREESAME pruning) — the same commits
/// `FileHistoryClient.getHistory` shows today.
pub(crate) async fn log_cli(
    git: &GitCli,
    root: &Path,
    revision: &str,
    path: &str,
    skip: usize,
    limit: usize,
    cancel: Option<&CancellationToken>,
) -> Result<LogPage> {
    let started = Instant::now();
    let args = [
        "log".to_owned(),
        format!("--max-count={}", limit + 1),
        format!("--skip={skip}"),
        "--format=%H%x09%h%x09%at%x09%an%x09%P%x09%s".to_owned(),
        revision.to_owned(),
        "--".to_owned(),
        path.to_owned(),
    ];
    let mut options = RunOptions::read();
    options.cancel = cancel.cloned();
    let out = git.run(root, &args, options).await?;
    if !out.success() {
        let stderr = out.stderr_text();
        // `git log` on an unborn branch: no history yet.
        if stderr.contains("does not have any commits yet")
            || stderr.contains("bad default revision")
        {
            return Ok(LogPage {
                commits: Vec::new(),
                took_ms: started.elapsed().as_millis() as u64,
                exhausted: true,
            });
        }
        return Err(out.failure("log", crate::GitErrorKind::Unknown));
    }
    let mut commits = parse_log_lines(&out.stdout_text());
    let exhausted = commits.len() <= limit;
    commits.truncate(limit);
    Ok(LogPage {
        commits,
        took_ms: started.elapsed().as_millis() as u64,
        exhausted,
    })
}

/// `parseGitLogLines` semantics, with the parents column added:
/// `H \t h \t at \t an \t P \t s` per line; malformed lines are skipped.
pub(crate) fn parse_log_lines(stdout: &str) -> Vec<LogCommit> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(6, '\t');
            let id = parts.next().filter(|s| !s.is_empty())?;
            let short_id = parts.next().filter(|s| !s.is_empty())?;
            let at = parts.next()?;
            let author = parts.next()?;
            let parents = parts.next()?;
            let subject = parts.next().unwrap_or("");
            Some(LogCommit {
                id: id.to_owned(),
                short_id: short_id.to_owned(),
                author: Some(author.to_owned()).filter(|a| !a.is_empty()),
                message: Some(subject.to_owned()).filter(|s| !s.is_empty()),
                timestamp_ms: at.parse::<i64>().map_or(0, |s| s * 1000),
                parents: parents.split_whitespace().map(str::to_owned).collect(),
            })
        })
        .collect()
}

/// The files `sha` changed versus its first parent (or the empty tree for a
/// root commit), with rename detection as configured.
pub(crate) fn commit_files(repo: &gix::Repository, sha: &str) -> Result<CommitFileList> {
    use gix::object::tree::diff::ChangeDetached as Change;
    assert_safe_revision(sha)?;
    let commit = repo
        .rev_parse_single(sha)
        .map_err(|e| GitError::invalid(format!("unknown revision {sha}: {e}")))?
        .object()
        .map_err(|e| GitError::unknown(e.to_string()))?
        .peel_to_commit()
        .map_err(|e| GitError::invalid(format!("{sha} is not a commit: {e}")))?;
    let tree = commit
        .tree()
        .map_err(|e| GitError::unknown(e.to_string()))?;
    let parent_tree = match commit.parent_ids().next() {
        Some(parent) => Some(
            parent
                .object()
                .map(|o| o.into_commit())
                .map_err(|e| GitError::unknown(e.to_string()))?
                .tree()
                .map_err(|e| GitError::unknown(e.to_string()))?,
        ),
        None => None,
    };
    let changes = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
        .map_err(|e| GitError::unknown(e.to_string()))?;
    let mut files: Vec<CommitFile> = changes
        .into_iter()
        .filter_map(|change| {
            let (path, orig_path, status, mode) = match change {
                Change::Addition {
                    location,
                    entry_mode,
                    ..
                } => (location, None, StatusLetter::Added, entry_mode),
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => (location, None, StatusLetter::Deleted, entry_mode),
                Change::Modification {
                    location,
                    entry_mode,
                    ..
                } => (location, None, StatusLetter::Modified, entry_mode),
                Change::Rewrite {
                    source_location,
                    location,
                    copy,
                    entry_mode,
                    ..
                } => {
                    let letter = if copy {
                        StatusLetter::Copied
                    } else {
                        StatusLetter::Renamed
                    };
                    (
                        location,
                        Some(source_location.to_string()),
                        letter,
                        entry_mode,
                    )
                }
            };
            // Directories are reported alongside their files; keep files.
            (!mode.is_tree()).then(|| CommitFile {
                path: path.to_string(),
                orig_path,
                status,
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = files.len() > COMMIT_FILES_CAP;
    files.truncate(COMMIT_FILES_CAP);
    Ok(CommitFileList { files, truncated })
}

/// `rev:path` bytes, or `None` when missing or at / above `hard_refuse`.
pub(crate) fn read_object(
    repo: &gix::ThreadSafeRepository,
    revision: &str,
    path: &str,
    hard_refuse: u64,
) -> Result<Option<Vec<u8>>> {
    let local = repo.to_thread_local();
    let Some(id) = blob_in_revision(&local, revision, path)? else {
        return Ok(None);
    };
    let side = crate::diff::BlobSide {
        repo: repo.clone(),
        id: Some(id),
    };
    read_capped(&side, hard_refuse)
}

/// Working-tree bytes, realpath-contained; `None` when missing or too large.
pub(crate) fn read_worktree_file(side: &WorktreeSide, hard_refuse: u64) -> Result<Option<Vec<u8>>> {
    read_capped(side, hard_refuse)
}

fn read_capped(side: &dyn DiffSide, hard_refuse: u64) -> Result<Option<Vec<u8>>> {
    match side.size()? {
        None => Ok(None),
        Some(size) if size >= hard_refuse => Ok(None),
        Some(_) => side.load(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_parse_like_parse_git_log_lines() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let text = format!(
            "{a}\taaaaaaa\t1700000000\tAda\t{b}\tfix: one\twith a tab\n\
             malformed line\n\
             {b}\tbbbbbbb\t1600000000\t\t\troot\n"
        );
        let commits = parse_log_lines(&text);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].message.as_deref(), Some("fix: one\twith a tab"));
        assert_eq!(commits[0].parents, vec![b.clone()]);
        assert_eq!(commits[0].timestamp_ms, 1_700_000_000_000);
        assert_eq!(commits[1].author, None);
        assert!(commits[1].parents.is_empty());
    }

    #[test]
    fn page_limits_default_and_clamp() {
        assert_eq!(page_limit(None, 100), 100);
        assert_eq!(page_limit(Some(0), 100), 100);
        assert_eq!(page_limit(Some(7), 100), 7);
        assert_eq!(page_limit(Some(50_000), 100), MAX_LOG_PAGE);
    }
}
