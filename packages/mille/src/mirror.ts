// ViewportMirror working state — Phase 8 commit 8.1.
//
// Mutable, per-client mirror state that lives alongside a
// PortFileExplorer. Never exposed to consumers directly — a frozen
// ClientMirrorSnapshot is published on every delta (8.2) and the
// delta reducer (8.3) clones this working state before mutating.
//
// Per SPEC §4.9.1 the client mirror tracks:
//   - byId                — every entry the client has been told about
//   - children            — known child lists for expanded folders
//   - orderedChildren     — parents whose child ids are host-authoritative
//   - directChildCounts   — immediate child counts for every folder
//                           (populated even for un-expanded ones so the
//                           twisty caret renders correctly)
//   - pendingExpansions   — folders the client has asked to expand but
//                           whose children haven't arrived yet
//   - expanded            — folders currently expanded by this client
//   - viewportIds         — authoritative ids in the mounted host viewport
//   - roots               — workspace roots in display order
//   - treeVersion         — the host's authoritative tree-version
//   - decorationVersion   — companion for Phase 9
//   - decorations         — Phase A1: merged DecorationOnWire arrays the
//                           host delta carried, keyed by entry id
//   - volatileSubtrees    — subtrees currently flagged dirty (SPEC §4.9.10)

/**
 * Local mirror of the wire `DecorationOnWire` shape (declared in
 * protocol.ts). Re-declared here to avoid a runtime import cycle —
 * `mirror.ts` must stay free of protocol-module dependencies so the
 * bare mirror tests can run without pulling the wire schema in.
 */
export interface DecorationOnWireLocal {
  readonly letter?: string;
  readonly badge?: string;
  readonly color?: string;
  readonly tooltip?: string;
  readonly propagate?: boolean;
}

/**
 * Mirror-local copy of an Entry record. Shape matches api.d.ts Entry
 * but with `undefined`-holes replaced by explicit `null` so both the binary
 * viewport codec and the legacy JSON fallback have one stable shape.
 */
export interface ClientEntry {
  id: number;
  parentId: number | null;
  name: string;
  /** 0=File, 1=Directory, 2=Symlink, 3=Unknown, 4=Unavailable. */
  kind: number;
  size: number;
  mtimeMs: number;
  ctimeMs: number;
  symlinkTargetIsDir: boolean | null;
  pathSegments: string[] | null;
  isIgnored: boolean;
  isReadonly: boolean;
  isHidden: boolean;
}

/** Packed or legacy authoritative child identities for one expanded folder. */
export type ChildIdList = number[] | Uint32Array | Float64Array;

export interface DirectoryLoadRecord {
  readonly generation: number;
  readonly state: 'loading' | 'error';
  readonly error?: {
    readonly code: string;
    readonly message: string;
  };
}

/**
 * Mutable working state. The reducer (8.3) produces a new
 * MirrorWorking via cloneMirror() + mutations; the snapshot wrapper
 * (8.2) wraps it in a frozen view before handing it to consumers.
 */
export interface MirrorWorking {
  /** Every entry the client has been told about. */
  byId: Map<number, ClientEntry>;
  /** Known-to-the-client child lists for expanded folders. */
  children: Map<number, ChildIdList>;
  /** Parents whose child arrays are already in authoritative display order. */
  orderedChildren: Set<number>;
  /** Direct-child counts for every folder (even un-expanded ones). */
  directChildCounts: Map<number, number>;
  /** Folders the client has asked to expand but whose children haven't arrived yet. */
  pendingExpansions: Set<number>;
  /** Active and failed directory reads; completion is represented by counts. */
  directoryLoads: Map<number, DirectoryLoadRecord>;
  /** Folders currently expanded by this client. */
  expanded: Set<number>;
  /** Entry ids in the latest authoritative host viewport patch. */
  viewportIds: Set<number>;
  /** Roots in display order. */
  roots: number[];
  /** Current tree version. */
  treeVersion: number;
  /** Advances when cached rows hydrate without an authoritative tree change. */
  projectionVersion: number;
  /** Current decoration version (Phase 9). */
  decorationVersion: number;
  /** Resolved host projection policy. */
  showHiddenFiles: boolean;
  showIgnoredFiles: boolean;
  compactFolders: boolean;
  /**
   * Phase A1 — merged decoration arrays received from the host, keyed
   * by entry id. Each entry is the wire-shape `DecorationOnWire[]`
   * (the host pre-merges across providers before shipping). Absent
   * keys mean "no decorations"; an empty array is valid too.
   */
  decorations: Map<number, readonly DecorationOnWireLocal[]>;
  /** Subtrees flagged volatile (SPEC §4.9.10). */
  volatileSubtrees: Set<number>;
  /**
   * LRU timestamp per entry id. Bumped on every insert/update; the
   * evictor (SPEC §4.9.7) drops the lowest-timestamp entries first
   * when byId.size exceeds mirrorCap. Map iteration order doesn't
   * make strong guarantees on cross-engine semantics, so we keep
   * explicit counters.
   */
  lruTouch: Map<number, number>;
  /** Monotonic counter for lruTouch. Wraps at Number.MAX_SAFE_INTEGER. */
  lruCounter: number;
}

/** Construct an empty working state. */
export function createMirror(): MirrorWorking {
  return {
    byId: new Map(),
    children: new Map(),
    orderedChildren: new Set(),
    directChildCounts: new Map(),
    pendingExpansions: new Set(),
    directoryLoads: new Map(),
    expanded: new Set(),
    viewportIds: new Set(),
    roots: [],
    treeVersion: 0,
    projectionVersion: 0,
    decorationVersion: 0,
    showHiddenFiles: true,
    showIgnoredFiles: true,
    compactFolders: false,
    decorations: new Map(),
    volatileSubtrees: new Set(),
    lruTouch: new Map(),
    lruCounter: 0,
  };
}

/**
 * Shallow clone the maps. ClientEntry records are treated as
 * immutable — the reducer replaces entries wholesale instead of
 * mutating them in place, so aliasing between the source and the
 * clone is safe.
 */
export function cloneMirror(m: MirrorWorking): MirrorWorking {
  return {
    byId: new Map(m.byId),
    children: new Map(m.children),
    orderedChildren: new Set(m.orderedChildren),
    directChildCounts: new Map(m.directChildCounts),
    pendingExpansions: new Set(m.pendingExpansions),
    directoryLoads: new Map(m.directoryLoads),
    expanded: new Set(m.expanded),
    viewportIds: new Set(m.viewportIds),
    roots: [...m.roots],
    treeVersion: m.treeVersion,
    projectionVersion: m.projectionVersion,
    decorationVersion: m.decorationVersion,
    showHiddenFiles: m.showHiddenFiles,
    showIgnoredFiles: m.showIgnoredFiles,
    compactFolders: m.compactFolders,
    decorations: new Map(m.decorations),
    volatileSubtrees: new Set(m.volatileSubtrees),
    lruTouch: new Map(m.lruTouch),
    lruCounter: m.lruCounter,
  };
}
