//! The drive as a read-only, browsable export (DR-D8 (a)).
//!
//! The descriptor is shaped to drop into mille-truffle's export config and a
//! read-only explorer session: one root, the drive's `built/` folder, where
//! every built version is a plain folder of hard links. Objects and the
//! document are never in it.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportDescriptor {
    /// Always `"drive"`.
    pub kind: String,
    pub label: String,
    /// Always `"read-only"`: a session over a drive never writes, because a
    /// built file is a hard link to an object, and a write through it would
    /// corrupt the object for every name it has.
    pub access: String,
    /// Canonical, absolute: `<root>/built`.
    pub roots: Vec<PathBuf>,
    /// Always `false`.
    pub follow_symlinks: bool,
}

impl ExportDescriptor {
    pub(crate) fn drive(label: &str, built: &Path) -> Self {
        Self {
            kind: "drive".to_owned(),
            label: label.to_owned(),
            access: "read-only".to_owned(),
            roots: vec![built.to_path_buf()],
            follow_symlinks: false,
        }
    }
}
