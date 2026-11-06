// The shapes of `@vibecook/mille/drive`. Every result type here is the JSON a
// `mille-drive` serde derive writes (crates/mille-drive), so the two change
// together.

/** A blake3 digest: 64 lowercase hex characters. */
export type Blake3Hex = string;

/**
 * Every code a drive call can fail with: mille-drive's own (its
 * `DriveErrorCode`), plus two of this wrapper's. `EUNSUPPORTED`: the loaded
 * native binary has no drive (built with `--no-default-features`).
 * `EUNKNOWN`: a code this wrapper does not know, from a newer binary.
 */
export type DriveErrorCode =
  | 'EIO'
  | 'ENOENT'
  | 'EEXIST'
  | 'EMISMATCH'
  | 'ETOOBIG'
  | 'ENOSPC'
  | 'EINVAL'
  | 'EBUSY'
  | 'EFORMAT'
  | 'EPROTO'
  | 'ETRANSPORT'
  | 'EUNKNOWNITEM'
  | 'EUNKNOWNVERSION'
  | 'EUNSUPPORTED'
  | 'EUNKNOWN';

export interface DriveHoldPolicy {
  /** Hold every item's current version. Default `true`. */
  readonly current?: boolean;
  /** Which versions of a kept item to hold. Default `'current'`. */
  readonly keptVersions?: 'current' | 'all';
  /** Hold the attachments (previews) of what is held. Default `true`. */
  readonly attachments?: boolean;
}

export interface DriveOptions {
  /** The drive's folder; created when missing. One open `Drive` per folder. */
  readonly root: string;
  /** This desktop's name for itself, recorded on each version it makes. */
  readonly device: string;
  /** The export's label. Default `'Drive'`. */
  readonly label?: string;
  readonly hold?: DriveHoldPolicy;
  /**
   * The object size limit (default 64 MiB). A host whose transport has its
   * own limit passes that one value to both.
   */
  readonly maxObjectBytes?: number;
  /** The largest frame the sync core hands a transport (default 4 MiB, at least 4096). */
  readonly maxFrameBytes?: number;
  /** The largest update a peer may send in chunks (default 256 MiB). */
  readonly maxUpdateBytes?: number;
  /** Pulls in flight at once (default 4). */
  readonly maxConcurrentPulls?: number;
  /**
   * Where temporaries go (default `<root>/staging`). A folder given here must
   * be on the drive's volume (`EINVAL` at open otherwise) and is this drive's
   * alone while it is open (`EBUSY` for a second drive).
   */
  readonly stagingDir?: string;
}

export type DrivePutMode = 'link' | 'copy' | 'link-or-copy';

export interface ObjectRef {
  readonly blake3: Blake3Hex;
  readonly bytes: number;
}

export interface PutOutcome {
  readonly object: ObjectRef;
  readonly how: 'linked' | 'copied' | 'existed';
}

/** One file of a version. `path` is relative and `/`-separated. */
export interface DriveFile {
  readonly path: string;
  readonly blake3: Blake3Hex;
  readonly bytes: number;
}

export interface DriveVersion {
  /** 64 hex: the blake3 of the version's canonical manifest. */
  readonly id: string;
  readonly files: readonly DriveFile[];
  readonly createdAt: number;
  readonly device: string;
  readonly attachments: Readonly<Record<string, ObjectRef>>;
  /** The record does not match its id: listed, never built, held or served. */
  readonly quarantined: boolean;
  /** Every file's object is held here. */
  readonly held: boolean;
}

export interface DriveItem {
  /** `[a-z0-9][a-z0-9-]{0,63}`. */
  readonly id: string;
  readonly meta: Readonly<Record<string, string>>;
  readonly current: string | null;
  readonly kept: boolean;
  readonly hidden: boolean;
  /** Oldest first. */
  readonly versions: readonly DriveVersion[];
}

export interface VersionKey {
  readonly item: string;
  readonly version: string;
}

export interface GcReport {
  readonly dryRun: boolean;
  readonly removed: readonly ObjectRef[];
  readonly retained: number;
  readonly bytesFreed: number;
}

/** The drive as a read-only export of its built versions. */
export interface DriveExport {
  readonly kind: 'drive';
  readonly label: string;
  readonly access: 'read-only';
  readonly roots: readonly string[];
  readonly followSymlinks: false;
}

export interface DriveChange {
  /** A local write, an update from a peer, or an object arriving. */
  readonly kind: 'local' | 'import' | 'held';
  readonly items: readonly string[];
}

/** What a host's sync loop does next. */
export type DriveSyncAction =
  | { readonly kind: 'send'; readonly peer: string; readonly frame: Uint8Array }
  | {
      readonly kind: 'pull';
      readonly peer: string;
      readonly blake3: Blake3Hex;
      /** A free name in the drive's staging folder; landing elsewhere on the volume is fine. */
      readonly into: string;
    };

/** The drive's own counts, from `Drive.syncStatus()`. */
export interface DriveSyncCounts {
  readonly peers: readonly string[];
  readonly pulling: number;
  readonly wants: number;
  /** Wants no open peer has offered. */
  readonly unsourced: number;
  /**
   * Objects the hold policy names that this drive can never hold: every
   * reference declares more than its `maxObjectBytes`. Not wanted, never pulled.
   */
  readonly oversized: number;
}

/**
 * How a drive reaches its peers. VibeField implements it over its desktop's
 * one truffle node; mille-truffle will over its own (M-D3).
 *
 * Frames to one peer must arrive in the order sent. A pull fetches the
 * holder's `objectsRoot()` joined with `objectRelpath(blake3)`, lands it on
 * the drive's volume (at `intoPath`, or anywhere there) and resolves to where
 * it landed. A transport verifies nothing and never retries: the drive does
 * both.
 */
export interface DriveTransport {
  send(peer: string, frame: Uint8Array): void | Promise<void>;
  pull(peer: string, blake3: Blake3Hex, intoPath: string): Promise<{ readonly path: string }>;
  onFrame(listener: (peer: string, frame: Uint8Array) => void): () => void;
  onPeer(listener: (event: DrivePeerEvent) => void): () => void;
}

export interface DrivePeerEvent {
  readonly peer: string;
  readonly state: 'up' | 'down';
}

/**
 * - `idle`: nothing wanted, nothing moving.
 * - `copying`: objects are being pulled.
 * - `waiting`: objects are wanted and none is moving — no reachable peer has
 *   offered them yet.
 */
export type DriveSyncState = 'idle' | 'copying' | 'waiting';

export interface DriveSyncError {
  readonly code: DriveErrorCode;
  readonly message: string;
  readonly peer?: string;
}

export interface DriveSyncStatus {
  readonly state: DriveSyncState;
  readonly peers: readonly string[];
  readonly pulling: number;
  readonly wants: number;
  readonly unsourced: number;
  /** Objects over this drive's `maxObjectBytes`: named, never to be held here. */
  readonly oversized: number;
  /** The last failure, kept until a later one replaces it. */
  readonly lastError: DriveSyncError | null;
}
