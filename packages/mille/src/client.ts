// Typed FileExplorer wrapper — Phase 6 commit 6.5.
//
// Wraps the raw napi-rs `FileExplorer` class (from fx-binding) with the
// typed surface declared in ../api.d.ts. The goals:
//
//  1. Every async native call routes through `wrap()` so rejections
//     arrive as typed `FileSystemError` (code + path + message).
//  2. The eight channel-specific `onChange / onChangeTree /
//     onChangeDecorations / onEvent / onBatch / onWarning / onError /
//     onReady` methods on the native collapse into a single
//     `on(event, listener): Disposable` that matches api.d.ts.
//  3. `readFileStream` turns the native `class-with-.next()` into a
//     real `AsyncIterable<Uint8Array>` via `Symbol.asyncIterator`.
//  4. `MirrorSnapshot` is wrapped so callers can pick between the
//     struct-marshaled `visibleRows` (small viewports) and the
//     bincode-bulk `visibleRowsBulk` (decoded via `decodeBulkRows`).
//
// api.d.ts is the package's public type surface; this module mirrors
// only the subset it needs without importing from api.d.ts at runtime
// — keeping the declaration file decoupled from the implementation.
// The public `.d.ts` consumers see is the generated one from this
// file plus api.d.ts at the package root.
//
// AbortSignal on async I/O: accepted in the signature to stay API-
// compatible with api.d.ts, but currently ignored (napi-rs 3.x !Send
// issue on async fns with references — tracked in PLAN 13.x).

import { basename, join as joinPath, sep as pathSep } from 'node:path';

import { native } from './native.js';
import { FileSystemError, wrap, wrapSync } from './errors.js';
import { decodeBulkRows, type VisibleRow as DecodedRow } from './decode.js';
import type { ChangeSet } from './delta.js';
import { DecorationStore, type DecorationProvider } from './decorations.js';
import type { ExplorerProjectionSettings, ResolvedExplorerSettings } from './explorer-settings.js';

/**
 * Platform-appropriate path join. On POSIX this is `path.join`; on
 * Windows the native resolver joins with the platform separator. The
 * configured root paths are always absolute, so we can rely on
 * `path.join` to DTRT across platforms.
 */
function joinPosix(root: string, ...parts: string[]): string {
  return joinPath(root, ...parts);
}

// `pathSep` is kept importable for future Windows-specific tweaks.
void pathSep;

// ─── Local type mirrors (subset of api.d.ts) ──────────────────────────
//
// Kept intentionally minimal — just what client.ts references. Consumers
// importing from '@vibecook/mille' get the rich surface through
// api.d.ts; this file stays decoupled.

export type EntryId = number;

export interface Entry {
  readonly id: EntryId;
  readonly parentId: EntryId | null;
  readonly name: string;
  readonly kind: number;
  readonly size: number;
  readonly mtimeMs: number;
  readonly ctimeMs: number;
  readonly symlinkTargetIsDir?: boolean;
  readonly pathSegments?: readonly string[];
  readonly isIgnored: boolean;
  readonly isReadonly: boolean;
  readonly isHidden: boolean;
}

export interface VisibleRow extends Entry {
  readonly depth: number;
  readonly hasChildren: boolean;
  readonly isExpanded: boolean;
  readonly pending?: true;
}

export interface VisibleRowsOptions {
  readonly expanded: ReadonlySet<EntryId>;
  readonly offset: number;
  readonly limit: number;
  readonly includeIgnored?: boolean;
}

export interface VisibleRowCount {
  readonly known: number;
  readonly pendingExpansions: ReadonlySet<EntryId>;
}

export type CollisionPolicy = 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';

// Undo types + normalizers live in browser-safe `undo.ts` so the port
// client can share them without pulling this Node/native module.
export type { UndoDescriptor, UndoKind, UndoResult } from './undo.js';
export {
  normalizeUndoDescriptor,
  normalizeUndoKind,
  normalizeUndoResult,
} from './undo.js';
import type { UndoDescriptor, UndoKind, UndoResult } from './undo.js';
import {
  normalizeUndoDescriptor,
  normalizeUndoKind,
  normalizeUndoResult,
} from './undo.js';

export type DestinationProbeStatus = 'free' | 'exists' | 'case_conflict';

export interface DestinationProbe {
  readonly status: DestinationProbeStatus;
  readonly existingName?: string;
  readonly path?: string;
}

export interface TransferProgress {
  readonly operationId: string;
  readonly phase?: 'copy';
  readonly done?: number;
  readonly total?: number;
  readonly path?: string;
  readonly status?: 'completed' | 'failed' | 'cancelled';
}

export interface TransferOptions {
  /** Cross-root transfers are denied unless explicitly enabled. */
  readonly crossRoot?: boolean;
  /**
   * Existing destinations:
   * - `error` (default) — fail with `EEXIST` before mutation
   * - `rename` — choose a free `copy` / `copy N` suffix
   * - `overwrite` — replace the destination
   * - `skip` — leave the destination untouched and succeed
   * - `merge` — for directories, merge children; files overwrite
   */
  readonly collision?: CollisionPolicy;
  /** Host-supplied id for progress/cancel tracking. */
  readonly operationId?: string;
  /** Emit progress warnings. Defaults to true when `operationId` is set. */
  readonly reportProgress?: boolean;
  /** AbortSignal for cooperative cancellation of recursive transfers. */
  readonly signal?: AbortSignal;
}

export interface ResyncOptions {
  /** Reconcile every known descendant. Defaults to direct children only. */
  readonly recursive?: boolean;
}

export interface Decoration {
  /**
   * A bare status GLYPH for this row (`'M'`, `'A'`, `'D'`), rendered on the
   * row's type ramp in a fixed-width slot so a column of them aligns.
   * Distinct from `badge`, which is a padded pill for a COUNT — the UI
   * styles the two differently and screen readers read a letter as
   * "status M" and a numeric badge as "N problems".
   */
  readonly letter?: string;
  readonly badge?: string;
  readonly color?: string;
  readonly tooltip?: string;
  readonly propagate?: boolean;
}

export interface SearchOptions {
  readonly limit?: number;
  readonly includeIgnored?: boolean;
  readonly caseSensitive?: boolean;
}

export interface SearchHit {
  readonly entry: Entry;
  readonly score: number;
  readonly matchedIndices: readonly number[];
}

export interface Disposable {
  dispose(): void;
}

export interface Uri {
  readonly scheme: string;
  readonly authority?: string;
  readonly path: string;
  readonly query?: string;
  readonly fragment?: string;
}

export interface ExplorerOptions {
  readonly roots: readonly (Uri | string)[];
  readonly respectIgnore?: boolean;
  readonly followSymlinks?: boolean | 'smart';
  readonly walkerConcurrency?: number;
  readonly watchDebounceMs?: number;
  readonly compactFolders?: boolean;
  readonly excludeGlobs?: readonly string[];
  readonly snapshotPath?: string;
  readonly maxCachedEntries?: number;
  /**
   * Phase B2 — initial walk policy. Currently advisory on the
   * `FileExplorer` itself (it never walks at construction time; callers
   * drive `populateFromRoots` / `prefetch` explicitly). The
   * `FileExplorerHost` in `host.ts` reads this field and picks the
   * corresponding hydration policy:
   *
   *   - `'full'` (default, v0.1 behaviour): the host does nothing; the
   *     consumer is responsible for calling `host.local.populateFromRoots()`
   *     or equivalent. Back-compat preserving — every existing consumer
   *     keeps working without a code change.
   *   - `'roots-only'`: the host publishes configured root placeholders
   *     synchronously without filesystem I/O. Metadata refresh, watching, and
   *     child hydration run independently; children arrive on expansion.
   *   - `'none'`: no walk at all. The consumer drives `prefetch` /
   *     `list` by hand — useful for tests and for consumers who want
   *     to layer their own lazy-hydration strategy.
   */
  readonly initialWalk?: 'full' | 'roots-only' | 'none';
  /** Resolved Phase 3 settings applied at the native snapshot boundary. */
  readonly settings?: ResolvedExplorerSettings;
}

export interface ListOptions {
  /**
   * How many levels below `parentId` to walk. `1` = direct children
   * only. Default: 1.
   */
  readonly depth?: number;
  readonly signal?: AbortSignal;
}

export interface ListPage {
  readonly entries: readonly Entry[];
  readonly total: number;
  readonly hasMore: boolean;
}

export type EventName =
  | 'change'
  | 'change:tree'
  | 'change:decorations'
  | 'event'
  | 'batch'
  | 'warning'
  | 'error'
  | 'ready';

// napi `onChange*` returns bigint subscription ids; off() expects Number.
type NativeFx = {
  readonly capabilities: number;
  getTreeVersion(): number;
  pathForId?(id: number): string | null;
  resolvePath?(path: string): Promise<number | null>;
  getSnapshot(): NativeSnapshot;
  takePendingChanges(): NativeChangeSet;
  updateProjectionSettings(settings: NativeProjectionSettings): number;
  reorderRoots(ids: number[]): number;
  updateWorkspaceRoots(roots: string[]): Promise<number>;
  refreshWorkspaceRoots(): Promise<number>;
  resync(id: number, recursive?: boolean): Promise<number>;
  resyncWorkspace(): Promise<number>;
  populateFromRoots(): Promise<number>;
  seedWorkspaceRoots(): number;
  startWatching(): Promise<number>;
  // Phase B2 — bounded-depth walk of a single path. Older native builds
  // (pre-v0.2) may not ship this method; the TS wrapper guards with
  // `typeof === 'function'` before invoking.
  populateFromPath?(
    path: string,
    maxDepth?: number | null,
    includeRoot?: boolean | null,
  ): Promise<number>;
  create(parentId: number, name: string, kind: number): Promise<Entry>;
  rename(id: number, newName: string): Promise<Entry>;
  move(
    id: number,
    newParentId: number,
    newName?: string,
    options?: TransferOptions,
  ): Promise<Entry>;
  delete(id: number, options?: { trash?: boolean; recursive?: boolean }): Promise<void>;
  canUndo(): boolean;
  peekUndo(): {
    id: number;
    kind: string;
    label: string;
    undoable: boolean;
    reason?: string;
    timestampMs: number;
  } | null;
  lastMutation(): {
    id: number;
    kind: string;
    label: string;
    undoable: boolean;
    reason?: string;
    timestampMs: number;
  } | null;
  undo(): Promise<{
    id: number;
    kind: string;
    label: string;
    entryId?: number | null;
  }>;
  copy(
    id: number,
    newParentId: number,
    newName?: string,
    options?: TransferOptions,
  ): Promise<Entry>;
  copyFromPath(
    sourcePath: string,
    newParentId: number,
    newName?: string,
    options?: TransferOptions,
  ): Promise<Entry>;
  probeDestination(
    parentId: number,
    name: string,
  ): Promise<{ status: string; existingName?: string; path?: string }>;
  cancelOperation(operationId: string): boolean;
  readFile(id: number): Promise<Buffer>;
  readText(id: number, encoding?: string): Promise<string>;
  writeFile(id: number, data: Buffer, options?: { atomic?: boolean }): Promise<void>;
  readFileStream(id: number): NativeReadStream;
  onChange(listener: (n: unknown) => void): bigint;
  onChangeTree(listener: (n: unknown) => void): bigint;
  onChangeDecorations(listener: (n: unknown) => void): bigint;
  onEvent(listener: (n: unknown) => void): bigint;
  onBatch(listener: (n: unknown) => void): bigint;
  onWarning(listener: (n: unknown) => void): bigint;
  onError(listener: (n: unknown) => void): bigint;
  onReady(listener: () => void): bigint;
  off(subscriptionId: number): boolean;
  emitReadyForTests(): void;
  search(
    query: string,
    options?: { limit?: number; includeIgnored?: boolean; caseSensitive?: boolean },
  ): Array<{ entry: Entry; score: number; matchedIndices: number[] }>;
  dispose(): Promise<void>;
};

type NativeProjectionSettings = {
  sortBy: string;
  caseSensitive: boolean;
  locale?: string;
  foldersOnTop: boolean;
  showHiddenFiles: boolean;
  showIgnoredFiles: boolean;
  compactFolders: boolean;
  excludeGlobs: string[];
  fileNestingRules: Array<{ parentPattern: string; childPatterns: string[] }>;
};

type NativeReadStream = {
  next(): Promise<Buffer | null>;
  cancel(): void;
};

// napi-rs `#[napi(object)]` emits camelCase field names on the JS side;
// the Rust source is snake_case. Keep this mirror aligned with
// crates/fx-binding/src/types.rs::ChangeSetJs.
type NativeChangeSet = {
  changedIds: number[];
  subtreeRootsChanged: number[];
  childSetChanged: number[];
  projectionChanged: boolean;
  reparentedIds: Array<{
    id: number;
    oldParentId: number | null;
    newParentId: number | null;
  }>;
  fromVersion: number;
  toVersion: number;
};

type NativeSnapshot = {
  readonly treeVersion: number;
  readonly decorationVersion: number;
  readonly showHiddenFiles: boolean;
  readonly showIgnoredFiles: boolean;
  readonly compactFolders: boolean;
  roots(): Entry[];
  getById(id: number): Entry | null;
  directChildCount(id: number): number | null;
  directoryChildrenLoaded(id: number): boolean;
  projectedChildCount(id: number, includeIgnored?: boolean): number | null;
  hasChildren(id: number): boolean;
  childrenOf(id: number): number[];
  projectedChildrenOf(id: number, includeIgnored?: boolean): number[];
  visibleRows(options: {
    expanded: number[];
    offset: number;
    limit: number;
    includeIgnored?: boolean;
  }): VisibleRow[];
  visibleRowIds(options: {
    expanded: number[];
    offset: number;
    limit: number;
    includeIgnored?: boolean;
  }): number[];
  visibleRowIndex(id: number, expanded: number[], includeIgnored?: boolean): number | null;
  visiblePrefixMatch(
    prefix: string,
    fromId: number | null,
    skipCurrent: boolean,
    expanded: number[],
    includeIgnored?: boolean,
  ): number | null;
  visibleRowsBin(options: {
    expanded: number[];
    offset: number;
    limit: number;
    includeIgnored?: boolean;
  }): Buffer;
  visibleRowCount(
    expanded: number[],
    includeIgnored?: boolean,
  ): { known: number; pendingExpansions: number[] };
};

// Map the public event names to the native method names.
const ON_METHOD: Record<EventName, keyof NativeFx> = {
  change: 'onChange',
  'change:tree': 'onChangeTree',
  'change:decorations': 'onChangeDecorations',
  event: 'onEvent',
  batch: 'onBatch',
  warning: 'onWarning',
  error: 'onError',
  ready: 'onReady',
};

function resolveRoot(u: Uri | string): string {
  if (typeof u === 'string') return u;
  return u.path;
}

/**
 * Linear scan of a folder's direct children for a name match. Used by
 * `getByUri` to walk the path segment-by-segment without pulling the
 * full snapshot tree into JS.
 */
function findNamedChild(
  snap: { getById(id: number): { id: number; name: string } | null },
  childIds: number[],
  name: string,
): { id: number; name: string } | null {
  for (const kidId of childIds) {
    const kid = snap.getById(kidId);
    if (kid !== null && kid.name === name) return kid;
  }
  return null;
}

function encodeFollowSymlinks(v: ExplorerOptions['followSymlinks']): string | undefined {
  if (v === undefined) return undefined;
  if (v === true) return 'true';
  if (v === false) return 'false';
  return 'smart';
}

function encodeProjectionSettings(settings: ExplorerProjectionSettings): NativeProjectionSettings {
  return {
    sortBy: settings.sortBy,
    caseSensitive: settings.caseSensitive,
    ...(settings.locale !== null ? { locale: settings.locale } : {}),
    foldersOnTop: settings.foldersOnTop,
    showHiddenFiles: settings.showHiddenFiles,
    showIgnoredFiles: settings.showIgnoredFiles,
    compactFolders: settings.compactFolders,
    excludeGlobs: [...settings.excludeGlobs],
    fileNestingRules: Object.entries(settings.fileNestingPatterns).map(
      ([parentPattern, childPatterns]) => ({
        parentPattern,
        childPatterns: [...childPatterns],
      }),
    ),
  };
}

/**
 * Typed `FileExplorer` wrapping the native binding. All async methods
 * funnel through `wrap()` so rejected napi errors surface as
 * `FileSystemError`. The public event API is a single `on()`; the
 * channel routing happens inside.
 */
export class FileExplorer {
  /** @internal — exposed so tests can reach `emitReadyForTests` etc. */
  private readonly nativeFx: NativeFx;
  /** @internal — host-side decoration merge store (Phase 9). */
  private readonly decorations = new DecorationStore();
  /** JS-side listeners for the decoration-scoped channels (Phase 9.3). */
  private readonly decorationListeners = new Set<(ids: readonly number[]) => void>();
  /** JS-side listeners for 'change' (fires on either dimension). */
  private readonly changeAnyListeners = new Set<(ids: readonly number[]) => void>();
  /** Every public event subscription, drained during explorer disposal. */
  private readonly eventSubscriptions = new Set<Disposable>();
  /** @internal — unsubscribe from the store's onChange when disposed. */
  private readonly decorationStoreSub: { dispose(): void };
  /**
   * @internal — Phase B2. Resolved absolute paths of configured roots.
   * Used by `pathOf(id)` to reconstruct an absolute path for
   * `populateFromPath`, matching the native `resolve_entry_path` helper.
   */
  private readonly rootPaths: string[];

  constructor(options: ExplorerOptions) {
    const Ctor = (native as unknown as { FileExplorer: new (opts: unknown) => NativeFx })
      .FileExplorer;
    const nativeOpts: Record<string, unknown> = {
      roots: options.roots.map(resolveRoot),
    };
    if (options.respectIgnore !== undefined) nativeOpts.respectIgnore = options.respectIgnore;
    const follow = encodeFollowSymlinks(options.followSymlinks);
    if (follow !== undefined) nativeOpts.followSymlinks = follow;
    if (options.walkerConcurrency !== undefined)
      nativeOpts.walkerConcurrency = options.walkerConcurrency;
    if (options.watchDebounceMs !== undefined) nativeOpts.watchDebounceMs = options.watchDebounceMs;
    if (options.compactFolders !== undefined) nativeOpts.compactFolders = options.compactFolders;
    const excludeGlobs = [
      ...new Set([...(options.excludeGlobs ?? []), ...(options.settings?.excludeGlobs ?? [])]),
    ];
    if (excludeGlobs.length > 0) nativeOpts.excludeGlobs = excludeGlobs;
    if (options.snapshotPath !== undefined) nativeOpts.snapshotPath = options.snapshotPath;
    if (options.maxCachedEntries !== undefined)
      nativeOpts.maxCachedEntries = options.maxCachedEntries;
    if (options.settings !== undefined) {
      nativeOpts.sortBy = options.settings.sortBy;
      nativeOpts.caseSensitive = options.settings.caseSensitive;
      if (options.settings.locale !== null) nativeOpts.locale = options.settings.locale;
      nativeOpts.foldersOnTop = options.settings.foldersOnTop;
      nativeOpts.showHiddenFiles = options.settings.showHiddenFiles;
      nativeOpts.showIgnoredFiles = options.settings.showIgnoredFiles;
      if (options.compactFolders === undefined) {
        nativeOpts.compactFolders = options.settings.compactFolders;
      }
      const fileNestingRules = Object.entries(options.settings.fileNestingPatterns).map(
        ([parentPattern, childPatterns]) => ({
          parentPattern,
          childPatterns: [...childPatterns],
        }),
      );
      if (fileNestingRules.length > 0) nativeOpts.fileNestingRules = fileNestingRules;
    }
    // `initialWalk` is consumed by the host wrapper (host.ts), not by the
    // native binding — don't pass it through.

    this.nativeFx = new Ctor(nativeOpts);
    // Stash the resolved root paths so `pathOf` can reconstruct absolute
    // paths for a given EntryId. Mirrors the Rust-side `self.roots`.
    this.rootPaths = options.roots.map(resolveRoot);

    // Route DecorationStore bumps to the JS-only 'change:decorations'
    // channel and to 'change' (the dimension-agnostic aggregate).
    // 'change:tree' deliberately does NOT fire here — that's the core
    // promise of SPEC §4.9.11: decoration churn doesn't invalidate
    // tree-keyed caches. Tree bumps still arrive on 'change:tree' via
    // the native binding.
    this.decorationStoreSub = this.decorations.onChange((ids) => {
      for (const l of this.decorationListeners) l(ids);
      for (const l of this.changeAnyListeners) l(ids);
    });
  }

  /**
   * Atomically update display-only projection settings on the existing tree.
   * Indexed entries are reclassified immediately when exclude globs change;
   * newly un-excluded directory contents stay lazily hydrated until expansion.
   * Locale changes rebuild one native collator and re-sort known sibling lists.
   */
  updateProjectionSettings(settings: ExplorerProjectionSettings): number {
    return this.nativeFx.updateProjectionSettings(encodeProjectionSettings(settings));
  }

  /**
   * Atomically reorder the current workspace roots by stable identity.
   * The input must contain every current root exactly once.
   */
  reorderRoots(ids: readonly EntryId[]): number {
    const orderedPaths = ids.map((id) => this.pathOf(id));
    const version = wrapSync(() => this.nativeFx.reorderRoots([...ids]));
    if (orderedPaths.every((path): path is string => path !== null)) {
      this.rootPaths.splice(0, this.rootPaths.length, ...orderedPaths);
    }
    return version;
  }

  /**
   * Atomically replace the configured workspace roots in display order.
   * New roots are seeded without walking descendants; removed roots and their
   * known subtrees disappear in the same immutable snapshot.
   */
  async updateWorkspaceRoots(roots: readonly (Uri | string)[]): Promise<number> {
    const rootPaths = roots.map(resolveRoot);
    const version = await wrap(this.nativeFx.updateWorkspaceRoots(rootPaths));
    this.rootPaths.splice(0, this.rootPaths.length, ...rootPaths);
    return version;
  }

  /**
   * Re-stat configured roots without scanning descendants. Missing or
   * inaccessible roots remain visible as unavailable; restored roots retain
   * identity and hydrate lazily.
   */
  refreshWorkspaceRoots(): Promise<number> {
    return wrap(this.nativeFx.refreshWorkspaceRoots());
  }

  /** Seed stable root rows without filesystem I/O or descendant walking. */
  seedWorkspaceRoots(): number {
    return wrapSync(() => this.nativeFx.seedWorkspaceRoots());
  }

  /** Start live filesystem watching independently from tree hydration. */
  startWatching(): Promise<number> {
    return wrap(this.nativeFx.startWatching());
  }

  /**
   * Reconcile one entry against disk through the watcher-tested scanner.
   * Files refresh through their containing directory.
   */
  resync(id: EntryId, options?: ResyncOptions): Promise<number> {
    return wrap(this.nativeFx.resync(id, options?.recursive ?? false));
  }

  /** Reconcile every configured root and descendant against disk. */
  resyncWorkspace(): Promise<number> {
    return wrap(this.nativeFx.resyncWorkspace());
  }

  get capabilities(): number {
    return this.nativeFx.capabilities;
  }

  getTreeVersion(): number {
    return this.nativeFx.getTreeVersion();
  }

  /** Decoration versions bump independently of the tree (Phase 9). */
  getDecorationVersion(): number {
    return this.decorations.version;
  }

  getSnapshot(): MirrorSnapshot {
    return new MirrorSnapshot(this.nativeFx.getSnapshot(), this.decorations);
  }

  /**
   * URI → Entry lookup. `api.d.ts` promises this as async; here it's
   * synchronous under the hood (the native snapshot is already in
   * memory) but kept async to match the contract. Resolves `file://`
   * paths by walking the snapshot tree from the root whose path
   * prefix matches.
   *
   * Other schemes (`memfs:`, `ssh:`, etc.) return null — provider
   * registration lands in a later phase.
   */
  async getByUri(uri: Uri): Promise<Entry | null> {
    if (uri.scheme !== 'file') return null;
    const nativeResolve = this.nativeFx.resolvePath;
    if (typeof nativeResolve === 'function') {
      const id = await nativeResolve.call(this.nativeFx, uri.path);
      if (id === null || id === undefined) return null;
      return (this.nativeFx.getSnapshot().getById(id) as Entry | null) ?? null;
    }

    // Find the configured root whose path is a prefix of `uri.path`.
    let match: { root: string; relative: string } | null = null;
    for (const rp of this.rootPaths) {
      if (uri.path === rp) {
        match = { root: rp, relative: '' };
        break;
      }
      const withSlash = rp.endsWith('/') ? rp : `${rp}/`;
      if (uri.path.startsWith(withSlash)) {
        match = { root: rp, relative: uri.path.slice(withSlash.length) };
        break;
      }
    }
    if (match === null) return null;

    const snap = this.nativeFx.getSnapshot();
    // Find the root Entry whose name matches the last path segment of
    // the configured root. Matches the same identity used by `pathOf`.
    // `basename`, not `split('/')` — `rootPaths` holds resolved native paths,
    // which are backslash-separated on Windows. Matches the identity used
    // when resolving a root path back from a root name below.
    const rootName = basename(match.root) || match.root;
    let cursor = snap.roots().find((e) => e.name === rootName) ?? null;
    if (cursor === null) return null;
    if (match.relative.length === 0) {
      return cursor as unknown as Entry;
    }

    // Walk each path segment, looking for a child whose name matches.
    // When a segment isn't in the store yet (lazy `roots-only` init),
    // kick a depth-1 prefetch on the current cursor and re-snapshot.
    // Tracks only the id across iterations so TS narrowing stays clean;
    // the final native entry is resolved from the last snapshot.
    const segments = match.relative.split('/').filter((s) => s.length > 0);
    let curId: number = cursor.id;
    let lastSnap = snap;
    for (const seg of segments) {
      let kids = lastSnap.childrenOf(curId);
      let found = findNamedChild(lastSnap, kids, seg);
      if (found === null) {
        // Miss: try a depth-1 walk. Native filters dedupe already-
        // known ids so this is idempotent when the parent was walked.
        try {
          await this.prefetch(curId, { depth: 1 });
        } catch {
          return null;
        }
        lastSnap = this.nativeFx.getSnapshot();
        kids = lastSnap.childrenOf(curId);
        found = findNamedChild(lastSnap, kids, seg);
      }
      if (found === null) return null;
      curId = found.id;
    }
    const finalEntry = lastSnap.getById(curId);
    return finalEntry === null ? null : (finalEntry as unknown as Entry);
  }

  /** Resolve a workspace-relative path without materializing or walking rows. */
  async resolvePath(path: string): Promise<EntryId | null> {
    const nativeResolve = this.nativeFx.resolvePath;
    if (typeof nativeResolve === 'function') {
      return (await nativeResolve.call(this.nativeFx, path)) ?? null;
    }

    // Compatibility with pre-resolvePath native artifacts.
    for (const root of this.rootPaths) {
      const entry = await this.getByUri({
        scheme: 'file',
        path: path.length === 0 ? root : joinPosix(root, path),
      });
      if (entry !== null) return entry.id;
    }
    return null;
  }

  /** Payload-free full-order typeahead fallback. */
  async findVisiblePrefix(
    prefix: string,
    fromId: EntryId | null,
    skipCurrent: boolean,
    expanded: ReadonlySet<EntryId>,
  ): Promise<EntryId | null> {
    return this.getSnapshot().visiblePrefixMatch(prefix, fromId, skipCurrent, expanded);
  }

  /**
   * Register a decoration provider. The provider's `onDidChange`
   * fires with a set of affected entry ids; for each we call
   * `provide()` and update the DecorationStore, then bump the
   * decoration version once at the end of the batch.
   *
   * Provider errors during `provide()` are swallowed — a buggy
   * provider shouldn't crash the explorer. Real deployments should
   * log somewhere; Phase 9 keeps this minimal.
   *
   * Disposing the returned Disposable removes all the provider's
   * decorations and bumps the version one more time so consumers
   * drop the stale overlays.
   */
  registerDecorationProvider(provider: DecorationProvider): Disposable {
    const sub = provider.onDidChange(async (changedIds) => {
      const changed: number[] = [];
      for (const id of changedIds) {
        try {
          const d = await provider.provide({ id });
          if (this.decorations.setForProvider(provider.id, id, d ?? null)) {
            changed.push(id);
          }
        } catch {
          // Swallow — see method doc.
        }
      }
      this.decorations.bump(changed);
    });

    let disposed = false;
    return {
      dispose: () => {
        if (disposed) return;
        disposed = true;
        sub.dispose();
        const affected = this.decorations.removeProvider(provider.id);
        this.decorations.bump(affected);
      },
    };
  }

  /**
   * Walk every configured root and seed the EntryStore with the
   * discovered entries. Returns the total entry count.
   *
   * Phase 5 intentionally keeps `new FileExplorer(...)` cheap — the
   * constructor does not walk the filesystem. Callers (and tests) that
   * need a populated tree invoke this method explicitly.
   */
  populateFromRoots(): Promise<number> {
    return wrap(this.nativeFx.populateFromRoots());
  }

  /**
   * Phase B2 — bounded-depth walk starting at `id`'s folder.
   *
   *   prefetch(id)              → walks one level below `id` (depth 1)
   *   prefetch(id, { depth: 0 }) → seeds only `id` itself
   *   prefetch(id, { depth: 2 }) → walks id + children + grandchildren
   *
   * Idempotent: the native `populateFromPath` filters entries already
   * known to the store, so repeated expansion of the same folder is a
   * cheap lookup. Does NOT change the session expansion state — the
   * host's `setExpanded` handler is responsible for that; `prefetch` just
   * warms the store so the next delta can fan children to clients.
   *
   * Graceful degradation: on older native builds that don't ship
   * `populateFromPath`, throws with a clear marker. Callers (the host)
   * try/catch and fall back to `populateFromRoots`.
   */
  async prefetch(id: EntryId, options?: { depth?: number; signal?: AbortSignal }): Promise<void> {
    const depth = options?.depth ?? 1;
    const populateFromPath = this.nativeFx.populateFromPath;
    if (typeof populateFromPath !== 'function') {
      throw new Error(
        'FileExplorer.prefetch: native binding does not support populateFromPath; rebuild mille-binding',
      );
    }
    const path = this.pathOf(id);
    if (path === null) {
      throw new Error(`FileExplorer.prefetch: id ${id} is not in the current snapshot`);
    }
    await wrap(populateFromPath.call(this.nativeFx, path, depth, true));
  }

  /**
   * Phase B2 — asynchronous listing of a folder's children. Triggers a
   * bounded walk (same primitive as `prefetch`) and then reads the
   * freshly-populated children from the snapshot. Matches the shape
   * described in api.d.ts but scoped to the subset the host needs today —
   * depth-1 listing without pagination. Pagination + `ListPage.hasMore`
   * are wired to the native search/list primitives in a later phase.
   */
  async list(parentId: EntryId, options?: ListOptions): Promise<ListPage> {
    const depth = options?.depth ?? 1;
    try {
      await this.prefetch(parentId, { depth });
    } catch (e) {
      // Don't hard-fail the consumer — a missing native method should
      // surface as "no children known yet", matching how a slow walker
      // appears from the outside.
      if (!(e instanceof Error) || !e.message.includes('populateFromPath')) {
        throw e;
      }
    }
    const snap = this.getSnapshot();
    const kids = snap.childrenOf(parentId);
    const entries: Entry[] = [];
    for (const kidId of kids) {
      const e = snap.getById(kidId);
      if (e) entries.push(e);
    }
    return { entries, total: entries.length, hasMore: false };
  }

  /**
   * Resolve an id through the native identity-to-path index. The parent-walk
   * implementation below remains only for compatibility with older native
   * artifacts that do not expose the indexed method.
   *
   * Mirrors the native `mutations::resolve_entry_path`. Lives on the TS
   * side so `prefetch` / `list` (both TS-only API surface) don't need a
   * dedicated NAPI round-trip to resolve the path.
   */
  private pathOf(id: EntryId): string | null {
    const nativePathForId = this.nativeFx.pathForId;
    if (typeof nativePathForId === 'function') {
      return nativePathForId.call(this.nativeFx, id) ?? null;
    }

    const snap = this.getSnapshot();
    const names: string[] = [];
    // Cap hops at 1024 — matches the malformed-chain defense in the
    // native resolver with more headroom for deep monorepos.
    let cur: EntryId | null = id;
    let rootName: string | null = null;
    for (let i = 0; i < 1024; i++) {
      // Loose null: napi-rs maps Rust `Option<i64>::None` to JS
      // `undefined` rather than `null`, so a strict `=== null` guard
      // lets undefined through and calling `snap.getById(undefined)`
      // throws "Failed to convert napi value Undefined into rust
      // type i64".
      if (cur == null) break;
      const entry = snap.getById(cur);
      if (!entry) return null;
      if (entry.parentId == null) {
        rootName = entry.name;
        break;
      }
      names.push(entry.name);
      cur = entry.parentId;
    }
    if (rootName === null) return null;
    // Compatibility fallback for pre-indexed native artifacts. Root order is
    // configured order, avoiding first-basename routing for duplicate names.
    const rootIndex = snap.roots().findIndex((root) => root.id === cur);
    const rootPath =
      rootIndex >= 0
        ? this.rootPaths[rootIndex]
        : this.rootPaths.find((path) => basename(path) === rootName);
    if (rootPath === undefined) return null;
    // Names were child-first; join root-first.
    names.reverse();
    return names.length === 0 ? rootPath : joinPosix(rootPath, ...names);
  }

  /**
   * Drain the native store's pending ChangeSet. The host's 16ms tick
   * loop (commit 7.6) calls this once per tick and fans the result out
   * to each attached session via `computeSessionDelta`.
   *
   * Returning a plain JS object (not a class) keeps this a zero-copy
   * NAPI struct marshal — a cleared `ChangeSet` is only six empty vecs
   * and two u32s so the allocation cost on quiet ticks is negligible.
   */
  takePendingChanges(): ChangeSet {
    const cs = this.nativeFx.takePendingChanges();
    // napi-rs already rewrites fields to camelCase, so the shape matches
    // ChangeSet as-is. The spread normalizes the prototype and strips any
    // non-own properties the binding might add in future versions.
    return {
      changedIds: cs.changedIds,
      subtreeRootsChanged: cs.subtreeRootsChanged,
      childSetChanged: cs.childSetChanged,
      projectionChanged: cs.projectionChanged,
      reparentedIds: cs.reparentedIds.map((r) => ({
        id: r.id,
        oldParentId: r.oldParentId,
        newParentId: r.newParentId,
      })),
      fromVersion: cs.fromVersion,
      toVersion: cs.toVersion,
    };
  }

  /**
   * Local-mode expansion is a consumer-side concern — the snapshot
   * serves every subtree unconditionally. Kept on the surface for
   * API parity with PortFileExplorer so consumers can swap back-ends
   * without editing call sites.
   */
  setExpanded(_diff: { add?: readonly EntryId[]; remove?: readonly EntryId[] }): void {
    /* no-op: local MirrorSnapshot is direct. */
  }

  /** Local-mode viewport is likewise a consumer concern. */
  setViewport(_window: { offset: number; limit: number; overscan?: number }): void {
    /* no-op */
  }

  // ─── Mutations ──────────────────────────────────────────────────────

  create(parentId: EntryId, name: string, kind: number): Promise<Entry> {
    return wrap(this.nativeFx.create(parentId, name, kind));
  }

  rename(id: EntryId, newName: string): Promise<Entry> {
    return wrap(this.nativeFx.rename(id, newName));
  }

  move(
    id: EntryId,
    newParentId: EntryId,
    newName?: string,
    options?: TransferOptions,
  ): Promise<Entry> {
    return this.runTransfer(options, (nativeOptions) =>
      wrap(this.nativeFx.move(id, newParentId, newName, nativeOptions)),
    );
  }

  delete(id: EntryId, options?: { trash?: boolean; recursive?: boolean }): Promise<void> {
    // Default trash:true is applied natively when options omit trash.
    return wrap(this.nativeFx.delete(id, options));
  }

  canUndo(): boolean {
    if (typeof this.nativeFx.canUndo !== 'function') return false;
    return Boolean(this.nativeFx.canUndo());
  }

  peekUndo(): UndoDescriptor | null {
    if (typeof this.nativeFx.peekUndo !== 'function') return null;
    return normalizeUndoDescriptor(this.nativeFx.peekUndo());
  }

  lastMutation(): UndoDescriptor | null {
    if (typeof this.nativeFx.lastMutation !== 'function') return null;
    return normalizeUndoDescriptor(this.nativeFx.lastMutation());
  }

  async undo(): Promise<UndoResult> {
    if (typeof this.nativeFx.undo !== 'function') {
      throw new FileSystemError('EUNSUPPORTED', 'undo is not available on this binding');
    }
    const raw = (await wrap(this.nativeFx.undo())) as Record<string, unknown>;
    const kind = normalizeUndoKind(raw.kind) ?? 'create';
    const entryId = raw.entryId ?? raw.entry_id;
    return {
      id: Number(raw.id),
      kind,
      label: String(raw.label ?? ''),
      ...(entryId !== undefined && entryId !== null ? { entryId: Number(entryId) } : null),
    };
  }

  copy(
    id: EntryId,
    newParentId: EntryId,
    newName?: string,
    options?: TransferOptions,
  ): Promise<Entry> {
    return this.runTransfer(options, (nativeOptions) =>
      wrap(this.nativeFx.copy(id, newParentId, newName, nativeOptions)),
    );
  }

  /**
   * Import an absolute path from outside the tree into a workspace folder.
   * Directories copy recursively; failures never create empty placeholders.
   */
  copyFromPath(
    sourcePath: string,
    newParentId: EntryId,
    newName?: string,
    options?: TransferOptions,
  ): Promise<Entry> {
    return this.runTransfer(options, (nativeOptions) =>
      wrap(this.nativeFx.copyFromPath(sourcePath, newParentId, newName, nativeOptions)),
    );
  }

  /**
   * Cancel a long transfer previously started with `operationId`.
   * Returns true when a matching in-flight operation was signalled.
   */
  cancelOperation(operationId: string): boolean {
    if (typeof this.nativeFx.cancelOperation !== 'function') {
      return false;
    }
    return Boolean(this.nativeFx.cancelOperation(operationId));
  }

  async probeDestination(parentId: EntryId, name: string): Promise<DestinationProbe> {
    const raw = await wrap(this.nativeFx.probeDestination(parentId, name));
    const status =
      raw.status === 'exists' || raw.status === 'case_conflict' || raw.status === 'free'
        ? raw.status
        : 'free';
    return {
      status,
      ...(raw.existingName !== undefined && raw.existingName !== null
        ? { existingName: raw.existingName }
        : null),
      ...(raw.path !== undefined && raw.path !== null ? { path: raw.path } : null),
    };
  }

  /**
   * Normalize transfer options for native: attach a generated operation id when
   * only `signal` is provided, and link AbortSignal → cancelOperation.
   */
  private async runTransfer<T>(
    options: TransferOptions | undefined,
    invoke: (nativeOptions: TransferOptions | undefined) => Promise<T>,
  ): Promise<T> {
    const signal = options?.signal;
    let operationId = options?.operationId;
    let generated = false;
    if (signal !== undefined && (operationId === undefined || operationId.length === 0)) {
      operationId = `op-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
      generated = true;
    }
    // Native does not accept AbortSignal; strip it before the boundary.
    const forNative: TransferOptions | undefined = (() => {
      if (options === undefined && operationId === undefined) return undefined;
      const out: TransferOptions = {};
      if (options?.crossRoot !== undefined) {
        (out as { crossRoot: boolean }).crossRoot = options.crossRoot;
      }
      if (options?.collision !== undefined) {
        (out as { collision: TransferOptions['collision'] }).collision = options.collision;
      }
      if (operationId !== undefined) {
        (out as { operationId: string }).operationId = operationId;
      }
      if (options?.reportProgress !== undefined) {
        (out as { reportProgress: boolean }).reportProgress = options.reportProgress;
      }
      return out;
    })();

    if (signal !== undefined && operationId !== undefined) {
      if (signal.aborted) {
        const err = new FileSystemError('ECANCELED', 'transfer aborted');
        throw err;
      }
      const onAbort = (): void => {
        void this.cancelOperation(operationId!);
      };
      signal.addEventListener('abort', onAbort, { once: true });
      try {
        return await invoke(forNative);
      } finally {
        signal.removeEventListener('abort', onAbort);
        void generated;
      }
    }
    return invoke(forNative);
  }

  // ─── I/O ────────────────────────────────────────────────────────────

  async readFile(id: EntryId, _signal?: AbortSignal): Promise<Uint8Array> {
    // TODO(PLAN 13.x): pass signal once napi-rs !Send constraint is
    // resolved and read_file restructures onto AsyncTask.
    const buf = await wrap(this.nativeFx.readFile(id));
    // Zero-copy Uint8Array view over the Node Buffer.
    return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
  }

  readText(id: EntryId, encoding?: string, _signal?: AbortSignal): Promise<string> {
    return wrap(this.nativeFx.readText(id, encoding));
  }

  writeFile(id: EntryId, data: Uint8Array, options?: { atomic?: boolean }): Promise<void> {
    return wrap(this.nativeFx.writeFile(id, Buffer.from(data), options));
  }

  /**
   * Async-iterable wrapper over the native `FileReadStream`. Callers
   * stop iteration with `break` or by exhausting; either path routes
   * through `finally` so the stream's `cancel()` always runs.
   */
  readFileStream(id: EntryId, _signal?: AbortSignal): AsyncIterable<Uint8Array> {
    // eslint-disable-next-line @typescript-eslint/no-this-alias
    const stream = this.nativeFx.readFileStream(id);
    return {
      [Symbol.asyncIterator]: async function* () {
        try {
          while (true) {
            const chunk = await wrap(stream.next());
            if (chunk === null) return;
            yield new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
          }
        } finally {
          stream.cancel();
        }
      },
    };
  }

  // ─── Search ─────────────────────────────────────────────────────────

  /**
   * Filename fuzzy search (Phase 10). Matches `query` against every
   * entry's `name` in the current snapshot via the nucleo matcher.
   * Returns hits sorted by score descending (ties broken by entry id).
   *
   * Local-mode only: runs synchronously on the main thread. The
   * `AbortSignal` field on api.d.ts `SearchOptions` is accepted but
   * ignored here — nothing to abort in a sub-millisecond sync call.
   * The client-port path would front this with a 'call' message over
   * the wire; not wired in this commit.
   */
  search(query: string, options?: SearchOptions): readonly SearchHit[] {
    const nativeOpts: {
      limit?: number;
      includeIgnored?: boolean;
      caseSensitive?: boolean;
    } = {};
    if (options?.limit !== undefined) nativeOpts.limit = options.limit;
    if (options?.includeIgnored !== undefined) nativeOpts.includeIgnored = options.includeIgnored;
    if (options?.caseSensitive !== undefined) nativeOpts.caseSensitive = options.caseSensitive;
    const hits = this.nativeFx.search(query, nativeOpts);
    // napi-rs already rewrites to camelCase + plain arrays; just pass through.
    return hits;
  }

  // ─── Events ─────────────────────────────────────────────────────────

  /**
   * Subscribe to one of the eight event channels. Returns a Disposable
   * whose `dispose()` detaches the listener (idempotent on the native).
   *
   * Decoration channels (Phase 9.3):
   *   - 'change:decorations' is JS-only. Native never fires it; the
   *     DecorationStore does, via an internal bridge.
   *   - 'change' is a union: fires on BOTH the native's onChange
   *     (tree bumps) AND on decoration bumps.
   *   - 'change:tree' routes to the native onChangeTree as before —
   *     decoration-only bumps deliberately do not fan out here.
   */
  on(event: EventName, listener: (...args: unknown[]) => void): Disposable {
    if (event === 'change:decorations') {
      // JS-only channel; no native wiring.
      const wrapped = (ids: readonly number[]) => {
        (listener as (ids: readonly number[]) => void)(ids);
      };
      this.decorationListeners.add(wrapped);
      let active = true;
      return this.trackEventSubscription({
        dispose: () => {
          if (!active) return;
          active = false;
          this.decorationListeners.delete(wrapped);
        },
      });
    }

    if (event === 'change') {
      // 'change' fires on either dimension. Wire both the native
      // aggregate (tree bumps) and the JS-only decoration bridge.
      const wrapped = (ids: readonly number[]) => {
        (listener as (ids: readonly number[]) => void)(ids);
      };
      this.changeAnyListeners.add(wrapped);
      const nativeSubId = this.nativeFx.onChange(listener);
      let active = true;
      return this.trackEventSubscription({
        dispose: () => {
          if (!active) return;
          active = false;
          this.changeAnyListeners.delete(wrapped);
          this.nativeFx.off(Number(nativeSubId));
        },
      });
    }

    const method = ON_METHOD[event];
    if (!method) {
      throw new Error(`unknown event channel: ${String(event)}`);
    }
    // The native onX methods all take a single-arg listener; the shape
    // of that arg depends on the channel. Public typings in api.d.ts
    // enforce the right signature at call sites.
    const register = this.nativeFx[method] as (fn: (...args: unknown[]) => void) => bigint;
    const subId = register.call(this.nativeFx, listener);
    let active = true;
    return this.trackEventSubscription({
      dispose: () => {
        if (!active) return;
        active = false;
        this.nativeFx.off(Number(subId));
      },
    });
  }

  private trackEventSubscription(inner: Disposable): Disposable {
    let active = true;
    const tracked: Disposable = {
      dispose: () => {
        if (!active) return;
        active = false;
        inner.dispose();
        this.eventSubscriptions.delete(tracked);
      },
    };
    this.eventSubscriptions.add(tracked);
    return tracked;
  }

  dispose(): Promise<void> {
    for (const subscription of [...this.eventSubscriptions]) subscription.dispose();
    this.decorationStoreSub.dispose();
    this.decorationListeners.clear();
    this.changeAnyListeners.clear();
    return wrap(this.nativeFx.dispose());
  }
}

/**
 * Typed, immutable view of the tree at a specific tree- and
 * decoration-version. Identity-stable between deltas so it can be
 * diffed with `===` and cached in refs.
 */
export class MirrorSnapshot {
  /** @internal */
  private readonly inner: NativeSnapshot;
  /** @internal — optional so Phase 9's wiring is backward-compatible. */
  private readonly decorations: DecorationStore | undefined;

  constructor(inner: NativeSnapshot, decorations?: DecorationStore) {
    this.inner = inner;
    this.decorations = decorations;
  }

  get treeVersion(): number {
    return this.inner.treeVersion;
  }

  get decorationVersion(): number {
    // Host-side decorations (Phase 9) live in DecorationStore; fall
    // back to the native decorationVersion (always 0 pre-Phase 9).
    return this.decorations?.version ?? this.inner.decorationVersion;
  }

  /** Local snapshots hydrate atomically, so their projection tracks the tree. */
  get projectionVersion(): number {
    return this.treeVersion;
  }

  get showHiddenFiles(): boolean {
    return this.inner.showHiddenFiles;
  }

  get showIgnoredFiles(): boolean {
    return this.inner.showIgnoredFiles;
  }

  get compactFolders(): boolean {
    return this.inner.compactFolders;
  }

  roots(): readonly Entry[] {
    return this.inner.roots();
  }

  getById(id: EntryId): Entry | null {
    return this.inner.getById(id) ?? null;
  }

  directChildCount(id: EntryId): number | null {
    const v = this.inner.directChildCount(id);
    return v ?? null;
  }

  /** @internal — distinguishes a pending directory from a loaded empty one. */
  directoryChildrenLoaded(id: EntryId): boolean {
    return this.inner.directoryChildrenLoaded(id);
  }

  /** @internal — child count after compact-folder and file-nesting projection. */
  projectedChildCount(id: EntryId, includeIgnored?: boolean): number | null {
    const v = this.inner.projectedChildCount(id, includeIgnored);
    return v ?? null;
  }

  hasChildren(id: EntryId): boolean {
    return this.inner.hasChildren(id);
  }

  /**
   * Immediate children of `id` in wire order. Used by the IPC host to
   * walk the tree for snapshot serialization (Phase 8 commit 8.6) —
   * consumers at the public surface normally route through
   * `visibleRows` instead.
   */
  childrenOf(id: EntryId): readonly EntryId[] {
    return this.inner.childrenOf(id);
  }

  /** @internal — authoritative child identities after compact-folder projection. */
  projectedChildrenOf(id: EntryId, includeIgnored?: boolean): readonly EntryId[] {
    return this.inner.projectedChildrenOf(id, includeIgnored);
  }

  /**
   * Struct-marshaled path. Preferred for small viewports (< ~100 rows
   * per SPEC §4.8). Per-row object marshaling, one NAPI call per row.
   */
  visibleRows(options: VisibleRowsOptions): readonly VisibleRow[] {
    const nativeOpts: {
      expanded: number[];
      offset: number;
      limit: number;
      includeIgnored?: boolean;
    } = {
      expanded: [...options.expanded],
      offset: options.offset,
      limit: options.limit,
    };
    if (options.includeIgnored !== undefined) {
      nativeOpts.includeIgnored = options.includeIgnored;
    }
    return this.inner.visibleRows(nativeOpts);
  }

  /** ID-only projection path for selection and other identity consumers. */
  visibleRowIds(options: VisibleRowsOptions): readonly EntryId[] {
    const nativeOpts: {
      expanded: number[];
      offset: number;
      limit: number;
      includeIgnored?: boolean;
    } = {
      expanded: [...options.expanded],
      offset: options.offset,
      limit: options.limit,
    };
    if (options.includeIgnored !== undefined) {
      nativeOpts.includeIgnored = options.includeIgnored;
    }
    return this.inner.visibleRowIds(nativeOpts);
  }

  visibleRowIndex(
    id: EntryId,
    expanded: ReadonlySet<EntryId>,
    includeIgnored?: boolean,
  ): number | null {
    return this.inner.visibleRowIndex(id, [...expanded], includeIgnored) ?? null;
  }

  visiblePrefixMatch(
    prefix: string,
    fromId: EntryId | null,
    skipCurrent: boolean,
    expanded: ReadonlySet<EntryId>,
    includeIgnored?: boolean,
  ): EntryId | null {
    return (
      this.inner.visiblePrefixMatch(prefix, fromId, skipCurrent, [...expanded], includeIgnored) ??
      null
    );
  }

  /**
   * Bincode bulk path. One Buffer hop, decoded in TS. Preferred above
   * the struct-marshaling threshold (SPEC §4.8: 100 rows).
   */
  visibleRowsBulk(options: VisibleRowsOptions): readonly DecodedRow[] {
    const nativeOpts: {
      expanded: number[];
      offset: number;
      limit: number;
      includeIgnored?: boolean;
    } = {
      expanded: [...options.expanded],
      offset: options.offset,
      limit: options.limit,
    };
    if (options.includeIgnored !== undefined) {
      nativeOpts.includeIgnored = options.includeIgnored;
    }
    const buf = this.inner.visibleRowsBin(nativeOpts);
    return decodeBulkRows(buf);
  }

  visibleRowCount(expanded: ReadonlySet<EntryId>, includeIgnored?: boolean): VisibleRowCount {
    const result = this.inner.visibleRowCount([...expanded], includeIgnored);
    return {
      known: result.known,
      pendingExpansions: new Set(result.pendingExpansions),
    };
  }

  /**
   * Merged decorations for `id` across all registered providers on
   * the owning FileExplorer. Returns `[]` when no decorations apply,
   * and also `[]` when no DecorationStore is wired — e.g. the
   * client-port MirrorSnapshot (shipping decoration deltas over the
   * wire is a Phase 10+ consideration).
   */
  getDecorations(id: EntryId): readonly Decoration[] {
    if (!this.decorations) return [];
    return this.decorations.getMerged(id) as readonly Decoration[];
  }
}
