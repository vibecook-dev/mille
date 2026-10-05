//! `git status --porcelain=v2 -z` parser — the CLI fallback when gix status
//! fails (SPEC mille-git §10.3), and the golden reference the gix mapping is
//! checked against.
//!
//! It follows `parsePorcelainV2` / `parseLetter` / `xyToEntry` in
//! `packages/mille-ui/src/git/shell-client.ts`, keeping XY instead of
//! collapsing it, and shares the golden fixtures under
//! `tests/fixtures/porcelain/` with the JavaScript parser:
//!
//! - `T` (type change) is the only remap: it becomes `M`.
//! - An unknown glyph is **not** remapped to `M`: `parseLetter` returns null
//!   for it, so the slot reads as `.`, and a row whose two slots are both
//!   `.` is skipped (§10.1).
//! - `!` (ignored) rows are skipped; `#` headers are not entries.
//! - A `2` record spans two NUL-separated chunks: `<new>\0<orig>`.
//!
//! One deliberate difference: the TypeScript parser finds an unmerged
//! (`u`) row's path with `lastIndexOf(' ')`, which truncates a path that
//! contains a space. This parser counts the record's fixed fields instead,
//! as it already does for `1` and `2`.

use crate::status::{StatusEntry, StatusLetter, StatusXy};

/// Fixed space-separated fields before the path, per record type:
/// `1 XY sub mH mI mW hH hI <path>`
/// `2 XY sub mH mI mW hH hI Xscore <path>`
/// `u XY sub m1 m2 m3 mW h1 h2 h3 <path>`
const ORDINARY_FIELDS: usize = 8;
const RENAME_FIELDS: usize = 9;
const UNMERGED_FIELDS: usize = 10;

/// Parse a buffered `-z` stream. Malformed rows are skipped and reported to
/// `warn` (the engine forwards them as `PARSE` warnings); they never poison
/// the rest of the snapshot.
pub fn parse_porcelain_v2(buffer: &[u8], warn: &mut dyn FnMut(String)) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    let mut tokens = buffer.split(|&b| b == 0).peekable();
    while let Some(token) = tokens.next() {
        if token.is_empty() {
            continue;
        }
        match token[0] {
            b'1' => match split_record(token, ORDINARY_FIELDS) {
                Some((x, y, path)) => entries.extend(xy_entry(x, y, path, None)),
                None => warn(malformed('1', token)),
            },
            b'2' => {
                // Always consume the orig-path chunk, even for a malformed
                // head, so it is not misread as a record of its own.
                let orig = tokens.next();
                match (split_record(token, RENAME_FIELDS), orig) {
                    (Some((x, y, path)), Some(orig)) if !orig.is_empty() => {
                        entries.extend(xy_entry(x, y, path, Some(lossy(orig))));
                    }
                    _ => warn(malformed('2', token)),
                }
            }
            b'u' => match split_record(token, UNMERGED_FIELDS) {
                Some((x, y, path)) => {
                    let xy = StatusXy {
                        index: StatusLetter::from_porcelain(x),
                        worktree: StatusLetter::from_porcelain(y),
                    };
                    entries.push(StatusEntry::conflicted(path, xy));
                }
                None => warn(malformed('u', token)),
            },
            b'?' => {
                let path = token.get(2..).unwrap_or_default();
                if path.is_empty() {
                    warn(malformed('?', token));
                } else {
                    entries.push(StatusEntry::untracked(lossy(path)));
                }
            }
            b'!' | b'#' => {}
            _ => warn(format!(
                "mille-git: unrecognised status tag: {:?}",
                String::from_utf8_lossy(&token[..token.len().min(40)])
            )),
        }
    }
    entries
}

fn xy_entry(x: u8, y: u8, path: String, orig_path: Option<String>) -> Option<StatusEntry> {
    let xy = StatusXy {
        index: StatusLetter::from_porcelain(x),
        worktree: StatusLetter::from_porcelain(y),
    };
    if xy.index.is_none() && xy.worktree.is_none() {
        return None;
    }
    Some(StatusEntry::tracked(path, orig_path, xy))
}

/// `(X, Y, path)` from a record with `fields` fixed fields before the path.
fn split_record(token: &[u8], fields: usize) -> Option<(u8, u8, String)> {
    if token.len() < 5 || token[1] != b' ' {
        return None;
    }
    let (x, y) = (token[2], token[3]);
    let mut spaces = 0;
    let start = token.iter().position(|&b| {
        if b == b' ' {
            spaces += 1;
        }
        spaces == fields
    })?;
    let path = token.get(start + 1..)?;
    if path.is_empty() {
        return None;
    }
    Some((x, y, lossy(path)))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn malformed(tag: char, token: &[u8]) -> String {
    format!(
        "mille-git: malformed '{tag}' line: {}",
        String::from_utf8_lossy(token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(buffer: &[u8]) -> (Vec<StatusEntry>, Vec<String>) {
        let mut warnings = Vec::new();
        let entries = parse_porcelain_v2(buffer, &mut |w| warnings.push(w));
        (entries, warnings)
    }

    #[test]
    fn a_path_with_spaces_survives_every_record_type() {
        let h = "0000000000000000000000000000000000000000";
        let buffer = format!(
            "1 .M N... 100644 100644 100644 {h} {h} a b.ts\0\
             2 R. N... 100644 100644 100644 {h} {h} R100 new name.ts\0old name.ts\0\
             u UU N... 100644 100644 100644 100644 {h} {h} {h} both sides.ts\0\
             ? un tracked.ts\0"
        );
        let (entries, warnings) = parse(buffer.as_bytes());
        assert!(warnings.is_empty(), "{warnings:?}");
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["a b.ts", "new name.ts", "both sides.ts", "un tracked.ts"]
        );
        assert_eq!(entries[1].orig_path.as_deref(), Some("old name.ts"));
        assert!(entries[2].conflicted);
    }

    #[test]
    fn a_truncated_rename_does_not_swallow_the_next_record() {
        let (entries, warnings) = parse(b"2 R. short\0orig\0? next.ts\0");
        assert_eq!(warnings.len(), 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "next.ts");
    }
}
