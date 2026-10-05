//! Worktree list liveness (SPEC mille-git §16): `git worktree list
//! --porcelain`, which — unlike gix's `worktrees()`, whose proxies are the
//! *linked* worktrees only — includes the primary checkout, so the list
//! matches what a user sees from `git worktree list`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{GitCli, RunOptions};
use crate::error::Result;
use crate::paths;

/// One worktree of a repository (§16).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitWorktree {
    /// Absolute path of the worktree.
    pub path: PathBuf,
    /// Short branch name; `None` when detached or bare.
    pub branch: Option<String>,
    /// Full hex id of its `HEAD`; `None` on an unborn branch or bare.
    pub head: Option<String>,
    /// The bare repository entry.
    pub bare: bool,
    /// `git worktree lock`ed.
    pub locked: bool,
    /// Its directory is gone; `git worktree prune` would remove it.
    pub prunable: bool,
    /// The worktree this session belongs to.
    pub is_current: bool,
}

/// Parse `git worktree list --porcelain`. `current` is the realpath of the
/// asking session's checkout root.
pub(crate) fn parse_worktree_list(text: &str, current: &Path) -> Vec<GitWorktree> {
    let mut out = Vec::new();
    let mut entry: Option<GitWorktree> = None;
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            out.extend(entry.take());
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            out.extend(entry.take());
            let path = PathBuf::from(value);
            let is_current = paths::canonicalize_lenient(&path) == current;
            entry = Some(GitWorktree {
                path,
                branch: None,
                head: None,
                bare: false,
                locked: false,
                prunable: false,
                is_current,
            });
            continue;
        }
        let Some(current_entry) = entry.as_mut() else {
            continue;
        };
        match key {
            "HEAD" => {
                current_entry.head = Some(value.to_owned()).filter(|h| h.bytes().any(|b| b != b'0'))
            }
            "branch" => {
                current_entry.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_owned(),
                )
            }
            "bare" => current_entry.bare = true,
            "locked" => current_entry.locked = true,
            "prunable" => current_entry.prunable = true,
            _ => {}
        }
    }
    out
}

pub(crate) async fn list_worktrees(
    git: &GitCli,
    root: &Path,
    current: &Path,
) -> Result<Vec<GitWorktree>> {
    let out = git
        .run(
            root,
            ["worktree", "list", "--porcelain"],
            RunOptions::read(),
        )
        .await?
        .check("worktree list")?;
    Ok(parse_worktree_list(&out.stdout_text(), current))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_blocks_become_worktrees() {
        let text = "worktree /r\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\n\
                    worktree /wt\nHEAD 2222222222222222222222222222222222222222\ndetached\nlocked agent busy\n\n\
                    worktree /gone\nHEAD 3333333333333333333333333333333333333333\nbranch refs/heads/old\nprunable gitdir file points to non-existent location\n";
        let list = parse_worktree_list(text, Path::new("/nowhere"));
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[1].branch, None);
        assert!(list[1].locked && !list[1].prunable);
        assert!(list[2].prunable);
        assert_eq!(
            list[2].head.as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
    }
}
