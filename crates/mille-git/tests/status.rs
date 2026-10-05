//! PR 2: XY status snapshots via gix, the CLI fallback, §10.6 merges.

mod common;

use std::sync::{Arc, Mutex};

use common::Repo;
use mille_git::{
    parse_porcelain_v2, sort_entries, GitEngine, GitEngineOptions, GitRepoKind, StatusBackend,
    StatusEntry, StatusSnapshot, WarningCode,
};

/// What `git status --porcelain=v2 -z --untracked-files=all` says, parsed and
/// sorted the way snapshots are.
fn porcelain(repo: &Repo) -> Vec<StatusEntry> {
    let out = common::git_output(
        &repo.root,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    );
    assert!(out.status.success());
    let mut entries = parse_porcelain_v2(&out.stdout, &mut |w| panic!("{w}"));
    sort_entries(&mut entries);
    entries
}

fn rows(snapshot: &StatusSnapshot) -> Vec<(String, String, Option<String>)> {
    snapshot
        .entries
        .iter()
        .map(|e| (e.path.clone(), e.xy.as_string(), e.orig_path.clone()))
        .collect()
}

fn find<'a>(snapshot: &'a StatusSnapshot, path: &str) -> Option<&'a StatusEntry> {
    snapshot.entries.iter().find(|e| e.path == path)
}

/// A repo exercising every ordinary XY shape.
fn mixed_repo() -> Repo {
    let repo = Repo::init();
    for name in ["a.ts", "b.ts", "c.ts", "d.ts", "e.ts", "mv-src.ts"] {
        repo.write(name, format!("{name} v1\nshared line\n"));
    }
    repo.write(".gitignore", "*.log\n");
    repo.commit_all("base");

    repo.write("a.ts", "a.ts v2\n"); // .M
    repo.write("b.ts", "b.ts v2\n");
    repo.git(&["add", "b.ts"]); // M.
    repo.write("c.ts", "c.ts v2\n");
    repo.git(&["add", "c.ts"]);
    repo.write("c.ts", "c.ts v3\n"); // MM
    repo.write("new.ts", "new\n");
    repo.git(&["add", "new.ts"]); // A.
    std::fs::remove_file(repo.path("d.ts")).unwrap(); // .D
    repo.git(&["rm", "-q", "e.ts"]); // D.
    repo.git(&["mv", "mv-src.ts", "renamed.ts"]); // R.
    repo.write("intent.ts", "intent\n");
    repo.git(&["add", "-N", "intent.ts"]); // .A
    repo.write("u.ts", "untracked\n");
    repo.write("newdir/sub/deep.ts", "nested untracked\n");
    repo.write("x.log", "ignored\n");
    repo
}

// T-STATUS, T-UNTRACKED-NESTED, AC-018
#[tokio::test]
async fn gix_status_matches_porcelain_for_every_shape() {
    require_git!();
    let repo = mixed_repo();
    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    let snapshot = engine.status(&repo.root).await.unwrap();

    assert_eq!(snapshot.status_backend, StatusBackend::Gix);
    assert_eq!(
        snapshot.entries,
        porcelain(&repo),
        "gix rows == git status --porcelain=v2 rows"
    );
    assert!(!snapshot.path_limited);
    assert_eq!(snapshot.truncated, None);
    assert_eq!(snapshot.worktree_path, repo.root);

    let xy = |p: &str| find(&snapshot, p).map(|e| e.xy.as_string());
    assert_eq!(xy("a.ts").as_deref(), Some(".M"));
    assert_eq!(xy("b.ts").as_deref(), Some("M."));
    assert_eq!(xy("c.ts").as_deref(), Some("MM"));
    assert_eq!(xy("new.ts").as_deref(), Some("A."));
    assert_eq!(xy("d.ts").as_deref(), Some(".D"));
    assert_eq!(xy("e.ts").as_deref(), Some("D."));
    assert_eq!(xy("intent.ts").as_deref(), Some(".A"));
    let renamed = find(&snapshot, "renamed.ts").unwrap();
    assert_eq!(
        (
            renamed.xy.as_string().as_str(),
            renamed.orig_path.as_deref()
        ),
        ("R.", Some("mv-src.ts"))
    );
    assert!(find(&snapshot, "mv-src.ts").is_none());
    // Untracked files are listed one by one, never as a collapsed `newdir/`.
    assert!(find(&snapshot, "newdir/sub/deep.ts").is_some_and(|e| e.untracked && !e.staged));
    assert!(find(&snapshot, "newdir/").is_none() && find(&snapshot, "newdir").is_none());
    assert!(
        find(&snapshot, "x.log").is_none(),
        "ignored files are never emitted"
    );
    assert!(find(&snapshot, "b.ts").unwrap().staged && !find(&snapshot, "a.ts").unwrap().staged);
}

// §21.3: the CLI backend produces the same snapshot.
#[tokio::test]
async fn the_cli_backend_agrees_with_gix() {
    require_git!();
    let repo = mixed_repo();
    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    let gix = engine.status(&repo.root).await.unwrap();
    engine
        .set_status_backend(&repo.root, StatusBackend::Cli)
        .unwrap();
    let cli = engine.status(&repo.root).await.unwrap();
    assert_eq!(cli.status_backend, StatusBackend::Cli);
    assert_eq!(rows(&cli), rows(&gix));
    assert!(cli.generation > gix.generation);
}

#[tokio::test]
async fn a_merge_conflict_is_one_conflicted_row() {
    require_git!();
    let repo = Repo::init();
    repo.write("f.ts", "base\n");
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("f.ts", "side\n");
    repo.commit_all("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("f.ts", "main\n");
    repo.commit_all("main");
    let merge = common::git_output(&repo.root, &["merge", "-q", "side"]);
    assert!(!merge.status.success(), "the merge must conflict");

    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    let snapshot = engine.status(&repo.root).await.unwrap();
    assert_eq!(snapshot.entries, porcelain(&repo));
    let row = find(&snapshot, "f.ts").unwrap();
    assert!(row.conflicted && !row.staged);
    assert_eq!(row.xy.as_string(), "UU");
    assert_eq!(
        snapshot.entries.len(),
        1,
        "no phantom deletion from the tree-index side"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_type_change_reads_as_modified() {
    require_git!();
    let repo = Repo::init();
    repo.write("link-me.txt", "file\n");
    repo.write("target.txt", "target\n");
    repo.commit_all("base");
    std::fs::remove_file(repo.path("link-me.txt")).unwrap();
    std::os::unix::fs::symlink("target.txt", repo.path("link-me.txt")).unwrap();

    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    let snapshot = engine.status(&repo.root).await.unwrap();
    assert_eq!(snapshot.entries, porcelain(&repo));
    assert_eq!(
        find(&snapshot, "link-me.txt").unwrap().xy.as_string(),
        ".M",
        "T maps to M"
    );
}

// T-HINT-MERGE through the engine, with real gix pathspecs.
#[tokio::test]
async fn a_path_limited_refresh_merges_into_the_previous_snapshot() {
    require_git!();
    let repo = Repo::init();
    for name in ["a.ts", "b.ts", "c.ts", "d.ts"] {
        repo.write(name, "v1\n");
    }
    repo.commit_all("base");
    for name in ["a.ts", "b.ts", "c.ts"] {
        repo.write(name, "v2\n");
    }
    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    assert_eq!(engine.status(&repo.root).await.unwrap().entries.len(), 3);

    // Hint a.ts while it is still dirty: three rows survive.
    let same = engine
        .refresh(&repo.root, Some(vec!["a.ts".into()]))
        .await
        .unwrap();
    assert!(same.path_limited);
    assert_eq!(same.entries.len(), 3);

    // Revert a.ts, dirty d.ts, hint only a.ts: a.ts leaves, d.ts waits for a
    // full run (it was never asked about).
    repo.write("a.ts", "v1\n");
    repo.write("d.ts", "v2\n");
    let merged = engine
        .refresh(&repo.root, Some(vec!["a.ts".into()]))
        .await
        .unwrap();
    let paths: Vec<_> = merged.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["b.ts", "c.ts"]);

    let full = engine.status(&repo.root).await.unwrap();
    assert!(!full.path_limited);
    assert_eq!(
        full.entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        ["b.ts", "c.ts", "d.ts"]
    );
}

// T-MERGE-RENAME through the engine. A rename is only *reported* when both
// names are inside the pathspec: under `-- new.ts` alone, git and gix both
// see an addition (`A`), because the deletion side is outside the pathspec
// and cannot be paired. So a `to`-only hint leaves the old row until the
// next full run (which a staged rename always triggers via the index watch);
// a `{from, to}` hint drops it at once. README "Deviations" item 12.
#[tokio::test]
async fn a_rename_drops_its_old_row_once_the_run_can_see_both_names() {
    require_git!();
    let repo = Repo::init();
    repo.write("old.ts", "content that survives the rename\n");
    repo.commit_all("base");
    repo.write("old.ts", "content that survives the rename\nplus an edit\n");
    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    assert!(find(&engine.status(&repo.root).await.unwrap(), "old.ts").is_some());
    repo.git(&["mv", "old.ts", "new.ts"]);

    let to_only = engine
        .refresh(&repo.root, Some(vec!["new.ts".into()]))
        .await
        .unwrap();
    let new = find(&to_only, "new.ts").expect("the new name is reported");
    assert_eq!(
        (new.xy.as_string().as_str(), new.orig_path.as_deref()),
        ("AM", None),
        "git agrees: no rename pair"
    );
    assert!(
        find(&to_only, "old.ts").is_some(),
        "nothing reported old.ts, so the merge keeps it"
    );

    let both = engine
        .refresh(&repo.root, Some(vec!["old.ts".into(), "new.ts".into()]))
        .await
        .unwrap();
    let new = find(&both, "new.ts").unwrap();
    assert_eq!(
        (new.xy.as_string().as_str(), new.orig_path.as_deref()),
        ("RM", Some("old.ts"))
    );
    assert!(
        find(&both, "old.ts").is_none(),
        "the renamed-away row is dropped"
    );

    let full = engine.status(&repo.root).await.unwrap();
    assert_eq!(rows(&full), rows(&both));
}

// Hints are literal pathspecs: `[id].ts` must not glob-match `d.ts`.
#[tokio::test]
async fn hint_paths_are_literal_not_globs() {
    require_git!();
    let repo = Repo::init();
    repo.write("[id].ts", "v1\n");
    repo.write("d.ts", "v1\n");
    repo.commit_all("base");
    let engine = common::engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    assert!(engine.status(&repo.root).await.unwrap().entries.is_empty());

    repo.write("[id].ts", "v2\n");
    repo.write("d.ts", "v2\n");
    let merged = engine
        .refresh(&repo.root, Some(vec!["[id].ts".into()]))
        .await
        .unwrap();
    let paths: Vec<_> = merged.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["[id].ts"], "d.ts was not asked about");
}

#[tokio::test]
async fn the_row_cap_reports_what_it_dropped() {
    require_git!();
    let repo = Repo::init();
    for i in 0..5 {
        repo.write(&format!("u{i}.ts"), "x\n");
    }
    let engine = GitEngine::new(GitEngineOptions {
        allowed_roots: vec![repo.root.clone()],
        status_row_cap: 3,
        ..GitEngineOptions::default()
    });
    engine.open(&repo.root).await.unwrap();
    let snapshot = engine.status(&repo.root).await.unwrap();
    assert_eq!(snapshot.entries.len(), 3);
    assert_eq!(snapshot.truncated, Some(2));
    let json = serde_json::to_value(&*snapshot).unwrap();
    assert_eq!(json["truncated"], 2);
    assert_eq!(json["statusBackend"], "gix");
    assert!(json["tookMs"].is_u64());
}

// T-NONE (status half), §10.5
#[tokio::test]
async fn not_a_repo_is_an_empty_snapshot_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = common::engine(&[tmp.path()]);
    engine.open(tmp.path()).await.unwrap();
    let first = engine.status(tmp.path()).await.unwrap();
    assert_eq!(first.identity.kind, GitRepoKind::None);
    assert!(first.entries.is_empty());
    let second = engine.status(tmp.path()).await.unwrap();
    assert!(second.generation > first.generation);
    let json = serde_json::to_value(&*second).unwrap();
    assert!(
        json.get("truncated").is_none(),
        "truncated is omitted when zero"
    );
}

#[tokio::test]
async fn a_bare_repository_has_an_empty_status() {
    require_git!();
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("b.git");
    std::fs::create_dir_all(&bare).unwrap();
    common::git(&bare, &["init", "-q", "--bare"]);
    let engine = common::engine(&[tmp.path()]);
    engine.open(&bare).await.unwrap();
    let snapshot = engine.status(&bare).await.unwrap();
    assert_eq!(snapshot.identity.kind, GitRepoKind::Bare);
    assert!(snapshot.entries.is_empty());
}

// §22.2: a missing git binary warns once per engine, and status stays empty
// rather than throwing.
#[tokio::test]
async fn a_missing_git_binary_warns_once() {
    require_git!();
    let repo = Repo::init();
    repo.write("u.ts", "x\n");
    let engine = GitEngine::new(GitEngineOptions {
        allowed_roots: vec![repo.root.clone()],
        git_path: Some("/definitely/not/git".into()),
        ..GitEngineOptions::default()
    });
    assert!(engine.git_path().is_none());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    engine.subscribe_warnings(Arc::new(move |w| sink.lock().unwrap().push(w.code)));

    engine.open(&repo.root).await.unwrap();
    // gix still reads without a git binary.
    assert_eq!(engine.status(&repo.root).await.unwrap().entries.len(), 1);

    engine
        .set_status_backend(&repo.root, StatusBackend::Cli)
        .unwrap();
    for _ in 0..2 {
        let snapshot = engine.status(&repo.root).await.unwrap();
        assert!(snapshot.entries.is_empty());
        assert_eq!(snapshot.status_backend, StatusBackend::Cli);
    }
    let codes = seen.lock().unwrap().clone();
    assert_eq!(
        codes.iter().filter(|c| **c == WarningCode::NoGit).count(),
        1,
        "{codes:?}"
    );
}
