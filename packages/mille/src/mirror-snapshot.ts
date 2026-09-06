// ClientMirrorSnapshot — Phase 8 commit 8.2.
//
// Frozen, consumer-facing wrapper around a MirrorWorking. Identity
// is stable until the next delta: the reducer (8.3) produces a new
// MirrorWorking + wraps it in a new ClientMirrorSnapshot only when
// something actually changed. That lets React's useSyncExternalStore
// gate re-renders on `===` identity per SPEC §4.9.1.
//
// Per api.d.ts MirrorSnapshot — visibleRows + visibleRowCount land
// in Phase 8.4 + 8.5 once the flat-slice algorithm + pendingExpansions
// logic arrive. They throw for now so the method is discoverable
// but accidental calls fail loudly.
//
// Entry / EntryId / VisibleRow / VisibleRowCount / VisibleRowsOptions
// come from client.ts; re-using those mirrors keeps both sides of the
// package on one type surface without coupling to the .d.ts at the
// package root.

import type {
  DirectoryLoadState,
  Decoration,
  Entry,
  EntryId,
  VisibleRow,
  VisibleRowCount,
  VisibleRowsOptions,
} from './client.js';
import type { ChildIdList, ClientEntry, MirrorWorking } from './mirror.js';
import { compareNaturalNames } from './natural-sort.js';
import { isVisibleEntry } from './entry-visibility.js';

/**
 * Translate a mirror-local ClientEntry (with `null`-holes) into the
 * public Entry shape (with `undefined`-holes). Spreads only the
 * optional keys that are present, satisfying
 * `exactOptionalPropertyTypes`.
 *
 * Exported (unprefixed) so the port client can reconstruct a public
 * Entry when invoking a DecorationProvider's `provide()` — the port
 * mirror stores ClientEntry internally, but decoration providers
 * consume the public Entry shape.
 */
export function clientEntryToEntry(c: ClientEntry): Entry {
  const base = {
    id: c.id,
    parentId: c.parentId,
    name: c.name,
    kind: c.kind,
    size: c.size,
    mtimeMs: c.mtimeMs,
    ctimeMs: c.ctimeMs,
    isIgnored: c.isIgnored,
    isReadonly: c.isReadonly,
    isHidden: c.isHidden,
  };
  const out: Entry = {
    ...base,
    ...(c.symlinkTargetIsDir !== null ? { symlinkTargetIsDir: c.symlinkTargetIsDir } : {}),
    ...(c.pathSegments !== null ? { pathSegments: c.pathSegments } : {}),
  };
  return out;
}

/**
 * Sort children for IDE-style trees: directories first, then files,
 * each group A→Z by name. Matches JetBrains / VS Code Project view
 * ordering (not pure alphabetical, which interleaves files and dirs).
 *
 * The client may receive child ids out-of-order (the host sends them
 * in id-allocation order); sort here using the byId map so wire-order
 * doesn't leak into the rendered tree.
 */
/** True for real directories and symlink-to-directory (pnpm workspace links). */
function isExpandableEntry(e: ClientEntry): boolean {
  return e.kind === 1 || e.symlinkTargetIsDir === true;
}

function sortChildrenByName(ids: ChildIdList, byId: Map<number, ClientEntry>): number[] {
  return Array.from(ids).sort((a, b) => {
    const ea = byId.get(a);
    const eb = byId.get(b);
    // Directories and symlink-to-dir (pnpm links) sort above files.
    const ka = ea && isExpandableEntry(ea) ? 0 : 1;
    const kb = eb && isExpandableEntry(eb) ? 0 : 1;
    if (ka !== kb) return ka - kb;
    const na = ea?.name ?? '';
    const nb = eb?.name ?? '';
    if (na === nb) return a - b;
    return compareNaturalNames(na, nb);
  });
}

/**
 * Build a snapshot-local child-order cache. A MirrorWorking is immutable once
 * published, so a parent's sorted ids remain valid for the entire snapshot.
 * The virtualizer asks for many one-row slices in a single render; without
 * this cache every slice re-sorted all previously traversed directories.
 *
 * Exported from the internal module for focused regression tests; it is not
 * part of the package's public entry point.
 */
export function createSortedChildrenLookup(
  byId: Map<number, ClientEntry>,
  orderedParents: ReadonlySet<number> = new Set(),
): (parentId: number, ids: ChildIdList) => ChildIdList {
  const cache = new Map<number, ChildIdList>();
  return (parentId, ids: ChildIdList) => {
    if (orderedParents.has(parentId)) return ids;
    const cached = cache.get(parentId);
    if (cached !== undefined) return cached;
    const sorted = sortChildrenByName(ids, byId);
    cache.set(parentId, sorted);
    return sorted;
  };
}

/**
 * Frozen view of a MirrorWorking. Consumers receive this via
 * `PortFileExplorer.getSnapshot()` (wired in Wave 2); identity is
 * stable until the reducer publishes a new one.
 *
 * The snapshot holds a reference to the working state — callers must
 * not retain the MirrorWorking and mutate it behind the snapshot's
 * back. The reducer always produces a fresh MirrorWorking via
 * cloneMirror() before wrapping it here, so the snapshot's view of
 * the data is stable for its lifetime.
 */
export class ClientMirrorSnapshot {
  /** @internal */
  private readonly state: MirrorWorking;
  private readonly sortedChildrenFor: (parentId: number, ids: ChildIdList) => ChildIdList;

  constructor(state: MirrorWorking) {
    this.state = state;
    this.sortedChildrenFor = createSortedChildrenLookup(state.byId, state.orderedChildren);
    // Freeze the wrapper so consumers can't swap the state reference
    // or attach extra properties. The inner maps are not frozen
    // because Map/Set have no Object.freeze-friendly equivalent, but
    // the reducer contract guarantees they won't be mutated after
    // the snapshot is published.
    Object.freeze(this);
  }

  get treeVersion(): number {
    return this.state.treeVersion;
  }

  get projectionVersion(): number {
    return this.state.projectionVersion;
  }

  get decorationVersion(): number {
    return this.state.decorationVersion;
  }

  get showHiddenFiles(): boolean {
    return this.state.showHiddenFiles;
  }

  get showIgnoredFiles(): boolean {
    return this.state.showIgnoredFiles;
  }

  get compactFolders(): boolean {
    return this.state.compactFolders;
  }

  roots(): readonly Entry[] {
    const out: Entry[] = [];
    for (const id of this.state.roots) {
      const entry = this.state.byId.get(id);
      if (entry !== undefined) out.push(clientEntryToEntry(entry));
    }
    return out;
  }

  getById(id: EntryId): Entry | null {
    const entry = this.state.byId.get(id);
    return entry !== undefined ? clientEntryToEntry(entry) : null;
  }

  directChildCount(id: EntryId): number | null {
    const v = this.state.directChildCounts.get(id);
    return v ?? null;
  }

  /** Whether the host has published an authoritative direct-child result. */
  directoryChildrenLoaded(id: EntryId): boolean {
    return this.state.directChildCounts.has(id);
  }

  directoryLoadState(id: EntryId): DirectoryLoadState {
    // A compact-chain task outlives its parent's direct listing. Explicit
    // loading/error state wins until the host completes the whole task.
    const record = this.state.directoryLoads.get(id);
    if (record?.state === 'loading') return { state: 'loading' };
    if (record?.state === 'error') {
      return {
        state: 'error',
        code: record.error?.code ?? 'EUNKNOWN',
        message: record.error?.message ?? 'Directory listing failed',
      };
    }
    // setExpanded publishes the request before its host acknowledgement. A
    // retry must not briefly reuse cached completion in that interval.
    if (this.state.pendingExpansions.has(id)) return { state: 'loading' };
    return this.state.directChildCounts.has(id) ? { state: 'complete' } : { state: 'idle' };
  }

  hasChildren(id: EntryId): boolean {
    return this.hasChildrenForProjection(id, false);
  }

  private hasChildrenForProjection(id: EntryId, includeIgnored: boolean): boolean {
    // With lazy expansion (v0.2 B2), directChildCount is `null` for
    // folders the walker hasn't visited yet. Returning `false` here
    // would hide the disclosure chevron, so the user can't expand
    // the folder, so the host never gets `setExpanded` and the walk
    // never fires — deadlock. For unwalked entries, fall back to
    // expandable kinds: real directories AND symlink-to-dir (pnpm /
    // npm workspace links — JetBrains shows these with a chevron).
    //
    // An EMPTY child list only means "childless" when the host actually sent
    // it as one. `orderedChildren` is that provenance mark: an incoming child
    // list sets it, a locally-rebuilt list clears it. A locally-rebuilt empty
    // list is a statement about this viewport-bounded mirror, not about the
    // filesystem, so fall through to the counts rather than hide the chevron.
    const knownChildren = this.state.children.get(id);
    if (
      knownChildren !== undefined &&
      (knownChildren.length > 0 || this.state.orderedChildren.has(id))
    ) {
      return Array.from(knownChildren).some((childId) => {
        const child = this.state.byId.get(childId);
        return (
          child === undefined ||
          isVisibleEntry(
            child,
            includeIgnored,
            this.state.showHiddenFiles,
            this.state.showIgnoredFiles,
          )
        );
      });
    }
    const count = this.state.directChildCounts.get(id);
    if (count !== undefined) return count > 0;
    const entry = this.state.byId.get(id);
    return entry !== undefined && isExpandableEntry(entry);
  }

  /**
   * Flat-slice of the visible tree, mirroring fx-core's
   * `snapshot::visible_rows`. Iterative DFS over byId + children:
   *
   *   - Roots are pushed onto the stack in reverse so children pop in
   *     left-to-right order (matches fx-core).
   *   - Ignored / hidden entries are skipped entirely (both themselves
   *     and their descendants), matching `entry_counts_visible`. An
   *     invisible-but-expanded parent MUST NOT emit its children —
   *     otherwise rows would appear at the wrong depth with no parent
   *     above them. SPEC §4.9.2 and the earlier audit both call this out.
   *   - Children are re-sorted by name on read: the wire delivers them
   *     in id-allocation order, not sorted.
   *   - offset/limit slice with early termination: once we've emitted
   *     `limit` rows we return without traversing the rest of the tree.
   *   - Expanded folders whose child list isn't in the mirror are
   *     simply not descended into here — `visibleRowCount` is the
   *     method that surfaces them as pending.
   *   - Cache-miss rows (SPEC §4.9.6): an id the mirror expected (via
   *     `children[parent]` or `roots`) but whose entry hasn't arrived
   *     yet emits a `pending: true` placeholder instead of being
   *     skipped. The renderer shows a skeleton at the correct depth
   *     and the row is replaced in-place once the real entry lands.
   */
  visibleRows(options: VisibleRowsOptions): readonly VisibleRow[] {
    const includeIgnored = options.includeIgnored ?? false;
    const limit = options.limit;
    if (limit === 0) return [];

    const out: VisibleRow[] = [];
    const expanded = options.expanded;
    let skipped = 0;

    type Frame = { id: number; depth: number };
    const stack: Frame[] = [];

    // Reverse-push so DFS emits roots left-to-right.
    for (let i = this.state.roots.length - 1; i >= 0; i--) {
      stack.push({ id: this.state.roots[i]!, depth: 0 });
    }

    while (stack.length > 0) {
      const frame = stack.pop()!;
      const entry = this.state.byId.get(frame.id);
      if (entry === undefined) {
        // Cache miss — emit a placeholder row so the renderer shows a
        // skeleton at this position. Descent stops here: we don't
        // know the children until the entry arrives. SPEC §4.9.6.
        if (skipped < options.offset) {
          skipped++;
          continue;
        }
        const placeholder: VisibleRow = {
          id: frame.id,
          parentId: null,
          name: '',
          kind: 0,
          size: 0,
          mtimeMs: 0,
          ctimeMs: 0,
          isIgnored: false,
          isReadonly: false,
          isHidden: false,
          depth: frame.depth,
          hasChildren: false,
          isExpanded: false,
          pending: true,
        };
        out.push(placeholder);
        if (out.length >= limit) return out;
        continue;
      }
      if (
        !isVisibleEntry(
          entry,
          includeIgnored,
          this.state.showHiddenFiles,
          this.state.showIgnoredFiles,
        )
      ) {
        continue;
      }

      if (skipped < options.offset) {
        skipped++;
      } else {
        // Same lazy-friendly semantic as hasChildren(id): unvisited
        // dirs / symlink-to-dir get a chevron so expand can fire.
        const hasChildren = this.hasChildrenForProjection(frame.id, includeIgnored);
        const base = clientEntryToEntry(entry);
        const row: VisibleRow = {
          ...base,
          depth: frame.depth,
          hasChildren,
          isExpanded: expanded.has(frame.id),
        };
        out.push(row);
        if (out.length >= limit) return out;
      }

      // Descend only when the expanded parent is itself visible (the
      // invisibility check above already guaranteed that). If the
      // children map doesn't hold this id yet (worker hasn't delivered
      // them), skip descent — visibleRowCount reports it as pending.
      if (expanded.has(frame.id)) {
        const kids = this.state.children.get(frame.id);
        if (kids !== undefined) {
          const sorted = this.sortedChildrenFor(frame.id, kids);
          const childDepth = frame.depth + 1;
          for (let i = sorted.length - 1; i >= 0; i--) {
            stack.push({ id: sorted[i]!, depth: childDepth });
          }
        }
      }
    }

    return out;
  }

  /** Visible ids without allocating complete row payloads. */
  visibleRowIds(options: VisibleRowsOptions): readonly EntryId[] {
    const includeIgnored = options.includeIgnored ?? false;
    const limit = options.limit;
    if (limit === 0) return [];

    const out: EntryId[] = [];
    const stack: number[] = [];
    let skipped = 0;
    for (let i = this.state.roots.length - 1; i >= 0; i--) {
      stack.push(this.state.roots[i]!);
    }

    while (stack.length > 0) {
      const id = stack.pop()!;
      const entry = this.state.byId.get(id);
      if (
        entry !== undefined &&
        !isVisibleEntry(
          entry,
          includeIgnored,
          this.state.showHiddenFiles,
          this.state.showIgnoredFiles,
        )
      ) {
        continue;
      }

      if (skipped < options.offset) {
        skipped += 1;
      } else {
        out.push(id);
        if (out.length >= limit) return out;
      }

      if (entry !== undefined && options.expanded.has(id)) {
        const kids = this.state.children.get(id);
        if (kids !== undefined) {
          const sorted = this.sortedChildrenFor(id, kids);
          for (let i = sorted.length - 1; i >= 0; i--) {
            stack.push(sorted[i]!);
          }
        }
      }
    }
    return out;
  }

  /** Exact logical index with one payload-free DFS over the mirror. */
  visibleRowIndex(
    target: EntryId,
    expanded: ReadonlySet<EntryId>,
    includeIgnored: boolean = false,
  ): number | null {
    let index = 0;
    const stack: number[] = [];
    for (let i = this.state.roots.length - 1; i >= 0; i--) {
      stack.push(this.state.roots[i]!);
    }

    while (stack.length > 0) {
      const id = stack.pop()!;
      const entry = this.state.byId.get(id);
      if (entry === undefined) {
        if (id === target) return index;
        index += 1;
        continue;
      }
      if (
        !isVisibleEntry(
          entry,
          includeIgnored,
          this.state.showHiddenFiles,
          this.state.showIgnoredFiles,
        )
      ) {
        continue;
      }
      if (id === target) return index;
      index += 1;

      if (expanded.has(id)) {
        const kids = this.state.children.get(id);
        if (kids !== undefined) {
          const sorted = this.sortedChildrenFor(id, kids);
          for (let i = sorted.length - 1; i >= 0; i--) {
            stack.push(sorted[i]!);
          }
        }
      }
    }
    return null;
  }

  /**
   * Count visible rows under the current expansion state, mirroring
   * fx-core's `snapshot::visible_row_count`. Returns:
   *
   *   - `known`: number of rows the mirror can render right now —
   *     matches `visibleRows(...).length` when no slicing is applied.
   *   - `pendingExpansions`: folders the client expanded whose child
   *     lists haven't arrived yet. Union of two sources:
   *
   *       a) Live DFS discovery — an expanded folder whose
   *          `children` map entry is missing (same condition that
   *          `visibleRows` uses to stop descending).
   *       b) The reducer's working-state `pendingExpansions` — folders
   *          the client marked expanded via setExpanded before the
   *          host delivered the child payload (SPEC §4.9.2: the
   *          virtualizer needs an honest row count even before the
   *          first tick brings children in).
   *
   * Consistent with `visibleRows`: an invisible-but-expanded parent
   * drops out of both itself AND its subtree, so `known` never counts
   * rows that `visibleRows` wouldn't emit.
   */
  visibleRowCount(
    expanded: ReadonlySet<EntryId>,
    includeIgnored: boolean = false,
  ): VisibleRowCount {
    let known = 0;
    const pending = new Set<EntryId>();

    const stack: number[] = [];
    for (let i = this.state.roots.length - 1; i >= 0; i--) {
      stack.push(this.state.roots[i]!);
    }

    while (stack.length > 0) {
      const id = stack.pop()!;
      const entry = this.state.byId.get(id);
      if (entry === undefined) continue;
      if (
        !isVisibleEntry(
          entry,
          includeIgnored,
          this.state.showHiddenFiles,
          this.state.showIgnoredFiles,
        )
      ) {
        continue;
      }
      known++;

      if (expanded.has(id)) {
        const kids = this.state.children.get(id);
        if (kids !== undefined) {
          // Order doesn't matter for a pure count — push as-is.
          for (let i = kids.length - 1; i >= 0; i--) {
            stack.push(kids[i]!);
          }
        } else {
          // Expanded folder whose children haven't arrived yet.
          pending.add(id as EntryId);
        }
      }
    }

    // Fold in the reducer's in-flight setExpanded tracker. These are
    // ids the client asked to expand but for which no delta has
    // landed yet — the DFS above wouldn't even reach them if their
    // parent is also still pending, so we report them unconditionally.
    for (const id of this.state.pendingExpansions) {
      pending.add(id as EntryId);
    }

    return { known, pendingExpansions: pending };
  }

  /**
   * Phase A1 — returns the merged decoration list the host delta
   * carried for this id. Empty array when the id has no decorations
   * or is unknown. Wire shape (`DecorationOnWire`) is a structural
   * superset of the public `Decoration`, so the cast is safe and
   * zero-copy (we hand back the same stored array).
   */
  getDecorations(id: EntryId): readonly Decoration[] {
    const stored = this.state.decorations.get(id);
    if (stored === undefined) return [];
    return stored as readonly Decoration[];
  }
}
