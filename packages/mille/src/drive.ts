// `@vibecook/mille/drive` — the drive (crates/mille-drive): a store of
// immutable versions by blake3, the synced document that names them, and the
// sync loop that joins a drive to its peers over a host's transport.
//
// Node / Electron UtilityProcess only: this entry loads the native binary.
//
// This file stays at the TOP of src/ and imports the binary as exactly
// `./native.js`, the way the root entry does: a host that bundles mille swaps
// that one specifier for its own loader (VibeField's files worker), and the
// git entry's path through native-loader.js would bypass the swap. The
// import throws when no binary loads, as the root entry's does; a binary
// built without the drive loads, and `isNativeDriveAvailable()` is `false`.
//
// The root `@vibecook/mille` barrel MUST NOT re-export this one.

import { bindNativeDrive } from './drive/engine.js';
import { native } from './native.js';

bindNativeDrive(native.Drive);

export { Drive, isNativeDriveAvailable } from './drive/engine.js';
export type {
  AddVersionOptions,
  BuildIntoOptions,
  GcOptions,
  NativeDrive,
  PutFileOptions,
} from './drive/engine.js';
export { DriveError, isDriveError } from './drive/error.js';
export { isBlake3Hex, objectRelpath } from './drive/layout.js';
export { createMemoryDriveNetwork } from './drive/memory.js';
export type { MemoryDriveNetwork } from './drive/memory.js';
export { DEFAULT_DRIVE_TICK_MS, syncDrive } from './drive/sync.js';
export type { DriveSync, SyncDriveOptions } from './drive/sync.js';
export type {
  Blake3Hex,
  DriveChange,
  DriveErrorCode,
  DriveExport,
  DriveFile,
  DriveHoldPolicy,
  DriveItem,
  DriveOptions,
  DrivePeerEvent,
  DrivePutMode,
  DriveSyncAction,
  DriveSyncCounts,
  DriveSyncError,
  DriveSyncState,
  DriveSyncStatus,
  DriveTransport,
  DriveVersion,
  GcReport,
  ObjectRef,
  PutOutcome,
  VersionKey,
} from './drive/types.js';
