// The typed wrapper over the napi `Drive` class (crates/mille-binding/src/drive.rs).
//
// Node / Electron UtilityProcess only. The class is in every `.node` built
// with mille-binding's default features, which is every published one and the
// plain `build:napi`; `--no-default-features` leaves it out, and
// `isNativeDriveAvailable()` says which one this process loaded.
//
// This module never loads the binary itself. `drive.ts` (the entry) imports
// `./native.js` and hands the class in through `bindNativeDrive`, because a
// host that bundles mille swaps exactly that specifier for its own loader
// (VibeField's files worker); a lookup from here would bypass the swap.
//
// What this layer adds over the raw class, and nothing more:
//   - results arrive as JSON text and are parsed here;
//   - `DRIVE|…` rejections become `DriveError`;
//   - sync actions arrive as `{json, frames}` and leave as `DriveSyncAction[]`.

import { DriveError, toDriveError, wrapDrive, wrapDriveAsync } from './error.js';
import type {
  Blake3Hex,
  DriveChange,
  DriveExport,
  DriveFile,
  DriveItem,
  DriveOptions,
  DrivePutMode,
  DriveSyncAction,
  DriveSyncCounts,
  GcReport,
  ObjectRef,
  PutOutcome,
  VersionKey,
} from './types.js';

type JsonListener = (json: string) => void;

/** Sync actions as the binding returns them: frames by index into `frames`. */
interface NativeDriveSyncActions {
  readonly json: string;
  readonly frames: readonly Buffer[];
}

// `undefined` is napi's `None`, so optional fields may be passed through as-is.
interface NativePutOptions {
  mode?: string | undefined;
  expect?: string | undefined;
}

interface NativeBuildIntoOptions {
  mode?: string | undefined;
  except?: string[] | undefined;
  replace?: boolean | undefined;
}

/** The raw napi class, as `crates/mille-binding/src/drive.rs` defines it. */
export interface NativeDrive {
  root(): string;
  objectsRoot(): string;
  has(blake3: string): boolean;
  objectPath(blake3: string): string | null;
  putFile(src: string, options?: NativePutOptions): Promise<string>;
  putBytes(data: Buffer): Promise<string>;
  verify(blake3: string): Promise<boolean>;
  gc(dryRun?: boolean, keepJson?: string): Promise<string>;
  addVersion(item: string, filesJson: string, makeCurrent?: boolean): string;
  setCurrent(item: string, version: string): void;
  setKept(item: string, kept: boolean): void;
  setHidden(item: string, hidden: boolean): void;
  setMeta(item: string, key: string, value: string | null): void;
  setAttachment(item: string, version: string, name: string, objectJson: string | null): void;
  items(): string;
  item(id: string): string | null;
  wants(): string[];
  build(item: string, version: string): Promise<string>;
  buildInto(
    item: string,
    version: string,
    dest: string,
    options?: NativeBuildIntoOptions,
  ): Promise<void>;
  unbuild(item: string, version: string): Promise<void>;
  exportDescriptor(): string;
  versionVector(): Buffer;
  syncOpen(peer: string): NativeDriveSyncActions;
  syncHandle(peer: string, frame: Buffer): NativeDriveSyncActions;
  syncLocalChanged(): NativeDriveSyncActions;
  syncPulled(peer: string, blake3: string, path: string): Promise<NativeDriveSyncActions>;
  syncPullFailed(peer: string, blake3: string): NativeDriveSyncActions;
  syncTick(): NativeDriveSyncActions;
  syncClose(peer: string): void;
  syncStatus(): string;
  onChange(listener: JsonListener): number;
  off(subscriptionId: number): boolean;
  dispose(): void;
}

type NativeDriveCtor = new (options: Record<string, unknown>) => NativeDrive;

let nativeDriveCtor: NativeDriveCtor | null = null;

/**
 * @internal — called once by `drive.ts` with the loaded binary's `Drive`
 * export (`undefined` when it was built without the drive).
 */
export function bindNativeDrive(ctor: unknown): void {
  nativeDriveCtor = typeof ctor === 'function' ? (ctor as NativeDriveCtor) : null;
}

/**
 * Whether this process loaded a `.node` that carries the drive. `false` only
 * for a binary built with `--no-default-features`. Never throws.
 */
export function isNativeDriveAvailable(): boolean {
  return nativeDriveCtor !== null;
}

function defined<T extends Record<string, unknown>>(value: T): T {
  for (const key of Object.keys(value)) {
    if (value[key] === undefined) delete value[key];
  }
  return value;
}

function listen<T>(cb: (value: T) => void): JsonListener {
  return (json: string) => {
    let value: T;
    try {
      value = JSON.parse(json) as T;
    } catch {
      return;
    }
    try {
      cb(value);
    } catch {
      // Swallowed, as git's listeners are: a listener bug must not crash the
      // host process from a native callback.
    }
  };
}

/** A view of `bytes` as a `Buffer`, without copying. */
function asBuffer(bytes: Uint8Array): Buffer {
  if (Buffer.isBuffer(bytes)) return bytes;
  return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

interface SendJson {
  readonly kind: 'send';
  readonly peer: string;
  /** An index into the actions' `frames`. */
  readonly frame: number;
}

interface PullJson {
  readonly kind: 'pull';
  readonly peer: string;
  readonly blake3: string;
  readonly into: string;
}

function decodeActions(raw: NativeDriveSyncActions): DriveSyncAction[] {
  const actions: DriveSyncAction[] = [];
  for (const action of JSON.parse(raw.json) as Array<SendJson | PullJson>) {
    if (action.kind === 'pull') {
      actions.push({ kind: 'pull', peer: action.peer, blake3: action.blake3, into: action.into });
      continue;
    }
    const frame = raw.frames[action.frame];
    if (frame === undefined) {
      throw new DriveError('EPROTO', `a send names frame ${action.frame} of ${raw.frames.length}`);
    }
    actions.push({ kind: 'send', peer: action.peer, frame });
  }
  return actions;
}

export interface PutFileOptions {
  /** `'link-or-copy'` by default. */
  readonly mode?: DrivePutMode;
  /** The digest the file must hash to, or it is refused (`EMISMATCH`). */
  readonly expect?: Blake3Hex;
}

export interface AddVersionOptions {
  /** Make the new version the item's current one. Default `true`. */
  readonly makeCurrent?: boolean;
}

export interface BuildIntoOptions {
  /** `'link-or-copy'` by default. */
  readonly mode?: DrivePutMode;
  /** Paths of the version to leave out, for the host to provide its own. */
  readonly except?: readonly string[];
  /** Replace an existing `dest` (an atomic swap) instead of `EEXIST`. */
  readonly replace?: boolean;
}

export interface GcOptions {
  /** Report what would go, and remove nothing. */
  readonly dryRun?: boolean;
  /** Versions the host still shows, held whatever the hold policy says. */
  readonly keep?: readonly VersionKey[];
}

/**
 * A drive: an object store of immutable versions and the synced document
 * that names them (crates/mille-drive/README.md). One open `Drive` per
 * folder, across processes (`EBUSY`).
 */
export class Drive {
  readonly #native: NativeDrive;
  readonly #subscriptions = new Set<number>();
  #disposed = false;

  private constructor(native: NativeDrive) {
    this.#native = native;
  }

  /** Open (or create) the drive at `options.root`. */
  static open(options: DriveOptions): Drive {
    const Ctor = nativeDriveCtor;
    if (Ctor === null) {
      throw new DriveError(
        'EUNSUPPORTED',
        'this @vibecook/mille native binary was built without the drive ' +
          '(--no-default-features); check isNativeDriveAvailable() first',
      );
    }
    const native: Record<string, unknown> = defined({
      root: options.root,
      device: options.device,
      label: options.label,
      holdCurrent: options.hold?.current,
      holdKeptVersions: options.hold?.keptVersions,
      holdAttachments: options.hold?.attachments,
      maxObjectBytes: options.maxObjectBytes,
      maxFrameBytes: options.maxFrameBytes,
      maxUpdateBytes: options.maxUpdateBytes,
      maxConcurrentPulls: options.maxConcurrentPulls,
      stagingDir: options.stagingDir,
    });
    return new Drive(wrapDrive(() => new Ctor(native)));
  }

  /** @internal — the raw napi instance. */
  get native(): NativeDrive {
    return this.#native;
  }

  root(): string {
    return wrapDrive(() => this.#native.root());
  }

  /**
   * The one folder a host registers as a pull root: objects only, never the
   * document or staging. A peer names a file in it as this joined with
   * `objectRelpath(blake3)`.
   */
  objectsRoot(): string {
    return wrapDrive(() => this.#native.objectsRoot());
  }

  has(blake3: Blake3Hex): boolean {
    return wrapDrive(() => this.#native.has(blake3));
  }

  /** The held object's path, or `null`. Read-only; never write through it. */
  objectPath(blake3: Blake3Hex): string | null {
    return wrapDrive(() => this.#native.objectPath(blake3));
  }

  /** Bring a file in: hard-linked when it can be (`mode`), else copied. */
  async putFile(path: string, options: PutFileOptions = {}): Promise<PutOutcome> {
    const native = defined({ mode: options.mode, expect: options.expect });
    const json = await this.#call(() => this.#native.putFile(path, native));
    return JSON.parse(json) as PutOutcome;
  }

  async putBytes(data: Uint8Array): Promise<PutOutcome> {
    const json = await this.#call(() => this.#native.putBytes(asBuffer(data)));
    return JSON.parse(json) as PutOutcome;
  }

  /** Re-hash a held object: `false` when its bytes no longer match its name. */
  verify(blake3: Blake3Hex): Promise<boolean> {
    return this.#call(() => this.#native.verify(blake3));
  }

  /** Record a version of `item` from objects this drive holds. Returns its id. */
  addVersion(item: string, files: readonly DriveFile[], options: AddVersionOptions = {}): string {
    const json = JSON.stringify(files);
    return wrapDrive(() => this.#native.addVersion(item, json, options.makeCurrent));
  }

  setCurrent(item: string, version: string): void {
    wrapDrive(() => this.#native.setCurrent(item, version));
  }

  setKept(item: string, kept: boolean): void {
    wrapDrive(() => this.#native.setKept(item, kept));
  }

  setHidden(item: string, hidden: boolean): void {
    wrapDrive(() => this.#native.setHidden(item, hidden));
  }

  /** `value: null` removes the key. */
  setMeta(item: string, key: string, value: string | null): void {
    wrapDrive(() => this.#native.setMeta(item, key, value));
  }

  /** `object: null` removes the attachment. */
  setAttachment(item: string, version: string, name: string, object: ObjectRef | null): void {
    const json = object === null ? null : JSON.stringify(object);
    wrapDrive(() => this.#native.setAttachment(item, version, name, json));
  }

  items(): DriveItem[] {
    return JSON.parse(wrapDrive(() => this.#native.items())) as DriveItem[];
  }

  item(id: string): DriveItem | null {
    const json = wrapDrive(() => this.#native.item(id));
    return json === null ? null : (JSON.parse(json) as DriveItem);
  }

  /** Objects the hold policy names and this drive does not hold yet. */
  wants(): Blake3Hex[] {
    return wrapDrive(() => this.#native.wants());
  }

  /** Build a version under the drive's `built/` folder. Resolves to its folder. */
  build(item: string, version: string): Promise<string> {
    return this.#call(() => this.#native.build(item, version));
  }

  /** Build a version into a host's own folder (absolute, outside the drive). */
  buildInto(
    item: string,
    version: string,
    dest: string,
    options: BuildIntoOptions = {},
  ): Promise<void> {
    const except = options.except === undefined ? undefined : [...options.except];
    const native = defined({ mode: options.mode, except, replace: options.replace });
    return this.#call(() => this.#native.buildInto(item, version, dest, native));
  }

  /** Remove a built version's folder. */
  unbuild(item: string, version: string): Promise<void> {
    return this.#call(() => this.#native.unbuild(item, version));
  }

  /** Remove objects nothing holds. */
  async gc(options: GcOptions = {}): Promise<GcReport> {
    const keep = options.keep === undefined ? undefined : JSON.stringify(options.keep);
    const json = await this.#call(() => this.#native.gc(options.dryRun, keep));
    return JSON.parse(json) as GcReport;
  }

  exportDescriptor(): DriveExport {
    return JSON.parse(wrapDrive(() => this.#native.exportDescriptor())) as DriveExport;
  }

  /** Hear every change: local writes, imports, objects arriving. */
  onChange(listener: (change: DriveChange) => void): () => void {
    const id = wrapDrive(() => this.#native.onChange(listen(listener)));
    this.#subscriptions.add(id);
    return () => {
      if (!this.#subscriptions.delete(id)) return;
      this.#native.off(id);
    };
  }

  // ── the sync core: frames in, actions out (`syncDrive` runs these) ──────

  /** This drive's version vector, encoded. */
  versionVector(): Uint8Array {
    return wrapDrive(() => this.#native.versionVector());
  }

  /** A peer is reachable. */
  syncOpen(peer: string): DriveSyncAction[] {
    return wrapDrive(() => decodeActions(this.#native.syncOpen(peer)));
  }

  /** A frame from `peer`. Throws `EPROTO`/`EFORMAT` for one the drive refuses. */
  syncHandle(peer: string, frame: Uint8Array): DriveSyncAction[] {
    return wrapDrive(() => decodeActions(this.#native.syncHandle(peer, asBuffer(frame))));
  }

  /** Something changed locally (`onChange` kind `'local'`). */
  syncLocalChanged(): DriveSyncAction[] {
    return wrapDrive(() => decodeActions(this.#native.syncLocalChanged()));
  }

  /**
   * A pull landed at `path`, anywhere on the drive's volume. The file is
   * consumed: verified and admitted, or refused (`EMISMATCH`) and removed.
   * After a refusal, call `syncPullFailed`.
   */
  async syncPulled(peer: string, blake3: Blake3Hex, path: string): Promise<DriveSyncAction[]> {
    return decodeActions(await this.#call(() => this.#native.syncPulled(peer, blake3, path)));
  }

  /** A pull failed: the drive picks the next holder. */
  syncPullFailed(peer: string, blake3: Blake3Hex): DriveSyncAction[] {
    return wrapDrive(() => decodeActions(this.#native.syncPullFailed(peer, blake3)));
  }

  /** The periodic nudge: greet peers again, retry failed pulls. */
  syncTick(): DriveSyncAction[] {
    return wrapDrive(() => decodeActions(this.#native.syncTick()));
  }

  /** A peer went away. */
  syncClose(peer: string): void {
    wrapDrive(() => this.#native.syncClose(peer));
  }

  syncStatus(): DriveSyncCounts {
    return JSON.parse(wrapDrive(() => this.#native.syncStatus())) as DriveSyncCounts;
  }

  /**
   * Drop every subscription and release the drive; its folder's lock goes
   * when the last in-flight call returns. Idempotent; every later call throws
   * `EINVAL`.
   */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#subscriptions.clear();
    this.#native.dispose();
  }

  /** A native call whose synchronous throw must reject, as a `DriveError`. */
  #call<T>(run: () => Promise<T>): Promise<T> {
    try {
      return wrapDriveAsync(run());
    } catch (e: unknown) {
      return Promise.reject(toDriveError(e));
    }
  }
}
