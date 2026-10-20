//! PR 3: gitdir and `<common>/worktrees/` watches.

mod common;

use std::time::Duration;

use common::{has, Published, Repo};

const SLOW: Duration = Duration::from_secs(10);

fn repo_with(names: &[&str]) -> Repo {
    let repo = Repo::init();
    for name in names {
        repo.write(name, "v1\n");
    }
    repo.commit_all("base");
    repo
}

// F4: `git add` outside the engine refreshes status through the index watch,
// with no path hint and no explorer.
#[tokio::test]
async fn an_external_git_add_refreshes_status() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    assert!(has(&engine.status(&repo.root).await.unwrap(), "a.ts", ".M"));
    let mut published = Published::subscribe(&engine, &repo.root);

    repo.git(&["add", "a.ts"]);
    let snapshot = published
        .until(SLOW, |s| has(s, "a.ts", "M."))
        .await
        .expect("index watch fired");
    assert!(!snapshot.path_limited, "a gitdir event is a full status");
}

#[tokio::test]
async fn a_commit_and_a_branch_switch_update_identity() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let mut published = Published::subscribe(&engine, &repo.root);

    repo.git(&["checkout", "-q", "-b", "topic"]);
    published
        .until(SLOW, |s| s.identity.branch.as_deref() == Some("topic"))
        .await
        .expect("HEAD watch fired");

    repo.write("a.ts", "v2\n");
    let sha = repo.commit_all("second");
    published
        .until(SLOW, |s| s.identity.head.as_deref() == Some(sha.as_str()))
        .await
        .expect("ref watch fired");
}

// T-LOCK (watch half): `index.lock` coming and going is not a trigger.
#[tokio::test]
async fn index_lock_events_are_ignored() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await; // let open's own events settle
    let before = engine.status_runs_started();

    let lock = repo.path(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::remove_file(&lock).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        engine.status_runs_started(),
        before,
        "index.lock is not a status trigger"
    );
}

// T-WT (watch half), AC-004: a linked worktree's HEAD/index live at the far
// end of its `gitdir:` redirect; the watch follows it.
#[tokio::test]
async fn a_linked_worktree_watches_its_own_index() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let wt = repo.base().join("wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feature",
        wt.to_str().unwrap(),
    ]);
    repo.write("a.ts", "primary is dirty\n");

    let engine = common::quiet_engine(&[&repo.base()]);
    engine.open(&wt).await.unwrap();
    let initial = engine.status(&wt).await.unwrap();
    assert!(
        initial.entries.is_empty(),
        "the primary's dirt is not the linked worktree's"
    );
    let mut published = Published::subscribe(&engine, &wt);

    std::fs::write(wt.join("b.ts"), "new in wt\n").unwrap();
    common::git(&wt, &["add", "b.ts"]);
    published
        .until(SLOW, |s| has(s, "b.ts", "A."))
        .await
        .expect("the redirected index watch fired");
}

#[tokio::test]
async fn an_empty_repository_sees_its_first_index() {
    require_git!();
    let repo = Repo::init();
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    assert!(!repo.path(".git/index").exists());
    let mut published = Published::subscribe(&engine, &repo.root);

    repo.write("first.ts", "x\n");
    repo.git(&["add", "first.ts"]);
    published
        .until(SLOW, |s| has(s, "first.ts", "A."))
        .await
        .expect("the directory watch saw index appear");
}

// F5, §11.4, §16
#[tokio::test]
async fn worktree_add_and_remove_update_the_list() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = common::quiet_engine(&[&repo.base()]);
    engine.open(&repo.root).await.unwrap();
    let list = engine.worktrees(&repo.root).await.unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].is_current && list[0].branch.as_deref() == Some("main"));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    engine
        .subscribe_worktrees(
            &repo.root,
            std::sync::Arc::new(move |l| drop(tx.send(l.to_vec()))),
        )
        .unwrap();

    let wt = repo.base().join("wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feature",
        wt.to_str().unwrap(),
    ]);
    // The refresh runs one debounce after the first event, which can be
    // before `git worktree add` has finished: the new worktree is listed
    // while its HEAD is still the null id git writes first, so without a
    // branch (seen on windows-latest, where git takes longer than the 75 ms
    // debounce). Its later writes refresh the list again; wait for that one.
    let added = loop {
        let list = tokio::time::timeout(SLOW, rx.recv())
            .await
            .expect("list refreshed")
            .unwrap();
        if list.iter().any(|w| !w.is_current && w.branch.is_some()) {
            break list;
        }
    };
    assert_eq!(added.len(), 2);
    let linked = added.iter().find(|w| !w.is_current).unwrap();
    assert_eq!(linked.branch.as_deref(), Some("feature"));

    // `worktrees/` did not exist before the first add; the engine watches it
    // now, so a second add is seen too.
    let wt2 = repo.base().join("wt2");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "second",
        wt2.to_str().unwrap(),
    ]);
    let more = loop {
        let list = tokio::time::timeout(SLOW, rx.recv())
            .await
            .expect("second add seen")
            .unwrap();
        if list.len() == 3 {
            break list;
        }
    };
    assert_eq!(more.len(), 3);

    repo.git(&["worktree", "remove", wt2.to_str().unwrap()]);
    let removed = loop {
        let list = tokio::time::timeout(SLOW, rx.recv())
            .await
            .expect("remove seen")
            .unwrap();
        if list.len() == 2 {
            break list;
        }
    };
    assert!(removed.iter().all(|w| w.path != wt2));
}

// §9.2 close: the last close stops the watches.
#[tokio::test]
async fn close_stops_watching() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    engine.close(&repo.root).await.unwrap();
    let before = engine.status_runs_started();
    repo.write("a.ts", "v2\n");
    repo.git(&["add", "a.ts"]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(engine.status_runs_started(), before);
    assert_eq!(
        engine.status(&repo.root).await.unwrap_err().code(),
        "EINVAL",
        "closed sessions are gone"
    );
}

// T-NONE: a plain directory has nothing to watch and stays quiet.
#[tokio::test]
async fn a_non_repo_session_is_quiet_and_subscribable() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = common::quiet_engine(&[tmp.path()]);
    engine.open(tmp.path()).await.unwrap();
    let mut published = Published::subscribe(&engine, tmp.path());
    assert!(engine.status(tmp.path()).await.unwrap().entries.is_empty());
    engine.hint_paths(tmp.path(), None);
    let snapshot = published.until(SLOW, |_| true).await.unwrap();
    assert!(snapshot.entries.is_empty());
    assert_eq!(
        engine.worktrees(tmp.path()).await.unwrap_err().code(),
        "ENOTAREPO"
    );
}
