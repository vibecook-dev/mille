//! Ids, paths and manifests: what may name an item, a version and a file, and
//! the content address a version's id is.
//!
//! An item id and a version id each become a folder name under `built/`, so
//! both grammars hold on every file system mille runs on: an item id is
//! `[a-z0-9][a-z0-9-]{0,63}` (and not a Windows device name), a version id is
//! 64 lowercase hex. Neither may hold `:` or `/`, which is what lets the
//! document join them as `<item>/<version>`. A host with its own ids (a URL, a
//! fold key with colons) names the item by a digest of its id and keeps the id
//! itself in the item's meta.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::digest::Blake3;
use crate::error::{DriveError, Result};

/// The longest item id, attachment name or meta key.
pub const MAX_ID_LEN: usize = 64;
/// The most files one version may hold.
pub const MAX_FILES: usize = 65_536;
/// The longest relative path in a manifest, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// The longest path segment, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 255;
/// The longest meta value, in bytes.
pub const MAX_META_BYTES: usize = 64 * 1024;

/// A version's id: the 64-hex blake3 of its canonical manifest.
pub type VersionId = String;

/// One file of a version: where it sits in the tree, and the object it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// A safe, relative, `/`-separated path ([`is_portable_relative_path`]).
    pub path: String,
    pub blake3: Blake3,
    pub bytes: u64,
}

/// Names Windows reserves in every folder, whatever the extension.
const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn is_reserved(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    let stem = stem.trim_end_matches(' ').to_ascii_lowercase();
    RESERVED.contains(&stem.as_str())
}

/// `[a-z0-9][a-z0-9-]{0,63}`, and not a Windows device name.
pub fn is_valid_item_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    let lower_alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    bytes.len() <= MAX_ID_LEN
        && lower_alnum(first)
        && bytes.iter().all(|b| lower_alnum(b) || *b == b'-')
        && !is_reserved(id)
}

/// `[a-z0-9][a-z0-9._-]{0,63}`: an attachment's name or a meta key. Never a
/// folder name, so a dot is allowed — but not `..`.
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    let allowed = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b);
    bytes.len() <= MAX_ID_LEN
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.iter().all(allowed)
        && !name.contains("..")
}

/// 64 lowercase hex.
pub fn is_valid_version_id(id: &str) -> bool {
    id.parse::<Blake3>().is_ok()
}

pub(crate) fn check_item_id(id: &str) -> Result<()> {
    if is_valid_item_id(id) {
        return Ok(());
    }
    let message = format!("{id:?} is not an item id: [a-z0-9][a-z0-9-], at most 64");
    Err(DriveError::invalid(message))
}

pub(crate) fn check_version_id(id: &str) -> Result<()> {
    if is_valid_version_id(id) {
        return Ok(());
    }
    let message = format!("{id:?} is not a version id: 64 lowercase hex");
    Err(DriveError::invalid(message))
}

pub(crate) fn check_name(what: &str, name: &str) -> Result<()> {
    if is_valid_name(name) {
        return Ok(());
    }
    let message = format!("{name:?} is not a valid {what}: [a-z0-9][a-z0-9._-], at most 64");
    Err(DriveError::invalid(message))
}

/// A path a version may hold, portable to every desktop that builds it.
///
/// mille-git's rule — non-empty, no NUL, no backslash, not absolute (`/`, `~`
/// or a drive letter), no empty, `.` or `..` segment — plus what Windows
/// refuses in a name: `< > : " | ? *`, control characters, a segment ending
/// in a dot or a space, and the device names. Bounded by [`MAX_PATH_BYTES`]
/// and [`MAX_SEGMENT_BYTES`].
pub fn is_portable_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return false;
    }
    if path.starts_with('/') || path.starts_with('~') {
        return false;
    }
    path.split('/').all(is_portable_segment)
}

fn is_portable_segment(segment: &str) -> bool {
    let forbidden = |c: char| c.is_control() || "<>:\"|?*\\".contains(c);
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_BYTES
        && segment != "."
        && segment != ".."
        && !segment.chars().any(forbidden)
        && !segment.ends_with('.')
        && !segment.ends_with(' ')
        && !is_reserved(segment)
}

/// Refuse a manifest any desktop could not build: no files, too many, an
/// unsafe path, two paths that differ only by case, or a file where another
/// path needs a folder.
pub(crate) fn validate_manifest(files: &[FileEntry]) -> Result<()> {
    if files.is_empty() {
        return Err(DriveError::invalid("a version holds at least one file"));
    }
    if files.len() > MAX_FILES {
        let message = format!("a version holds at most {MAX_FILES} files");
        return Err(DriveError::invalid(message));
    }
    let mut seen = BTreeSet::new();
    let mut folders = BTreeSet::new();
    for file in files {
        if !is_portable_relative_path(&file.path) {
            let message = format!("{:?} is not a portable relative path", file.path);
            return Err(DriveError::invalid(message));
        }
        let folded = file.path.to_lowercase();
        if !seen.insert(folded.clone()) {
            let message = format!("{:?} is in the version twice (ignoring case)", file.path);
            return Err(DriveError::invalid(message));
        }
        let mut prefix = String::new();
        let segments: Vec<&str> = folded.split('/').collect();
        for segment in &segments[..segments.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            folders.insert(prefix.clone());
        }
    }
    if let Some(clash) = seen.intersection(&folders).next() {
        let message = format!("{clash:?} is both a file and a folder in the version");
        return Err(DriveError::invalid(message));
    }
    Ok(())
}

/// The canonical encoding a version id hashes: a header line, then one line
/// per file in byte order of path — `path NUL hex NUL bytes LF`. Independent of
/// the order a caller listed the files in, so the same tree is the same
/// version whoever makes it.
pub(crate) fn canonical_manifest(files: &[FileEntry]) -> Vec<u8> {
    let mut sorted: Vec<&FileEntry> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut out = b"mille-drive manifest v1\n".to_vec();
    for file in sorted {
        out.extend_from_slice(file.path.as_bytes());
        out.push(0);
        out.extend_from_slice(file.blake3.to_hex().as_bytes());
        out.push(0);
        out.extend_from_slice(file.bytes.to_string().as_bytes());
        out.push(b'\n');
    }
    out
}

/// A version's id: the blake3 of its canonical manifest, as 64 hex.
pub fn version_id(files: &[FileEntry]) -> VersionId {
    Blake3::of(&canonical_manifest(files)).to_hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, data: &[u8]) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            blake3: Blake3::of(data),
            bytes: data.len() as u64,
        }
    }

    #[test]
    fn item_ids_are_lowercase_alnum_and_dash() {
        for good in ["a", "app-1", "0abc"] {
            assert!(is_valid_item_id(good), "{good}");
        }
        assert!(is_valid_item_id(&"f".repeat(64)));
        assert!(!is_valid_item_id(&"a".repeat(65)));
        for bad in [
            "", "-a", "A", "a_b", "a.b", "a/b", "a:b", "con", "nul", "lpt1",
        ] {
            assert!(!is_valid_item_id(bad), "{bad}");
        }
    }

    #[test]
    fn names_allow_a_dot_but_not_two() {
        assert!(is_valid_name("preview.png"));
        assert!(is_valid_name("fold-key"));
        assert!(!is_valid_name(".hidden"));
        assert!(!is_valid_name("a..b"));
        assert!(!is_valid_name("a/b"));
        assert!(!is_valid_name("A"));
    }

    #[test]
    fn version_ids_are_hex_with_no_prefix() {
        let id = version_id(&[entry("index.html", b"<p>")]);
        assert!(is_valid_version_id(&id));
        assert!(!is_valid_version_id(&format!("b3:{id}")));
    }

    #[test]
    fn unsafe_and_unportable_paths_are_refused() {
        for good in ["index.html", "a/b/c.js", "assets/logo 2.png", "é/ü.txt"] {
            assert!(is_portable_relative_path(good), "{good}");
        }
        for bad in [
            "", "/abs", "~/home", "C:/x", "a\\b", "a//b", "./a", "a/../b", "a\0b", "a/b.", "a/b ",
            "a:b", "q?", "aux.txt", "dir/CON",
        ] {
            assert!(!is_portable_relative_path(bad), "{bad:?}");
        }
        assert!(!is_portable_relative_path(&"a".repeat(256)));
        let deep = vec!["d"; 600].join("/");
        assert!(!is_portable_relative_path(&deep));
    }

    fn refused(files: &[FileEntry]) -> bool {
        validate_manifest(files).is_err()
    }

    #[test]
    fn manifests_refuse_duplicates_case_folds_and_file_folder_clashes() {
        assert!(refused(&[]));
        assert!(!refused(&[entry("a", b"1"), entry("b/c", b"2")]));
        assert!(refused(&[entry("a", b"1"), entry("a", b"2")]));
        assert!(refused(&[entry("Read.md", b"1"), entry("read.MD", b"2")]));
        assert!(refused(&[entry("a", b"1"), entry("a/b", b"2")]));
        assert!(refused(&[entry("x/A/b", b"1"), entry("x/a", b"2")]));
        assert!(refused(&[entry("../a", b"1")]));
    }

    #[test]
    fn a_version_id_does_not_depend_on_file_order() {
        let one = vec![entry("b", b"2"), entry("a", b"1"), entry("c/d", b"3")];
        let mut two = one.clone();
        two.reverse();
        assert_eq!(version_id(&one), version_id(&two));
        let changed = vec![entry("b", b"2"), entry("a", b"other"), entry("c/d", b"3")];
        assert_ne!(version_id(&one), version_id(&changed));
        let renamed = vec![entry("b", b"2"), entry("z", b"1"), entry("c/d", b"3")];
        assert_ne!(version_id(&one), version_id(&renamed));
    }

    #[test]
    fn too_many_files_is_refused() {
        let files: Vec<FileEntry> = (0..=MAX_FILES)
            .map(|i| entry(&format!("f{i}"), b""))
            .collect();
        assert!(validate_manifest(&files).is_err());
    }
}
