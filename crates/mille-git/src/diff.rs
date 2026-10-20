//! Pull-based, one-path diffs with caps (SPEC mille-git §13).
//!
//! **Size before bytes.** Every side is asked for its size first — the
//! object *header* for a blob (no inflate), `stat` for a worktree file — and
//! a side at or above `hardRefuseBytes` ends the request with
//! `tooLarge: true` before anything is read. The engine lives inside the
//! editor's utility process: a 500 MB generated file in a repo must cost a
//! header read, not a 500 MB allocation and a Myers run. Only after both
//! sizes pass are the bytes loaded; a NUL in either side's first 8 KiB makes
//! it binary (no hunks); a patch over the auto-render caps is reported as
//! `tooLarge` with its stats and no hunks. `tookMs` is on every result,
//! short-circuits included (§22.1).
//!
//! There is no all-files diff and no unbounded `git diff`: one path per
//! request (§13.1).

use std::path::{Path, PathBuf};
use std::time::Instant;

use gix::diff::blob::{Algorithm, Diff, InternedInput};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::containment::assert_real_path_under_root;
use crate::error::{GitError, Result};
use crate::paths;

/// Which two sides to compare.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffSpec {
    /// Index → worktree (`git diff -- <path>`).
    Worktree,
    /// HEAD → index (`git diff --cached -- <path>`).
    Index,
    /// First parent → `commit` (`git show <commit> -- <path>`).
    Commit,
}

/// One diff request (§13.1).
#[derive(Clone, Debug)]
pub struct DiffRequest {
    /// Worktree-relative POSIX path.
    pub path: String,
    /// Which sides.
    pub spec: DiffSpec,
    /// Required for [`DiffSpec::Commit`]; a safe revision.
    pub commit: Option<String>,
    /// Lower the auto-render byte cap for this request.
    pub max_bytes: Option<usize>,
    /// Lower the auto-render line cap for this request.
    pub max_lines: Option<usize>,
    /// Abort the request.
    pub cancel: Option<CancellationToken>,
}

/// `ctx` / `add` / `del`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineKind {
    /// Unchanged context.
    Ctx,
    /// Added on the new side.
    Add,
    /// Deleted from the old side.
    Del,
}

/// One line of a hunk, without its newline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiffLine {
    /// Which side(s) the line belongs to.
    pub kind: DiffLineKind,
    /// The text (lossy UTF-8), at most [`MAX_LINE_CHARS`] characters.
    pub text: String,
    /// The text was cut at [`MAX_LINE_CHARS`] (§13.2 step 5).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// A unified hunk; starts are 1-based, as in `@@ -a,b +c,d @@`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    /// First old line.
    pub old_start: u32,
    /// Old line count.
    pub old_lines: u32,
    /// First new line.
    pub new_start: u32,
    /// New line count.
    pub new_lines: u32,
    /// Context, deletions and additions in order.
    pub lines: Vec<DiffLine>,
}

/// The answer to a [`DiffRequest`] (§13.1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    /// The requested path.
    pub path: String,
    /// Over a cap: no hunks.
    pub too_large: bool,
    /// Either side looks binary: no hunks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    /// The larger side's size (hard refuse), or the patch's size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Patch line count, when it was computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
    /// Present when the diff rendered (empty for binary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<DiffHunk>>,
    /// Engine time for this request, always present.
    pub took_ms: u64,
}

/// Lines longer than this are cut and flagged.
pub const MAX_LINE_CHARS: usize = 1024;
/// Unchanged lines around each change.
pub const CONTEXT_LINES: u32 = 3;
/// How much of each side the binary heuristic inspects.
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// The three caps of §13.2.
#[derive(Copy, Clone, Debug)]
pub struct DiffCaps {
    /// Patch bytes above this: `tooLarge`, no hunks.
    pub auto_render_max_bytes: usize,
    /// Patch lines above this: `tooLarge`, no hunks.
    pub auto_render_max_lines: usize,
    /// Either side at or above this: `tooLarge` before loading anything.
    pub hard_refuse_bytes: usize,
}

/// One side of a diff whose size is known before its bytes.
pub trait DiffSide {
    /// The side's size without loading it (object header, `stat`); `None`
    /// when the side does not exist (an added or deleted file).
    fn size(&self) -> Result<Option<u64>>;
    /// The side's bytes. Only called once [`size`](Self::size) passed the
    /// hard-refuse check.
    fn load(&self) -> Result<Option<Vec<u8>>>;
}

/// Run the §13.2 pipeline on two sides.
pub fn diff_sides(
    path: &str,
    old: &dyn DiffSide,
    new: &dyn DiffSide,
    caps: DiffCaps,
) -> Result<DiffResult> {
    let started = Instant::now();
    let took = |started: Instant| started.elapsed().as_millis() as u64;
    let result = |too_large, binary, bytes, lines, hunks, took_ms| DiffResult {
        path: path.to_owned(),
        too_large,
        binary,
        bytes,
        lines,
        hunks,
        took_ms,
    };

    // 1. Header / stat first.
    let largest = old.size()?.unwrap_or(0).max(new.size()?.unwrap_or(0));
    if largest >= caps.hard_refuse_bytes as u64 {
        return Ok(result(true, None, Some(largest), None, None, took(started)));
    }
    // 2. Only now load.
    let before = old.load()?.unwrap_or_default();
    let after = new.load()?.unwrap_or_default();
    if looks_binary(&before) || looks_binary(&after) {
        return Ok(result(
            false,
            Some(true),
            Some(largest),
            None,
            Some(Vec::new()),
            took(started),
        ));
    }
    // 3. Diff, then the auto-render caps on the patch itself.
    let hunks = unified_hunks(&before, &after);
    let patch_lines: u64 = hunks.iter().map(|h| h.lines.len() as u64).sum();
    let patch_bytes: u64 = hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| l.text.len() as u64 + 2)
        .sum();
    if patch_bytes > caps.auto_render_max_bytes as u64
        || patch_lines > caps.auto_render_max_lines as u64
        || patch_bytes >= caps.hard_refuse_bytes as u64
    {
        return Ok(result(
            true,
            None,
            Some(patch_bytes),
            Some(patch_lines),
            None,
            took(started),
        ));
    }
    Ok(result(
        false,
        None,
        Some(patch_bytes),
        Some(patch_lines),
        Some(hunks),
        took(started),
    ))
}

/// git's heuristic: a NUL byte in the first 8 KiB.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

fn line(kind: DiffLineKind, raw: &[u8]) -> DiffLine {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    let text = String::from_utf8_lossy(raw);
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => DiffLine {
            kind,
            text: text[..cut].to_owned(),
            truncated: true,
        },
        None => DiffLine {
            kind,
            text: text.into_owned(),
            truncated: false,
        },
    }
}

/// Histogram diff (imara, as gix uses it) grouped into unified hunks with
/// [`CONTEXT_LINES`] of context.
fn unified_hunks(before: &[u8], after: &[u8]) -> Vec<DiffHunk> {
    let input = InternedInput::new(before, after);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let changes: Vec<gix::diff::blob::Hunk> = diff.hunks().collect();
    let old_len = input.before.len() as u32;
    let text = |token: gix::diff::blob::Token| -> &[u8] { input.interner[token] };

    let mut hunks = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        // Group changes whose gap is small enough for their contexts to touch.
        let mut j = i;
        while j + 1 < changes.len()
            && changes[j + 1].before.start - changes[j].before.end <= 2 * CONTEXT_LINES
        {
            j += 1;
        }
        let first = &changes[i];
        let last = &changes[j];
        let lead = CONTEXT_LINES.min(first.before.start);
        let trail = CONTEXT_LINES.min(old_len - last.before.end);
        let (old_begin, new_begin) = (first.before.start - lead, first.after.start - lead);
        let mut lines = Vec::new();
        let mut cursor = old_begin;
        for change in &changes[i..=j] {
            for k in cursor..change.before.start {
                lines.push(line(DiffLineKind::Ctx, text(input.before[k as usize])));
            }
            for k in change.before.clone() {
                lines.push(line(DiffLineKind::Del, text(input.before[k as usize])));
            }
            for k in change.after.clone() {
                lines.push(line(DiffLineKind::Add, text(input.after[k as usize])));
            }
            cursor = change.before.end;
        }
        for k in cursor..last.before.end + trail {
            lines.push(line(DiffLineKind::Ctx, text(input.before[k as usize])));
        }
        let old_lines = last.before.end + trail - old_begin;
        let new_lines = last.after.end + trail - new_begin;
        // git prints an empty range's start as the line *before* it.
        let start = |begin: u32, count: u32| if count == 0 { begin } else { begin + 1 };
        hunks.push(DiffHunk {
            old_start: start(old_begin, old_lines),
            old_lines,
            new_start: start(new_begin, new_lines),
            new_lines,
            lines,
        });
        i = j + 1;
    }
    hunks
}

/// A blob side: header for the size, the object store for the bytes.
pub(crate) struct BlobSide {
    pub(crate) repo: gix::ThreadSafeRepository,
    pub(crate) id: Option<gix::ObjectId>,
}

impl DiffSide for BlobSide {
    fn size(&self) -> Result<Option<u64>> {
        let Some(id) = self.id else { return Ok(None) };
        let header = self
            .repo
            .to_thread_local()
            .find_header(id)
            .map_err(|e| GitError::unknown(e.to_string()))?;
        Ok(Some(header.size()))
    }

    fn load(&self) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.id else { return Ok(None) };
        let repo = self.repo.to_thread_local();
        let object = repo
            .find_object(id)
            .map_err(|e| GitError::unknown(e.to_string()))?;
        Ok(Some(object.detach().data))
    }
}

/// A working-tree side: realpath'd and contained before `stat`, so a
/// symlink cannot point a read outside the checkout (§13.2 step 2).
pub(crate) struct WorktreeSide {
    pub(crate) real_root: PathBuf,
    pub(crate) relative: String,
}

impl WorktreeSide {
    /// The realpath of the file, `None` if it does not exist, an error if it
    /// resolves outside the checkout.
    pub(crate) fn resolve(&self) -> Result<Option<PathBuf>> {
        let joined = paths::join_posix(&self.real_root, &self.relative);
        let Ok(real) = std::fs::canonicalize(&joined) else {
            return Ok(None);
        };
        assert_real_path_under_root(&self.real_root, &real, &self.relative)?;
        Ok(real.is_file().then_some(real))
    }
}

impl DiffSide for WorktreeSide {
    fn size(&self) -> Result<Option<u64>> {
        let Some(real) = self.resolve()? else {
            return Ok(None);
        };
        Ok(Some(
            std::fs::metadata(&real)
                .map_err(|e| GitError::io(&e, &self.relative))?
                .len(),
        ))
    }

    fn load(&self) -> Result<Option<Vec<u8>>> {
        let Some(real) = self.resolve()? else {
            return Ok(None);
        };
        std::fs::read(&real)
            .map(Some)
            .map_err(|e| GitError::io(&e, &self.relative))
    }
}

/// Resolve the blob at `path` in a commit-ish's tree.
pub(crate) fn blob_in_revision(
    repo: &gix::Repository,
    revision: &str,
    path: &str,
) -> Result<Option<gix::ObjectId>> {
    let Ok(object) = repo.rev_parse_single(revision) else {
        return Ok(None);
    };
    let Ok(tree) = object
        .object()
        .map_err(|e| e.to_string())
        .and_then(|o| o.peel_to_tree().map_err(|e| e.to_string()))
    else {
        return Ok(None);
    };
    blob_in_tree(&tree, path)
}

pub(crate) fn blob_in_tree(tree: &gix::Tree<'_>, path: &str) -> Result<Option<gix::ObjectId>> {
    let entry = tree
        .lookup_entry_by_path(Path::new(path))
        .map_err(|e| GitError::unknown(e.to_string()))?;
    Ok(entry
        .filter(|e| e.mode().is_blob() || e.mode().is_link())
        .map(|e| e.object_id()))
}

/// The index's stage-0 blob for `path`.
pub(crate) fn blob_in_index(repo: &gix::Repository, path: &str) -> Result<Option<gix::ObjectId>> {
    let index = repo
        .index_or_empty()
        .map_err(|e| GitError::unknown(e.to_string()))?;
    Ok(index.entry_by_path(path.into()).map(|entry| entry.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Option<Vec<u8>>);
    impl DiffSide for Mem {
        fn size(&self) -> Result<Option<u64>> {
            Ok(self.0.as_ref().map(|b| b.len() as u64))
        }
        fn load(&self) -> Result<Option<Vec<u8>>> {
            Ok(self.0.clone())
        }
    }

    const CAPS: DiffCaps = DiffCaps {
        auto_render_max_bytes: 262_144,
        auto_render_max_lines: 5_000,
        hard_refuse_bytes: 1 << 20,
    };

    fn render(hunks: &[DiffHunk]) -> String {
        let mut out = String::new();
        for h in hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.old_start, h.old_lines, h.new_start, h.new_lines
            ));
            for l in &h.lines {
                let sign = match l.kind {
                    DiffLineKind::Ctx => ' ',
                    DiffLineKind::Add => '+',
                    DiffLineKind::Del => '-',
                };
                out.push_str(&format!("{sign}{}\n", l.text));
            }
        }
        out
    }

    #[test]
    fn hunks_carry_three_lines_of_context_and_merge_when_close() {
        let before: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let after = before
            .replace("line 5\n", "line five\n")
            .replace("line 9\n", "line nine\n")
            .replace("line 19\n", "");
        let result = diff_sides(
            "f",
            &Mem(Some(before.into())),
            &Mem(Some(after.into())),
            CAPS,
        )
        .unwrap();
        let text = render(result.hunks.as_ref().unwrap());
        assert_eq!(
            text,
            "@@ -2,11 +2,11 @@\n line 2\n line 3\n line 4\n-line 5\n+line five\n line 6\n line 7\n line 8\n-line 9\n+line nine\n line 10\n line 11\n line 12\n\
             @@ -16,5 +16,4 @@\n line 16\n line 17\n line 18\n-line 19\n line 20\n"
        );
        assert!(!result.too_large);
        assert_eq!(result.lines, Some(18));
    }

    #[test]
    fn an_added_file_starts_at_zero() {
        let result = diff_sides("f", &Mem(None), &Mem(Some(b"a\nb\n".to_vec())), CAPS).unwrap();
        assert_eq!(
            render(result.hunks.as_ref().unwrap()),
            "@@ -0,0 +1,2 @@\n+a\n+b\n"
        );
    }

    #[test]
    fn binary_sides_have_no_hunks() {
        let result = diff_sides(
            "f",
            &Mem(Some(b"a\0b".to_vec())),
            &Mem(Some(b"text\n".to_vec())),
            CAPS,
        )
        .unwrap();
        assert_eq!((result.binary, result.too_large), (Some(true), false));
        assert_eq!(result.hunks, Some(vec![]));
    }

    #[test]
    fn long_lines_are_cut_and_flagged() {
        let long = "x".repeat(MAX_LINE_CHARS + 10);
        let result = diff_sides("f", &Mem(None), &Mem(Some(long.into_bytes())), CAPS).unwrap();
        let line = &result.hunks.unwrap()[0].lines[0];
        assert!(line.truncated);
        assert_eq!(line.text.chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn a_patch_over_the_auto_render_caps_reports_stats_only() {
        let after: String = (0..50).map(|i| format!("{i}\n")).collect();
        let caps = DiffCaps {
            auto_render_max_lines: 10,
            ..CAPS
        };
        let result = diff_sides("f", &Mem(None), &Mem(Some(after.into())), caps).unwrap();
        assert!(result.too_large);
        assert_eq!(result.lines, Some(50));
        assert!(result.hunks.is_none() && result.bytes.is_some());
    }
}
