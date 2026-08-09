// UtilityProcess-side entry — Phase 7 commits 7.1 + 7.3.
//
// `createFileExplorerHost` wraps a native `FileExplorer` and owns the
// per-MessagePort `Session` map. Each attached port gets its own
// expansion set, viewport, knownIds (for delta filtering in 7.5), and
// request-id counter. Attach/detach lifecycle is testable end-to-end
// via Node `worker_threads` MessageChannels — the same shape Electron's
// `MessageChannelMain` produces.
//
// 7.3 adds message routing: handshake -> snapshot, setExpanded -> delta,
// setViewport -> authoritative bounded patch, mutate ->
// dispatchMutation -> mutateResult, call -> dispatchCall -> callResult,
// dispose -> detach. Version gating + handshake-first sequencing are
// enforced; malformed or wrong-version frames produce an `error` frame.
// Root and viewport entry records use a shared bincode-compatible encoder.

import { FileExplorer, type Entry, type MirrorSnapshot, type TransferOptions } from './client.js';
import type { EntryId, ExplorerOptions } from './client.js';
import { DecorationStore, type Decoration, type DecorationProvider } from './decorations.js';
import { computeSessionDelta, type SessionView } from './delta.js';
import { FileSystemError, isFileSystemError } from './errors.js';
import {
  MAX_OWNED_OPERATIONS,
  RESYNC_LIMIT,
  RESYNC_WINDOW_MS,
  authorizeCall,
  authorizeCancel,
  authorizeDecorations,
  authorizeMutation,
  effectiveCapabilities,
} from './channel/policy.js';
import { encodeClientEntries } from './entry-codec.js';
import { encodeChildLists } from './child-list-codec.js';
import type { ClientEntry } from './mirror.js';
import {
  frame,
  isCompatibleVersion,
  validateFrameVersion,
  type DecorationOnWire,
  type DecorationsFrameBody,
  type HostToClientMessage,
} from './protocol.js';
import { createMessagePortHostChannel } from './channel/message-port.js';
import { resolveSessionContext } from './channel/types.js';
import type {
  ExplorerHostChannel,
  ExplorerSessionContext,
  ResolvedSessionContext,
} from './channel/types.js';
import type { Disposable, FileExplorerHost, MessagePortLike } from './types.js';
import type { ExplorerProjectionSettings } from './explorer-settings.js';

/**
 * Project a public `Entry` into the mirror-local `ClientEntry` shape.
 * The public Entry uses `undefined`-holes for optional fields; the
 * binary and legacy JSON wire shapes use explicit `null` so round-trips
 * don't lose the distinction between "absent" and "present-undefined".
 */
function entryToClient(e: Entry): ClientEntry {
  return {
    id: e.id,
    parentId: e.parentId ?? null,
    name: e.name,
    kind: e.kind,
    size: e.size,
    mtimeMs: e.mtimeMs,
    ctimeMs: e.ctimeMs,
    symlinkTargetIsDir: e.symlinkTargetIsDir ?? null,
    pathSegments: e.pathSegments !== undefined ? [...e.pathSegments] : null,
    isIgnored: e.isIgnored,
    isReadonly: e.isReadonly,
    isHidden: e.isHidden,
  };
}

/**
 * Pull an `operationId` out of a warning's JSON `detail`, if it has one.
 *
 * Transfer progress arrives as `OP_PROGRESS` / `OP_COMPLETE` with a
 * JSON-encoded detail string. Anything that does not parse, or that carries
 * no operation id, is a general warning and stays broadcast.
 */
function extractOperationId(detail: string | undefined): string | null {
  if (typeof detail !== 'string' || detail.length === 0) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(detail);
  } catch {
    return null;
  }
  if (typeof parsed !== 'object' || parsed === null) return null;
  const id = (parsed as { operationId?: unknown }).operationId;
  return typeof id === 'string' && id.length > 0 ? id : null;
}

/**
 * Accept a write payload in either wire form (SPEC §12.5).
 *
 * New clients send a `Uint8Array`; older ones send a plain number array,
 * and the framed codec can hand back any typed-array view. All three have
 * to keep working — a host must not require a client upgrade.
 */
function toBytes(data: unknown): Uint8Array {
  if (data instanceof Uint8Array) return data;
  if (ArrayBuffer.isView(data)) {
    const v = data as ArrayBufferView;
    return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
  }
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  return new Uint8Array(data as ArrayLike<number>);
}

/** The `operationId` a mutation's arguments ask the host to track it under. */
function requestedOperationId(args: Record<string, unknown>): string | null {
  const options = args.options as { operationId?: unknown } | undefined;
  const id = options?.operationId;
  return typeof id === 'string' && id.length > 0 ? id : null;
}

/** `EntryKind.Symlink` — mirrors api.d.ts. Windows junctions land here too. */
const ENTRY_KIND_SYMLINK = 2;

/**
 * Does reaching `id` require crossing a symlink or junction?
 *
 * `followSymlinks: false` is honoured by the *walker*, which never descends a
 * reparse point — but `resolvePath` resolves through the filesystem and
 * hydrates whatever it finds, inserting the target as a child of the link.
 * The result is an entry that is structurally inside the tree and physically
 * outside it, and a subsequent `readFile` serves the real file.
 *
 * Found by the AC-008 junction test: a read-only remote session on an export
 * with `followSymlinks: false` could resolve `escape-link/secret.txt` and read
 * a file outside the export root. SPEC §17.2 requires that reparse points are
 * not followed on a remote export, and SEC-002 requires roots be inaccessible
 * "through traversal or alternate path spelling".
 *
 * The link itself is fine to see — listing a symlink discloses nothing. What
 * must not happen is traversing *through* one, so only strict ancestors are
 * examined.
 */
function crossesSymlink(snapshot: MirrorSnapshot, id: EntryId): boolean {
  let cursor = snapshot.getById(id)?.parentId ?? null;
  let guard = 0;
  while (cursor !== null && guard < 1024) {
    const entry = snapshot.getById(cursor);
    if (entry === null) return false;
    if (entry.kind === ENTRY_KIND_SYMLINK) return true;
    cursor = entry.parentId ?? null;
    guard += 1;
  }
  return false;
}

/** Native store order is authoritative for viewport and structural metadata. */
function sortedChildIds(snap: MirrorSnapshot, parentId: number): number[] {
  return [...snap.projectedChildrenOf(parentId)];
}

/**
 * Per-connection session state. Owned by the host, one per attached port.
 */
interface Session {
  readonly id: number;
  readonly channel: ExplorerHostChannel;
  /**
   * Who is on the other end, with defaults applied. PR 1 records it;
   * the permission tables that read `context.policy` land in PR 3.
   */
  readonly context: ResolvedSessionContext;
  /** Expansion set this client has declared via `setExpanded`. */
  expanded: Set<number>;
  /** Current viewport window the client has requested. */
  viewport: { offset: number; limit: number; overscan: number };
  /** Ids covered by the last viewport patch sent to this client. */
  viewportIds: Set<number>;
  /** Fallback row budget used when expansion precedes the first viewport. */
  prefetchRows: number;
  /** Whether this client advertised the packed child-order wire channel. */
  packedChildLists: boolean;
  /**
   * Entry ids whose full records this session has already received.
   * Phase 7.5 uses this to filter deltas down to hydrated rows —
   * new entries outside the client's viewport stay off the wire until
   * the viewport moves to cover them.
   */
  knownIds: Set<number>;
  /** Next request id to use on outgoing host->client frames. */
  nextReqId: number;
  /** Whether the handshake frame has been observed. */
  handshook: boolean;
  /**
   * Phase B1 — the last ordered root-id list this session has been told about.
   * Populated with the ids shipped in the handshake's snapshot; the
   * per-tick delta builder compares the host's current roots against
   * this and re-ships the full list when membership or order changed. Kept per
   * session because sessions can attach at different phases of the
   * walker lifecycle — session A may have handshaken empty while B
   * handshook after a root was added.
   */
  lastRootIds: number[];
  /**
   * Highest `treeVersion` this session has confirmed it applied, via an
   * `ack` frame. Only advanced for deltas the host marked `ackRequested`,
   * so it lags during ordinary churn and is meaningful only at explicit
   * synchronization points. `-1` means "has never acked", which is also the
   * state of a client too old to send them.
   */
  ackedVersion: number;
  /**
   * Whether this session is believed capable of acknowledging at all.
   *
   * A client that predates the `ack` frame handshakes normally and consumes
   * deltas but never replies, so waiting on it always costs the full fallback.
   * That was tolerable when only `resync` waited; mutations are the hot path,
   * and one such client made every rename take the whole `timeoutMs`. Starts
   * optimistic, flips to `false` when a synchronization point times out
   * waiting on it, and flips straight back on any `ack` — so a merely slow
   * renderer recovers its guarantee instead of being written off permanently.
   */
  ackCapable: boolean;
  /**
   * Transfer operation ids this session currently owns (SPEC §16.3).
   * Progress and completion for these route here and nowhere else, and only
   * the owner (or an admin) may cancel them.
   */
  readonly ownedOperationIds: Set<string>;
  /**
   * Timestamps of recent `resync` calls, for the per-session rate limit.
   * A resync is a bounded re-walk; unmetered it is a cheap way for one
   * remote peer to keep the shared host busy.
   */
  resyncTimes: number[];
  /** Teardown for the message listener + port. Replaced during attach. */
  detach: () => void;
}

/**
 * 16ms ≈ one render frame. SPEC §4.9 sizes the coalescer + fan-out tick
 * so the host never spends more than one frame between draining changes
 * and posting deltas. Adjusting this in tests is intentionally not
 * supported — the tick is an implementation detail.
 */
const TICK_MS = 16;

class FileExplorerHostImpl implements FileExplorerHost {
  private readonly explorer: FileExplorer;
  private readonly sessions = new Map<number, Session>();
  /** Callbacks waiting for sessions to catch up; see `flushTickAcked`. */
  private readonly ackWaiters = new Set<() => void>();
  /** Marks the next tick's deltas as needing an ack from every session. */
  private ackRequestedForNextTick = false;
  private nextSessionId = 1;
  private disposed = false;
  /**
   * Phase B2 — ids the host has already triggered a prefetch for. Guards
   * against re-firing a walk when a client re-expands the same folder
   * across sessions or after a collapse/re-expand cycle. The native
   * `populateFromPath` is already idempotent (snapshot-filter), but
   * skipping the call entirely also saves the NAPI hop. Keyed by id;
   * never pruned — prefetch is a one-shot per id per host lifetime.
   */
  private readonly prefetched: Set<number> = new Set();
  /** Phase B2 — initial-walk policy. See ExplorerOptions.initialWalk. */
  private readonly initialWalk: 'full' | 'roots-only' | 'none';
  /**
   * Phase B2 — has the initial walk (roots-only seeding) run yet? The
   * first `attachPort` triggers it so sessions can attach and handshake
   * before any filesystem work. Subsequent attaches skip the walk but
   * still ship whatever the store holds.
   */
  private initialWalkDone = false;
  /** setInterval handle for the fan-out tick. Null when idle. */
  private tickHandle: ReturnType<typeof setInterval> | null = null;
  /**
   * Subtree roots flagged for coarse invalidation since the last tick.
   * Drained into each session's outgoing delta as `coarseSubtrees`.
   * Wired to the native watcher's Overflow signal in Phase 5 — exposed
   * now via `markSubtreeCoarse` so the protocol end is testable.
   */
  private pendingCoarseSubtrees: Set<number> = new Set();
  /**
   * Volatile-subtree markers per SPEC §4.9.10. A root cannot ride both
   * fields in the same delta — the mark* methods enforce the
   * dirty-xor-resynced invariant at enqueue time so the tick never has
   * to reconcile them.
   */
  private pendingSubtreeDirty: Set<number> = new Set();
  private pendingSubtreeResynced: Set<number> = new Set();
  /**
   * Serial promise chain all mutations hang off. SPEC §5.1's ordering
   * guarantee — delta fan-out to every session must precede the
   * initiator's mutateResult — falls out of enqueuing each mutation on
   * this single chain and awaiting inside the entry.
   */
  private mutationQueue: Promise<void> = Promise.resolve();
  /**
   * Phase A1 — shared decoration store. Any client that ships a
   * `decorations` frame merges into this store; the next tick's delta
   * fan-out piggybacks `decorationChangedIds` + serialized merged
   * decoration payload onto every session's delta frame.
   */
  private readonly decorationStore = new DecorationStore();
  /** Native watcher event bridge (overflow/coarse invalidation). */
  private readonly watcherEventSub: Disposable;
  /** Native warning bridge (transfer progress / OP_*). */
  private readonly warningSub: Disposable;
  /** Ids whose merged decorations changed since the last tick. */
  private pendingDecorationChangedIds: Set<number> = new Set();

  constructor(options: ExplorerOptions) {
    this.explorer = new FileExplorer(options);
    this.initialWalk = options.initialWalk ?? 'full';
    this.watcherEventSub = this.explorer.on('event', (raw) => {
      const event = raw as { kind?: string; id?: number } | undefined;
      if (event?.kind !== 'overflow' || typeof event.id !== 'number') return;
      // The native watcher has already reconciled this subtree before it
      // emits overflow. Force the next delta to replace the mirror's child
      // list and allow a later expansion to prefetch again if needed.
      this.prefetched.delete(event.id);
      this.markSubtreeCoarse(event.id);
    });
    // Forward warnings to attached renderers. Operation-scoped warnings go
    // only to the session that owns the operation: `OP_PROGRESS` detail
    // carries the source and destination paths, and broadcasting that to
    // every session leaks one peer's filesystem activity to all the others
    // (SPEC SEC-005, §16.3). Non-operation warnings stay global.
    this.warningSub = this.explorer.on('warning', (raw) => {
      const payload = raw as { code?: string; detail?: string } | undefined;
      if (!payload || typeof payload.code !== 'string') return;
      const body = {
        code: payload.code,
        ...(typeof payload.detail === 'string' ? { detail: payload.detail } : null),
      };

      const operationId = extractOperationId(payload.detail);
      if (operationId !== null) {
        const owner = this.findOperationOwner(operationId);
        if (owner !== null) {
          this.send(owner, frame('warning', body));
          // A terminal record releases the claim so the id can be reused and
          // the session's budget is not consumed by finished work.
          if (payload.code === 'OP_COMPLETE' || payload.code === 'OP_CANCELLED') {
            owner.ownedOperationIds.delete(operationId);
          }
        }
        // An unowned operation id means the host itself started it (via
        // `host.local`), so there is no session to inform.
        return;
      }

      for (const session of this.sessions.values()) {
        this.send(session, frame('warning', body));
      }
    });
  }

  get local(): FileExplorer {
    return this.explorer;
  }

  get sessionCount(): number {
    return this.sessions.size;
  }

  markSubtreeCoarse(rootId: number): void {
    this.pendingCoarseSubtrees.add(rootId);
  }

  markSubtreeDirty(rootId: number): void {
    this.pendingSubtreeDirty.add(rootId);
    // A root cannot be simultaneously dirty and resynced — the pair
    // maps to opposite transitions in VolatileTracker's flip/release
    // state machine. If a previous resynced hadn't drained yet, the
    // latest transition wins.
    this.pendingSubtreeResynced.delete(rootId);
  }

  markSubtreeResynced(rootId: number): void {
    this.pendingSubtreeResynced.add(rootId);
    this.pendingSubtreeDirty.delete(rootId);
  }

  /**
   * In-process decoration provider registration. Writes into the
   * host's DecorationStore (same store the `decorations` wire frame
   * feeds) so the next tick fans out to every attached session.
   *
   * `host.local.registerDecorationProvider` is NOT equivalent — that
   * registers against the `FileExplorer`'s independent DecorationStore
   * and decorations never reach clients. Use this method for any
   * provider that should be visible to renderer sessions.
   */
  registerDecorationProvider(rawProvider: unknown): Disposable {
    if (this.disposed) {
      throw new Error('FileExplorerHost is disposed');
    }
    const provider = rawProvider as DecorationProvider;
    const sub = provider.onDidChange(async (ids) => {
      const changed: number[] = [];
      for (const id of ids) {
        try {
          const maybe = provider.provide({ id });
          const d =
            maybe !== null && typeof maybe === 'object' && 'then' in maybe
              ? await maybe
              : (maybe as Decoration | null);
          if (this.decorationStore.setForProvider(provider.id, id, d ?? null)) {
            changed.push(id);
          }
        } catch {
          // Swallow provider errors — a buggy provider shouldn't
          // crash the host. Matches FileExplorer's behaviour.
        }
      }
      if (changed.length > 0) {
        this.decorationStore.bump(changed);
        for (const id of changed) this.pendingDecorationChangedIds.add(id);
      }
    });
    return {
      dispose: () => {
        sub.dispose();
        const cleared = this.decorationStore.removeProvider(provider.id);
        if (cleared.length > 0) {
          this.decorationStore.bump(cleared);
          for (const id of cleared) this.pendingDecorationChangedIds.add(id);
        }
      },
    };
  }

  /**
   * Back-compat wrapper: wrap the port in a MessagePort channel and attach
   * it with local-admin permissions, which is exactly what an in-process
   * UtilityProcess consumer had before channels existed.
   */
  attachPort(rawPort: MessagePortLike): Disposable {
    return this.attachChannel(createMessagePortHostChannel(rawPort));
  }

  attachChannel(channel: ExplorerHostChannel, context?: ExplorerSessionContext): Disposable {
    if (this.disposed) {
      throw new Error('FileExplorerHost is disposed');
    }
    const id = this.nextSessionId++;
    const session: Session = {
      id,
      channel,
      context: resolveSessionContext(context),
      expanded: new Set<number>(),
      viewport: { offset: 0, limit: 0, overscan: 0 },
      viewportIds: new Set<number>(),
      prefetchRows: 100,
      packedChildLists: false,
      knownIds: new Set<number>(),
      nextReqId: 1,
      handshook: false,
      lastRootIds: [],
      ackedVersion: -1,
      ackCapable: true,
      ownedOperationIds: new Set<string>(),
      resyncTimes: [],
      detach: () => {
        /* replaced below */
      },
    };

    const messageSub = channel.onMessage((msg) => this.handleMessage(session, msg));
    // A transport that dies on its own must retire just this session —
    // never the shared host (SPEC NFR-005).
    const closeSub = channel.onClose(() => this.detachSession(id));

    session.detach = (): void => {
      messageSub.dispose();
      closeSub.dispose();
      channel.close();
    };
    this.sessions.set(id, session);
    this.ensureTick();

    // Phase B2 — kick off the configured initial walk on first attach.
    // Done non-blocking so handshake can fire immediately; `roots-only`
    // drops root Entry records into the store within one NAPI hop, and
    // the next tick's delta fan-out ships `roots` to every attached
    // session. Errors surface as warnings (not fatal — an unreachable
    // root is the user's concern, not the host's).
    this.ensureInitialWalk();

    return { dispose: () => this.detachSession(id) };
  }

  /**
   * Phase B2 — run the configured initial walk exactly once, lazily, at
   * the first `attachPort`. `'full'` is a no-op here (the consumer is
   * expected to drive `populateFromRoots` themselves — back-compat with
   * v0.1). `'roots-only'` walks each configured root at depth 0 so root
   * Entry records exist in the store before the client asks to expand.
   * `'none'` is a no-op (consumer handles hydration end-to-end).
   */
  private ensureInitialWalk(): void {
    if (this.initialWalkDone) return;
    this.initialWalkDone = true;
    if (this.initialWalk === 'full' || this.initialWalk === 'none') return;
    // roots-only — walk each configured root at depth 0. The native
    // `populateFromPath` with depth=0 + includeRoot=true seeds only the
    // root Entry; children arrive when a client expands the root.
    void this.doRootsOnlyWalk();
  }

  private async doRootsOnlyWalk(): Promise<void> {
    // Reach into the Rust-configured roots via the raw native binding.
    // The TS-side `FileExplorer` doesn't expose them separately; we use
    // the wrapper's internal knowledge of the configured root paths.
    // Rather than reconstruct them, we defer to the typed wrapper:
    // `FileExplorer` accepts `Uri | string` roots and stores them on
    // `this.rootPaths` (B2 addition). The public surface is
    // `populateFromRoots` at full depth, but for roots-only we call
    // the native `populateFromPath` per root at depth 0.
    const rootsInternal = (this.explorer as unknown as { rootPaths?: readonly string[] }).rootPaths;
    if (!rootsInternal || rootsInternal.length === 0) return;
    const nativeFx = (
      this.explorer as unknown as {
        nativeFx?: {
          populateFromPath?: (p: string, d?: number | null, r?: boolean | null) => Promise<number>;
        };
      }
    ).nativeFx;
    if (!nativeFx || typeof nativeFx.populateFromPath !== 'function') {
      // Older native builds. Silently fall back to nothing; the
      // playground's setExpanded-triggered prefetch still fills the
      // root's children on first expansion.
      return;
    }
    for (const rootPath of rootsInternal) {
      try {
        // Invoke as a method on nativeFx so napi-rs preserves the
        // receiver — destructuring the method ref and calling it
        // bare drops `this` and throws TypeError: Illegal invocation.
        await nativeFx.populateFromPath(rootPath, 0, true);
      } catch (e) {
        // eslint-disable-next-line no-console
        console.warn(`[mille] initialWalk: roots-only walk failed for ${rootPath}:`, e);
      }
    }
  }

  /** Start the 16ms fan-out tick if it isn't already running. */
  private ensureTick(): void {
    if (this.tickHandle !== null || this.disposed) return;
    this.tickHandle = setInterval(() => this.tick(), TICK_MS);
    // Don't keep the process alive just for the tick. Node's default
    // setInterval ref() behaviour would otherwise prevent graceful
    // shutdown in host harnesses that rely on the event loop draining.
    const h = this.tickHandle as unknown as { unref?: () => void };
    h.unref?.();
  }

  /** Stop the tick if it's running — called once sessionCount hits 0. */
  private stopTick(): void {
    if (this.tickHandle === null) return;
    clearInterval(this.tickHandle);
    this.tickHandle = null;
  }

  /**
   * Drain one ChangeSet from the native and fan out per-session deltas.
   *
   * No-op fast path when the ChangeSet is empty (quiet ticks): Phase 7's
   * tick loop runs at 60Hz, so idle hosts pay only a single NAPI call
   * per 16ms. Wave 4 adds a dirty-flag bypass if even that shows up on a
   * flame graph.
   */
  private tick(): boolean {
    if (this.disposed || this.sessions.size === 0) return false;
    const cs = this.explorer.takePendingChanges();
    const changeSetEmpty =
      cs.changedIds.length === 0 &&
      cs.childSetChanged.length === 0 &&
      cs.subtreeRootsChanged.length === 0 &&
      cs.reparentedIds.length === 0 &&
      cs.projectionChanged !== true;

    // Drain pending subtree markers. Doing this once per tick (not once
    // per session) guarantees all attached sessions see the same marker
    // set in the delta they receive this frame.
    const coarse = this.pendingCoarseSubtrees.size > 0 ? [...this.pendingCoarseSubtrees] : [];
    const subtreeDirty = this.pendingSubtreeDirty.size > 0 ? [...this.pendingSubtreeDirty] : [];
    const subtreeResynced =
      this.pendingSubtreeResynced.size > 0 ? [...this.pendingSubtreeResynced] : [];
    if (coarse.length > 0) this.pendingCoarseSubtrees.clear();
    if (subtreeDirty.length > 0) this.pendingSubtreeDirty.clear();
    if (subtreeResynced.length > 0) this.pendingSubtreeResynced.clear();

    // Phase A1 — drain pending decoration changes. Every attached
    // session sees the same fan-out; absence of a decoration change
    // leaves these fields empty on the outgoing delta.
    const decorationChangedIds =
      this.pendingDecorationChangedIds.size > 0 ? [...this.pendingDecorationChangedIds] : [];
    if (decorationChangedIds.length > 0) this.pendingDecorationChangedIds.clear();

    // Phase B1 — check whether any session's root view is out of date.
    // Root changes usually show up in the ChangeSet too (the walker
    // adds the root entry to the native store), but we guard
    // independently so a stray root-list change without an attendant
    // ChangeSet still reaches the wire.
    let rootsChangedAnySession = false;
    if (
      changeSetEmpty &&
      coarse.length === 0 &&
      subtreeDirty.length === 0 &&
      subtreeResynced.length === 0 &&
      decorationChangedIds.length === 0
    ) {
      const currentRootIds = this.explorer
        .getSnapshot()
        .roots()
        .map((e) => e.id);
      for (const session of this.sessions.values()) {
        if (!session.handshook) continue;
        if (!arraysEqual(currentRootIds, session.lastRootIds)) {
          rootsChangedAnySession = true;
          break;
        }
      }
      if (!rootsChangedAnySession) return false;
    }

    // Build the decoration payload once. Every session's delta carries
    // the same serialized snapshot — the per-session knownIds filter
    // only applies to tree entries, not decorations (SCM status is
    // observable across every window regardless of viewport).
    let decorationsJson: string | undefined;
    if (decorationChangedIds.length > 0) {
      const payload: Record<string, readonly DecorationOnWire[]> = {};
      for (const id of decorationChangedIds) {
        const merged = this.decorationStore.getMerged(id);
        payload[String(id)] = merged.map(toWireDecoration);
      }
      decorationsJson = JSON.stringify(payload);
    }

    // Freshest snapshot — used to lift ClientEntry records for any id
    // that moved (changed, added, or reparented) this tick.
    const snap = this.explorer.getSnapshot();

    // Phase B1 — the host's current root-id list, computed once per tick
    // and diffed per session below. The snapshot's `roots()` call is cheap
    // (native snapshot is a cached view) and we want every attached
    // session to see the same root picture on any given tick.
    const currentRootIds = snap.roots().map((e) => e.id);
    // Whether this tick actually put a delta on the wire. `flushTickAcked`
    // needs to know: a tick that posts nothing has nothing to acknowledge,
    // and waiting for an ack that can never arrive would burn the fallback
    // timeout on every quiet call.
    let posted = false;
    for (const session of this.sessions.values()) {
      if (!session.handshook) continue;
      const view: SessionView = {
        expanded: session.expanded,
        knownIds: session.knownIds,
      };
      const delta = computeSessionDelta(cs, view);

      // Phase B1 — has the ordered root list changed for this session since
      // last tick? Compare by id and position. If so, we'll ship
      // the full current list on this delta; the client replaces its
      // `working.roots` verbatim. Also ensure every current root id is
      // in `knownIds` so the below changedIds/childSetChanged filter
      // doesn't silently drop the root Entry's ClientEntry payload —
      // root entries otherwise would be treated as "unknown to session"
      // and stay off the wire.
      const rootsChangedForSession = !arraysEqual(currentRootIds, session.lastRootIds);
      if (rootsChangedForSession) {
        for (const id of currentRootIds) session.knownIds.add(id);
        session.lastRootIds = [...currentRootIds];
      }

      // Bundle the ClientEntry payloads for every id whose record
      // changed. Also sweep in children of `childSetChanged` parents
      // that the session doesn't know about — expanded folders get live
      // updates when their child list grows (SPEC §4.9.5).
      const outEntries: ClientEntry[] = [];
      const outDirectChildCounts: Record<string, number> = {};
      const emitted = new Set<number>();
      const removedIds: number[] = [];
      const liveChangedIds: number[] = [];
      for (const id of delta.changedIds) {
        if (emitted.has(id)) continue;
        const entry = snap.getById(id);
        if (!entry) {
          removedIds.push(id);
          session.knownIds.delete(id);
          continue;
        }
        liveChangedIds.push(id);
        outEntries.push(entryToClient(entry));
        emitted.add(id);
        const c = snap.projectedChildCount(id);
        if (c !== null) outDirectChildCounts[String(id)] = c;
      }
      const childSetChanged = new Set(delta.childSetChanged);
      for (const rootId of coarse) childSetChanged.add(rootId);
      // A real directory child-set mutation can change both its projected
      // top-level list and the virtual children of any sibling file that
      // acts as a nesting parent.
      for (const directoryId of [...childSetChanged]) {
        for (const siblingId of snap.childrenOf(directoryId)) {
          if (session.expanded.has(siblingId)) childSetChanged.add(siblingId);
        }
      }
      const childLists = new Map<number, readonly number[]>();
      for (const parentId of childSetChanged) {
        const pc = snap.projectedChildCount(parentId);
        if (pc !== null) {
          outDirectChildCounts[String(parentId)] = pc;
        } else if (session.expanded.has(parentId)) {
          // An expanded nesting parent that just lost its final projected
          // child must actively clear the mirror's previous non-zero count.
          outDirectChildCounts[String(parentId)] = 0;
        }
        if (!session.expanded.has(parentId)) continue;
        const kids = sortedChildIds(snap, parentId);
        childLists.set(parentId, kids);
      }

      // Phase B1 — also ensure any fresh root id's ClientEntry actually
      // rides this delta. The changedIds channel above only fires for
      // ids in the native ChangeSet; if a root was pre-existing in the
      // store (e.g. populated before this session handshook) but is
      // newly visible to *this* session because `roots` just started
      // shipping, emit its Entry record too so the client can look it
      // up via `byId` when resolving `roots`.
      if (rootsChangedForSession) {
        for (const id of currentRootIds) {
          if (emitted.has(id)) continue;
          const entry = snap.getById(id);
          if (!entry) continue;
          outEntries.push(entryToClient(entry));
          emitted.add(id);
          const c = snap.projectedChildCount(id);
          if (c !== null) outDirectChildCounts[String(id)] = c;
        }
      }

      const shouldRefreshViewport =
        !changeSetEmpty ||
        coarse.length > 0 ||
        subtreeDirty.length > 0 ||
        subtreeResynced.length > 0 ||
        rootsChangedForSession;
      const viewportPatch = shouldRefreshViewport ? this.collectViewportPatch(session, snap) : null;
      if (viewportPatch !== null) {
        for (const entry of viewportPatch.entries) {
          if (emitted.has(entry.id)) continue;
          outEntries.push(entry);
          emitted.add(entry.id);
        }
        Object.assign(outDirectChildCounts, viewportPatch.directChildCounts);
      }

      this.send(
        session,
        frame('delta', {
          // Deliberately the ChangeSet's version, not the host's current one.
          // A marker-only delta understates — it reports a version older than
          // the host is at — and that is the safe direction: the mirror keeps
          // its own version monotonic, so an understated delta cannot drag it
          // backwards, and an ack simply stays truthful. Reporting
          // `snap.treeVersion` here instead would overstate whenever native
          // changes land between `takePendingChanges()` above and the snapshot
          // read, shipping a version whose entries are not in this delta. A
          // client would then ack content it does not have and `resync` would
          // resolve early — a silent false success, which is worse than the
          // timeout this replaced.
          version: delta.version,
          changedIds: liveChangedIds,
          ...(outEntries.length > 0 ? { viewportPatch: encodeClientEntries(outEntries) } : {}),
          childSetChanged: [...childSetChanged],
          ...(childLists.size > 0
            ? session.packedChildLists
              ? { childListsBin: encodeChildLists(childLists) }
              : { childLists: Object.fromEntries(childLists) }
            : {}),
          ...(viewportPatch !== null ? { viewportIds: viewportPatch.viewportIds } : {}),
          removedIds,
          directChildCounts: outDirectChildCounts,
          newVisibleCount: 0,
          coarseSubtrees: coarse,
          subtreeDirty,
          subtreeResynced,
          ...(this.ackRequestedForNextTick ? { ackRequested: true } : {}),
          ...(cs.projectionChanged
            ? {
                visibility: {
                  showHiddenFiles: snap.showHiddenFiles,
                  showIgnoredFiles: snap.showIgnoredFiles,
                  compactFolders: snap.compactFolders,
                },
              }
            : {}),
          ...(decorationChangedIds.length > 0
            ? {
                decorationChangedIds,
                ...(decorationsJson !== undefined ? { decorationsJson } : {}),
              }
            : {}),
          ...(rootsChangedForSession ? { roots: [...currentRootIds] } : {}),
        }),
      );
      posted = true;
    }
    return posted;
  }

  private handleMessage(session: Session, data: unknown): void {
    const f = validateFrameVersion(data);
    if (!f) {
      this.sendError(session, 'EINVAL', 'malformed frame');
      return;
    }
    if (!isCompatibleVersion(f.v)) {
      this.sendError(session, 'EUNSUPPORTED', `unsupported protocol v${f.v}`);
      return;
    }
    if (!session.handshook && f.type !== 'handshake') {
      this.sendError(session, 'EINVAL', 'expected handshake first');
      return;
    }
    switch (f.type) {
      case 'handshake':
        this.handleHandshake(session, f.body as { options?: { prefetchRows?: number } });
        return;
      case 'setExpanded':
        this.handleSetExpanded(session, f.body as { add?: number[]; remove?: number[] });
        return;
      case 'setViewport':
        this.handleSetViewport(
          session,
          f.body as { offset: number; limit: number; overscan?: number },
        );
        return;
      case 'mutate':
        this.handleMutate(
          session,
          f.body as { reqId: number; op: string; args: Record<string, unknown> },
        );
        return;
      case 'call':
        void this.handleCall(session, f.body as { reqId: number; method: string; args: unknown[] });
        return;
      case 'ack': {
        const version = (f.body as { version?: unknown })?.version;
        if (typeof version === 'number' && version > session.ackedVersion) {
          // Any ack proves the session speaks the protocol, so restore its
          // standing even if an earlier synchronization point timed out on a
          // momentarily busy renderer.
          session.ackCapable = true;
          session.ackedVersion = version;
          this.notifyAckWaiters();
        }
        return;
      }
      case 'dispose':
        this.detachSession(session.id);
        return;
      case 'decorations':
        this.handleDecorations(session, f.body as DecorationsFrameBody);
        return;
      default:
        this.sendError(session, 'EINVAL', `unknown message type: ${f.type}`);
    }
  }

  /**
   * Phase A1 — merge a client's decoration push into the shared
   * DecorationStore and schedule a fan-out. `replaceAll: true` wipes
   * the provider's slot first; otherwise we apply each `[id, deco]`
   * tuple as an upsert (non-null) or clear (null). Malformed bodies
   * produce a targeted `error` frame without disrupting other sessions.
   */
  private handleDecorations(session: Session, body: DecorationsFrameBody): void {
    // Client decorations write into the store every session reads from, so
    // one remote peer could otherwise paint badges in another's tree.
    const verdict = authorizeDecorations(session.context);
    if (!verdict.allowed) {
      this.sendError(session, verdict.code, verdict.message);
      return;
    }
    if (
      typeof body.providerId !== 'string' ||
      body.providerId.length === 0 ||
      !Array.isArray(body.entries)
    ) {
      this.sendError(session, 'EINVAL', 'malformed decorations frame');
      return;
    }
    const providerId = body.providerId;
    const changed = new Set<number>();

    if (body.replaceAll === true) {
      const cleared = this.decorationStore.removeProvider(providerId);
      for (const id of cleared) changed.add(id);
    }

    for (const tuple of body.entries) {
      if (!Array.isArray(tuple) || tuple.length !== 2) continue;
      const rawTuple = tuple as unknown as readonly [unknown, unknown];
      const id = rawTuple[0];
      const deco = rawTuple[1];
      if (typeof id !== 'number' || !Number.isFinite(id)) continue;
      // deco may be null (clear) or a DecorationOnWire object.
      let d: Decoration | null;
      if (deco === null) {
        d = null;
      } else if (typeof deco === 'object' && deco !== null) {
        d = deco as Decoration;
      } else {
        continue;
      }
      const entryId: number = id;
      if (this.decorationStore.setForProvider(providerId, entryId, d)) {
        changed.add(entryId);
      }
    }

    if (changed.size === 0) return;
    // Bump the store version for consumers of the 'change:decorations'
    // channel on the host-local FileExplorer, then schedule a fan-out
    // tick so every session observes the change.
    this.decorationStore.bump([...changed]);
    for (const id of changed) this.pendingDecorationChangedIds.add(id);
    this.ensureTick();
  }

  private handleHandshake(
    session: Session,
    body: { options?: { prefetchRows?: number; packedChildLists?: boolean } },
  ): void {
    session.handshook = true;
    session.packedChildLists = body.options?.packedChildLists === true;
    const requestedPrefetch = body.options?.prefetchRows;
    session.prefetchRows =
      requestedPrefetch !== undefined && Number.isFinite(requestedPrefetch)
        ? Math.min(0xffff_ffff, Math.max(0, Math.trunc(requestedPrefetch)))
        : 100;
    const snap = this.explorer.getSnapshot();
    const roots = snap.roots().map((e) => e.id);
    const rootEntries = roots
      .map((id) => snap.getById(id))
      .filter((entry): entry is Entry => entry !== null)
      .map(entryToClient);
    const directChildCounts: Record<string, number> = {};
    for (const e of rootEntries) {
      const c = snap.projectedChildCount(e.id);
      if (c !== null) directChildCounts[String(e.id)] = c;
      session.knownIds.add(e.id);
    }
    // Phase B1 — seed lastRootIds with whatever we just shipped so the
    // per-tick delta builder only re-emits `roots` when membership or order
    // changes post-handshake (walker discovery, live reorder, etc.).
    session.lastRootIds = [...roots];
    this.send(
      session,
      frame('snapshot', {
        version: snap.treeVersion,
        roots,
        // Empty ArrayBuffers are omitted because Electron's utility↔renderer
        // structured clone may drop messages that contain them.
        ...(rootEntries.length > 0 ? { mirror: encodeClientEntries(rootEntries) } : {}),
        directChildCounts,
        visibleCount: rootEntries.length,
        visibility: {
          showHiddenFiles: snap.showHiddenFiles,
          showIgnoredFiles: snap.showIgnoredFiles,
          compactFolders: snap.compactFolders,
        },
      }),
    );
  }

  private handleSetExpanded(session: Session, body: { add?: number[]; remove?: number[] }): void {
    for (const id of body.add ?? []) session.expanded.add(id);
    for (const id of body.remove ?? []) session.expanded.delete(id);

    // Headless clients may expand before publishing a viewport. Give that
    // first expansion a bounded useful window rather than returning only
    // structural placeholders; UI clients replace it with their exact range.
    if (session.viewport.limit === 0 && (body.add?.length ?? 0) > 0) {
      session.viewport = { offset: 0, limit: session.prefetchRows, overscan: 0 };
    }

    // Ship authoritative child ordering for each newly-expanded folder,
    // then hydrate only full entry records that intersect the viewport.
    // Newly-arrived children are covered by the same ordered-id plus
    // viewport-patch contract during normal delta fan-out.
    const snap = this.explorer.getSnapshot();

    // Phase B2 — auto-walk newly-expanded folders whose children aren't
    // in the store yet. Fires a depth-1 prefetch per id; delibrately
    // does NOT await — the walker publishes children via the ChangeSet
    // and the next tick's delta fan-out delivers them. We still ship
    // whatever's already in the snapshot below so the reply isn't empty
    // in the (common) case where the folder was already walked.
    //
    // Guard with `prefetched` to skip repeat walks and with `hasChildren`
    // so known-leaf folders don't trigger a pointless NAPI round-trip.
    for (const id of body.add ?? []) {
      if (this.prefetched.has(id)) continue;
      const expandable = snap.getById(id);
      if (expandable !== null && expandable.kind !== 1 && expandable.symlinkTargetIsDir !== true) {
        // Nested files are virtual containers whose child records already
        // reside in their real parent directory; never attempt a filesystem
        // walk "inside" the file.
        this.prefetched.add(id);
        continue;
      }
      const kids = snap.childrenOf(id);
      if (kids.length > 0) {
        // Already walked; mark as covered to skip future expansions too.
        this.prefetched.add(id);
        continue;
      }
      // Check hasChildren — if the snapshot says this is a known leaf,
      // there's nothing to walk. The store returns `true` when the
      // directory has cached children; when the folder hasn't been
      // walked at all, it returns `false` (can't distinguish
      // "unknown-but-maybe-has-children" from "genuine leaf" without
      // doing the walk). Fire the walk regardless for now — depth-1
      // walks of empty / leaf folders are cheap.
      this.prefetched.add(id);
      try {
        const prefetch = snap.compactFolders
          ? this.prefetchCompactChain(id)
          : this.explorer.prefetch(id, { depth: 1 });
        void prefetch
          .then(async () => {
            // The first depth-1 result may have published the raw chain head.
            // Drain its structural ChangeSet first, then re-emit this parent's
            // authoritative projected child list last so a raw walker entry
            // cannot overwrite the compact row metadata.
            if (snap.compactFolders && this.sessions.has(session.id) && session.handshook) {
              await this.flushTickNow();
              this.handleSetExpanded(session, { add: [id] });
            }
          })
          .catch((e) => {
            // eslint-disable-next-line no-console
            console.warn(`[mille] setExpanded prefetch failed for id ${id}:`, e);
          });
      } catch (e) {
        // Synchronous throw (older native missing populateFromPath).
        // Fall back to the v0.1 behaviour — ship whatever's already in
        // the snapshot — and log once.
        // eslint-disable-next-line no-console
        console.warn(`[mille] setExpanded: prefetch not available for id ${id}; carrying on:`, e);
      }
    }

    const childLists = new Map<number, readonly number[]>();
    const newDirectChildCounts: Record<string, number> = {};
    const childSetIds: number[] = [];
    for (const id of body.add ?? []) {
      const kids = sortedChildIds(snap, id);
      const childCount = snap.projectedChildCount(id);
      if (kids.length > 0 || childCount === 0) {
        childSetIds.push(id);
        childLists.set(id, kids);
      }
      if (childCount !== null) newDirectChildCounts[String(id)] = childCount;
    }

    const viewportPatch = this.collectViewportPatch(session, snap);
    Object.assign(newDirectChildCounts, viewportPatch.directChildCounts);

    this.send(
      session,
      frame('delta', {
        version: this.explorer.getTreeVersion(),
        changedIds: [],
        ...(viewportPatch.entries.length > 0
          ? { viewportPatch: encodeClientEntries(viewportPatch.entries) }
          : {}),
        childSetChanged: childSetIds,
        ...(childLists.size > 0
          ? session.packedChildLists
            ? { childListsBin: encodeChildLists(childLists) }
            : { childLists: Object.fromEntries(childLists) }
          : {}),
        viewportIds: viewportPatch.viewportIds,
        removedIds: [],
        directChildCounts: newDirectChildCounts,
        newVisibleCount: snap.visibleRowCount(session.expanded).known,
        coarseSubtrees: [],
        subtreeDirty: [],
        subtreeResynced: [],
      }),
    );
  }

  /**
   * Hydrate only the single-directory chain below an expanded folder.
   * Each step is depth-1, so a branch never causes an eager subtree walk.
   */
  private async prefetchCompactChain(parentId: number): Promise<void> {
    let current = parentId;
    for (let depth = 0; depth < 256; depth++) {
      await this.explorer.prefetch(current, { depth: 1 });
      const snapshot = this.explorer.getSnapshot();
      const children = snapshot.projectedChildrenOf(current);
      if (children.length !== 1) return;
      const child = snapshot.getById(children[0]!);
      if (child === null || child.kind !== 1) return;
      current = child.id;
    }
  }

  private handleSetViewport(
    session: Session,
    body: { offset: number; limit: number; overscan?: number },
  ): void {
    const normalize = (value: number): number =>
      Number.isFinite(value) ? Math.min(0xffff_ffff, Math.max(0, Math.trunc(value))) : 0;
    const offset = normalize(body.offset);
    const limit = normalize(body.limit);
    const overscan = normalize(body.overscan ?? 0);
    session.viewport = { offset, limit, overscan };

    const snap = this.explorer.getSnapshot();
    const viewportPatch = this.collectViewportPatch(session, snap);

    this.send(
      session,
      frame('delta', {
        version: snap.treeVersion,
        changedIds: [],
        ...(viewportPatch.entries.length > 0
          ? { viewportPatch: encodeClientEntries(viewportPatch.entries) }
          : {}),
        viewportIds: viewportPatch.viewportIds,
        childSetChanged: [],
        removedIds: [],
        directChildCounts: viewportPatch.directChildCounts,
        newVisibleCount: snap.visibleRowCount(session.expanded).known,
        coarseSubtrees: [],
        subtreeDirty: [],
        subtreeResynced: [],
      }),
    );
  }

  private collectViewportPatch(
    session: Session,
    snap: MirrorSnapshot,
  ): {
    entries: ClientEntry[];
    directChildCounts: Record<string, number>;
    viewportIds: number[];
  } {
    const { offset, limit, overscan } = session.viewport;
    const before = Math.min(offset, overscan);
    const viewportOffset = offset - before;
    const viewportLimit = Math.min(0xffff_ffff, limit + before + overscan);
    const rows = snap.visibleRows({
      expanded: session.expanded,
      offset: viewportOffset,
      limit: viewportLimit,
    });
    const entries: ClientEntry[] = [];
    const directChildCounts: Record<string, number> = {};
    const viewportIds: number[] = [];
    for (const row of rows) {
      viewportIds.push(row.id);
      session.knownIds.add(row.id);
      const childCount = snap.projectedChildCount(row.id);
      if (childCount !== null) {
        directChildCounts[String(row.id)] = childCount;
      } else if (row.kind === 0) {
        // Actively clear a stale nesting-parent count when a rename/delete
        // makes an ordinary file a leaf again.
        directChildCounts[String(row.id)] = 0;
      }
      if (session.viewportIds.has(row.id) && row.pathSegments === undefined) continue;
      // Keep projection metadata (notably compact-folder pathSegments)
      // instead of re-reading the raw entry by id.
      entries.push(entryToClient(row));
    }
    session.viewportIds = new Set(viewportIds);
    return { entries, directChildCounts, viewportIds };
  }

  private handleMutate(
    session: Session,
    body: { reqId: number; op: string; args: Record<string, unknown> },
  ): void {
    // SPEC §5.1 ordering: serialize every mutation on a single promise
    // chain. Inside the chain entry we
    //   1. dispatch the op against the local FileExplorer
    //   2. flush a delta to every session synchronously (before step 3)
    //   3. THEN post mutateResult back to the initiator
    // so a remote session observes the state change before the
    // initiator's own resolve — windows never disagree about "did that
    // rename happen yet?".
    //
    // The outer .catch() is essential: without it, a rejection inside
    // the entry would poison the chain and block every subsequent
    // mutation forever. We swallow (log) chain-level rejections but
    // keep the next mutation unblocked.
    this.mutationQueue = this.mutationQueue
      .then(async () => {
        let claimedOperationId: string | null = null;
        try {
          // SPEC §12.3 — policy is checked host-side, before native
          // dispatch, so no transport or client can route around it.
          const verdict = authorizeMutation(session.context, body.op);
          if (!verdict.allowed) {
            throw new FileSystemError(verdict.code, verdict.message);
          }
          // Defence in depth for the §17.2 boundary. `resolvePath` no longer
          // hands a restricted session an id beyond a symlink, but ids are
          // just numbers on the wire: one could arrive from a guess, or from
          // an entry another session hydrated into the shared store. Refuse
          // to act on it here too, rather than trusting that the only way to
          // learn an id is the one we closed.
          this.assertWithinBoundary(session, body.args);
          // SPEC §16.3 — claim the operation id. The mutation queue
          // serializes every session's mutations, so "check then claim" is
          // atomic here by construction rather than by locking.
          claimedOperationId = this.claimOperation(session, body.args);

          const result = await this.dispatchMutation(session, body.op, body.args);
          // Fan out first, reply second — and wait for the fan-out to be
          // acknowledged, not merely posted. `flushTickNow` only yields one
          // setImmediate, which is a guess about when the peer runs; it holds
          // on an idle Linux runner and loses on Windows, where the other
          // session's mirror was still a version behind when the initiator's
          // promise resolved.
          await this.flushTickAcked();
          this.send(session, frame('mutateResult', { reqId: body.reqId, result }));
        } catch (e: unknown) {
          const err = toErrorPayload(e);
          // Still flush a delta: partial state (e.g. a rename that
          // created the target before failing on the source) may have
          // landed and other sessions need to see it.
          await this.flushTickAcked();
          this.send(
            session,
            frame('mutateResult', { reqId: body.reqId, result: null, error: err }),
          );
        } finally {
          // SPEC §16.3 — release on every terminal path. A claim that
          // outlived its mutation would deny the id forever and eat the
          // session's budget. `OP_COMPLETE` may already have released it;
          // deleting twice is harmless.
          if (claimedOperationId !== null) {
            session.ownedOperationIds.delete(claimedOperationId);
          }
        }
      })
      .catch((e: unknown) => {
        // Queue-level failure (e.g. flushTickNow threw). Don't let it
        // poison the chain for subsequent mutations from any session.
        // eslint-disable-next-line no-console
        console.error('[mille] mutation queue error:', e);
      });
  }

  /**
   * Refuse an operation whose target lies beyond a symlink or junction.
   *
   * Applies to every id-bearing argument, because a move or copy names two.
   * Admin sessions are exempt — see `crossesSymlink`.
   */
  private assertWithinBoundary(session: Session, args: Record<string, unknown>): void {
    if (session.context.policy.access === 'admin') return;
    const snapshot = this.explorer.getSnapshot();
    for (const key of ['id', 'parentId', 'newParentId'] as const) {
      const value = args[key];
      if (typeof value !== 'number') continue;
      if (crossesSymlink(snapshot, value as EntryId)) {
        throw new FileSystemError('EACCES', 'target is outside the workspace boundary');
      }
    }
  }

  /** The live session that owns `operationId`, if any. */
  private findOperationOwner(operationId: string): Session | null {
    for (const session of this.sessions.values()) {
      if (session.ownedOperationIds.has(operationId)) return session;
    }
    return null;
  }

  /**
   * Claim the operation id a mutation asked to be tracked under.
   *
   * Returns the claimed id, or null when the mutation named none. Throws
   * `EEXIST` when another live session already owns it — two sessions
   * sharing an id would cross their progress streams and let either cancel
   * the other's work.
   */
  private claimOperation(session: Session, args: Record<string, unknown>): string | null {
    const operationId = requestedOperationId(args);
    if (operationId === null) return null;

    const owner = this.findOperationOwner(operationId);
    if (owner !== null && owner.id !== session.id) {
      throw new FileSystemError('EEXIST', `operation ${operationId} is already in progress`);
    }
    if (
      !session.ownedOperationIds.has(operationId) &&
      session.ownedOperationIds.size >= MAX_OWNED_OPERATIONS
    ) {
      throw new FileSystemError(
        'EBUSY',
        `session has ${MAX_OWNED_OPERATIONS} operations in flight`,
      );
    }
    session.ownedOperationIds.add(operationId);
    return operationId;
  }

  /**
   * SPEC §20.2 — 10 entry resyncs per minute per session. Sliding window
   * rather than a fixed bucket so a peer cannot burst 20 across a boundary.
   */
  private checkResyncRate(session: Session): void {
    const now = Date.now();
    session.resyncTimes = session.resyncTimes.filter((t) => now - t < RESYNC_WINDOW_MS);
    if (session.resyncTimes.length >= RESYNC_LIMIT) {
      throw new FileSystemError('EBUSY', `resync rate limit reached (${RESYNC_LIMIT} per minute)`);
    }
    session.resyncTimes.push(now);
  }

  /**
   * Immediate, out-of-band delta flush. Used by the mutation queue so
   * fan-out happens synchronously with the op rather than waiting for
   * the next 16ms tick boundary. A microtask yield after `tick()` lets
   * queued `postMessage` calls land before we reply to the initiator.
   */
  private async flushTickNow(): Promise<void> {
    this.tick();
    await new Promise<void>((resolve) => {
      // setImmediate is preferable to setTimeout(0) — it fires after
      // the current I/O phase, which is when MessagePort-postMessage
      // actually drops the message onto the peer's queue.
      setImmediate(resolve);
    });
  }

  /** Wake anything waiting in `flushTickAcked`. */
  private notifyAckWaiters(): void {
    const waiters = [...this.ackWaiters];
    for (const waiter of waiters) waiter();
  }

  /**
   * Flush a tick and wait until every handshaked session confirms it applied
   * it — the guarantee `resync` advertises.
   *
   * Posting to a MessagePort tells the host nothing about when the peer runs,
   * so the previous "tick, wait one setImmediate" was a guess that held on an
   * idle machine and lost under load. Deltas flushed here ask for an ack and
   * this waits for them.
   *
   * Falls back to resolving on `timeoutMs` so one wedged or outdated client
   * cannot hang a mutation: the caller then gets the old best-effort
   * behaviour rather than a hung promise.
   */
  private async flushTickAcked(timeoutMs = 1_000): Promise<void> {
    const sessions = [...this.sessions.values()].filter((s) => s.handshook);
    if (sessions.length === 0) {
      await this.flushTickNow();
      return;
    }

    this.ackRequestedForNextTick = true;
    const posted = this.tick();
    this.ackRequestedForNextTick = false;

    // A quiet tick put nothing on the wire, so no ack can arrive. Yield once
    // (matching `flushTickNow`) instead of waiting out the fallback timeout —
    // otherwise every no-op mutation would cost `timeoutMs`.
    if (!posted) {
      await new Promise<void>((resolve) => setImmediate(resolve));
      return;
    }

    const target = this.explorer.getSnapshot().treeVersion;
    // Sessions already known not to acknowledge are excluded rather than
    // waited on: they cannot satisfy the condition, so including them turns
    // every synchronization point into a full `timeoutMs` stall.
    const pending = (): Session[] =>
      sessions.filter((s) => this.sessions.has(s.id) && s.ackCapable && s.ackedVersion < target);
    const satisfied = (): boolean => pending().length === 0;
    if (satisfied()) {
      await new Promise<void>((resolve) => setImmediate(resolve));
      return;
    }

    await new Promise<void>((resolve) => {
      let done = false;
      const finish = (): void => {
        if (done) return;
        done = true;
        this.ackWaiters.delete(check);
        clearTimeout(timer);
        resolve();
      };
      const giveUp = (): void => {
        // Timed out. Whoever is still outstanding just demonstrated it does
        // not answer within the window; remember that so the next mutation is
        // not charged for it again. An `ack` from them clears the flag.
        for (const s of pending()) s.ackCapable = false;
        finish();
      };
      const check = (): void => {
        if (satisfied()) finish();
      };
      const timer = setTimeout(giveUp, timeoutMs);
      // Node keeps the process alive for pending timers; a host waiting on a
      // detached client should not.
      (timer as unknown as { unref?: () => void }).unref?.();
      this.ackWaiters.add(check);
      check();
    });
  }

  private async dispatchMutation(
    session: Session,
    op: string,
    args: Record<string, unknown>,
  ): Promise<unknown> {
    switch (op) {
      case 'create':
        return this.explorer.create(
          args.parentId as EntryId,
          args.name as string,
          args.kind as number,
        );
      case 'rename':
        return this.explorer.rename(args.id as EntryId, args.newName as string);
      case 'move':
        return this.explorer.move(
          args.id as EntryId,
          args.newParentId as EntryId,
          args.newName as string | undefined,
          args.options as TransferOptions | undefined,
        );
      case 'delete':
        return this.explorer.delete(
          args.id as EntryId,
          args.options as { trash?: boolean; recursive?: boolean } | undefined,
        );
      case 'copy':
        return this.explorer.copy(
          args.id as EntryId,
          args.newParentId as EntryId,
          args.newName as string | undefined,
          args.options as TransferOptions | undefined,
        );
      case 'copyFromPath': {
        const sourcePath = args.sourcePath;
        if (typeof sourcePath !== 'string' || sourcePath.length === 0) {
          throw new Error('copyFromPath requires a non-empty sourcePath string');
        }
        return this.explorer.copyFromPath(
          sourcePath,
          args.newParentId as EntryId,
          args.newName as string | undefined,
          args.options as TransferOptions | undefined,
        );
      }
      case 'undo':
        return this.explorer.undo();
      case 'readFile': {
        // SPEC §12.5 — return the Uint8Array as-is. It used to be expanded
        // into a plain number array so structured clone would not mistake
        // it for a TypedArray; that cost roughly an order of magnitude in
        // size on both transports, and the framed codec ships a typed array
        // as a raw attachment. Clients accept both forms.
        const id = args.id as EntryId;
        this.assertFileSizeAllowed(session, id);
        const bytes = await this.explorer.readFile(id);
        this.assertPayloadSizeAllowed(session, bytes.byteLength);
        return bytes;
      }
      case 'readText': {
        const id = args.id as EntryId;
        this.assertFileSizeAllowed(session, id);
        const text = await this.explorer.readText(id, args.encoding as string | undefined);
        if (typeof text === 'string') {
          this.assertPayloadSizeAllowed(session, new TextEncoder().encode(text).byteLength);
        }
        return text;
      }
      case 'writeFile': {
        const bytes = toBytes(args.data);
        this.assertPayloadSizeAllowed(session, bytes.byteLength);
        return this.explorer.writeFile(
          args.id as EntryId,
          bytes,
          args.options as { atomic?: boolean } | undefined,
        );
      }
      default:
        throw new Error(`unknown op: ${op}`);
    }
  }

  private assertFileSizeAllowed(session: Session, id: EntryId): void {
    const max = session.context.policy.maxFileBytes;
    if (max === undefined) return;
    const entry = this.explorer.getSnapshot().getById(id);
    if (entry !== null && entry.size > max) {
      throw new FileSystemError(
        'EFBIG',
        `file is ${entry.size} bytes; remote export limit is ${max} bytes`,
      );
    }
  }

  private assertPayloadSizeAllowed(session: Session, size: number): void {
    const max = session.context.policy.maxFileBytes;
    if (max !== undefined && size > max) {
      throw new FileSystemError(
        'EFBIG',
        `payload is ${size} bytes; remote export limit is ${max} bytes`,
      );
    }
  }

  private async handleCall(
    session: Session,
    body: { reqId: number; method: string; args: unknown[] },
  ): Promise<void> {
    try {
      // SPEC §12.3 — authoritative, host-side, before native dispatch.
      const verdict = authorizeCall(session.context, body.method);
      if (!verdict.allowed) {
        throw new FileSystemError(verdict.code, verdict.message);
      }
      const result = await this.dispatchCall(session, body.method, body.args);
      this.send(session, frame('callResult', { reqId: body.reqId, result }));
    } catch (e: unknown) {
      const err = toErrorPayload(e);
      this.send(session, frame('callResult', { reqId: body.reqId, result: null, error: err }));
    }
  }

  private async dispatchCall(session: Session, method: string, args: unknown[]): Promise<unknown> {
    switch (method) {
      case 'getTreeVersion':
        return this.explorer.getTreeVersion();
      case 'capabilities':
        // SPEC §12.4 — what the session is told must match what it is
        // allowed to do, or a read-only UI renders enabled write actions
        // that only fail at EROFS.
        return effectiveCapabilities(session.context, this.explorer.capabilities);
      case 'updateProjectionSettings': {
        const settings = args[0];
        if (typeof settings !== 'object' || settings === null || Array.isArray(settings)) {
          throw new Error('updateProjectionSettings requires a settings object');
        }
        const version = this.explorer.updateProjectionSettings(
          settings as ExplorerProjectionSettings,
        );
        // Publish the new ordering/visibility to every session before the
        // initiating renderer observes RPC completion.
        await this.flushTickAcked();
        return version;
      }
      case 'reorderRoots': {
        const ids = args[0];
        if (
          !Array.isArray(ids) ||
          !ids.every((id) => typeof id === 'number' && Number.isSafeInteger(id) && id >= 0)
        ) {
          throw new Error('reorderRoots requires an array of non-negative integer ids');
        }
        const version = this.explorer.reorderRoots(ids as EntryId[]);
        // The RPC is a synchronization point: every attached mirror has the
        // new order before the initiating client observes completion.
        await this.flushTickAcked();
        return version;
      }
      case 'updateWorkspaceRoots': {
        const roots = args[0];
        if (!Array.isArray(roots) || !roots.every((root) => typeof root === 'string')) {
          throw new Error('updateWorkspaceRoots requires an array of absolute path strings');
        }
        const version = await this.explorer.updateWorkspaceRoots(roots);
        await this.flushTickAcked();
        return version;
      }
      case 'refreshWorkspaceRoots': {
        const version = await this.explorer.refreshWorkspaceRoots();
        await this.flushTickAcked();
        return version;
      }
      case 'probeDestination': {
        const [parentId, name] = args;
        if (
          typeof parentId !== 'number' ||
          !Number.isSafeInteger(parentId) ||
          parentId < 0 ||
          typeof name !== 'string' ||
          name.length === 0
        ) {
          throw new Error('probeDestination requires a parent id and non-empty name');
        }
        return this.explorer.probeDestination(parentId as EntryId, name);
      }
      case 'cancelOperation': {
        const [operationId] = args;
        if (typeof operationId !== 'string' || operationId.length === 0) {
          throw new Error('cancelOperation requires a non-empty operationId');
        }
        // SPEC §16.3 / SEC-005 — a session may cancel only what it owns.
        // The denial is deliberately indistinguishable from "no such
        // operation" so an unprivileged peer cannot probe for live ids.
        const cancelVerdict = authorizeCancel(
          session.context,
          session.ownedOperationIds,
          operationId,
        );
        if (!cancelVerdict.allowed) {
          throw new FileSystemError(cancelVerdict.code, cancelVerdict.message);
        }
        return this.explorer.cancelOperation(operationId);
      }
      case 'canUndo':
        return this.explorer.canUndo();
      case 'peekUndo':
        return this.explorer.peekUndo();
      case 'lastMutation':
        return this.explorer.lastMutation();
      case 'resync': {
        const [id, recursive] = args;
        if (
          typeof id !== 'number' ||
          !Number.isSafeInteger(id) ||
          id < 0 ||
          typeof recursive !== 'boolean'
        ) {
          throw new Error('resync requires a non-negative integer id and recursive boolean');
        }
        this.checkResyncRate(session);
        const requested = this.explorer.getSnapshot().getById(id);
        const markerId =
          requested !== null && requested.kind !== 1 && requested.symlinkTargetIsDir !== true
            ? (requested.parentId ?? id)
            : id;
        const version = await this.explorer.resync(id, { recursive });
        this.prefetched.delete(markerId);
        this.markSubtreeResynced(markerId);
        await this.flushTickAcked();
        return version;
      }
      case 'resyncWorkspace': {
        const rootIds = this.explorer
          .getSnapshot()
          .roots()
          .map((root) => root.id);
        const version = await this.explorer.resyncWorkspace();
        for (const rootId of rootIds) {
          this.prefetched.delete(rootId);
          this.markSubtreeResynced(rootId);
        }
        await this.flushTickAcked();
        return version;
      }
      case 'resolvePath': {
        const path = args[0];
        if (typeof path !== 'string') throw new Error('resolvePath requires a string path');
        const id = await this.explorer.resolvePath(path);
        if (id === null) return null;

        // SPEC §17.2 / SEC-002 — a session under a restrictive policy must not
        // reach outside the configured roots by naming a path that traverses a
        // symlink or junction. Returning null rather than an error also keeps
        // "outside the export" indistinguishable from "does not exist", so a
        // remote peer cannot probe the filesystem beyond its boundary.
        //
        // Admin (in-process) sessions are exempt: they already hold the raw
        // explorer via `host.local` and can read anything the process can, so
        // gating them would break existing local consumers for no gain.
        if (
          session.context.policy.access !== 'admin' &&
          crossesSymlink(this.explorer.getSnapshot(), id)
        ) {
          return null;
        }

        // Return only the target-to-root records. This makes lazy path reveal
        // immediately usable by the renderer without shipping a full tree or
        // pretending that a partial path is an authoritative child listing.
        const snapshot = this.explorer.getSnapshot();
        const entries: ClientEntry[] = [];
        let cursor: EntryId | null = id;
        let guard = 0;
        while (cursor !== null && guard < 10_000) {
          const entry = snapshot.getById(cursor);
          if (entry === null) break;
          entries.push(entryToClient(entry));
          session.knownIds.add(cursor);
          cursor = entry.parentId ?? null;
          guard += 1;
        }
        return { id, version: snapshot.treeVersion, entries };
      }
      case 'findVisiblePrefix': {
        const [prefix, fromId, skipCurrent, expanded] = args;
        if (typeof prefix !== 'string') throw new Error('findVisiblePrefix requires a prefix');
        if (fromId !== null && typeof fromId !== 'number') {
          throw new Error('findVisiblePrefix requires a numeric or null fromId');
        }
        if (typeof skipCurrent !== 'boolean' || !Array.isArray(expanded)) {
          throw new Error('findVisiblePrefix requires skipCurrent and expanded');
        }
        const id = this.explorer
          .getSnapshot()
          .visiblePrefixMatch(prefix, fromId, skipCurrent, new Set(expanded as EntryId[]));
        if (id === null) return null;
        // Same boundary as resolvePath: a typeahead match must not be the way
        // a restricted session learns about an entry beyond a symlink that
        // some other session hydrated into the shared store.
        if (
          session.context.policy.access !== 'admin' &&
          crossesSymlink(this.explorer.getSnapshot(), id)
        ) {
          return null;
        }
        const snapshot = this.explorer.getSnapshot();
        const entries: ClientEntry[] = [];
        let cursor: EntryId | null = id;
        let guard = 0;
        while (cursor !== null && guard < 10_000) {
          const entry = snapshot.getById(cursor);
          if (entry === null) break;
          entries.push(entryToClient(entry));
          session.knownIds.add(cursor);
          cursor = entry.parentId ?? null;
          guard += 1;
        }
        return { id, version: snapshot.treeVersion, entries };
      }
      default:
        throw new Error(`unknown method: ${method}`);
    }
  }

  private send(session: Session, msg: unknown): void {
    try {
      // The host builds frames through `frame()`, which is generic over the
      // body; the channel is typed to the HostToClient union. One cast at
      // the single send site beats threading the union through every caller.
      session.channel.send(msg as HostToClientMessage);
    } catch {
      // Port may be closed mid-flight. Detach this session quietly so
      // the host can keep serving other sessions.
      this.detachSession(session.id);
    }
  }

  private sendError(session: Session, code: string, message: string): void {
    this.send(session, frame('error', { code, message }));
  }

  private detachSession(id: number): void {
    const session = this.sessions.get(id);
    if (!session) return;
    session.detach();
    // SPEC §23.3 — releasing the claims here is what stops a session that
    // dropped mid-transfer from reserving its operation ids forever.
    session.ownedOperationIds.clear();
    session.resyncTimes = [];
    this.sessions.delete(id);
    if (this.sessions.size === 0) this.stopTick();
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    this.watcherEventSub.dispose();
    this.warningSub.dispose();
    this.stopTick();
    for (const id of [...this.sessions.keys()]) {
      this.detachSession(id);
    }
    await this.explorer.dispose();
  }
}

/**
 * Phase B1 / 3.2 — ordered equality for root-id lists. Position matters:
 * a pure reorder must re-ship `roots` even though membership is unchanged.
 */
function arraysEqual(a: readonly number[], b: readonly number[]): boolean {
  if (a.length !== b.length) return false;
  for (let index = 0; index < a.length; index += 1) {
    if (a[index] !== b[index]) return false;
  }
  return true;
}

/**
 * Project an in-memory `Decoration` into its wire shape. Keeps the
 * same key set; spread-only-when-defined satisfies
 * `exactOptionalPropertyTypes`.
 */
function toWireDecoration(d: Decoration): DecorationOnWire {
  const out: { -readonly [K in keyof DecorationOnWire]: DecorationOnWire[K] } = {};
  if (d.badge !== undefined) out.badge = d.badge;
  if (d.color !== undefined) out.color = d.color;
  if (d.tooltip !== undefined) out.tooltip = d.tooltip;
  if (d.propagate !== undefined) out.propagate = d.propagate;
  return out;
}

function toErrorPayload(e: unknown): { code: string; message: string; path?: string } {
  if (isFileSystemError(e)) {
    const payload: { code: string; message: string; path?: string } = {
      code: e.code,
      message: e.message,
    };
    if (e.path !== undefined) payload.path = e.path;
    return payload;
  }
  const msg = (e as { message?: unknown } | null)?.message;
  return {
    code: 'EUNKNOWN',
    message: typeof msg === 'string' ? msg : String(e),
  };
}

/**
 * Construct a host around a native `FileExplorer`. Returns a handle
 * whose `attachPort` registers renderer sessions and whose `local`
 * exposes the explorer for same-process consumers (SCM providers,
 * background indexers) that don't need port indirection.
 */
export async function createFileExplorerHost(options: ExplorerOptions): Promise<FileExplorerHost> {
  return new FileExplorerHostImpl(options);
}

export type { FileExplorerHost, MessagePortLike } from './types.js';
