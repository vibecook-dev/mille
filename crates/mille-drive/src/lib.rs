//! mille-drive — a drive: immutable versions of trees, held by every replica
//! and synced among them.
//!
//! - **Objects** are files named by their blake3 under `objects/`, written
//!   once and read-only after, verified on every way in (`store`).
//! - **The document** is a Loro document of flat root maps: items, their
//!   versions (manifests of path, blake3, size), and intents — current, kept,
//!   hidden, meta, attachments (`doc`). Bytes never ride it.
//! - **Builds** lay a version out as a folder of hard links to its objects
//!   (`build`), and `built/` is the drive's read-only export (`export`).
//! - **Sync** is a core with no I/O of its own: frames in, actions out
//!   (`sync`), carried by a host's `Transport` (`transport`).
//!
//! The engine knows trees, versions, objects and intents — never what an
//! item means to the host that made it. See `README.md` for the on-disk
//! layout, the document's schema and the protocol.

mod build;
mod digest;
mod doc;
mod drive;
mod error;
mod export;
mod layout;
mod lock;
mod store;
mod sync;
mod transport;
mod version;

pub use digest::{object_relpath, Blake3, BLAKE3_HEX_LEN};
pub use drive::{
    BuildIntoOptions, ChangeKind, ChangeListener, Drive, DriveChange, DriveOptions, GcOptions,
    HoldPolicy, ImportReport, ItemView, KeptScope, VersionKey, VersionView,
    DEFAULT_MAX_CONCURRENT_PULLS, DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_OBJECT_BYTES,
    DEFAULT_MAX_UPDATE_BYTES, MIN_FRAME_BYTES,
};
pub use error::{DriveError, DriveErrorCode, Result};
pub use export::ExportDescriptor;
pub use store::{GcReport, ObjectRef, PutHow, PutMode, PutOutcome};
pub use sync::{
    SyncAction, SyncStatus, FRAME_VERSION, MAX_BACKOFF_TICKS, MAX_DIGESTS_PER_FRAME,
    PULL_EXPIRY_TICKS,
};
pub use transport::{
    apply_actions, ApplyReport, MemoryNetwork, MemoryTransport, Transport, TransportError,
};
pub use version::{
    is_portable_relative_path, is_valid_item_id, is_valid_name, is_valid_version_id, version_id,
    FileEntry, VersionId, MAX_FILES, MAX_ID_LEN, MAX_META_BYTES,
};
