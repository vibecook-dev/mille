// IPC wire protocol for the host/client split — Phase 7 commit 7.2.
//
// Per SPEC §4.9.3, every message on the MessagePort wire is a versioned
// frame `{ v, type, body }`. This file defines the discriminated unions
// for both directions and a pair of helpers (frame / validateFrameVersion
// / isCompatibleVersion) that gate inbound frames before dispatch.
//
// No routing lives here — message handlers land in 7.3 (host) and 7.4
// (client). Entry payloads (`snapshot.mirror`, `delta.viewportPatch`)
// are compact bincode-compatible ArrayBuffers. MessagePort currently
// structured-clones them so the same frame works in Node and Electron.

/**
 * Wire protocol version. Bump on shape changes; clients with a mismatched
 * major version are rejected at handshake (§4.9.10).
 *
 * Phase A1 adds a client→host `decorations` frame. Additive: v1 clients
 * that never send the new frame keep working unchanged. The `isCompatibleVersion`
 * gate stays exact-match on the shipped major, and the host simply ignores
 * unknown frame types from a well-versioned client — so no bump is required
 * for a purely additive client→host extension. Kept at 1 to avoid churning
 * every hand-written test harness that hard-codes `v: 1`.
 */
export const PROTOCOL_VERSION = 1;

/**
 * Common envelope for every message on the wire.
 */
export interface Frame<T = unknown> {
  v: number; // protocol version
  type: string; // discriminator
  body: T;
}

// ─── Client → Host messages ─────────────────────────────────────────────

export interface HandshakeMsg {
  type: 'handshake';
  body: {
    version: number; // the client's PROTOCOL_VERSION
    clientId: string; // opaque label, logged by the host
    options: {
      prefetchRows?: number;
      mirrorCap?: number;
      /** Client can decode packed authoritative child-order buffers. */
      packedChildLists?: boolean;
    };
  };
}

export interface SetExpandedMsg {
  type: 'setExpanded';
  body: {
    add: number[]; // EntryIds to add to the expansion set
    remove: number[];
  };
}

export interface SetViewportMsg {
  type: 'setViewport';
  body: {
    offset: number;
    limit: number;
    overscan?: number;
  };
}

export interface MutateMsg {
  type: 'mutate';
  body: {
    reqId: number;
    op: 'create' | 'rename' | 'move' | 'delete' | 'copy' | 'writeFile' | 'readFile' | 'readText';
    args: Record<string, unknown>;
  };
}

export interface CallMsg {
  type: 'call';
  body: {
    reqId: number;
    method: string;
    args: unknown[];
  };
}

export interface DisposeMsg {
  type: 'dispose';
  body: Record<string, never>;
}

/**
 * Client → host: "I have applied every frame up to `version`."
 *
 * Sent only in response to a delta carrying `ackRequested`. Hosts that never
 * request one never see this frame, and clients predating it simply do not
 * send it — the host's wait falls back to its timeout.
 */
export interface AckMsg {
  type: 'ack';
  body: {
    /** `treeVersion` of the most recent delta this client has applied. */
    version: number;
  };
}

/**
 * Minimal decoration shape on the wire. Matches the public `Decoration`
 * interface in `api.d.ts`; re-declared here so `protocol.ts` stays a
 * runtime-agnostic schema module. Keep these two in sync.
 */
export interface DecorationOnWire {
  readonly letter?: string;
  readonly badge?: string;
  readonly color?: string;
  readonly tooltip?: string;
  readonly propagate?: boolean;
}

/**
 * Phase A1 — client→host bulk decoration push.
 *
 * A port-side `DecorationProvider` lives on the client; on register (and on
 * `onDidChange`) the client walks its known-entry set, calls
 * `provider.provide(entry)` for each, and ships the result to the host as a
 * single `decorations` frame. The host merges the entries into its
 * `DecorationStore` (provider-id keyed), bumps `decorationVersion`, and the
 * existing §4.9.11 delta fan-out carries `decorationChangedIds` back to every
 * session.
 *
 * `replaceAll: true` on register clears any prior decorations under the
 * provider id before applying the new set (first-push semantics). On
 * incremental `onDidChange` pushes with `replaceAll: false`, `null` values
 * clear that entry's slot and non-null values upsert.
 */
export interface DecorationsMsg {
  type: 'decorations';
  body: DecorationsFrameBody;
}

/** Wire body shape for the `decorations` frame. Exported for consumers. */
export interface DecorationsFrameBody {
  /** Provider id — keys the per-provider slot on the host's DecorationStore. */
  readonly providerId: string;
  /**
   * `(entryId, Decoration | null)` tuples. `null` clears the slot for
   * that id under this provider; a non-null value upserts.
   */
  readonly entries: ReadonlyArray<readonly [number, DecorationOnWire | null]>;
  /**
   * When true, the host clears every decoration under this provider id
   * before applying `entries`. Sent on initial registration and on
   * whole-set re-pushes. When false / omitted, entries are applied as
   * an incremental diff (the default for `onDidChange(ids)` bursts).
   */
  readonly replaceAll?: boolean;
}

export type ClientToHostMessage =
  | HandshakeMsg
  | SetExpandedMsg
  | SetViewportMsg
  | MutateMsg
  | CallMsg
  | AckMsg
  | DisposeMsg
  | DecorationsMsg;

// ─── Host → Client messages ─────────────────────────────────────────────

export interface SnapshotMsg {
  type: 'snapshot';
  body: {
    version: number; // tree version
    roots: number[];
    /** Bincode-compatible root ClientEntry records. */
    mirror?: ArrayBuffer;
    /** Legacy JSON fallback accepted during protocol-v1 migration. */
    entriesJson?: string;
    directChildCounts: Record<string, number>; // keys are stringified ids
    /** Reserved native viewport-row payload (see SPEC §4.8). */
    viewport?: ArrayBuffer;
    visibleCount: number;
    visibility?: {
      showHiddenFiles: boolean;
      showIgnoredFiles: boolean;
      compactFolders?: boolean;
    };
  };
}

export interface DeltaMsg {
  type: 'delta';
  body: {
    version: number;
    changedIds: number[];
    /** Bincode-compatible changed/added ClientEntry records. */
    addedRows?: ArrayBuffer;
    /** Legacy JSON fallback accepted during protocol-v1 migration. */
    entriesJson?: string;
    removedIds: number[];
    directChildCounts: Record<string, number>;
    /** Complete child-id arrays in authoritative display order. */
    childLists?: Record<string, number[]>;
    /** Packed authoritative child identities; preferred over childLists. */
    childListsBin?: ArrayBuffer;
    /** Bincode-compatible viewport-refill ClientEntry records. */
    viewportPatch?: ArrayBuffer;
    /** Authoritative ids covered by this viewport patch. */
    viewportIds?: number[];
    newVisibleCount: number;
    coarseSubtrees: number[];
    subtreeDirty: number[];
    subtreeResynced: number[];
    visibility?: {
      showHiddenFiles: boolean;
      showIgnoredFiles: boolean;
      compactFolders?: boolean;
    };
    /**
     * Phase A1 — decoration fan-out piggybacks on the delta frame.
     *
     * `decorationChangedIds` lists the entry ids whose merged decoration
     * set changed since the previous delta. `decorationsJson` is a
     * JSON-encoded `Record<string, DecorationOnWire[]>` keyed by
     * stringified entry id, carrying the fresh merged value for every
     * id in `decorationChangedIds`. Omitted fields / an empty id list
     * means "no decoration changes this tick".
     */
    decorationChangedIds?: number[];
    decorationsJson?: string;
    /**
     * Phase B1 — root fan-out piggybacks on the delta frame.
     *
     * When present, this is the **full** current root-id list on the
     * host as of this tick. Shipped only when membership or order changed
     * since the previous delta to this session (by id/position, not reference);
     * omitted when unchanged. Clients that ignore the field continue to
     * render against the roots delivered at handshake time — no
     * regression. Before v0.2, `snapshot.roots` at handshake was the
     * only wire-path for roots; if the walker hadn't discovered them
     * yet, the client's `roots` stayed empty forever.
     */
    roots?: number[];
    /**
     * Ask the client to reply with an `ack` once this frame is applied.
     *
     * Set only for frames flushed by an explicit synchronization point
     * (`resync` / `resyncWorkspace`), whose contract is that every attached
     * mirror reflects the change before the call resolves. Delivery to a
     * MessagePort is not observable from the host, so without a reply the
     * host can only wait a tick and hope — which is a race, not a guarantee.
     * Ordinary churn leaves this unset and stays one-way.
     */
    ackRequested?: boolean;
  };
}

export interface EventMsg {
  type: 'event';
  body: {
    kind: string;
    id?: number;
    parentId?: number;
    path?: string;
    code?: string;
    message?: string;
  };
}

export interface MutateResultMsg {
  type: 'mutateResult';
  body: {
    reqId: number;
    result: unknown;
    error?: {
      code: string;
      message: string;
      path?: string;
    };
  };
}

export interface CallResultMsg {
  type: 'callResult';
  body: {
    reqId: number;
    result: unknown;
    error?: {
      code: string;
      message: string;
      path?: string;
    };
  };
}

export interface WarningMsg {
  type: 'warning';
  body: { code: string; detail?: string };
}

export interface ErrorMsg {
  type: 'error';
  body: { code: string; message: string };
}

export type HostToClientMessage =
  | SnapshotMsg
  | DeltaMsg
  | EventMsg
  | MutateResultMsg
  | CallResultMsg
  | WarningMsg
  | ErrorMsg;

// ─── Helpers ────────────────────────────────────────────────────────────

/**
 * Shape-check an inbound value as a `Frame`. Returns the frame when
 * the envelope is well-formed, `null` otherwise. Version negotiation
 * is a separate step — call `isCompatibleVersion` on the result.
 */
export function validateFrameVersion(frame: unknown): Frame | null {
  if (
    typeof frame !== 'object' ||
    frame === null ||
    !('v' in frame) ||
    !('type' in frame) ||
    !('body' in frame)
  ) {
    return null;
  }
  const f = frame as Frame;
  if (typeof f.v !== 'number' || typeof f.type !== 'string') {
    return null;
  }
  return f;
}

/** Exact-match gate: bump PROTOCOL_VERSION to break older clients. */
export function isCompatibleVersion(v: number): boolean {
  return v === PROTOCOL_VERSION;
}

/** Wrap a body in a versioned frame, ready to hand to `postMessage`. */
export function frame<T>(type: string, body: T): Frame<T> {
  return { v: PROTOCOL_VERSION, type, body };
}
