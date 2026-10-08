// Phase 2.5: ripgrep `ignore` crate integration.
//
// We use the `gitignore::Gitignore` matcher directly rather than
// `ignore::WalkBuilder` — SPEC §4.3 flags WalkBuilder's API as awkward and
// hides controls we need. IgnoreMatcher is a thin stack: one Gitignore per
// `.gitignore` / `.ignore` / `.rgignore` file we've seen, checked in order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use smallvec::SmallVec;

use crate::error::{ErrorCode, FxError};

/// Filenames we treat as gitignore-format ignore files.
pub const IGNORE_FILE_NAMES: &[&str] = &[".gitignore", ".ignore", ".rgignore"];

/// A single rule source, remembered verbatim so the matcher can be cloned /
/// rebuilt (e.g. to share across jwalk worker threads in B3).
#[derive(Clone, Debug)]
enum IgnoreSource {
    File(PathBuf),
    /// A file's text captured by a bounded read (`read_boundary`), so
    /// `clone_rules` replays it without touching the disk again.
    Contents {
        file: PathBuf,
        contents: String,
    },
    String {
        root: PathBuf,
        patterns: String,
    },
}

/// Up to this many stacked matchers, `is_ignored` simply tries each one.
/// Past it, it looks up only the matchers anchored on the path's ancestors.
const LINEAR_SCAN_MAX: usize = 8;

#[derive(Default)]
pub struct IgnoreMatcher {
    matchers: Vec<(PathBuf, Gitignore)>,
    /// Indices into `matchers`, by anchor directory. A walk stacks one
    /// matcher per ignore file it meets, so under a home folder of projects
    /// the stack grows with the tree; trying every matcher for every entry
    /// made walking quadratic (M8). Only matchers anchored on one of a path's
    /// ancestors can apply to it, and a path has a handful of ancestors.
    by_root: HashMap<PathBuf, SmallVec<[usize; 1]>>,
    /// Replayable record of every `add_from_*` call. Used by `clone_rules`
    /// — `Gitignore` itself doesn't impl Clone, so we rebuild each matcher
    /// from its original input.
    sources: Vec<IgnoreSource>,
}

impl IgnoreMatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load an ignore file from disk. `root` is the directory the ignore file
    /// lives in — paths in the file are resolved relative to it.
    pub fn add_from_file(&mut self, ignore_file: &Path) -> Result<(), FxError> {
        let root = ignore_file.parent().ok_or_else(|| {
            FxError::InvalidInput(format!(
                "ignore file {:?} has no parent directory",
                ignore_file
            ))
        })?;
        let mut builder = GitignoreBuilder::new(root);
        if let Some(e) = builder.add(ignore_file) {
            return Err(FxError::Io {
                code: ErrorCode::EINVAL,
                path: ignore_file.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            });
        }
        let gi = builder.build().map_err(|e| FxError::Io {
            code: ErrorCode::EINVAL,
            path: ignore_file.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        })?;
        self.push(root.to_path_buf(), gi);
        self.sources
            .push(IgnoreSource::File(ignore_file.to_path_buf()));
        Ok(())
    }

    /// Load an ignore file whose text was already read — by the bounded read
    /// boundary, which must not leave a second, unbounded `open` of the same
    /// file to the engine thread. Line handling matches `add_from_file`
    /// exactly (BOM, `\r\n`, all-or-nothing on a bad glob), because both go
    /// through `GitignoreBuilder::add_line` with the file as the origin.
    pub fn add_from_contents(&mut self, ignore_file: &Path, contents: &str) -> Result<(), FxError> {
        let root = ignore_file.parent().ok_or_else(|| {
            FxError::InvalidInput(format!(
                "ignore file {:?} has no parent directory",
                ignore_file
            ))
        })?;
        let invalid = |e: ::ignore::Error| FxError::Io {
            code: ErrorCode::EINVAL,
            path: ignore_file.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        };
        let mut builder = GitignoreBuilder::new(root);
        for (index, line) in contents.lines().enumerate() {
            // Same as `GitignoreBuilder::add`: git ignores a leading BOM.
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            builder
                .add_line(Some(ignore_file.to_path_buf()), line)
                .map_err(invalid)?;
        }
        let gi = builder.build().map_err(invalid)?;
        self.push(root.to_path_buf(), gi);
        self.sources.push(IgnoreSource::Contents {
            file: ignore_file.to_path_buf(),
            contents: contents.to_owned(),
        });
        Ok(())
    }

    /// Load ignore rules from a string. Primarily for tests and callers who
    /// compute ignores programmatically. `root` is the anchor directory.
    pub fn add_from_string(&mut self, root: &Path, patterns: &str) -> Result<(), FxError> {
        let mut builder = GitignoreBuilder::new(root);
        for line in patterns.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            builder.add_line(None, trimmed).map_err(|e| FxError::Io {
                code: ErrorCode::EINVAL,
                path: root.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
            })?;
        }
        let gi = builder.build().map_err(|e| FxError::Io {
            code: ErrorCode::EINVAL,
            path: root.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        })?;
        self.push(root.to_path_buf(), gi);
        self.sources.push(IgnoreSource::String {
            root: root.to_path_buf(),
            patterns: patterns.to_owned(),
        });
        Ok(())
    }

    /// Replay every stored source into a fresh matcher. Used by the B3
    /// symlink-aware walker to hand a mutex-wrapped copy to jwalk's
    /// worker threads without moving the caller's matcher.
    ///
    /// Sources that fail to reload (e.g. a `.gitignore` file was deleted
    /// between build and walk) are silently skipped — callers already
    /// treat ignore matching as best-effort.
    pub fn clone_rules(&self) -> Self {
        let mut fresh = Self::default();
        for source in &self.sources {
            match source {
                IgnoreSource::File(path) => {
                    let _ = fresh.add_from_file(path);
                }
                IgnoreSource::Contents { file, contents } => {
                    let _ = fresh.add_from_contents(file, contents);
                }
                IgnoreSource::String { root, patterns } => {
                    let _ = fresh.add_from_string(root, patterns);
                }
            }
        }
        fresh
    }

    /// The one place a matcher joins the stack, so `by_root` stays in step
    /// whichever `add_from_*` was used — a rule that bypassed it would be
    /// invisible to the indexed lookup in `decision` once the stack is long.
    fn push(&mut self, root: PathBuf, gi: Gitignore) {
        self.by_root
            .entry(root.clone())
            .or_default()
            .push(self.matchers.len());
        self.matchers.push((root, gi));
    }

    /// True when any stacked matcher reports `path` as ignored. Gitignore
    /// semantics: a matcher only applies to paths under its anchor directory;
    /// `matched_path_or_any_parents` walks the path's ancestors so nested dir
    /// matches don't miss. Matchers are consulted in the order they were
    /// added, and the first one with an opinion decides.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.decision(path, is_dir).unwrap_or(false)
    }

    /// The first decisive rule for `path`: `Some(true)` ignored,
    /// `Some(false)` whitelisted, `None` when no stacked matcher has an
    /// opinion. Lets a caller consult two matchers as one stack — the walker
    /// checks the caller's rules, then the nested files it discovered, which
    /// is the order a single stack would have held them in.
    pub fn decision(&self, path: &Path, is_dir: bool) -> Option<bool> {
        if self.matchers.len() <= LINEAR_SCAN_MAX {
            return Self::decide(
                self.matchers
                    .iter()
                    .filter(|(root, _)| path.starts_with(root)),
                path,
                is_dir,
            );
        }
        // The same matchers the scan would accept — `path.starts_with(root)`
        // holds exactly when `root` is one of `path.ancestors()` — in the
        // same order.
        let mut applicable: SmallVec<[usize; 8]> = path
            .ancestors()
            .filter_map(|ancestor| self.by_root.get(ancestor))
            .flatten()
            .copied()
            .collect();
        applicable.sort_unstable();
        Self::decide(
            applicable.iter().map(|&index| &self.matchers[index]),
            path,
            is_dir,
        )
    }

    fn decide<'a>(
        matchers: impl Iterator<Item = &'a (PathBuf, Gitignore)>,
        path: &Path,
        is_dir: bool,
    ) -> Option<bool> {
        for (_, gi) in matchers {
            let m = gi.matched_path_or_any_parents(path, is_dir);
            if m.is_ignore() {
                return Some(true);
            }
            if m.is_whitelist() {
                return Some(false);
            }
        }
        None
    }

    pub fn matcher_count(&self) -> usize {
        self.matchers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn empty_matcher_does_not_ignore() {
        let m = IgnoreMatcher::new();
        assert!(!m.is_ignored(Path::new("/tmp/foo.txt"), false));
    }

    #[test]
    fn string_pattern_ignores_match() {
        let td = TempDir::new().unwrap();
        let mut m = IgnoreMatcher::new();
        m.add_from_string(td.path(), "*.log\nnode_modules/\n")
            .unwrap();
        assert!(m.is_ignored(&td.path().join("debug.log"), false));
        assert!(m.is_ignored(&td.path().join("node_modules"), true));
        assert!(m.is_ignored(&td.path().join("node_modules/react"), true));
        assert!(!m.is_ignored(&td.path().join("src/main.rs"), false));
    }

    #[test]
    fn negation_whitelist_takes_effect_at_same_level() {
        // Gitignore semantics: within one file, `!pattern` overrides earlier
        // rules. Across stacked matchers we respect whichever fires first;
        // this test keeps everything inside a single matcher to avoid
        // depending on that ordering subtlety.
        let td = TempDir::new().unwrap();
        let mut m = IgnoreMatcher::new();
        m.add_from_string(td.path(), "*.log\n!keep.log\n").unwrap();
        assert!(m.is_ignored(&td.path().join("x.log"), false));
        assert!(!m.is_ignored(&td.path().join("keep.log"), false));
    }

    #[test]
    fn loads_from_real_gitignore_file() {
        let td = TempDir::new().unwrap();
        let gi = td.path().join(".gitignore");
        fs::write(&gi, "target/\n*.bak\n").unwrap();

        let mut m = IgnoreMatcher::new();
        m.add_from_file(&gi).unwrap();
        assert!(m.is_ignored(&td.path().join("target"), true));
        assert!(m.is_ignored(&td.path().join("src/foo.bak"), false));
        assert!(!m.is_ignored(&td.path().join("src/foo.rs"), false));
    }

    /// The bounded walker loads ignore files from text it already read; the
    /// rules must be the ones `add_from_file` builds from the same bytes.
    #[test]
    fn contents_load_matches_file_load() {
        let td = TempDir::new().unwrap();
        let text = "\u{feff}# comment\r\n*.log\r\n!keep.log\n/anchored/\n\\#literal\ntrailing\\ \nbuild/\n";
        let file = td.path().join(".gitignore");
        fs::write(&file, text).unwrap();
        let mut from_file = IgnoreMatcher::new();
        from_file.add_from_file(&file).unwrap();
        let mut from_text = IgnoreMatcher::new();
        from_text.add_from_contents(&file, text).unwrap();
        for (path, is_dir) in [
            ("a.log", false),
            ("keep.log", false),
            ("anchored", true),
            ("sub/anchored", true),
            ("#literal", false),
            ("trailing ", false),
            ("trailing", false),
            ("build", true),
            ("src/build", true),
            ("src/main.rs", false),
        ] {
            let path = td.path().join(path);
            assert_eq!(
                from_text.decision(&path, is_dir),
                from_file.decision(&path, is_dir),
                "{path:?}"
            );
        }
        // Replaying text-backed rules touches no file.
        fs::remove_file(&file).unwrap();
        assert!(from_text
            .clone_rules()
            .is_ignored(&td.path().join("a.log"), false));
    }

    #[test]
    fn stacked_matchers_each_apply_under_their_root() {
        let td = TempDir::new().unwrap();
        let sub = td.path().join("sub");
        fs::create_dir(&sub).unwrap();

        let mut m = IgnoreMatcher::new();
        m.add_from_string(td.path(), "top-secret.txt\n").unwrap();
        m.add_from_string(&sub, "sub-secret.txt\n").unwrap();

        // Top-level matcher applies everywhere under td.
        assert!(m.is_ignored(&td.path().join("top-secret.txt"), false));
        assert!(m.is_ignored(&sub.join("top-secret.txt"), false));
        // Sub-level matcher applies only under sub.
        assert!(!m.is_ignored(&td.path().join("sub-secret.txt"), false));
        assert!(m.is_ignored(&sub.join("sub-secret.txt"), false));
    }

    #[test]
    fn missing_ignore_file_returns_error() {
        let td = TempDir::new().unwrap();
        let missing = td.path().join(".gitignore-not-here");
        let mut m = IgnoreMatcher::new();
        let err = m.add_from_file(&missing).unwrap_err();
        assert_eq!(err.code(), ErrorCode::EINVAL);
    }

    /// What `is_ignored` answered before M8: try every stacked matcher.
    fn is_ignored_by_scan(m: &IgnoreMatcher, path: &Path, is_dir: bool) -> bool {
        IgnoreMatcher::decide(
            m.matchers.iter().filter(|(root, _)| path.starts_with(root)),
            path,
            is_dir,
        )
        .unwrap_or(false)
    }

    #[test]
    fn ancestor_lookup_agrees_with_scanning_every_matcher() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        let mut m = IgnoreMatcher::new();
        m.add_from_string(root, "*.log\nbuild/\n!keep.log\n")
            .unwrap();
        for project in 0..12 {
            let dir = root.join(format!("p{project}"));
            // Alternate rules that re-include, re-exclude and shadow the
            // root's, so the order matchers are consulted in decides cases.
            let rules = match project % 3 {
                0 => "*.tmp\n!build/\n",
                1 => "!*.log\nsecret/\n",
                _ => "*.txt\n!important.txt\n",
            };
            m.add_from_string(&dir, rules).unwrap();
            m.add_from_string(&dir.join("nested"), "*.rs\n!*.log\n")
                .unwrap();
        }
        // A second matcher on an already-anchored directory.
        m.add_from_string(&root.join("p4"), "*.md\n").unwrap();
        assert!(m.matcher_count() > LINEAR_SCAN_MAX);

        let mut checked = 0;
        for project in 0..13 {
            for dir in ["", "nested", "nested/deeper", "build", "secret"] {
                for name in [
                    "a.log",
                    "keep.log",
                    "a.tmp",
                    "a.txt",
                    "important.txt",
                    "a.rs",
                    "a.md",
                    "build",
                    "secret",
                    "plain",
                ] {
                    let path = root.join(format!("p{project}")).join(dir).join(name);
                    for is_dir in [false, true] {
                        assert_eq!(
                            m.is_ignored(&path, is_dir),
                            is_ignored_by_scan(&m, &path, is_dir),
                            "{path:?} dir={is_dir}"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(m.is_ignored(&root.join("p0/a.tmp"), false));
        // The root's `*.log` was added before p1's `!*.log`: first opinion wins.
        assert!(m.is_ignored(&root.join("p1/a.log"), false));
        assert!(!m.is_ignored(&root.join("p1/keep.log"), false));
        assert!(m.is_ignored(&root.join("p4/a.md"), false));
        assert!(!m.is_ignored(&root.join("outside/a.tmp"), false));
        assert_eq!(checked, 13 * 5 * 10 * 2);

        let cloned = m.clone_rules();
        assert_eq!(cloned.matcher_count(), m.matcher_count());
        assert!(cloned.is_ignored(&root.join("p4/a.md"), false));
    }

    #[test]
    fn unrelated_projects_do_not_slow_a_lookup() {
        // A walk stacks one matcher per ignore file it meets. Before M8 every
        // lookup tried all of them, so 2,000 projects made each lookup 100×
        // the cost of 20 projects. Only the path's own ancestors may count.
        let root = Path::new("/m8-home");
        let stack = |projects: usize| {
            let mut m = IgnoreMatcher::new();
            m.add_from_string(root, "*.log\n").unwrap();
            for project in 0..projects {
                m.add_from_string(&root.join(format!("p{project}")), "*.tmp\n")
                    .unwrap();
            }
            m
        };
        let (few, many) = (stack(20), stack(2_000));
        let paths: Vec<PathBuf> = (0..20_000)
            .map(|index| root.join(format!("p7/src/deep/file-{index}.rs")))
            .collect();
        let lookups = |m: &IgnoreMatcher| {
            let started = std::time::Instant::now();
            let ignored = paths
                .iter()
                .filter(|path| m.is_ignored(path, false))
                .count();
            assert_eq!(ignored, 0);
            started.elapsed()
        };
        let mut few_time = std::time::Duration::MAX;
        let mut many_time = std::time::Duration::MAX;
        for _ in 0..3 {
            few_time = few_time.min(lookups(&few));
            many_time = many_time.min(lookups(&many));
        }
        let growth = many_time.as_secs_f64() / few_time.as_secs_f64();
        assert!(
            growth <= 4.0,
            "20 projects: {few_time:?}; 2,000 projects: {many_time:?} ({growth:.1}×)"
        );
    }
}

#[cfg(test)]
mod expand_ignore_debug {
    use super::*;
    use std::fs;

    #[test]
    fn matcher_on_children_inside_ignored_dir() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        let nm = root.join("node_modules");
        fs::create_dir(&nm).unwrap();
        let child = nm.join("left-pad");
        fs::write(&child, "1").unwrap();

        let mut m = IgnoreMatcher::new();
        m.add_from_file(&root.join(".gitignore")).unwrap();
        assert!(m.is_ignored(&nm, true), "nm dir ignored");
        assert!(m.is_ignored(&child, false), "child file ignored");
        // walk without process_read_dir would list them
    }
}
