//! PR 4: CLI stage / unstage / restore and the stdin include-set commit.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(unix)]
use common::Published;
use common::{has, Repo};
#[cfg(unix)]
use mille_git::CancellationToken;
use mille_git::{CommitRequest, GitEngine, GitEngineOptions, OpOptions};

/// Only the Unix-only hook tests wait on a subscription.
#[cfg(unix)]
const SLOW: Duration = Duration::from_secs(10);

fn repo_with(names: &[&str]) -> Repo {
    let repo = Repo::init();
    for name in names {
        repo.write(name, "v1\n");
    }
    repo.commit_all("base");
    repo
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn porcelain_xy(repo: &Repo, path: &str) -> Option<String> {
    let out = common::git_output(
        &repo.root,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    );
    let entries = mille_git::parse_porcelain_v2(&out.stdout, &mut |_| {});
    entries
        .into_iter()
        .find(|e| e.path == path)
        .map(|e| e.xy.as_string())
}

#[cfg(unix)]
fn install_hook(repo: &Repo, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = repo.path(&format!(".git/hooks/{name}"));
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

async fn opened(repo: &Repo) -> GitEngine {
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine.status(&repo.root).await.unwrap();
    engine
}

// T-COMMIT, AC-006
#[cfg(unix)]
#[tokio::test]
async fn commit_takes_exactly_the_include_set_with_a_stdin_message() {
    require_git!();
    let repo = repo_with(&["a.ts", "b.ts", "c.ts", "d.ts"]);
    for name in ["a.ts", "b.ts", "c.ts", "d.ts"] {
        repo.write(name, "v2\n");
    }
    repo.git(&["add", "d.ts"]); // staged, but not in the include-set
    install_hook(
        &repo,
        "commit-msg",
        "touch \"$(git rev-parse --git-dir)/commit-msg-ran\"",
    );
    let engine = opened(&repo).await;

    let body = "First body line.\n\n# not a comment: -F keeps it\nLast line.";
    let sha = engine
        .commit(
            &repo.root,
            CommitRequest {
                summary: "feat: commit two of four".into(),
                body: Some(body.into()),
                paths: strings(&["a.ts", "b.ts"]),
                cancel: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(sha.len(), 40);
    assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), sha);
    assert_eq!(
        repo.git(&["log", "-1", "--format=%B"]),
        format!("feat: commit two of four\n\n{body}\n\n")
    );
    assert!(
        repo.path(".git/commit-msg-ran").exists(),
        "the commit-msg hook ran"
    );
    let committed = repo.git(&["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(committed.lines().collect::<Vec<_>>(), ["a.ts", "b.ts"]);
    assert_eq!(
        porcelain_xy(&repo, "c.ts").as_deref(),
        Some(".M"),
        "the excluded dirty file stays dirty"
    );
    assert_eq!(
        porcelain_xy(&repo, "d.ts").as_deref(),
        Some(".M"),
        "the excluded staged file was unstaged"
    );

    // The blocking section's end refreshed status.
    let snapshot = engine.status(&repo.root).await.unwrap();
    assert_eq!(snapshot.identity.head.as_deref(), Some(sha.as_str()));
    assert!(has(&snapshot, "c.ts", ".M") && has(&snapshot, "d.ts", ".M"));
    assert!(snapshot
        .entries
        .iter()
        .all(|e| e.path != "a.ts" && e.path != "b.ts"));
}

#[tokio::test]
async fn commit_of_a_staged_rename_keeps_the_pair() {
    require_git!();
    let repo = repo_with(&["old.ts"]);
    repo.git(&["mv", "old.ts", "new.ts"]);
    let engine = opened(&repo).await;
    let request = CommitRequest {
        summary: "rename".into(),
        paths: strings(&["new.ts"]),
        ..Default::default()
    };
    engine.commit(&repo.root, request).await.unwrap();
    let changed = repo.git(&["show", "--name-status", "--format=", "-M", "HEAD"]);
    assert!(changed.starts_with("R100\told.ts\tnew.ts"), "{changed}");
}

#[tokio::test]
async fn commit_rejects_an_empty_summary_or_include_set_and_clean_paths() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = opened(&repo).await;
    let empty_summary = CommitRequest {
        summary: "  ".into(),
        paths: strings(&["a.ts"]),
        ..Default::default()
    };
    assert_eq!(
        engine
            .commit(&repo.root, empty_summary)
            .await
            .unwrap_err()
            .code(),
        "EINVAL"
    );
    let empty_set = CommitRequest {
        summary: "x".into(),
        paths: vec![],
        ..Default::default()
    };
    assert_eq!(
        engine
            .commit(&repo.root, empty_set)
            .await
            .unwrap_err()
            .code(),
        "EINVAL"
    );
    let escape = CommitRequest {
        summary: "x".into(),
        paths: strings(&["../x"]),
        ..Default::default()
    };
    assert_eq!(
        engine.commit(&repo.root, escape).await.unwrap_err().code(),
        "EINVAL"
    );
    let clean = CommitRequest {
        summary: "x".into(),
        paths: strings(&["a.ts"]),
        ..Default::default()
    };
    let err = engine.commit(&repo.root, clean).await.unwrap_err();
    assert_eq!(err.code(), "EINVAL", "{err}");
}

// T-REVERT-INDEX, AC-017
#[tokio::test]
async fn restore_puts_index_bytes_back_and_keeps_the_stage() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "staged\n");
    repo.git(&["add", "a.ts"]);
    repo.write("a.ts", "staged\nplus unstaged\n");
    let engine = opened(&repo).await;
    assert_eq!(porcelain_xy(&repo, "a.ts").as_deref(), Some("MM"));

    engine
        .restore(&repo.root, &strings(&["a.ts"]), OpOptions::default())
        .await
        .unwrap();
    assert_eq!(repo.read("a.ts"), "staged\n", "index bytes, not HEAD's");
    assert_eq!(
        porcelain_xy(&repo, "a.ts").as_deref(),
        Some("M."),
        "the index is untouched"
    );
    assert!(has(&engine.status(&repo.root).await.unwrap(), "a.ts", "M."));
}

#[tokio::test]
async fn stage_and_unstage_round_trip() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    repo.write("new.ts", "new\n");
    let engine = opened(&repo).await;

    engine
        .stage(
            &repo.root,
            &strings(&["a.ts", "new.ts"]),
            OpOptions::default(),
        )
        .await
        .unwrap();
    let staged = engine.status(&repo.root).await.unwrap();
    assert!(has(&staged, "a.ts", "M.") && has(&staged, "new.ts", "A."));

    engine
        .unstage(
            &repo.root,
            &strings(&["a.ts", "new.ts"]),
            OpOptions::default(),
        )
        .await
        .unwrap();
    let unstaged = engine.status(&repo.root).await.unwrap();
    assert!(has(&unstaged, "a.ts", ".M"));
    assert!(unstaged
        .entries
        .iter()
        .any(|e| e.path == "new.ts" && e.untracked));

    // Empty path lists are no-op successes (§15.3).
    engine
        .stage(&repo.root, &[], OpOptions::default())
        .await
        .unwrap();
    engine
        .unstage(&repo.root, &[], OpOptions::default())
        .await
        .unwrap();
    engine
        .restore(&repo.root, &[], OpOptions::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn unstage_works_on_an_unborn_branch() {
    require_git!();
    let repo = Repo::init();
    repo.write("first.ts", "x\n");
    repo.git(&["add", "first.ts"]);
    let engine = opened(&repo).await;
    engine
        .unstage(&repo.root, &strings(&["first.ts"]), OpOptions::default())
        .await
        .unwrap();
    assert_eq!(porcelain_xy(&repo, "first.ts").as_deref(), Some(".?"));
}

// `--literal-pathspecs`: `app/[id]/page.tsx` is a file, not a glob.
#[tokio::test]
async fn paths_are_staged_literally() {
    require_git!();
    let repo = repo_with(&["app/[id]/page.tsx", "app/i/page.tsx", "with space.ts"]);
    for name in ["app/[id]/page.tsx", "app/i/page.tsx", "with space.ts"] {
        repo.write(name, "v2\n");
    }
    let engine = opened(&repo).await;
    engine
        .stage(
            &repo.root,
            &strings(&["app/[id]/page.tsx", "with space.ts"]),
            OpOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        porcelain_xy(&repo, "app/[id]/page.tsx").as_deref(),
        Some("M.")
    );
    assert_eq!(porcelain_xy(&repo, "with space.ts").as_deref(), Some("M."));
    assert_eq!(
        porcelain_xy(&repo, "app/i/page.tsx").as_deref(),
        Some(".M"),
        "not glob-matched"
    );
}

// Windows' argv ceiling, exercised everywhere: > 30 000 characters of paths.
#[tokio::test]
async fn a_huge_selection_is_chunked() {
    require_git!();
    let repo = Repo::init();
    let names: Vec<String> = (0..400)
        .map(|i| format!("{}/{i:04}.ts", "d".repeat(80)))
        .collect();
    for name in &names {
        repo.write(name, "x\n");
    }
    assert!(names.iter().map(|n| n.len() + 1).sum::<usize>() > mille_git::MAX_PATHS_ARGV_CHARS);
    let engine = opened(&repo).await;
    engine
        .stage(&repo.root, &names, OpOptions::default())
        .await
        .unwrap();
    let staged = repo.git(&["diff", "--cached", "--name-only"]);
    assert_eq!(staged.lines().count(), names.len());
}

// T-ALIAS
#[tokio::test]
async fn a_repo_alias_cannot_hijack_commit() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.git(&["config", "alias.commit", "!touch pwned"]);
    repo.write("a.ts", "v2\n");
    let engine = opened(&repo).await;
    let request = CommitRequest {
        summary: "safe".into(),
        paths: strings(&["a.ts"]),
        ..Default::default()
    };
    engine.commit(&repo.root, request).await.unwrap();
    assert!(!repo.path("pwned").exists(), "the alias ran");
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]).trim(), "safe");
}

// T-ALIAS-ADD
#[tokio::test]
async fn a_repo_alias_cannot_hijack_add() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.git(&["config", "alias.add", "!touch pwned-add"]);
    repo.write("a.ts", "v2\n");
    let engine = opened(&repo).await;
    engine
        .stage(&repo.root, &strings(&["a.ts"]), OpOptions::default())
        .await
        .unwrap();
    assert!(!repo.path("pwned-add").exists(), "the alias ran");
    assert_eq!(porcelain_xy(&repo, "a.ts").as_deref(), Some("M."));
}

// T-PATH at the engine, T-OPEN-SUBDIR mutations, AC-016
#[tokio::test]
async fn mutations_stay_inside_allowed_roots() {
    require_git!();
    let repo = repo_with(&["packages/app/a.ts", "packages/other/b.ts", "top.ts"]);
    for name in ["packages/app/a.ts", "packages/other/b.ts", "top.ts"] {
        repo.write(name, "v2\n");
    }
    let app = repo.path("packages/app");
    let engine = common::quiet_engine(&[&app]);
    let identity = engine.open(&app).await.unwrap();
    assert_eq!(identity.worktree_path, repo.root);

    for bad in [
        "../../outside",
        "/etc/passwd",
        "packages/other/b.ts",
        "top.ts",
        "packages/app/../other/b.ts",
    ] {
        let err = engine
            .stage(&app, &strings(&[bad]), OpOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "EINVAL", "{bad}: {err}");
    }
    engine
        .stage(&app, &strings(&["packages/app/a.ts"]), OpOptions::default())
        .await
        .unwrap();
    assert_eq!(
        porcelain_xy(&repo, "packages/app/a.ts").as_deref(),
        Some("M.")
    );
    assert_eq!(
        porcelain_xy(&repo, "packages/other/b.ts").as_deref(),
        Some(".M")
    );

    // Status stays whole-repo even though mutations do not.
    let snapshot = engine.status(&app).await.unwrap();
    assert!(snapshot.entries.iter().any(|e| e.path == "top.ts"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_directory_cannot_carry_a_mutation_out_of_the_allow_list() {
    require_git!();
    let repo = repo_with(&["packages/app/a.ts", "packages/other/b.ts"]);
    std::os::unix::fs::symlink("../other", repo.path("packages/app/escape")).unwrap();
    let app = repo.path("packages/app");
    let engine = common::quiet_engine(&[&app]);
    engine.open(&app).await.unwrap();
    let err = engine
        .restore(
            &app,
            &strings(&["packages/app/escape/b.ts"]),
            OpOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "EINVAL");
}

#[tokio::test]
async fn mutation_error_codes_for_non_repos_bare_repos_and_missing_git() {
    require_git!();
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let bare = tmp.path().join("bare.git");
    std::fs::create_dir_all(&bare).unwrap();
    common::git(&bare, &["init", "-q", "--bare"]);
    let engine = common::quiet_engine(&[tmp.path()]);
    engine.open(&plain).await.unwrap();
    engine.open(&bare).await.unwrap();
    let one = strings(&["a.ts"]);
    assert_eq!(
        engine
            .stage(&plain, &one, OpOptions::default())
            .await
            .unwrap_err()
            .code(),
        "ENOTAREPO"
    );
    let request = CommitRequest {
        summary: "x".into(),
        paths: one.clone(),
        ..Default::default()
    };
    assert_eq!(
        engine.commit(&plain, request).await.unwrap_err().code(),
        "ENOTAREPO"
    );
    assert_eq!(
        engine
            .restore(&bare, &one, OpOptions::default())
            .await
            .unwrap_err()
            .code(),
        "EUNSUPPORTED"
    );

    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    let no_git = GitEngine::new(GitEngineOptions {
        allowed_roots: vec![repo.root.clone()],
        git_path: Some("/definitely/not/git".into()),
        ..GitEngineOptions::default()
    });
    no_git.open(&repo.root).await.unwrap();
    assert_eq!(
        no_git
            .stage(&repo.root, &one, OpOptions::default())
            .await
            .unwrap_err()
            .code(),
        "ENOGIT"
    );
}

#[tokio::test]
async fn a_held_index_lock_is_elocked() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    let engine = opened(&repo).await;
    std::fs::write(repo.path(".git/index.lock"), "").unwrap();
    let err = engine
        .stage(&repo.root, &strings(&["a.ts"]), OpOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "ELOCKED", "{err}");
    std::fs::remove_file(repo.path(".git/index.lock")).unwrap();
    engine
        .stage(&repo.root, &strings(&["a.ts"]), OpOptions::default())
        .await
        .unwrap();
}

// T-ABORT: cancelling a commit stuck in a hook terminates the whole process
// group and leaves no index.lock behind.
#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_commit_kills_the_hook_and_releases_the_index() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    install_hook(&repo, "commit-msg", "sleep 30");
    let engine = opened(&repo).await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let request = CommitRequest {
        summary: "never".into(),
        paths: strings(&["a.ts"]),
        cancel: Some(cancel),
        ..Default::default()
    };
    let err = engine.commit(&repo.root, request).await.unwrap_err();
    assert_eq!(err.code(), "ECANCELED", "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancel took {:?}",
        started.elapsed()
    );
    assert!(
        !repo.path(".git/index.lock").exists(),
        "git cleaned up its lock"
    );
    assert_eq!(
        repo.git(&["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "nothing was committed"
    );
    // The worktree is usable again.
    engine
        .unstage(&repo.root, &strings(&["a.ts"]), OpOptions::default())
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_hook_that_outlives_commit_timeout_is_etimeout() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    install_hook(&repo, "pre-commit", "sleep 30");
    let engine = GitEngine::new(GitEngineOptions {
        allowed_roots: vec![repo.root.clone()],
        commit_timeout: Duration::from_millis(500),
        ..GitEngineOptions::default()
    });
    engine.open(&repo.root).await.unwrap();
    let request = CommitRequest {
        summary: "slow".into(),
        paths: strings(&["a.ts"]),
        ..Default::default()
    };
    let started = Instant::now();
    assert_eq!(
        engine.commit(&repo.root, request).await.unwrap_err().code(),
        "ETIMEOUT"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!repo.path(".git/index.lock").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_rejecting_hook_is_ehook_with_its_message() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    install_hook(&repo, "pre-commit", "echo 'lint failed: a.ts' >&2; exit 1");
    let engine = opened(&repo).await;
    let mut published = Published::subscribe(&engine, &repo.root);
    let request = CommitRequest {
        summary: "rejected".into(),
        paths: strings(&["a.ts"]),
        ..Default::default()
    };
    let err = engine.commit(&repo.root, request).await.unwrap_err();
    assert_eq!(err.code(), "EHOOK");
    assert!(err.message().contains("lint failed: a.ts"), "{err}");
    // §23: status still refreshes after a failed hook.
    published
        .until(SLOW, |s| has(s, "a.ts", "M."))
        .await
        .expect("status refreshed");
}

// T-LOCK: a mutation never waits behind an in-flight status — it drops it,
// and the status runs again afterwards.
#[tokio::test]
async fn a_mutation_drops_an_in_flight_status_and_status_retries() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    repo.write("a.ts", "v2\n");
    let engine = std::sync::Arc::new(opened(&repo).await);
    engine.set_status_delay_for_test(Duration::from_millis(1500));
    let background = {
        let (engine, root) = (engine.clone(), repo.root.clone());
        tokio::spawn(async move { engine.refresh(&root, None).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await; // the slow status is in flight

    let started = Instant::now();
    engine
        .stage(&repo.root, &strings(&["a.ts"]), OpOptions::default())
        .await
        .unwrap();
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(1000),
        "stage waited behind the status ({took:?})"
    );

    let snapshot = background.await.unwrap().unwrap();
    assert!(
        has(&snapshot, "a.ts", "M."),
        "the retried status saw the stage: {:?}",
        snapshot.entries
    );
}

#[tokio::test]
async fn performance_advice_is_returned_and_written_only_on_request() {
    require_git!();
    let repo = repo_with(&["a.ts"]);
    let engine = opened(&repo).await;
    let advice = engine.advise_performance_config(&repo.root).await.unwrap();
    assert_eq!(
        advice.get("core.untrackedCache").map(String::as_str),
        Some("true")
    );
    if cfg!(any(target_os = "macos", windows)) {
        assert_eq!(
            advice.get("core.fsmonitor").map(String::as_str),
            Some("true")
        );
    }
    // The test repo set core.untrackedCache=false; advice alone leaves it.
    assert_eq!(
        repo.git(&["config", "--get", "core.untrackedCache"]).trim(),
        "false"
    );

    let writer = GitEngine::new(GitEngineOptions {
        allowed_roots: vec![repo.root.clone()],
        write_performance_config: true,
        ..GitEngineOptions::default()
    });
    writer.open(&repo.root).await.unwrap();
    writer.advise_performance_config(&repo.root).await.unwrap();
    assert_eq!(
        repo.git(&["config", "--get", "core.untrackedCache"]).trim(),
        "true"
    );
    // Never leave a fsmonitor daemon behind a temp repo.
    let _ = common::git_output(&repo.root, &["config", "--unset", "core.fsmonitor"]);
    let _ = Path::new(&repo.root);
}
