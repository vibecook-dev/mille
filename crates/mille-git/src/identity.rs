//! `open()`'s discovery half: which checkout a caller path belongs to, and
//! what that checkout's identity is (SPEC mille-git §9.2–§9.4).
//!
//! **The session key is the discovered checkout root, not the caller path.**
//! A FileTree is often rooted at a package inside a monorepo
//! (`/repo/packages/app`), and `allowedRoots` is that package — but the
//! repository is `/repo`, with its `.git` on an ancestor *outside* the allow
//! list. So:
//!
//! - the `.git` walk-up MAY leave `allowedRoots` (only the caller path and
//!   later mutations are gated — `containment.rs`);
//! - `gix::open` does not walk parents, so it is handed the discovered root,
//!   never the caller path;
//! - two packages of one repo open onto ONE session (one gitdir watch, one
//!   status queue), because they share one gitdir — keying by caller path
//!   would double every watch and race two status runs over one `index`.
//!
//! The session is keyed by the checkout root's realpath; `worktreePath` on
//! the identity keeps the caller's spelling of that root, because the
//! `GitClient` adapter builds decoration keys from it and those must match
//! the explorer's URIs (`/var/folders/…`, not `/private/var/folders/…`).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{GitCli, RunOptions};
use crate::gitdir;
use crate::paths;

/// `'none' | 'worktree' | 'bare'`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GitRepoKind {
    /// Not a repository. The session still exists so decorations stay quiet
    /// and `subscribe_status` works before the folder becomes a repo.
    None,
    /// A checkout with a working tree (main or linked).
    Worktree,
    /// A bare repository: empty status, worktree mutations are `EUNSUPPORTED`.
    Bare,
}

/// SPEC §9.4. Field names serialize as the TypeScript contract spells them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitIdentity {
    /// What kind of thing was opened.
    pub kind: GitRepoKind,
    /// Absolute checkout root (`kind != None`), or the caller path (`None`).
    pub worktree_path: PathBuf,
    /// The resolved gitdir — for a linked worktree, the far end of the
    /// `gitdir:` redirect, never the `.git` file.
    pub gitdir: Option<PathBuf>,
    /// The common dir (objects, refs, `worktrees/`).
    pub common_dir: Option<PathBuf>,
    /// `kind == Bare`.
    pub bare: bool,
    /// Full hex id of `HEAD`, or `None` on an unborn branch / no repo.
    pub head: Option<String>,
    /// Short branch name (no `refs/heads/`); `None` when detached or no repo.
    pub branch: Option<String>,
    /// `HEAD` points at a commit directly.
    pub detached: bool,
}

impl GitIdentity {
    /// The identity of an allow-listed directory that is not a repository.
    pub fn none(caller: &Path) -> Self {
        Self {
            kind: GitRepoKind::None,
            worktree_path: caller.to_path_buf(),
            gitdir: None,
            common_dir: None,
            bare: false,
            head: None,
            branch: None,
            detached: false,
        }
    }
}

/// What discovery found for one caller path.
pub(crate) struct Discovered {
    /// The checkout root in the caller's spelling (or the caller path for
    /// `kind: none`).
    pub(crate) root: PathBuf,
    pub(crate) identity: GitIdentity,
    /// `None` for `kind: none`, and when only the CLI could read the repo.
    pub(crate) repo: Option<gix::ThreadSafeRepository>,
}

/// Blocking discovery (run under `spawn_blocking`): walk up for `.git`,
/// open the checkout root with gix. `Err(root)` asks the caller to try the
/// CLI fallback for that checkout root.
pub(crate) fn discover_blocking(caller: &Path) -> Result<Discovered, PathBuf> {
    if let Some(dot_git) = gitdir::find_dot_git(caller) {
        let root = dot_git
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| caller.to_path_buf());
        return match gix::ThreadSafeRepository::open(root.clone()) {
            Ok(repo) => {
                let identity = identity_from_repo(&repo.to_thread_local(), &root);
                Ok(Discovered {
                    root,
                    identity,
                    repo: Some(repo),
                })
            }
            Err(err) => {
                tracing::debug!(root = %root.display(), %err, "gix::open failed; trying the git CLI");
                Err(root)
            }
        };
    }
    // No `.git` entry anywhere above: the caller may *be* a bare repository,
    // which has no `.git` to find.
    match gix::ThreadSafeRepository::discover(caller) {
        Ok(repo) if repo.to_thread_local().is_bare() => {
            let root = paths::normalize_lexical(repo.git_dir());
            let identity = identity_from_repo(&repo.to_thread_local(), &root);
            Ok(Discovered {
                root,
                identity,
                repo: Some(repo),
            })
        }
        _ => Ok(Discovered {
            root: caller.to_path_buf(),
            identity: GitIdentity::none(caller),
            repo: None,
        }),
    }
}

pub(crate) fn identity_from_repo(repo: &gix::Repository, root: &Path) -> GitIdentity {
    let bare = repo.is_bare();
    let (head, branch, detached) = read_head(repo);
    GitIdentity {
        kind: if bare {
            GitRepoKind::Bare
        } else {
            GitRepoKind::Worktree
        },
        worktree_path: root.to_path_buf(),
        gitdir: Some(absolute(root, repo.git_dir())),
        common_dir: Some(absolute(root, repo.common_dir())),
        bare,
        head,
        branch,
        detached,
    }
}

/// `(head, branch, detached)` — the cheap part re-read on every status run
/// (§10.6 rule 5).
pub(crate) fn read_head(repo: &gix::Repository) -> (Option<String>, Option<String>, bool) {
    match repo.head() {
        Ok(head) => (
            head.id().map(|id| id.to_string()),
            head.referent_name().map(|name| name.shorten().to_string()),
            head.is_detached(),
        ),
        Err(_) => (None, None, false),
    }
}

fn absolute(root: &Path, path: &Path) -> PathBuf {
    paths::normalize_lexical(&if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

/// §9.2 step 6: gix could not open the checkout; ask git once. The spec's
/// single `rev-parse … --abbrev-ref HEAD` fails outright on an unborn
/// branch, so the location query and the HEAD queries run separately and
/// only the location query decides whether this is a repository.
pub(crate) async fn identity_via_cli(git: &GitCli, root: &Path) -> Option<GitIdentity> {
    let out = git
        .run(
            root,
            [
                "rev-parse",
                "--git-dir",
                "--git-common-dir",
                "--is-bare-repository",
            ],
            RunOptions::read(),
        )
        .await
        .ok()?
        .check("rev-parse")
        .ok()?;
    let text = out.stdout_text();
    let mut lines = text.lines();
    let gitdir = absolute(root, Path::new(lines.next()?.trim()));
    let common_dir = absolute(root, Path::new(lines.next()?.trim()));
    let bare = lines.next()?.trim() == "true";

    let branch = match git
        .run(
            root,
            ["rev-parse", "--abbrev-ref", "HEAD"],
            RunOptions::read(),
        )
        .await
    {
        Ok(out) if out.success() => {
            Some(out.stdout_text().trim().to_owned()).filter(|b| b != "HEAD" && !b.is_empty())
        }
        _ => None,
    };
    let head = match git
        .run(
            root,
            ["rev-parse", "--verify", "-q", "HEAD"],
            RunOptions::read(),
        )
        .await
    {
        Ok(out) if out.success() => {
            Some(out.stdout_text().trim().to_owned()).filter(|h| !h.is_empty())
        }
        _ => None,
    };
    Some(GitIdentity {
        kind: if bare {
            GitRepoKind::Bare
        } else {
            GitRepoKind::Worktree
        },
        worktree_path: root.to_path_buf(),
        gitdir: Some(gitdir),
        common_dir: Some(common_dir),
        bare,
        detached: head.is_some() && branch.is_none(),
        head,
        branch,
    })
}
