//! Status snapshots: the paint-ready list of changed files for one checkout
//! (SPEC mille-git §10).
//!
//! Two things in this file are easy to get subtly wrong, so they are spelled
//! out here rather than discovered later:
//!
//! **Untracked files are listed one by one (`UntrackedFiles::Files`).**
//! `gix::Repository::status` defaults to `status.showUntrackedFiles`, which
//! *collapses* a new untracked directory into one `dir/` row. FileTree
//! badges and `projectChangedFilesView` key on files, so a collapsed row
//! leaves every file inside the new directory undecorated — the "nested
//! untracked file has no badge" bug. Files mode matches
//! `git status --porcelain=v2 -z --untracked-files=all`, which is also the
//! CLI fallback's command line (T-UNTRACKED-NESTED).
//!
//! **A path-limited run merges; only a full run replaces (§10.6).** The
//! hottest status path is "one file was saved": gix runs with that file as
//! its only pathspec and reports at most that file. Publishing that result
//! as *the* snapshot would wipe every other dirty row from the list. So a
//! path-limited result is merged into the previous snapshot — upsert what
//! came back, drop hinted paths that came back clean, keep everything the
//! run never asked about — and the renamed-away `origPath` of any reported
//! rename is always dropped, hinted or not (the old name is gone from disk
//! whichever path the watcher happened to report). See [`merge_status`].

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gix::bstr::BString;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

use crate::identity::GitIdentity;

/// A status letter (`GitStatusLetter` in `client.ts`). The empty slot `.`
/// is `None` in [`StatusXy`], never a letter.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub enum StatusLetter {
    /// `M` — also every type change (`T` maps here).
    #[serde(rename = "M")]
    Modified,
    /// `A`
    #[serde(rename = "A")]
    Added,
    /// `D`
    #[serde(rename = "D")]
    Deleted,
    /// `U` — unmerged.
    #[serde(rename = "U")]
    Unmerged,
    /// `R`
    #[serde(rename = "R")]
    Renamed,
    /// `C`
    #[serde(rename = "C")]
    Copied,
    /// `?` — untracked.
    #[serde(rename = "?")]
    Untracked,
    /// `!` — ignored. v1 never emits it.
    #[serde(rename = "!")]
    Ignored,
}

impl StatusLetter {
    /// `parseLetter`: `T` becomes `M`, `.` and unknown glyphs are `None`.
    pub fn from_porcelain(byte: u8) -> Option<Self> {
        Some(match byte {
            b'M' | b'T' => StatusLetter::Modified,
            b'A' => StatusLetter::Added,
            b'D' => StatusLetter::Deleted,
            b'R' => StatusLetter::Renamed,
            b'C' => StatusLetter::Copied,
            b'U' => StatusLetter::Unmerged,
            _ => return None,
        })
    }

    /// The one-character spelling.
    pub fn as_char(self) -> char {
        match self {
            StatusLetter::Modified => 'M',
            StatusLetter::Added => 'A',
            StatusLetter::Deleted => 'D',
            StatusLetter::Unmerged => 'U',
            StatusLetter::Renamed => 'R',
            StatusLetter::Copied => 'C',
            StatusLetter::Untracked => '?',
            StatusLetter::Ignored => '!',
        }
    }

    /// `GIT_ORDER` in `mille-ui/src/views/projectors.ts`.
    fn order(self) -> u8 {
        match self {
            StatusLetter::Unmerged => 0,
            StatusLetter::Modified => 10,
            StatusLetter::Added => 20,
            StatusLetter::Deleted => 30,
            StatusLetter::Renamed => 40,
            StatusLetter::Copied => 50,
            StatusLetter::Untracked => 60,
            StatusLetter::Ignored => 70,
        }
    }
}

/// The two porcelain status columns: index (`X`) and worktree (`Y`).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct StatusXy {
    /// `X`; `None` is `.`.
    pub index: Option<StatusLetter>,
    /// `Y`; `None` is `.`.
    pub worktree: Option<StatusLetter>,
}

impl StatusXy {
    /// `"M."`, `".M"`, `"UU"`, `".?"` …
    pub fn as_string(&self) -> String {
        let slot = |l: Option<StatusLetter>| l.map_or('.', StatusLetter::as_char);
        format!("{}{}", slot(self.index), slot(self.worktree))
    }
}

impl Serialize for StatusXy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let slot = |l: Option<StatusLetter>| l.map_or('.', StatusLetter::as_char).to_string();
        let mut out = serializer.serialize_struct("StatusXy", 2)?;
        out.serialize_field("index", &slot(self.index))?;
        out.serialize_field("worktree", &slot(self.worktree))?;
        out.end()
    }
}

/// One changed path (§10.2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEntry {
    /// Worktree-relative POSIX path, no leading slash. For a rename, the new
    /// name.
    pub path: String,
    /// Rename / copy source, worktree-relative POSIX.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
    /// Both columns, preserved (the `GitClient` adapter collapses them).
    pub xy: StatusXy,
    /// The index column is non-empty. False for untracked and conflicted
    /// rows: neither has anything staged a commit would take.
    pub staged: bool,
    /// `?` — the row's `xy` is `.?`.
    pub untracked: bool,
    /// Unmerged; `xy` carries git's conflict pair (`UU`, `AA`, `DU`, …).
    pub conflicted: bool,
}

impl StatusEntry {
    /// A tracked change.
    pub fn tracked(path: String, orig_path: Option<String>, xy: StatusXy) -> Self {
        Self {
            staged: xy.index.is_some(),
            path,
            orig_path,
            xy,
            untracked: false,
            conflicted: false,
        }
    }

    /// An untracked file.
    pub fn untracked(path: String) -> Self {
        let xy = StatusXy {
            index: None,
            worktree: Some(StatusLetter::Untracked),
        };
        Self {
            path,
            orig_path: None,
            xy,
            staged: false,
            untracked: true,
            conflicted: false,
        }
    }

    /// An unmerged path.
    pub fn conflicted(path: String, xy: StatusXy) -> Self {
        Self {
            path,
            orig_path: None,
            xy,
            staged: false,
            untracked: false,
            conflicted: true,
        }
    }

    /// The one letter a FileTree badge would show (`xyToEntry`).
    pub fn collapsed_letter(&self) -> StatusLetter {
        if self.conflicted {
            StatusLetter::Unmerged
        } else if self.untracked {
            StatusLetter::Untracked
        } else {
            self.xy
                .index
                .or(self.xy.worktree)
                .unwrap_or(StatusLetter::Modified)
        }
    }
}

/// Which backend computed a snapshot.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusBackend {
    /// In-process gix status.
    Gix,
    /// The `git status --porcelain=v2 -z` fallback.
    Cli,
}

/// One published status generation (§10.2). Cheap to share as `Arc`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    /// The checkout root (`identity.worktree_path`).
    pub worktree_path: PathBuf,
    /// Identity as of this run (HEAD and branch refresh every run).
    pub identity: GitIdentity,
    /// Sorted, capped rows.
    pub entries: Vec<StatusEntry>,
    /// Rows omitted by the cap; `None` when nothing was omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<usize>,
    /// Compute time of this run (not time since `open`).
    pub took_ms: u64,
    /// This generation merged a path-limited run (§10.6).
    pub path_limited: bool,
    /// Monotonic per session.
    pub generation: u64,
    /// gix or the CLI fallback.
    pub status_backend: StatusBackend,
}

impl StatusSnapshot {
    /// An empty snapshot (not-a-repo, bare, missing git).
    pub fn empty(identity: GitIdentity, generation: u64) -> Self {
        Self {
            worktree_path: identity.worktree_path.clone(),
            identity,
            entries: Vec::new(),
            truncated: None,
            took_ms: 0,
            path_limited: false,
            generation,
            status_backend: StatusBackend::Gix,
        }
    }
}

/// Above this many hinted paths, a path-limited run costs more than it
/// saves; run a full status instead (§10.3).
pub const MAX_HINT_PATHS: usize = 512;

/// Is `path` covered by the hint set? A hinted path covers itself and,
/// exactly like the pathspec it becomes, everything beneath it — so a
/// hinted directory that came back clean also clears its stale children.
fn covered(hints: &HashSet<&str>, path: &str) -> bool {
    if hints.contains(path) {
        return true;
    }
    let mut prefix = path;
    while let Some(slash) = prefix.rfind('/') {
        prefix = &prefix[..slash];
        if hints.contains(prefix) {
            return true;
        }
    }
    false
}

/// Merge a path-limited or full `run` into `prev` (§10.6). `prev` must be
/// the previous **uncapped** set; apply [`apply_row_cap`] to the result.
///
/// - `hints == None`, empty, or larger than [`MAX_HINT_PATHS`]: a full run;
///   its entries replace `prev`'s.
/// - Otherwise: start from `prev`, drop every row named as the `origPath` of
///   a `run` row, drop hinted paths `run` no longer reports, upsert `run`'s
///   rows (keyed by the new path), keep the rest.
///
/// `generation` is one past `prev`'s (or `run`'s own, if larger); identity,
/// `took_ms` and backend come from `run`.
pub fn merge_status(
    prev: Option<&StatusSnapshot>,
    run: StatusSnapshot,
    hints: Option<&[String]>,
) -> StatusSnapshot {
    let generation = prev
        .map_or(run.generation, |p| p.generation + 1)
        .max(run.generation);
    let hints = hints.filter(|h| !h.is_empty() && h.len() <= MAX_HINT_PATHS);
    let (mut entries, path_limited) = match (hints, prev) {
        (Some(hints), Some(prev)) => {
            let hinted: HashSet<&str> = hints.iter().map(String::as_str).collect();
            let reported: HashSet<&str> = run.entries.iter().map(|e| e.path.as_str()).collect();
            let renamed_away: HashSet<&str> = run
                .entries
                .iter()
                .filter_map(|e| e.orig_path.as_deref())
                .collect();
            let mut merged: BTreeMap<String, StatusEntry> = prev
                .entries
                .iter()
                .filter(|e| !renamed_away.contains(e.path.as_str()))
                .filter(|e| !(covered(&hinted, &e.path) && !reported.contains(e.path.as_str())))
                .map(|e| (e.path.clone(), e.clone()))
                .collect();
            for entry in run.entries {
                merged.insert(entry.path.clone(), entry);
            }
            (merged.into_values().collect(), true)
        }
        (Some(_), None) => (run.entries, true),
        (None, _) => (run.entries, false),
    };
    sort_entries(&mut entries);
    StatusSnapshot {
        worktree_path: run.worktree_path,
        identity: run.identity,
        entries,
        truncated: None,
        took_ms: run.took_ms,
        path_limited,
        generation,
        status_backend: run.status_backend,
    }
}

/// Conflicts first, then modified, added, deleted, renamed, copied,
/// untracked (`GIT_ORDER`); by path within a letter.
pub fn sort_entries(entries: &mut [StatusEntry]) {
    entries.sort_by(|a, b| {
        a.collapsed_letter()
            .order()
            .cmp(&b.collapsed_letter().order())
            .then_with(|| a.path.cmp(&b.path))
    });
}

/// Keep the first `cap` (already sorted) rows; set `truncated` to the rest.
pub fn apply_row_cap(snapshot: &mut StatusSnapshot, cap: usize) {
    if snapshot.entries.len() > cap {
        snapshot.truncated = Some(snapshot.entries.len() - cap);
        snapshot.entries.truncate(cap);
    } else {
        snapshot.truncated = None;
    }
}

/// gix's `index_worktree::Options.thread_limit`: at most four threads per
/// status, so eight open worktrees cannot start eight core-count pools
/// (§6.2). Never `RAYON_NUM_THREADS`, which is process-global.
pub fn status_thread_limit() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(4)
}

/// Pathspecs for a path-limited run: worktree-relative POSIX, literal (a
/// file named `[id].tsx` is a file, not a glob).
fn literal_pathspecs(hints: Option<&[String]>) -> Vec<BString> {
    hints
        .unwrap_or_default()
        .iter()
        .map(|p| BString::from(format!(":(literal){p}")))
        .collect()
}

/// Per-path accumulator: tree↔index and index↔worktree arrive as separate,
/// unordered items (the two run on separate threads under `parallel`).
#[derive(Default)]
struct Row {
    index: Option<StatusLetter>,
    worktree: Option<StatusLetter>,
    orig_path: Option<String>,
    untracked: bool,
    conflict: Option<StatusXy>,
}

fn conflict_xy(conflict: gix::status::plumbing::index_as_worktree::Conflict) -> StatusXy {
    use gix::status::plumbing::index_as_worktree::Conflict;
    use StatusLetter::{Added as A, Deleted as D, Unmerged as U};
    let (index, worktree) = match conflict {
        Conflict::BothDeleted => (D, D),
        Conflict::AddedByUs => (A, U),
        Conflict::DeletedByThem => (U, D),
        Conflict::AddedByThem => (U, A),
        Conflict::DeletedByUs => (D, U),
        Conflict::BothAdded => (A, A),
        Conflict::BothModified => (U, U),
    };
    StatusXy {
        index: Some(index),
        worktree: Some(worktree),
    }
}

/// Run gix status (whole repo, or limited to `hints`) and map it onto
/// porcelain-v2 XY rows, unsorted.
pub(crate) fn gix_status(
    repo: &gix::Repository,
    hints: Option<&[String]>,
    interrupt: Arc<AtomicBool>,
) -> Result<Vec<StatusEntry>, String> {
    use gix::status::index_worktree::Item as Worktree;
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
    use gix::status::{Item, UntrackedFiles};

    let threads = status_thread_limit();
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| e.to_string())?
        .untracked_files(UntrackedFiles::Files)
        .dirwalk_options(|d| d.emit_ignored(None).emit_empty_directories(false))
        // Porcelain status does not pair a worktree deletion with an
        // untracked file as a rename; neither do we.
        .index_worktree_rewrites(None)
        .index_worktree_options_mut(|o| o.thread_limit = Some(threads))
        .should_interrupt_owned(interrupt);
    let iter = platform
        .into_iter(literal_pathspecs(hints))
        .map_err(|e| e.to_string())?;

    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for item in iter {
        match item.map_err(|e| e.to_string())? {
            Item::TreeIndex(change) => {
                use gix::diff::index::ChangeRef;
                match change {
                    ChangeRef::Addition { location, .. } => {
                        rows.entry(location.to_string()).or_default().index =
                            Some(StatusLetter::Added)
                    }
                    ChangeRef::Deletion { location, .. } => {
                        rows.entry(location.to_string()).or_default().index =
                            Some(StatusLetter::Deleted)
                    }
                    ChangeRef::Modification { location, .. } => {
                        rows.entry(location.to_string()).or_default().index =
                            Some(StatusLetter::Modified)
                    }
                    ChangeRef::Rewrite {
                        source_location,
                        location,
                        copy,
                        ..
                    } => {
                        let row = rows.entry(location.to_string()).or_default();
                        row.index = Some(if copy {
                            StatusLetter::Copied
                        } else {
                            StatusLetter::Renamed
                        });
                        row.orig_path = Some(source_location.to_string());
                    }
                }
            }
            Item::IndexWorktree(Worktree::Modification {
                rela_path, status, ..
            }) => {
                let row = rows.entry(rela_path.to_string()).or_default();
                match status {
                    EntryStatus::Conflict { summary, .. } => {
                        row.conflict = Some(conflict_xy(summary))
                    }
                    EntryStatus::Change(Change::Removed) => {
                        row.worktree = Some(StatusLetter::Deleted)
                    }
                    EntryStatus::Change(
                        Change::Type { .. }
                        | Change::Modification { .. }
                        | Change::SubmoduleModification(_),
                    ) => row.worktree = Some(StatusLetter::Modified),
                    // `git add -N`: porcelain reports `.A`.
                    EntryStatus::IntentToAdd => row.worktree = Some(StatusLetter::Added),
                    // Racily clean: only the stat cache is stale.
                    EntryStatus::NeedsUpdate(_) => {}
                }
            }
            Item::IndexWorktree(Worktree::DirectoryContents { entry, .. }) => {
                if entry.status == gix::dir::entry::Status::Untracked {
                    let mut path = entry.rela_path.to_string();
                    // A nested repository is listed as its directory, the
                    // way porcelain spells it.
                    if matches!(
                        entry.disk_kind,
                        Some(gix::dir::entry::Kind::Repository | gix::dir::entry::Kind::Directory)
                    ) {
                        path.push('/');
                    }
                    rows.entry(path).or_default().untracked = true;
                }
            }
            Item::IndexWorktree(Worktree::Rewrite {
                source,
                dirwalk_entry,
                ..
            }) => {
                // Disabled above; if it ever arrives, report it the porcelain
                // way: the source deleted, the destination untracked.
                rows.entry(source.rela_path().to_string())
                    .or_default()
                    .worktree = Some(StatusLetter::Deleted);
                rows.entry(dirwalk_entry.rela_path.to_string())
                    .or_default()
                    .untracked = true;
            }
        }
    }

    Ok(rows
        .into_iter()
        .filter_map(|(path, row)| {
            if let Some(xy) = row.conflict {
                return Some(StatusEntry::conflicted(path, xy));
            }
            if row.untracked {
                return Some(StatusEntry::untracked(path));
            }
            let xy = StatusXy {
                index: row.index,
                worktree: row.worktree,
            };
            (xy.index.is_some() || xy.worktree.is_some())
                .then(|| StatusEntry::tracked(path, row.orig_path, xy))
        })
        .collect())
}

/// The CLI fallback (§10.3):
/// `git --no-optional-locks status --porcelain=v2 -z --untracked-files=all [-- <paths>]`,
/// parsed by the same rules as `parsePorcelainV2`. Returns the rows and the
/// parse warnings for skipped rows.
pub(crate) async fn cli_status(
    git: &crate::cli::GitCli,
    root: &Path,
    hints: Option<&[String]>,
) -> crate::error::Result<(Vec<StatusEntry>, Vec<String>)> {
    let mut args: Vec<String> = ["status", "--porcelain=v2", "-z", "--untracked-files=all"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    if let Some(hints) = hints {
        args.push("--".to_owned());
        args.extend(hints.iter().cloned());
    }
    let out = git
        .run(root, &args, crate::cli::RunOptions::read())
        .await?
        .check("status")?;
    let mut warnings = Vec::new();
    let rows = crate::porcelain::parse_porcelain_v2(&out.stdout, &mut |w| warnings.push(w));
    Ok((rows, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::GitIdentity;

    fn snap(entries: Vec<StatusEntry>, generation: u64) -> StatusSnapshot {
        let mut s = StatusSnapshot::empty(GitIdentity::none(Path::new("/r")), generation);
        s.entries = entries;
        s
    }

    fn modified(path: &str) -> StatusEntry {
        StatusEntry::tracked(
            path.to_owned(),
            None,
            StatusXy {
                index: None,
                worktree: Some(StatusLetter::Modified),
            },
        )
    }

    fn paths(s: &StatusSnapshot) -> Vec<&str> {
        s.entries.iter().map(|e| e.path.as_str()).collect()
    }

    fn hints(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    // T-MERGE / T-HINT-MERGE worked example
    #[test]
    fn a_one_path_hint_never_wipes_the_other_rows() {
        let prev = snap(
            vec![modified("a.ts"), modified("b.ts"), modified("c.ts")],
            1,
        );
        let still_dirty = merge_status(
            Some(&prev),
            snap(vec![modified("a.ts")], 0),
            Some(&hints(&["a.ts"])),
        );
        assert_eq!(paths(&still_dirty), ["a.ts", "b.ts", "c.ts"]);
        assert!(still_dirty.path_limited);
        assert_eq!(still_dirty.generation, 2);

        let reverted = merge_status(Some(&still_dirty), snap(vec![], 0), Some(&hints(&["a.ts"])));
        assert_eq!(paths(&reverted), ["b.ts", "c.ts"]);
        assert_eq!(reverted.generation, 3);
    }

    // T-MERGE-RENAME
    #[test]
    fn a_rename_always_drops_its_old_name() {
        let prev = snap(vec![modified("a.ts"), modified("z.ts")], 4);
        let renamed = StatusEntry::tracked(
            "b.ts".into(),
            Some("a.ts".into()),
            StatusXy {
                index: Some(StatusLetter::Renamed),
                worktree: None,
            },
        );
        for hint in [hints(&["b.ts"]), hints(&["a.ts", "b.ts"])] {
            let merged = merge_status(Some(&prev), snap(vec![renamed.clone()], 0), Some(&hint));
            assert_eq!(paths(&merged), ["z.ts", "b.ts"], "hint {hint:?}");
            assert_eq!(merged.entries[1].orig_path.as_deref(), Some("a.ts"));
        }
    }

    #[test]
    fn untracked_files_outside_the_hint_neither_appear_nor_vanish() {
        let prev = snap(vec![StatusEntry::untracked("old-untracked.ts".into())], 1);
        let merged = merge_status(
            Some(&prev),
            snap(vec![modified("a.ts")], 0),
            Some(&hints(&["a.ts"])),
        );
        assert_eq!(paths(&merged), ["a.ts", "old-untracked.ts"]);
    }

    #[test]
    fn a_hinted_directory_that_came_back_clean_clears_its_children() {
        let prev = snap(
            vec![
                StatusEntry::untracked("gen/out.js".into()),
                modified("keep.ts"),
            ],
            1,
        );
        let merged = merge_status(Some(&prev), snap(vec![], 0), Some(&hints(&["gen"])));
        assert_eq!(paths(&merged), ["keep.ts"]);
    }

    #[test]
    fn a_full_run_replaces_and_huge_hint_sets_count_as_full() {
        let prev = snap(vec![modified("a.ts"), modified("b.ts")], 7);
        let full = merge_status(Some(&prev), snap(vec![modified("c.ts")], 0), None);
        assert_eq!(paths(&full), ["c.ts"]);
        assert!(!full.path_limited);
        assert_eq!(full.generation, 8);

        let many: Vec<String> = (0..=MAX_HINT_PATHS).map(|i| format!("f{i}")).collect();
        let huge = merge_status(Some(&prev), snap(vec![modified("c.ts")], 0), Some(&many));
        assert_eq!(paths(&huge), ["c.ts"]);
        assert!(!huge.path_limited);

        let empty = merge_status(Some(&prev), snap(vec![modified("c.ts")], 0), Some(&[]));
        assert!(!empty.path_limited);
    }

    #[test]
    fn rows_sort_by_git_order_and_the_cap_counts_the_rest() {
        let mut s = merge_status(
            None,
            snap(
                vec![
                    StatusEntry::untracked("u".into()),
                    modified("m2"),
                    StatusEntry::conflicted(
                        "c".into(),
                        StatusXy {
                            index: Some(StatusLetter::Unmerged),
                            worktree: Some(StatusLetter::Unmerged),
                        },
                    ),
                    StatusEntry::tracked(
                        "a".into(),
                        None,
                        StatusXy {
                            index: Some(StatusLetter::Added),
                            worktree: None,
                        },
                    ),
                    modified("m1"),
                ],
                1,
            ),
            None,
        );
        assert_eq!(paths(&s), ["c", "m1", "m2", "a", "u"]);
        apply_row_cap(&mut s, 3);
        assert_eq!(paths(&s), ["c", "m1", "m2"]);
        assert_eq!(s.truncated, Some(2));
        apply_row_cap(&mut s, 10);
        assert_eq!(s.truncated, None);
    }

    #[test]
    fn xy_serializes_with_dots_for_empty_slots() {
        let e = StatusEntry::untracked("x".into());
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["xy"]["index"], ".");
        assert_eq!(json["xy"]["worktree"], "?");
        assert!(json.get("origPath").is_none());
    }
}
