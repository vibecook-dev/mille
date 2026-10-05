//! Temp-repo helpers. Every repository here is created with the real `git`
//! CLI inside a `tempfile` directory; nothing touches a real checkout.
//! Tests skip cleanly when `git --version` fails (SPEC mille-git §28).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mille_git::{GitEngine, GitEngineOptions};

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip the calling test when git is missing.
#[macro_export]
macro_rules! require_git {
    () => {
        if !common::git_available() {
            eprintln!("skipping: git is not on PATH");
            return;
        }
    };
}

/// Run git in `dir` with a scrubbed environment: a test harness started from
/// inside a git hook must not leak `GIT_DIR` into these temp repos.
pub fn git_output(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git")
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Configure a fresh repository so the user's global config (signing,
/// hooksPath, autocrlf, fsmonitor) cannot change what a test observes.
pub fn configure(dir: &Path) {
    for (key, value) in [
        ("user.name", "Mille Test"),
        ("user.email", "mille@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("core.autocrlf", "false"),
        ("core.fsmonitor", "false"),
        ("core.untrackedCache", "false"),
        ("status.showUntrackedFiles", "all"),
    ] {
        git(dir, &["config", key, value]);
    }
    let hooks = dir.join(".git").join("hooks");
    if dir.join(".git").is_dir() {
        std::fs::create_dir_all(&hooks).unwrap();
        git(dir, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    }
}

pub struct Repo {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
}

impl Repo {
    /// `git init -b main` in `<tmp>/repo`.
    pub fn init() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        configure(&root);
        Self { tmp, root }
    }

    /// The temp dir holding the repo — a convenient allow-list root that
    /// also covers sibling worktrees.
    pub fn base(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        let mut out = self.root.clone();
        for part in rel.split('/') {
            out.push(part);
        }
        out
    }

    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap()
    }

    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }
}

pub fn engine(roots: &[&Path]) -> GitEngine {
    GitEngine::new(GitEngineOptions {
        allowed_roots: roots.iter().map(|r| r.to_path_buf()).collect(),
        ..GitEngineOptions::default()
    })
}

pub fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

/// Collect every snapshot a worktree publishes.
pub struct Published {
    rx: tokio::sync::mpsc::UnboundedReceiver<std::sync::Arc<mille_git::StatusSnapshot>>,
}

impl Published {
    pub fn subscribe(engine: &GitEngine, worktree: &Path) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        engine
            .subscribe_status(worktree, std::sync::Arc::new(move |s| drop(tx.send(s))))
            .expect("subscribe");
        Self { rx }
    }

    /// Wait (up to `timeout`) for a snapshot satisfying `pred`.
    pub async fn until(
        &mut self,
        timeout: std::time::Duration,
        pred: impl Fn(&mille_git::StatusSnapshot) -> bool,
    ) -> Option<std::sync::Arc<mille_git::StatusSnapshot>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(snapshot)) if pred(&snapshot) => return Some(snapshot),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }

    /// Did anything publish within `window`?
    pub async fn any_within(&mut self, window: std::time::Duration) -> bool {
        tokio::time::timeout(window, self.rx.recv()).await.is_ok()
    }
}

pub fn has(snapshot: &mille_git::StatusSnapshot, path: &str, xy: &str) -> bool {
    snapshot
        .entries
        .iter()
        .any(|e| e.path == path && e.xy.as_string() == xy)
}

pub fn quiet_engine(roots: &[&Path]) -> GitEngine {
    GitEngine::new(GitEngineOptions {
        allowed_roots: roots.iter().map(|r| r.to_path_buf()).collect(),
        // Keep the idle full rescan out of tests that count runs.
        idle_rescan: std::time::Duration::from_secs(3600),
        ..GitEngineOptions::default()
    })
}
