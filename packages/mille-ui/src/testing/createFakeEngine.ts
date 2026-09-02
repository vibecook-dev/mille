// createFakeEngine — scriptable FileExplorer double for mille-ui tests.
//
// Matches the subset of `@vibecook/mille`'s `FileExplorer` surface that
// Phase 1–10 of mille-ui actually exercises. Tests drive it with the
// test-only methods `emitDelta`, `setMirror`, `reset`, and inspect
// recorded mutation calls on `calls`.
//
// The snapshot shape mirrors the engine's `MirrorSnapshot` contract
// (see packages/mille/api.d.ts §MirrorSnapshot) closely enough that
// Phase 3's tree rendering can exercise it end-to-end.

import type {
  Decoration,
  DirectoryLoadState,
  Entry,
  EntryId,
  SearchHit,
  SearchOptions,
  VisibleRow,
  VisibleRowCount,
  VisibleRowsOptions,
} from '@vibecook/mille';

// ─── Snapshot contract (structurally compatible with MirrorSnapshot) ──

export interface FakeMirrorSnapshot {
  readonly treeVersion: number;
  readonly decorationVersion: number;
  readonly showHiddenFiles?: boolean;
  readonly showIgnoredFiles?: boolean;
  roots(): readonly Entry[];
  visibleRows(options: VisibleRowsOptions): readonly VisibleRow[];
  visibleRowIds(options: VisibleRowsOptions): readonly EntryId[];
  visibleRowCount(
    expanded: ReadonlySet<EntryId>,
    includeIgnored?: boolean,
  ): VisibleRowCount;
  visibleRowIndex(
    id: EntryId,
    expanded: ReadonlySet<EntryId>,
    includeIgnored?: boolean,
  ): number | null;
  getById(id: EntryId): Entry | null;
  directChildCount(id: EntryId): number | null;
  hasChildren(id: EntryId): boolean;
  directoryLoadState(id: EntryId): DirectoryLoadState;
  getDecorations(id: EntryId): readonly Decoration[];
}

/**
 * Shape of a scriptable snapshot. Tests construct these via
 * `createFakeSnapshot({...})` or pass a custom object. Identity
 * equality (`===`) is what drives re-render behavior, matching the
 * real engine's contract.
 */
export interface FakeSnapshotInit {
  readonly treeVersion?: number;
  readonly decorationVersion?: number;
  readonly showHiddenFiles?: boolean;
  readonly showIgnoredFiles?: boolean;
  readonly roots?: readonly Entry[];
  /** Ordered, flat list of rows — the source of truth for `visibleRows`. */
  readonly rows?: readonly VisibleRow[];
  readonly decorations?: ReadonlyMap<EntryId, readonly Decoration[]>;
  readonly pendingExpansions?: ReadonlySet<EntryId>;
  readonly directoryLoads?: ReadonlyMap<EntryId, DirectoryLoadState>;
}

// ─── Recorded mutation calls ──────────────────────────────────────────

export interface FakeEngineCalls {
  create: Array<{ parentId: EntryId; name: string; kind: number }>;
  rename: Array<{ id: EntryId; newName: string }>;
  delete: Array<{ id: EntryId; options: { trash?: boolean; recursive?: boolean } | undefined }>;
  move: Array<{
    id: EntryId;
    newParentId: EntryId;
    newName: string | undefined;
    options:
      | {
          readonly crossRoot?: boolean;
          readonly collision?: 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';
        }
      | undefined;
  }>;
  copy: Array<{
    id: EntryId;
    newParentId: EntryId;
    newName: string | undefined;
    options:
      | {
          readonly crossRoot?: boolean;
          readonly collision?: 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';
        }
      | undefined;
  }>;
  copyFromPath: Array<{
    sourcePath: string;
    newParentId: EntryId;
    newName: string | undefined;
    options:
      | {
          readonly crossRoot?: boolean;
          readonly collision?: 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';
        }
      | undefined;
  }>;
  setExpanded: Array<{ add: readonly EntryId[]; remove: readonly EntryId[] }>;
  setViewport: Array<{ offset: number; limit: number; overscan: number | undefined }>;
  resync: Array<{ id: EntryId; recursive: boolean }>;
  resyncWorkspace: Array<Record<string, never>>;
  search: Array<{ query: string; options: SearchOptions | undefined; aborted: boolean }>;
}

// ─── FakeEngine surface ───────────────────────────────────────────────

export interface FakeEngine {
  // FileExplorer-compatible surface (subset used by Phase 1–10).
  readonly capabilities: number;
  getSnapshot(): FakeMirrorSnapshot;
  getTreeVersion(): number;
  getDecorationVersion(): number;
  on(event: 'change', listener: (n?: unknown) => void): { dispose(): void };
  setExpanded(diff: { add?: readonly EntryId[]; remove?: readonly EntryId[] }): void;
  setViewport(opts: { offset: number; limit: number; overscan?: number }): void;
  resync(id: EntryId, options?: { readonly recursive?: boolean }): Promise<number>;
  resyncWorkspace(): Promise<number>;

  create(parentId: EntryId, name: string, kind: number): Promise<Entry>;
  rename(id: EntryId, newName: string): Promise<Entry>;
  delete(
    id: EntryId,
    options?: { trash?: boolean; recursive?: boolean },
  ): Promise<void>;
  move(
    id: EntryId,
    newParentId: EntryId,
    newName?: string,
    options?: {
      readonly crossRoot?: boolean;
      readonly collision?: 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';
    },
  ): Promise<Entry>;
  copy(
    id: EntryId,
    newParentId: EntryId,
    newName?: string,
    options?: {
      readonly crossRoot?: boolean;
      readonly collision?: 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';
    },
  ): Promise<Entry>;
  copyFromPath(
    sourcePath: string,
    newParentId: EntryId,
    newName?: string,
    options?: {
      readonly crossRoot?: boolean;
      readonly collision?: 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';
    },
  ): Promise<Entry>;
  probeDestination(
    parentId: EntryId,
    name: string,
  ): Promise<{ status: 'free' | 'exists' | 'case_conflict'; existingName?: string; path?: string }>;
  /**
   * Phase 8 — fuzzy search. Returns whatever was scripted via
   * `setSearchResults(query, hits)`. Absent script → `[]`. The returned
   * promise honors `options.signal` by rejecting with an `AbortError`
   * when aborted while in flight.
   */
  search(query: string, options?: SearchOptions): Promise<readonly SearchHit[]>;

  // ─── Test-only controls ──────────────────────────────────────────
  /** Swap the current snapshot and notify every listener once. */
  emitDelta(next: FakeMirrorSnapshot | FakeSnapshotInit): void;
  /**
   * Convenience: replace the row list and bump treeVersion; identical
   * to `emitDelta` but reuses existing decorations/roots unless
   * overridden.
   */
  setMirror(rows: readonly VisibleRow[]): void;
  /** Drop every listener and reset to a fresh empty snapshot. */
  reset(): void;
  /** Recorded mutation / expansion / viewport calls for assertion. */
  readonly calls: FakeEngineCalls;

  // ─── Phase 10 decoration scripting ───────────────────────────────
  /**
   * Script the decoration array for a single row. Pass `null` to clear.
   * Does NOT emit a delta by itself — pair with `bumpDecorationVersion`
   * to publish a decoration-only snapshot change.
   */
  setDecorations(id: EntryId, decorations: readonly Decoration[] | null): void;
  /**
   * Phase 8 — script the hits returned for `fx.search(query)`. Pass
   * `null` to remove the scripted mapping (subsequent searches for
   * that query return `[]`). Multiple entries supported; the engine
   * does not perform its own matching.
   *
   * `query` matches the exact string passed to `search()`.
   */
  setSearchResults(query: string, hits: readonly SearchHit[] | null): void;
  /**
   * Phase 8 — control the async resolution model for `search()`.
   *
   * - `'microtask'` (default): each `search(query)` resolves on the
   *   next microtask. Tests can `await` it directly.
   * - `'manual'`: pending searches queue up; call `flushSearch()` to
   *   resolve all currently pending.
   * - positive number N: each search resolves after `setTimeout(N)`.
   *   Tests exercising the 150 ms debounce use `'microtask'` so the
   *   debounce dominates timing.
   */
  setSearchResolution(mode: 'microtask' | 'manual' | number): void;
  /** Resolve every pending `search()` call (mode `'manual'` only). */
  flushSearch(): void;
  /**
   * Publish a decoration-only snapshot change:
   * - `decorationVersion` advances by one.
   * - `treeVersion` stays unchanged (critical for the memo invariant).
   * - Row list and roots are preserved by reference (identity stable).
   * - Decoration arrays returned by `getDecorations` are identity-stable
   *   for ids NOT listed in the optional `changedIds` argument; rows in
   *   `changedIds` get a fresh array reference so the row memo treats
   *   them as "changed" even if field values coincidentally match.
   */
  bumpDecorationVersion(changedIds?: readonly EntryId[]): void;
}

// ─── Helpers ──────────────────────────────────────────────────────────

const EMPTY_SNAPSHOT: FakeMirrorSnapshot = freeze({
  treeVersion: 0,
  decorationVersion: 0,
  showHiddenFiles: true,
  showIgnoredFiles: true,
  roots: () => [],
  visibleRows: () => [],
  visibleRowIds: () => [],
  visibleRowCount: () => ({ known: 0, pendingExpansions: new Set<EntryId>() }),
  visibleRowIndex: () => null,
  getById: () => null,
  directChildCount: () => null,
  hasChildren: () => false,
  directoryLoadState: () => IDLE_DIRECTORY_LOAD,
  getDecorations: () => [],
});

function freeze<T>(v: T): T {
  return Object.freeze(v) as T;
}

const EMPTY_DECORATIONS: readonly Decoration[] = freeze(
  [] as readonly Decoration[],
);
const IDLE_DIRECTORY_LOAD: DirectoryLoadState = freeze({ state: 'idle' });

function nextEntryIdSource(start: number): () => EntryId {
  let n = start;
  return () => {
    n += 1;
    return n;
  };
}

/**
 * Build a `FakeMirrorSnapshot` from an init object. Any omitted field
 * gets a reasonable default. Returned object is frozen.
 *
 * The `decorations` map reference is stored by reference (not copied).
 * Per-id decoration array references are preserved across `getDecorations`
 * calls — critical for the Phase 10 row-memo identity contract.
 */
export function createFakeSnapshot(init: FakeSnapshotInit = {}): FakeMirrorSnapshot {
  const rows = init.rows ?? [];
  const roots = init.roots ?? rows.filter((r) => r.parentId === null);
  const decorations = init.decorations ?? new Map<EntryId, readonly Decoration[]>();
  const pending = init.pendingExpansions ?? new Set<EntryId>();
  const directoryLoads = init.directoryLoads ?? new Map<EntryId, DirectoryLoadState>();
  const showHiddenFiles = init.showHiddenFiles ?? true;
  const showIgnoredFiles = init.showIgnoredFiles ?? true;
  const projectedRows: VisibleRow[] = [];
  let excludedDepth: number | null = null;
  for (const row of rows) {
    if (excludedDepth !== null) {
      if (row.depth > excludedDepth) continue;
      excludedDepth = null;
    }
    if (
      (!showHiddenFiles && row.isHidden) ||
      (!showIgnoredFiles && row.isIgnored)
    ) {
      excludedDepth = row.depth;
      continue;
    }
    projectedRows.push(row);
  }

  const byId = new Map<EntryId, VisibleRow>();
  const rowIndexes = new Map<EntryId, number>();
  const projectedRowIndexes = new Map<EntryId, number>();
  const childCounts = new Map<EntryId, number>();
  for (let index = 0; index < rows.length; index += 1) {
    const r = rows[index]!;
    byId.set(r.id, r);
    rowIndexes.set(r.id, index);
    if (r.parentId !== null) {
      childCounts.set(r.parentId, (childCounts.get(r.parentId) ?? 0) + 1);
    }
  }
  for (let index = 0; index < projectedRows.length; index += 1) {
    projectedRowIndexes.set(projectedRows[index]!.id, index);
  }

  // Empty-array singleton so absent-id lookups never allocate.
  const snap: FakeMirrorSnapshot = {
    treeVersion: init.treeVersion ?? 0,
    decorationVersion: init.decorationVersion ?? 0,
    showHiddenFiles,
    showIgnoredFiles,
    roots: () => roots,
    visibleRows: (options: VisibleRowsOptions) => {
      const start = options.offset;
      const end = start + options.limit;
      return (options.includeIgnored ? rows : projectedRows).slice(start, end);
    },
    visibleRowIds: (options: VisibleRowsOptions) => {
      const start = options.offset;
      const end = start + options.limit;
      return (options.includeIgnored ? rows : projectedRows)
        .slice(start, end)
        .map((row) => row.id);
    },
    visibleRowCount: (_expanded, includeIgnored) => ({
      known: includeIgnored ? rows.length : projectedRows.length,
      pendingExpansions: pending,
    }),
    visibleRowIndex: (id, _expanded, includeIgnored) =>
      (includeIgnored ? rowIndexes : projectedRowIndexes).get(id) ?? null,
    getById: (id) => byId.get(id) ?? null,
    directChildCount: (id) => childCounts.get(id) ?? null,
    hasChildren: (id) => (childCounts.get(id) ?? 0) > 0,
    directoryLoadState: (id) => directoryLoads.get(id) ?? IDLE_DIRECTORY_LOAD,
    getDecorations: (id) => decorations.get(id) ?? EMPTY_DECORATIONS,
  };
  return freeze(snap);
}

/**
 * Construct a fake engine. Starts with an empty snapshot; callers
 * drive it via `emitDelta` / `setMirror` / `reset`.
 *
 * `emitDelta` is the only way a snapshot identity changes — matches
 * the engine's contract that snapshot reference is stable between
 * notifications.
 */
export function createFakeEngine(): FakeEngine {
  const listeners = new Set<(n?: unknown) => void>();
  const calls: FakeEngineCalls = {
    create: [],
    rename: [],
    delete: [],
    move: [],
    copy: [],
    copyFromPath: [],
    setExpanded: [],
    setViewport: [],
    resync: [],
    resyncWorkspace: [],
    search: [],
  };
  // Phase 8 — scripted search results keyed by exact query string.
  const searchScript = new Map<string, readonly SearchHit[]>();
  let searchResolution: 'microtask' | 'manual' | number = 'microtask';
  // Pending resolvers for mode `'manual'`.
  const pendingSearchResolvers: Array<() => void> = [];
  const allocId = nextEntryIdSource(10_000);
  let current: FakeMirrorSnapshot = EMPTY_SNAPSHOT;

  // Mutable per-engine decoration store. Shared across snapshots so
  // `bumpDecorationVersion` can advance `decorationVersion` without
  // re-keying the rows / roots. Each id's value is a stable array
  // reference until `setDecorations(id, ...)` replaces it, which is
  // how the Phase 10 row memo stays stable for untouched rows.
  const decorationMap = new Map<EntryId, readonly Decoration[]>();

  // Latest row list + roots + pending we synthesized, kept on the side
  // so `bumpDecorationVersion` can reuse them.
  let lastRows: readonly VisibleRow[] = [];
  let lastRoots: readonly Entry[] = [];
  let lastPending: ReadonlySet<EntryId> = new Set<EntryId>();

  function notify(): void {
    // Copy so a listener added during emission doesn't get it this round,
    // and removal during emission is safe.
    for (const l of [...listeners]) l();
  }

  function toSnapshot(next: FakeMirrorSnapshot | FakeSnapshotInit): FakeMirrorSnapshot {
    if (isSnapshot(next)) {
      // Full snapshot passed by the caller — honor it verbatim. Only
      // refresh our side caches so a later `bumpDecorationVersion` has
      // something to rebuild from.
      lastRows = next.visibleRows({ expanded: new Set<EntryId>(), offset: 0, limit: Number.MAX_SAFE_INTEGER });
      lastRoots = next.roots();
      lastPending = next.visibleRowCount(new Set<EntryId>()).pendingExpansions;
      return next;
    }
    const init = next;
    // Side-cache so `bumpDecorationVersion` can rebuild the snapshot
    // without the caller re-supplying rows/roots/pending.
    if (init.rows !== undefined) lastRows = init.rows;
    if (init.roots !== undefined) lastRoots = init.roots;
    else if (init.rows !== undefined) lastRoots = init.rows.filter((r) => r.parentId === null);
    if (init.pendingExpansions !== undefined) lastPending = init.pendingExpansions;

    // If the init supplied its own decorations map, use it — but also
    // snapshot those entries into our side store so `setDecorations`
    // starts from the same view. Otherwise thread the engine-wide
    // decoration store so scripted decorations survive across deltas.
    if (init.decorations !== undefined) {
      decorationMap.clear();
      for (const [id, d] of init.decorations) decorationMap.set(id, d);
    }
    return createFakeSnapshot({
      ...(init.treeVersion !== undefined ? { treeVersion: init.treeVersion } : {}),
      ...(init.decorationVersion !== undefined ? { decorationVersion: init.decorationVersion } : {}),
      ...(init.rows !== undefined ? { rows: init.rows } : {}),
      ...(init.roots !== undefined ? { roots: init.roots } : {}),
      ...(init.pendingExpansions !== undefined ? { pendingExpansions: init.pendingExpansions } : {}),
      decorations: decorationMap,
    });
  }

  const engine: FakeEngine = {
    capabilities: 0,
    getSnapshot: () => current,
    getTreeVersion: () => current.treeVersion,
    getDecorationVersion: () => current.decorationVersion,
    on: (event, listener) => {
      if (event !== 'change') {
        // Fake only models 'change' for Phase 1; extend in later phases.
        throw new Error(`createFakeEngine: unsupported event channel: ${String(event)}`);
      }
      listeners.add(listener);
      let active = true;
      return {
        dispose: () => {
          if (!active) return;
          active = false;
          listeners.delete(listener);
        },
      };
    },

    setExpanded: (diff) => {
      calls.setExpanded.push({
        add: diff.add ?? [],
        remove: diff.remove ?? [],
      });
    },
    setViewport: (opts) => {
      calls.setViewport.push({
        offset: opts.offset,
        limit: opts.limit,
        overscan: opts.overscan,
      });
    },
    resync: async (id, options) => {
      calls.resync.push({ id, recursive: options?.recursive ?? false });
      return current.treeVersion;
    },
    resyncWorkspace: async () => {
      calls.resyncWorkspace.push({});
      return current.treeVersion;
    },

    // ─── Mutations — record + synthesize a minimal delta ──────────
    create: async (parentId, name, kind) => {
      calls.create.push({ parentId, name, kind });
      const id = allocId();
      const entry: Entry = synthesizeEntry(id, parentId, name, kind);
      // Minimal delta: bump treeVersion so subscribers observe the call.
      engine.emitDelta({
        treeVersion: current.treeVersion + 1,
        decorationVersion: current.decorationVersion,
      });
      return entry;
    },
    rename: async (id, newName) => {
      calls.rename.push({ id, newName });
      const prev = current.getById(id);
      const entry: Entry = prev
        ? { ...prev, name: newName }
        : synthesizeEntry(id, null, newName, 0);
      engine.emitDelta({
        treeVersion: current.treeVersion + 1,
        decorationVersion: current.decorationVersion,
      });
      return entry;
    },
    delete: async (id, options) => {
      calls.delete.push({ id, options });
      engine.emitDelta({
        treeVersion: current.treeVersion + 1,
        decorationVersion: current.decorationVersion,
      });
    },
    move: async (id, newParentId, newName, options) => {
      calls.move.push({ id, newParentId, newName, options });
      const prev = current.getById(id);
      const entry: Entry = prev
        ? { ...prev, parentId: newParentId, name: newName ?? prev.name }
        : synthesizeEntry(id, newParentId, newName ?? String(id), 0);
      engine.emitDelta({
        treeVersion: current.treeVersion + 1,
        decorationVersion: current.decorationVersion,
      });
      return entry;
    },
    copy: async (id, newParentId, newName, options) => {
      calls.copy.push({ id, newParentId, newName, options });
      const prev = current.getById(id);
      const newId = allocId();
      const entry: Entry = prev
        ? { ...prev, id: newId, parentId: newParentId, name: newName ?? prev.name }
        : synthesizeEntry(newId, newParentId, newName ?? String(newId), 0);
      engine.emitDelta({
        treeVersion: current.treeVersion + 1,
        decorationVersion: current.decorationVersion,
      });
      return entry;
    },
    copyFromPath: async (sourcePath, newParentId, newName, options) => {
      calls.copyFromPath.push({ sourcePath, newParentId, newName, options });
      const newId = allocId();
      const basename =
        newName ??
        sourcePath.split(/[/\\]/).filter(Boolean).pop() ??
        'imported';
      const entry: Entry = synthesizeEntry(newId, newParentId, basename, 0);
      engine.emitDelta({
        treeVersion: current.treeVersion + 1,
        decorationVersion: current.decorationVersion,
      });
      return entry;
    },
    probeDestination: async (parentId, name) => {
      // Default free; tests can override by monkey-patching.
      void parentId;
      void name;
      return { status: 'free' as const };
    },

    // ─── Phase 8: fuzzy search ────────────────────────────────────
    search: (query, options) => {
      const callRecord = { query, options, aborted: false };
      calls.search.push(callRecord);
      const hits = searchScript.get(query) ?? [];
      // `api.d.ts` declares a `signal` field on `SearchOptions`, but
      // the published TS client surface omits it. Fakes accept signals
      // structurally so `useSearchResults`' AbortController works.
      const signal = (options as unknown as { signal?: AbortSignal } | undefined)?.signal;

      return new Promise<readonly SearchHit[]>((resolve, reject) => {
        const done = (): void => {
          if (signal && signal.aborted) {
            callRecord.aborted = true;
            const err = new Error('Aborted');
            (err as { name?: string }).name = 'AbortError';
            reject(err);
            return;
          }
          resolve(hits);
        };
        const onAbort = (): void => {
          callRecord.aborted = true;
          const err = new Error('Aborted');
          (err as { name?: string }).name = 'AbortError';
          reject(err);
        };
        if (signal) {
          if (signal.aborted) {
            onAbort();
            return;
          }
          signal.addEventListener('abort', onAbort, { once: true });
        }

        if (searchResolution === 'microtask') {
          // Next microtask.
          Promise.resolve().then(() => {
            if (signal) signal.removeEventListener('abort', onAbort);
            done();
          });
        } else if (searchResolution === 'manual') {
          pendingSearchResolvers.push(() => {
            if (signal) signal.removeEventListener('abort', onAbort);
            done();
          });
        } else {
          const delay = searchResolution as number;
          setTimeout(() => {
            if (signal) signal.removeEventListener('abort', onAbort);
            done();
          }, delay);
        }
      });
    },

    // ─── Test controls ────────────────────────────────────────────
    emitDelta: (next) => {
      current = toSnapshot(next);
      notify();
    },
    setMirror: (rows) => {
      lastRows = rows;
      lastRoots = rows.filter((r) => r.parentId === null);
      current = createFakeSnapshot({
        rows,
        treeVersion: current.treeVersion + 1,
        decorationVersion: current.decorationVersion,
        decorations: decorationMap,
      });
      notify();
    },
    reset: () => {
      listeners.clear();
      for (const k of Object.keys(calls) as Array<keyof FakeEngineCalls>) {
        calls[k].length = 0;
      }
      decorationMap.clear();
      lastRows = [];
      lastRoots = [];
      lastPending = new Set<EntryId>();
      current = EMPTY_SNAPSHOT;
      searchScript.clear();
      searchResolution = 'microtask';
      pendingSearchResolvers.length = 0;
    },
    calls,

    // ─── Phase 8 search scripting ────────────────────────────────
    setSearchResults: (query, hits) => {
      if (hits === null) {
        searchScript.delete(query);
        return;
      }
      searchScript.set(query, hits);
    },
    setSearchResolution: (mode) => {
      searchResolution = mode;
    },
    flushSearch: () => {
      const snapshot = pendingSearchResolvers.slice();
      pendingSearchResolvers.length = 0;
      for (const r of snapshot) r();
    },

    // ─── Phase 10 decoration scripting ───────────────────────────
    setDecorations: (id, decorations) => {
      if (decorations === null) decorationMap.delete(id);
      else decorationMap.set(id, decorations);
      // No notify — paired with bumpDecorationVersion.
    },
    bumpDecorationVersion: (changedIds) => {
      // Freshen the identity of listed ids so the Phase 10 row memo
      // treats them as changed even when field values coincide.
      if (changedIds && changedIds.length > 0) {
        for (const id of changedIds) {
          const prev = decorationMap.get(id);
          if (prev !== undefined) {
            // New readonly-array wrapper around the same elements; a
            // different identity so `prev === next` returns false.
            decorationMap.set(id, prev.slice());
          }
        }
      }
      // Decoration-only publication must be O(changed decorations), not
      // O(total rows). Reuse the complete structural snapshot and swap only
      // the decoration reader/version, matching the production mirror contract.
      const structural = current;
      current = freeze({
        treeVersion: structural.treeVersion,
        decorationVersion: structural.decorationVersion + 1,
        showHiddenFiles: structural.showHiddenFiles ?? true,
        showIgnoredFiles: structural.showIgnoredFiles ?? true,
        roots: () => structural.roots(),
        visibleRows: (options) => structural.visibleRows(options),
        visibleRowIds: (options) => structural.visibleRowIds(options),
        visibleRowCount: (expanded, includeIgnored) =>
          structural.visibleRowCount(expanded, includeIgnored),
        visibleRowIndex: (id, expanded, includeIgnored) =>
          structural.visibleRowIndex(id, expanded, includeIgnored),
        getById: (id) => structural.getById(id),
        directChildCount: (id) => structural.directChildCount(id),
        hasChildren: (id) => structural.hasChildren(id),
        directoryLoadState: (id) => structural.directoryLoadState(id),
        getDecorations: (id) => decorationMap.get(id) ?? EMPTY_DECORATIONS,
      });
      notify();
    },
  };

  return engine;
}

// Duck-type check: a full snapshot has the method surface; an init object does not.
function isSnapshot(v: FakeMirrorSnapshot | FakeSnapshotInit): v is FakeMirrorSnapshot {
  const candidate = v as { visibleRows?: unknown; getById?: unknown };
  return (
    typeof candidate.visibleRows === 'function' &&
    typeof candidate.getById === 'function'
  );
}

function synthesizeEntry(
  id: EntryId,
  parentId: EntryId | null,
  name: string,
  kind: number,
): Entry {
  return {
    id,
    parentId,
    name,
    kind,
    size: 0,
    mtimeMs: 0,
    ctimeMs: 0,
    isIgnored: false,
    isReadonly: false,
    isHidden: false,
  };
}
