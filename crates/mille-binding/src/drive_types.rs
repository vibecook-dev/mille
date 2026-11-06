//! `#[napi(object)]` mirrors for what JavaScript hands the drive, and the one
//! object it hands back that is not JSON text (sync actions, whose frames are
//! bytes). `cfg(feature = "drive")`.
//!
//! As with the git engine (`git_types.rs`), only inputs are mirrored; results
//! cross as JSON text built from `mille-drive`'s own `Serialize` derives.
//! Sizes are `i64` here because an object limit may pass 4 GiB; they are
//! refused when negative.

use std::path::PathBuf;

use mille_drive::{
    BuildIntoOptions, DriveError, DriveErrorCode, DriveOptions, HoldPolicy, KeptScope, PutMode,
};
use napi::bindgen_prelude::Buffer;

/// `DriveOptions` from `@vibecook/mille/drive`, flattened (`hold.*` become
/// `hold*`). Absent fields take mille-drive's defaults.
#[napi(object)]
pub struct DriveOptionsJs {
    /// The drive's folder; created when missing.
    pub root: String,
    pub device: String,
    pub label: Option<String>,
    pub hold_current: Option<bool>,
    /// `'current' | 'all'`.
    pub hold_kept_versions: Option<String>,
    pub hold_attachments: Option<bool>,
    pub max_object_bytes: Option<i64>,
    pub max_frame_bytes: Option<u32>,
    pub max_update_bytes: Option<i64>,
    pub max_concurrent_pulls: Option<u32>,
    pub staging_dir: Option<String>,
}

fn invalid(message: impl Into<String>) -> DriveError {
    DriveError::new(DriveErrorCode::Invalid, message)
}

fn size(name: &str, value: Option<i64>, default: u64) -> Result<u64, DriveError> {
    match value {
        None => Ok(default),
        Some(n) if n > 0 => Ok(n as u64),
        Some(n) => Err(invalid(format!("{name} must be positive, got {n}"))),
    }
}

impl DriveOptionsJs {
    pub(crate) fn into_options(self) -> Result<(PathBuf, DriveOptions), DriveError> {
        let mut options = DriveOptions::new(self.device);
        if let Some(label) = self.label {
            options.label = label;
        }
        let defaults = HoldPolicy::default();
        options.hold = HoldPolicy {
            current: self.hold_current.unwrap_or(defaults.current),
            kept_versions: match self.hold_kept_versions.as_deref() {
                None => defaults.kept_versions,
                Some("current") => KeptScope::Current,
                Some("all") => KeptScope::All,
                Some(other) => {
                    let message = format!("holdKeptVersions is 'current' or 'all', not {other:?}");
                    return Err(invalid(message));
                }
            },
            attachments: self.hold_attachments.unwrap_or(defaults.attachments),
        };
        options.max_object_bytes = size(
            "maxObjectBytes",
            self.max_object_bytes,
            options.max_object_bytes,
        )?;
        let update_default = options.max_update_bytes as u64;
        let max_update = size("maxUpdateBytes", self.max_update_bytes, update_default)?;
        options.max_update_bytes = usize::try_from(max_update).unwrap_or(usize::MAX);
        if let Some(bytes) = self.max_frame_bytes {
            options.max_frame_bytes = bytes as usize;
        }
        if let Some(pulls) = self.max_concurrent_pulls {
            options.max_concurrent_pulls = pulls as usize;
        }
        let staging = self.staging_dir.filter(|dir| !dir.is_empty());
        options.staging_dir = staging.map(PathBuf::from);
        Ok((PathBuf::from(self.root), options))
    }
}

pub(crate) fn put_mode(mode: Option<&str>) -> Result<PutMode, DriveError> {
    match mode {
        None | Some("link-or-copy") => Ok(PutMode::LinkOrCopy),
        Some("link") => Ok(PutMode::Link),
        Some("copy") => Ok(PutMode::Copy),
        Some(other) => {
            let message = format!("mode is 'link', 'copy' or 'link-or-copy', not {other:?}");
            Err(invalid(message))
        }
    }
}

/// `putFile`'s options.
#[napi(object)]
pub struct PutOptionsJs {
    /// `'link' | 'copy' | 'link-or-copy'` (the default).
    pub mode: Option<String>,
    /// The digest the file must hash to, or it is refused (`EMISMATCH`).
    pub expect: Option<String>,
}

/// `buildInto`'s options.
#[napi(object)]
pub struct BuildIntoOptionsJs {
    /// `'link' | 'copy' | 'link-or-copy'` (the default).
    pub mode: Option<String>,
    /// Paths of the version to leave out.
    pub except: Option<Vec<String>>,
    /// Replace an existing destination instead of `EEXIST`, by two renames
    /// (the destination is briefly absent).
    pub replace: Option<bool>,
}

impl BuildIntoOptionsJs {
    pub(crate) fn into_options(self) -> Result<BuildIntoOptions, DriveError> {
        Ok(BuildIntoOptions {
            mode: put_mode(self.mode.as_deref())?,
            except: self.except.unwrap_or_default(),
            replace: self.replace.unwrap_or(false),
        })
    }
}

/// Sync actions for the TypeScript loop: `json` is the array of actions
/// (`{kind: 'send', peer, frame}` with `frame` an index into `frames`, or
/// `{kind: 'pull', peer, blake3, into}`), and `frames` carries the bytes, so
/// no frame is ever base64 in JSON.
#[napi(object)]
pub struct DriveSyncActionsJs {
    pub json: String,
    pub frames: Vec<Buffer>,
}
