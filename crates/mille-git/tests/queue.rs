//! PR 3: the per-worktree queue, path hints and §11.2.1 routing.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{has, real, Published, Repo};
use mille_git::{GitEngine, GitEngineOptions};

const SLOW: Duration = Duration::from_secs(10);

/// A committed repo with `names` checked in at "v1".
fn repo_with(names: &[&str]) -> Repo {
    let repo = Repo::init();
    for name in names {
        repo.write(name, "v1\n");
    }
    repo.commit_all("base");
    repo
}

// T-HINT, AC-003: a worktree-only save (index untouched) refreshes status
// from a path hint, without any HEAD/index event.
#[tokio::test]
async fn a_relative_hint_refreshes_a_worktree_only_save() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let mut published = Published::subscribe(&engine, &repo.root);

    repo.write("a.ts", "v2\n");
    let hinted = Instant::now();
    engine.hint_paths(&repo.root, Some(vec!["a.ts".into()]));
    let snapshot = published
        .until(SLOW, |s| has(s, "a.ts", ".M"))
        .await
        .expect("status saw the save");
    let latency = hinted.elapsed();
    eprintln!("hint -> published: {latency:?} (debounce 75 ms)");
    assert!(snapshot.path_limited);
    assert!(
        latency >= Duration::from_millis(60),
        "the debounce held the run back ({latency:?})"
    );
}

// T-HINT-LAZY at the engine boundary: the explorer's `'event'` carries an
// absolute OS path in the explorer's spelling (`/tmp/…` while the checkout
// realpath is `/private/tmp/…` on macOS) for a file no tree ever expanded.
#[tokio::test]
async fn an_os_path_in_another_spelling_routes_to_its_checkout() {
    require_git!();
    let repo = repo_with(&["deep/never/expanded/file.ts"]);
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let mut published = Published::subscribe(&engine, &repo.root);

    repo.write("deep/never/expanded/file.ts", "v2\n");
    engine.hint_os_paths(&[repo.path("deep/never/expanded/file.ts")]);
    let snapshot = published
        .until(SLOW, |s| has(s, "deep/never/expanded/file.ts", ".M"))
        .await
        .expect("routed and refreshed");
    assert!(
        snapshot.path_limited,
        "routed as a worktree-relative pathspec, not a full run"
    );

    // The realpath spelling routes identically.
    repo.write("deep/never/expanded/file.ts", "v1\n");
    engine.hint_os_paths(&[real(&repo.root).join("deep/never/expanded/file.ts")]);
    published
        .until(SLOW, |s| s.entries.is_empty())
        .await
        .expect("the revert routed too");
}

// T-HINT-MERGE, AC-015
#[tokio::test]
async fn hinting_one_of_three_dirty_files_keeps_the_other_two() {
    require_git!();
    let repo = repo_with(&["a.ts", "b.ts", "c.ts"]);
    for name in ["a.ts", "b.ts", "c.ts"] {
        repo.write(name, "v2\n");
    }
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    assert_eq!(engine.status(&repo.root).await.unwrap().entries.len(), 3);
    let mut published = Published::subscribe(&engine, &repo.root);

    engine.hint_paths(&repo.root, Some(vec!["a.ts".into()]));
    let still = published.until(SLOW, |s| s.path_limited).await.unwrap();
    assert_eq!(still.entries.len(), 3);

    repo.write("a.ts", "v1\n");
    engine.hint_paths(&repo.root, Some(vec!["a.ts".into()]));
    let reverted = published
        .until(SLOW, |s| s.entries.len() == 2)
        .await
        .expect("a.ts left the list");
    assert!(reverted.entries.iter().all(|e| e.path != "a.ts"));
}

// T-HINT-ROUTE: two checkouts, one nested inside the other (a linked
// worktree under the primary, `/repo/wt2`), plus the coarse/overflow rules.
#[tokio::test]
async fn hints_reach_only_the_checkout_that_owns_the_path() {
    require_git!();
    let repo = repo_with(&["a.ts", "packages/app/src/x.ts"]);
    let wt2 = repo.path("wt2");
    repo.write(".git/info/exclude", "wt2/\n");
    repo.git(&["worktree", "add", "-q", "-b", "two", wt2.to_str().unwrap()]);

    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.open(&wt2).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    engine.status(&wt2).await.unwrap();
    let mut primary = Published::subscribe(&engine, &repo.root);
    let mut linked = Published::subscribe(&engine, &wt2);

    // A save inside the nested linked worktree: longest prefix wins.
    std::fs::write(wt2.join("a.ts"), "linked edit\n").unwrap();
    engine.hint_os_paths(&[wt2.join("a.ts")]);
    let snapshot = linked
        .until(SLOW, |s| has(s, "a.ts", ".M"))
        .await
        .expect("wt2 refreshed");
    assert!(snapshot.path_limited);
    assert!(
        !primary.any_within(Duration::from_millis(400)).await,
        "the primary was not asked"
    );

    // A save in the primary does not refresh wt2.
    repo.write("packages/app/src/x.ts", "primary edit\n");
    engine.hint_os_paths(&[repo.path("packages/app/src/x.ts")]);
    primary
        .until(SLOW, |s| has(s, "packages/app/src/x.ts", ".M"))
        .await
        .expect("primary refreshed");
    assert!(
        !linked.any_within(Duration::from_millis(400)).await,
        "wt2 was not asked"
    );

    // Gitdir noise and paths outside every checkout are dropped.
    let before = engine.status_runs_started();
    engine.hint_os_paths(&[
        repo.path(".git/index"),
        repo.path("src/a.ts.lock"),
        repo.base().join("elsewhere.ts"),
    ]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(engine.status_runs_started(), before);

    // Overflow under the primary's subtree: the primary full-scans, wt2
    // (not a prefix of that subtree) does not.
    engine.hint_coarse_os_path(&repo.path("packages/app/src"));
    primary
        .until(SLOW, |s| !s.path_limited)
        .await
        .expect("primary full scan");
    assert!(!linked.any_within(Duration::from_millis(400)).await);

    // The inverted rule is forbidden: a coarse path *above* both checkouts
    // matches neither.
    let before = engine.status_runs_started();
    engine.hint_coarse_os_path(&repo.base());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(engine.status_runs_started(), before);
}

// §12.2 drop-and-replace + §6.2 "one coalesced status": hints that arrive
// during a run coalesce into exactly one follow-up run.
#[tokio::test]
async fn hints_during_a_run_coalesce_into_one_follow_up() {
    require_git!();
    let repo = repo_with(&["a.ts", "b.ts", "c.ts"]);
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let runs_after_open = engine.status_runs_started();
    let mut published = Published::subscribe(&engine, &repo.root);

    engine.set_status_delay_for_test(Duration::from_millis(400));
    engine.hint_paths(&repo.root, None);
    tokio::time::sleep(Duration::from_millis(150)).await; // the full run is in flight
    for name in ["a.ts", "b.ts", "c.ts"] {
        repo.write(name, "v2\n");
        engine.hint_paths(&repo.root, Some(vec![name.into()]));
    }
    // The delayed full run computes after the writes, so it may already
    // show all three; the coalesced follow-up is the path-limited run.
    let last = published
        .until(SLOW, |s| s.path_limited && s.entries.len() == 3)
        .await
        .expect("the coalesced follow-up published");
    assert!(last.path_limited);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        engine.status_runs_started() - runs_after_open,
        2,
        "one in flight + one coalesced follow-up"
    );
}

// §6.2: an agent writing 40 files within a second produces one (or very
// few) status runs, not forty — the max-wait bound still refreshes.
#[tokio::test]
async fn a_burst_of_forty_writes_coalesces() {
    require_git!();
    let names: Vec<String> = (0..40).map(|i| format!("f{i:02}.ts")).collect();
    let repo = repo_with(&names.iter().map(String::as_str).collect::<Vec<_>>());
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let runs_after_open = engine.status_runs_started();
    let mut published = Published::subscribe(&engine, &repo.root);

    let started = Instant::now();
    for name in &names {
        repo.write(name, "v2\n");
        engine.hint_os_paths(&[repo.path(name)]);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let burst = started.elapsed();
    published
        .until(SLOW, |s| s.entries.len() == 40)
        .await
        .expect("all forty published");
    let runs = engine.status_runs_started() - runs_after_open;
    eprintln!("40 writes in {burst:?} -> {runs} status run(s)");
    // `max_wait` refreshes a continuous burst at least that often, so the
    // bound is four runs per `max_wait` window the burst spans: four for the
    // ~450 ms it takes on Linux. windows-latest stretches it (a 10 ms sleep
    // is ~16 ms there) and, inside the whole workspace suite, measured 6 runs
    // for 650 ms and 8 for 1.2 s; with this binary alone, 2–4. Why Windows
    // runs more often within a window is open (PR #43). Forty still fails.
    let max_wait = GitEngineOptions::default().max_wait.as_millis();
    let spans = burst.as_millis().div_ceil(max_wait).max(1) as u64;
    assert!(
        (1..=4 * spans).contains(&runs),
        "{runs} runs for one burst of {burst:?}"
    );
}

// AC-008: eight worktrees, no global lock — statuses overlap.
#[tokio::test]
async fn eight_worktrees_run_status_concurrently() {
    require_git!();
    let repos: Vec<Repo> = (0..8).map(|_| repo_with(&["a.ts"])).collect();
    let roots: Vec<PathBuf> = repos.iter().map(|r| r.root.clone()).collect();
    let engine = GitEngine::new(GitEngineOptions {
        allowed_roots: roots.clone(),
        ..GitEngineOptions::default()
    });
    for root in &roots {
        engine.open(root).await.unwrap();
    }
    for root in &roots {
        engine.status(root).await.unwrap();
    }
    engine.set_status_delay_for_test(Duration::from_millis(300));
    let started = Instant::now();
    let refreshes = roots.iter().map(|root| engine.refresh(root, None));
    for result in futures_join_all(refreshes).await {
        result.unwrap();
    }
    let elapsed = started.elapsed();
    eprintln!(
        "8 delayed statuses in {elapsed:?}, peak concurrency {}",
        engine.status_peak_concurrency()
    );
    assert!(
        engine.status_peak_concurrency() >= 2,
        "statuses of different worktrees overlap"
    );
    assert!(
        elapsed < Duration::from_millis(8 * 300),
        "not serialized behind one lock"
    );
}

async fn futures_join_all<F: std::future::Future>(
    futures: impl IntoIterator<Item = F>,
) -> Vec<F::Output> {
    let mut out = Vec::new();
    let pinned: Vec<_> = futures.into_iter().map(Box::pin).collect();
    // Poll them concurrently by spawning nothing: a simple select loop over
    // a FuturesUnordered-free join.
    let mut pending: Vec<_> = pinned.into_iter().map(Some).collect();
    let mut results: Vec<Option<F::Output>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for (slot, result) in pending.iter_mut().zip(results.iter_mut()) {
            if let Some(fut) = slot {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => {
                        *result = Some(value);
                        *slot = None;
                    }
                    std::task::Poll::Pending => done = false,
                }
            }
        }
        if done {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    out.extend(results.into_iter().map(Option::unwrap));
    out
}

// §21.1: a path-limited burst is followed by an idle full rescan that finds
// untracked files no hint asked about.
#[tokio::test]
async fn an_idle_rescan_finds_what_no_hint_asked_about() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = GitEngine::new(GitEngineOptions {
        allowed_roots: vec![repo.root.clone()],
        idle_rescan: Duration::from_millis(300),
        ..GitEngineOptions::default()
    });
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let mut published = Published::subscribe(&engine, &repo.root);

    repo.write("unhinted.ts", "new\n");
    repo.write("a.ts", "v2\n");
    engine.hint_paths(&repo.root, Some(vec!["a.ts".into()]));
    let limited = published.until(SLOW, |s| s.path_limited).await.unwrap();
    assert!(
        limited.entries.iter().all(|e| e.path != "unhinted.ts"),
        "not magically present"
    );
    let full = published
        .until(SLOW, |s| !s.path_limited)
        .await
        .expect("idle rescan ran");
    assert!(full
        .entries
        .iter()
        .any(|e| e.path == "unhinted.ts" && e.untracked));
}

#[tokio::test]
async fn unsafe_and_unknown_hints_are_ignored() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let before = engine.status_runs_started();
    engine.hint_paths(
        &repo.root,
        Some(vec!["../escape".into(), "/etc/passwd".into()]),
    );
    engine.hint_paths(&repo.base().join("not-open"), None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(engine.status_runs_started(), before);
}

// hint_paths is callable from a plain OS thread with no runtime context —
// the notify thread's situation (§11.2 rule 5).
#[tokio::test]
async fn hint_paths_is_safe_off_the_runtime() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = std::sync::Arc::new(common::quiet_engine(&[&repo.root]));
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    let mut published = Published::subscribe(&engine, &repo.root);
    repo.write("a.ts", "v2\n");
    let (e, root) = (engine.clone(), repo.root.clone());
    std::thread::spawn(move || e.hint_paths(&root, Some(vec!["a.ts".into()])))
        .join()
        .unwrap();
    published
        .until(SLOW, |s| has(s, "a.ts", ".M"))
        .await
        .expect("hinted from a bare thread");
}
