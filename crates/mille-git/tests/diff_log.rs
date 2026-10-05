//! PR 5: pull-based diff caps, log pages, commit files, readObject.

mod common;

use std::time::{Duration, Instant};

use common::Repo;
use mille_git::{
    diff_sides, CancellationToken, DiffCaps, DiffLineKind, DiffRequest, DiffResult, DiffSide,
    DiffSpec, GitEngine, GitError, GitErrorKind, LogRequest, OpOptions, ReadObjectRequest,
    StatusLetter,
};

fn diff_req(path: &str, spec: DiffSpec) -> DiffRequest {
    DiffRequest {
        path: path.into(),
        spec,
        commit: None,
        max_bytes: None,
        max_lines: None,
        cancel: None,
    }
}

fn lines_of(result: &DiffResult, kind: DiffLineKind) -> Vec<String> {
    result
        .hunks
        .iter()
        .flatten()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == kind)
        .map(|l| l.text.clone())
        .collect()
}

async fn opened(repo: &Repo) -> GitEngine {
    let engine = common::quiet_engine(&[&repo.root]);
    engine.open(&repo.root).await.unwrap();
    engine
}

/// ~3 MiB of distinct text lines.
fn three_mib() -> String {
    (0..60_000)
        .map(|i| format!("line {i:06} of a generated file that is far too large to render\n"))
        .collect()
}

// T-DIFF: hunks for a small file on each spec; tookMs always present.
#[tokio::test]
async fn small_files_diff_on_every_spec() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.ts", "one\ntwo\nthree\n");
    let base = repo.commit_all("base");
    repo.write("a.ts", "one\nTWO\nthree\n");
    let second = repo.commit_all("second");
    repo.write("a.ts", "one\nTWO\nthree\nstaged\n");
    repo.git(&["add", "a.ts"]);
    repo.write("a.ts", "one\nTWO\nthree\nstaged\nworktree\n");
    let engine = opened(&repo).await;

    let worktree = engine
        .diff(&repo.root, diff_req("a.ts", DiffSpec::Worktree))
        .await
        .unwrap();
    assert_eq!(
        (
            lines_of(&worktree, DiffLineKind::Add),
            lines_of(&worktree, DiffLineKind::Del)
        ),
        (vec!["worktree".to_owned()], vec![])
    );
    let index = engine
        .diff(&repo.root, diff_req("a.ts", DiffSpec::Index))
        .await
        .unwrap();
    assert_eq!(lines_of(&index, DiffLineKind::Add), ["staged"]);
    let mut commit = diff_req("a.ts", DiffSpec::Commit);
    commit.commit = Some(second.clone());
    let commit = engine.diff(&repo.root, commit).await.unwrap();
    assert_eq!(
        (
            lines_of(&commit, DiffLineKind::Del),
            lines_of(&commit, DiffLineKind::Add)
        ),
        (vec!["two".to_owned()], vec!["TWO".to_owned()])
    );
    let hunk = &commit.hunks.as_ref().unwrap()[0];
    assert_eq!(
        (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines
        ),
        (1, 3, 1, 3)
    );

    // The root commit diffs against nothing.
    let mut root = diff_req("a.ts", DiffSpec::Commit);
    root.commit = Some(base);
    let root = engine.diff(&repo.root, root).await.unwrap();
    assert_eq!(lines_of(&root, DiffLineKind::Add), ["one", "two", "three"]);

    for result in [&worktree, &index, &commit, &root] {
        assert!(!result.too_large && result.binary.is_none());
        let json = serde_json::to_value(result).unwrap();
        assert!(json["tookMs"].is_u64(), "tookMs on every result");
    }
}

#[tokio::test]
async fn untracked_and_deleted_files_diff_against_nothing() {
    require_git!();
    let repo = Repo::init();
    repo.write("gone.ts", "bye\n");
    repo.commit_all("base");
    std::fs::remove_file(repo.path("gone.ts")).unwrap();
    repo.write("new.ts", "hello\n");
    let engine = opened(&repo).await;
    let deleted = engine
        .diff(&repo.root, diff_req("gone.ts", DiffSpec::Worktree))
        .await
        .unwrap();
    assert_eq!(lines_of(&deleted, DiffLineKind::Del), ["bye"]);
    let added = engine
        .diff(&repo.root, diff_req("new.ts", DiffSpec::Worktree))
        .await
        .unwrap();
    assert_eq!(lines_of(&added, DiffLineKind::Add), ["hello"]);
}

// T-DIFF tooLarge fixture, AC-005: a 3 MiB text file is refused by size, on
// every spec, well under two seconds, with no hunks.
#[tokio::test]
async fn a_three_mib_file_is_too_large_without_a_diff() {
    require_git!();
    let repo = Repo::init();
    let big = three_mib();
    assert!(big.len() > 3 * 1024 * 1024 - 400_000 && big.len() < 4 * 1024 * 1024);
    repo.write("big.txt", &big);
    let sha = repo.commit_all("big");
    repo.write("big.txt", format!("{big}one more line\n"));
    repo.git(&["add", "big.txt"]);
    repo.write("big.txt", format!("{big}one more line\nand another\n"));
    let engine = opened(&repo).await;

    let started = Instant::now();
    let mut commit = diff_req("big.txt", DiffSpec::Commit);
    commit.commit = Some(sha);
    for request in [
        diff_req("big.txt", DiffSpec::Worktree),
        diff_req("big.txt", DiffSpec::Index),
        commit,
    ] {
        let result = engine.diff(&repo.root, request).await.unwrap();
        assert!(result.too_large);
        assert!(result.hunks.is_none());
        assert!(result.bytes.unwrap() >= 1_048_576);
    }
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "three refusals took {took:?}"
    );
}

// T-HEADER-SIZE: a side whose header says 64 MiB is refused without its
// reader ever being called.
#[test]
fn a_huge_header_refuses_before_the_blob_is_read() {
    struct Huge;
    impl DiffSide for Huge {
        fn size(&self) -> mille_git::Result<Option<u64>> {
            Ok(Some(64 * 1024 * 1024))
        }
        fn load(&self) -> mille_git::Result<Option<Vec<u8>>> {
            panic!("the blob read path was called for a 64 MiB header");
        }
    }
    struct Small;
    impl DiffSide for Small {
        fn size(&self) -> mille_git::Result<Option<u64>> {
            Ok(Some(3))
        }
        fn load(&self) -> mille_git::Result<Option<Vec<u8>>> {
            Err(GitError::new(GitErrorKind::Unknown, "must not load either"))
        }
    }
    let caps = DiffCaps {
        auto_render_max_bytes: 262_144,
        auto_render_max_lines: 5_000,
        hard_refuse_bytes: 1_048_576,
    };
    for (old, new) in [
        (&Huge as &dyn DiffSide, &Small as &dyn DiffSide),
        (&Small, &Huge),
    ] {
        let result = diff_sides("huge.bin", old, new, caps).unwrap();
        assert!(result.too_large);
        assert_eq!(result.bytes, Some(64 * 1024 * 1024));
        assert!(result.hunks.is_none());
    }
}

#[tokio::test]
async fn binary_files_and_auto_render_caps() {
    require_git!();
    let repo = Repo::init();
    repo.write("img.bin", b"\x89PNG\0\0\0binary");
    let fifty: String = (0..50).map(|i| format!("{i}\n")).collect();
    repo.write("many.txt", &fifty);
    repo.commit_all("base");
    repo.write("img.bin", b"\x89PNG\0\0\0changed");
    repo.write("many.txt", fifty.replace('\n', " edited\n"));
    let engine = opened(&repo).await;

    let binary = engine
        .diff(&repo.root, diff_req("img.bin", DiffSpec::Worktree))
        .await
        .unwrap();
    assert_eq!((binary.binary, binary.too_large), (Some(true), false));
    assert_eq!(binary.hunks.as_deref(), Some(&[][..]));

    let mut capped = diff_req("many.txt", DiffSpec::Worktree);
    capped.max_lines = Some(10);
    let capped = engine.diff(&repo.root, capped).await.unwrap();
    assert!(capped.too_large && capped.hunks.is_none());
    assert_eq!(capped.lines, Some(100));
}

#[tokio::test]
async fn diff_and_contents_reads_validate_their_inputs() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.ts", "x\n");
    repo.commit_all("base");
    let engine = opened(&repo).await;
    let code = |r: Result<DiffResult, GitError>| r.unwrap_err().code();
    assert_eq!(
        code(
            engine
                .diff(&repo.root, diff_req("../x", DiffSpec::Worktree))
                .await
        ),
        "EINVAL"
    );
    assert_eq!(
        code(
            engine
                .diff(&repo.root, diff_req("a.ts", DiffSpec::Commit))
                .await
        ),
        "EINVAL",
        "commit required"
    );
    let mut unsafe_rev = diff_req("a.ts", DiffSpec::Commit);
    unsafe_rev.commit = Some("--output=/tmp/x".into());
    assert_eq!(code(engine.diff(&repo.root, unsafe_rev).await), "EINVAL");
    let mut unknown = diff_req("a.ts", DiffSpec::Commit);
    unknown.commit = Some("no-such-branch".into());
    assert_eq!(code(engine.diff(&repo.root, unknown).await), "EINVAL");

    let read = |path: &str, revision: &str| ReadObjectRequest {
        path: path.into(),
        revision: revision.into(),
        cancel: None,
    };
    assert_eq!(
        engine
            .read_object(&repo.root, read("a.ts", "-o"))
            .await
            .unwrap_err()
            .code(),
        "EINVAL"
    );
    assert_eq!(
        engine
            .read_object(&repo.root, read("a.ts", "HEAD:a.ts"))
            .await
            .unwrap_err()
            .code(),
        "EINVAL"
    );
    assert_eq!(
        engine
            .read_object(&repo.root, read("/etc/passwd", "HEAD"))
            .await
            .unwrap_err()
            .code(),
        "EINVAL"
    );
    assert_eq!(
        engine
            .read_object(&repo.root, read("missing.ts", "HEAD"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        engine
            .read_object(&repo.root, read("a.ts", "no-such-ref"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        engine
            .read_object(&repo.root, read("a.ts", "WORKTREE"))
            .await
            .unwrap(),
        None,
        "not a revision"
    );
}

// §14.2 / §20.1: a symlink inside the workspace cannot read outside it.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_checkout_is_refused_for_working_tree_reads() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.ts", "x\n");
    repo.commit_all("base");
    let secret = repo.base().join("secret.txt");
    std::fs::write(&secret, "top secret\n").unwrap();
    std::os::unix::fs::symlink(&secret, repo.path("leak.txt")).unwrap();
    let engine = opened(&repo).await;
    let err = engine
        .read_worktree_file(&repo.root, "leak.txt", OpOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "EINVAL");
    let err = engine
        .diff(&repo.root, diff_req("leak.txt", DiffSpec::Worktree))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "EINVAL");
}

#[tokio::test]
async fn working_tree_reads_return_bytes_none_or_refuse() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.ts", "hello\n");
    repo.write("big.txt", three_mib());
    repo.commit_all("base");
    let engine = opened(&repo).await;
    let read = |p: &'static str| engine.read_worktree_file(&repo.root, p, OpOptions::default());
    assert_eq!(
        read("a.ts").await.unwrap().as_deref(),
        Some(&b"hello\n"[..])
    );
    assert_eq!(read("missing.ts").await.unwrap(), None);
    assert_eq!(
        read("big.txt").await.unwrap(),
        None,
        "at or above hardRefuseBytes"
    );
    let big = ReadObjectRequest {
        path: "big.txt".into(),
        revision: "HEAD".into(),
        cancel: None,
    };
    assert_eq!(
        engine.read_object(&repo.root, big).await.unwrap(),
        None,
        "the header refuses it too"
    );
}

// T-LOG, AC-007: getHistory order and getContents via readObject.
#[tokio::test]
async fn log_pages_and_get_contents() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.ts", "v1\n");
    let first = repo.commit_all("first: add a");
    std::thread::sleep(Duration::from_millis(1100)); // distinct commit times
    repo.write("b.ts", "b\n");
    let second = repo.commit_all("second: add b");
    std::thread::sleep(Duration::from_millis(1100));
    repo.write("a.ts", "v2\n");
    let third = repo.commit_all("third: edit a\n\nwith a body");
    let engine = opened(&repo).await;

    let page = engine.log(&repo.root, LogRequest::default()).await.unwrap();
    let ids: Vec<_> = page.commits.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [third.as_str(), second.as_str(), first.as_str()]);
    assert!(page.exhausted);
    let top = &page.commits[0];
    assert_eq!(
        top.message.as_deref(),
        Some("third: edit a"),
        "subject only"
    );
    assert_eq!(top.author.as_deref(), Some("Mille Test"));
    assert!(third.starts_with(&top.short_id) && top.short_id.len() >= 7);
    assert_eq!(top.parents, vec![second.clone()]);
    assert!(page.commits[2].parents.is_empty());
    assert!(top.timestamp_ms > page.commits[2].timestamp_ms);
    let cli_at: i64 = repo
        .git(&["log", "-1", "--format=%at"])
        .trim()
        .parse()
        .unwrap();
    assert_eq!(top.timestamp_ms, cli_at * 1000);

    // Paging.
    let first_page = engine
        .log(
            &repo.root,
            LogRequest {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first_page.commits.len(), 2);
    assert!(!first_page.exhausted);
    let rest = engine
        .log(
            &repo.root,
            LogRequest {
                limit: Some(2),
                skip: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        rest.commits
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        [first.as_str()]
    );
    assert!(rest.exhausted);

    // A path filter (the CLI path) — FileHistoryClient.getHistory.
    let history = engine
        .log(
            &repo.root,
            LogRequest {
                path: Some("a.ts".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        history
            .commits
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        [third.as_str(), first.as_str()]
    );
    assert_eq!(history.commits[0].parents, vec![second.clone()]);

    // A revision.
    let from = engine
        .log(
            &repo.root,
            LogRequest {
                revision: Some("HEAD~1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(from.commits[0].id, second);
    let bad = engine
        .log(
            &repo.root,
            LogRequest {
                revision: Some("--all".into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(bad.unwrap_err().code(), "EINVAL");
    let escape = engine
        .log(
            &repo.root,
            LogRequest {
                path: Some("../x".into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(escape.unwrap_err().code(), "EINVAL");

    // getContents at the first sha.
    let read = ReadObjectRequest {
        path: "a.ts".into(),
        revision: first.clone(),
        cancel: None,
    };
    assert_eq!(
        engine
            .read_object(&repo.root, read)
            .await
            .unwrap()
            .as_deref(),
        Some(&b"v1\n"[..])
    );
}

#[tokio::test]
async fn an_unborn_branch_has_an_empty_exhausted_log() {
    require_git!();
    let repo = Repo::init();
    let engine = opened(&repo).await;
    let page = engine.log(&repo.root, LogRequest::default()).await.unwrap();
    assert!(page.commits.is_empty() && page.exhausted);
    let page = engine
        .log(
            &repo.root,
            LogRequest {
                path: Some("a.ts".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(page.commits.is_empty() && page.exhausted);
}

// §14.3
#[tokio::test]
async fn commit_files_lists_what_a_commit_changed() {
    require_git!();
    let repo = Repo::init();
    repo.write("mod.ts", "v1\n");
    repo.write("del.ts", "bye\n");
    repo.write(
        "dir/rename-me.ts",
        "a file long enough to be recognised as the same file after its rename\n",
    );
    let root = repo.commit_all("base");
    repo.write("mod.ts", "v2\n");
    std::fs::remove_file(repo.path("del.ts")).unwrap();
    repo.write("new/dir/added.ts", "new\n");
    repo.git(&["mv", "dir/rename-me.ts", "dir/renamed.ts"]);
    let sha = repo.commit_all("changes");
    let engine = opened(&repo).await;

    let files = engine
        .commit_files(&repo.root, &sha, OpOptions::default())
        .await
        .unwrap();
    let summary: Vec<(String, Option<String>, StatusLetter)> = files
        .iter()
        .map(|f| (f.path.clone(), f.orig_path.clone(), f.status))
        .collect();
    assert_eq!(
        summary,
        [
            ("del.ts".to_owned(), None, StatusLetter::Deleted),
            (
                "dir/renamed.ts".to_owned(),
                Some("dir/rename-me.ts".to_owned()),
                StatusLetter::Renamed
            ),
            ("mod.ts".to_owned(), None, StatusLetter::Modified),
            ("new/dir/added.ts".to_owned(), None, StatusLetter::Added),
        ]
    );
    let first = engine
        .commit_files(&repo.root, &root, OpOptions::default())
        .await
        .unwrap();
    assert!(first.iter().all(|f| f.status == StatusLetter::Added));
    assert_eq!(first.len(), 3, "directories are not listed");
    let bad = engine
        .commit_files(&repo.root, "-o", OpOptions::default())
        .await;
    assert_eq!(bad.unwrap_err().code(), "EINVAL");
}

#[tokio::test]
async fn a_cancelled_read_is_ecanceled() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.ts", "x\n");
    repo.commit_all("base");
    let engine = opened(&repo).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut request = diff_req("a.ts", DiffSpec::Worktree);
    request.cancel = Some(cancel.clone());
    assert_eq!(
        engine.diff(&repo.root, request).await.unwrap_err().code(),
        "ECANCELED"
    );
    let log = engine
        .log(
            &repo.root,
            LogRequest {
                cancel: Some(cancel.clone()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(log.unwrap_err().code(), "ECANCELED");
    let read = engine
        .read_worktree_file(
            &repo.root,
            "a.ts",
            OpOptions {
                cancel: Some(cancel),
            },
        )
        .await;
    assert_eq!(read.unwrap_err().code(), "ECANCELED");
}

#[tokio::test]
async fn reads_on_bare_and_plain_directories() {
    require_git!();
    let repo = Repo::init();
    repo.write("a.ts", "x\n");
    let sha = repo.commit_all("base");
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("bare.git");
    common::git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            repo.root.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let engine = common::quiet_engine(&[tmp.path()]);
    engine.open(&bare).await.unwrap();
    engine.open(&plain).await.unwrap();

    let page = engine.log(&bare, LogRequest::default()).await.unwrap();
    assert_eq!(page.commits[0].id, sha);
    let read = ReadObjectRequest {
        path: "a.ts".into(),
        revision: "HEAD".into(),
        cancel: None,
    };
    assert_eq!(
        engine.read_object(&bare, read).await.unwrap().as_deref(),
        Some(&b"x\n"[..])
    );
    assert_eq!(
        engine
            .diff(&bare, diff_req("a.ts", DiffSpec::Worktree))
            .await
            .unwrap_err()
            .code(),
        "EUNSUPPORTED"
    );
    let worktree_read = engine
        .read_worktree_file(&bare, "a.ts", OpOptions::default())
        .await;
    assert_eq!(worktree_read.unwrap_err().code(), "EUNSUPPORTED");
    assert_eq!(
        engine
            .log(&plain, LogRequest::default())
            .await
            .unwrap_err()
            .code(),
        "ENOTAREPO"
    );
}

// §12.1: a mutation waits for in-flight reads and reads resume after it.
#[tokio::test]
async fn reads_and_a_mutation_interleave_safely() {
    require_git!();
    let repo = Repo::init();
    for i in 0..20 {
        repo.write(&format!("f{i}.ts"), "v1\n");
    }
    repo.commit_all("base");
    for i in 0..20 {
        repo.write(&format!("f{i}.ts"), "v2\n");
    }
    let engine = std::sync::Arc::new(opened(&repo).await);
    let mut tasks = Vec::new();
    for i in 0..20 {
        let (engine, root) = (engine.clone(), repo.root.clone());
        tasks.push(tokio::spawn(async move {
            engine
                .diff(&root, diff_req(&format!("f{i}.ts"), DiffSpec::Worktree))
                .await
        }));
    }
    let paths: Vec<String> = (0..20).map(|i| format!("f{i}.ts")).collect();
    engine
        .stage(&repo.root, &paths, OpOptions::default())
        .await
        .unwrap();
    for task in tasks {
        let result = task.await.unwrap().unwrap();
        // Each diff saw the file either before or after staging, never a torn index.
        assert!(result.hunks.is_some());
    }
    assert_eq!(
        repo.git(&["diff", "--cached", "--name-only"])
            .lines()
            .count(),
        20
    );
}
