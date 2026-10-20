//! PR 1: identity, gitdir resolution, checkout-root discovery, allow-list.

mod common;

use common::{git, real, Repo};
use mille_git::{assert_path_under_root, GitRepoKind};

// T-OPEN
#[tokio::test]
async fn open_reports_an_unborn_main_then_its_head() {
    require_git!();
    let repo = Repo::init();
    let engine = common::engine(&[&repo.root]);
    let id = engine.open(&repo.root).await.unwrap();
    assert_eq!(id.kind, GitRepoKind::Worktree);
    assert_eq!(id.worktree_path, repo.root);
    assert_eq!(id.branch.as_deref(), Some("main"));
    assert_eq!(id.head, None, "an unborn branch has no HEAD id");
    assert!(!id.bare && !id.detached);
    assert_eq!(
        real(id.gitdir.as_ref().unwrap()),
        real(&repo.root.join(".git"))
    );
    assert_eq!(
        real(id.common_dir.as_ref().unwrap()),
        real(&repo.root.join(".git"))
    );

    repo.write("a.txt", "a\n");
    let sha = repo.commit_all("first");
    let id = engine.identity(&repo.root).unwrap();
    assert_eq!(id.head.as_deref(), Some(sha.as_str()));
}

// T-OPEN-SUBDIR, AC-016
#[tokio::test]
async fn a_package_subdirectory_opens_its_parent_checkout_and_siblings_share_it() {
    require_git!();
    let repo = Repo::init();
    let app = repo.path("packages/app");
    let other = repo.path("packages/other");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&other).unwrap();

    // `.git` is on an ancestor outside the allow-list: still a repository.
    let engine = common::engine(&[&app, &other]);
    let from_app = engine.open(&app).await.unwrap();
    assert_eq!(from_app.kind, GitRepoKind::Worktree);
    assert_eq!(
        from_app.worktree_path, repo.root,
        "the session key is the checkout root"
    );

    let from_other = engine.open(&other).await.unwrap();
    assert_eq!(from_other.worktree_path, repo.root);
    assert_eq!(
        engine.session_count(),
        1,
        "sibling packages de-dupe onto one session"
    );
    assert_eq!(engine.session_key(&app), engine.session_key(&other));
    assert_eq!(engine.session_key(&repo.root), Some(real(&repo.root)));

    // Containment stays relative to the files, not the discovered root.
    assert_eq!(
        assert_path_under_root(&app, "../../outside")
            .unwrap_err()
            .code(),
        "EINVAL"
    );

    // Refcounted close: one package closing keeps the other's session.
    engine.close(&app).await.unwrap();
    assert_eq!(engine.session_count(), 1);
    assert!(engine.identity(&other).is_ok());
    engine.close(&other).await.unwrap();
    assert_eq!(engine.session_count(), 0);
    engine.close(&other).await.unwrap(); // idempotent

    // With only `app` allowed, a sibling package is refused.
    let narrow = common::engine(&[&app]);
    narrow.open(&app).await.unwrap();
    assert_eq!(narrow.open(&other).await.unwrap_err().code(), "EINVAL");
}

#[tokio::test]
async fn reopening_the_same_path_takes_another_reference() {
    require_git!();
    let repo = Repo::init();
    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.open(&repo.root).await.unwrap();
    engine.close(&repo.root).await.unwrap();
    assert_eq!(engine.session_count(), 1);
    engine.close(&repo.root).await.unwrap();
    assert_eq!(engine.session_count(), 0);
}

// T-NONE
#[tokio::test]
async fn a_plain_directory_opens_as_kind_none() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let engine = common::engine(&[&ws]);
    let id = engine.open(&ws).await.unwrap();
    assert_eq!(id.kind, GitRepoKind::None);
    assert_eq!(id.worktree_path, ws);
    assert_eq!(id.gitdir, None);
    assert_eq!(id.head, None);
    assert_eq!(engine.session_count(), 1);
}

// T-ALLOW
#[tokio::test]
async fn open_outside_the_allow_list_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&other).unwrap();

    let engine = common::engine(&[&ws]);
    assert_eq!(engine.open(&other).await.unwrap_err().code(), "EINVAL");
    assert_eq!(
        engine
            .open(std::path::Path::new("relative/ws"))
            .await
            .unwrap_err()
            .code(),
        "EINVAL"
    );
    assert_eq!(
        engine
            .open(&ws.join("..").join("other"))
            .await
            .unwrap_err()
            .code(),
        "EINVAL"
    );

    let closed = common::engine(&[]);
    assert_eq!(
        closed.open(&ws).await.unwrap_err().code(),
        "EINVAL",
        "empty allow-list"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_caller_path_that_leaves_the_allow_list_is_refused() {
    require_git!();
    let repo = Repo::init();
    let ws = repo.base().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::os::unix::fs::symlink(&repo.root, ws.join("escape")).unwrap();
    let engine = common::engine(&[&ws]);
    assert_eq!(
        engine.open(&ws.join("escape")).await.unwrap_err().code(),
        "EINVAL"
    );
}

// The macOS `/var` vs `/private/var` lesson: the allow-list compares
// realpaths, so either spelling admits the other.
#[tokio::test]
async fn allow_list_and_caller_may_use_different_spellings_of_one_directory() {
    require_git!();
    let repo = Repo::init();
    let engine = common::engine(&[&real(&repo.root)]);
    let id = engine.open(&repo.root).await.unwrap();
    assert_eq!(
        id.worktree_path, repo.root,
        "identity keeps the caller's spelling"
    );

    let engine = common::engine(&[&repo.root]);
    engine.open(&real(&repo.root)).await.unwrap();
    assert!(
        engine.identity(&repo.root).is_ok(),
        "both spellings resolve to the session"
    );
}

// T-WT (identity half), AC-004
#[tokio::test]
async fn a_linked_worktree_resolves_its_gitdir_redirect() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.txt", "a\n");
    repo.commit_all("first");
    let wt = repo.base().join("wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feature",
        wt.to_str().unwrap(),
    ]);

    let engine = common::engine(&[&repo.base()]);
    let id = engine.open(&wt).await.unwrap();
    assert_eq!(id.kind, GitRepoKind::Worktree);
    assert_eq!(id.worktree_path, wt);
    assert_eq!(id.branch.as_deref(), Some("feature"));
    assert_eq!(
        real(id.gitdir.as_ref().unwrap()),
        real(&repo.root.join(".git/worktrees/wt"))
    );
    assert_eq!(
        real(id.common_dir.as_ref().unwrap()),
        real(&repo.root.join(".git"))
    );
    assert!(
        wt.join(".git").is_file(),
        "the linked worktree's .git is a redirect file"
    );

    let main = engine.open(&repo.root).await.unwrap();
    assert_eq!(main.branch.as_deref(), Some("main"));
    assert_eq!(
        engine.session_count(),
        2,
        "linked worktrees are separate sessions"
    );
}

#[tokio::test]
async fn a_bare_repository_opens_as_bare() {
    require_git!();
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("bare.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    let engine = common::engine(&[tmp.path()]);
    let id = engine.open(&bare).await.unwrap();
    assert_eq!(id.kind, GitRepoKind::Bare);
    assert!(id.bare);
}

#[tokio::test]
async fn a_detached_head_has_an_id_and_no_branch() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.txt", "a\n");
    let sha = repo.commit_all("first");
    repo.git(&["checkout", "-q", "--detach"]);
    let engine = common::engine(&[&repo.root]);
    let id = engine.open(&repo.root).await.unwrap();
    assert!(id.detached);
    assert_eq!(id.branch, None);
    assert_eq!(id.head.as_deref(), Some(sha.as_str()));
}

// A `.git` file gix cannot follow and git cannot read either: discovery
// degrades to `kind: none` instead of throwing.
#[tokio::test]
async fn an_unreadable_redirect_degrades_to_kind_none() {
    require_git!();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join(".git"), "gitdir: /definitely/missing/gitdir\n").unwrap();
    let engine = common::engine(&[&ws]);
    let id = engine.open(&ws).await.unwrap();
    assert_eq!(id.kind, GitRepoKind::None);
    assert_eq!(id.worktree_path, ws);
}
