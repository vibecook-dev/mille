// Shared runtime types for the host/client split — Phase 7 commit 7.1.
//
// The package build generates public declarations from these exports.

import type { FileExplorer } from './client.js';
import type { ExplorerHostChannel, ExplorerSessionContext } from './channel/types.js';

/**
 * Minimal `MessagePort`-like surface satisfied by `MessagePortMain`,
 * the DOM `MessagePort`, and Node's `worker_threads` `MessagePort`.
 * Mirrors api.d.ts.
 */
export interface MessagePortLike {
  postMessage(message: unknown, transfer?: readonly unknown[]): void;
  addEventListener(type: 'message', listener: (ev: { data: unknown }) => void): void;
  removeEventListener(type: 'message', listener: (ev: { data: unknown }) => void): void;
  start?(): void;
  close?(): void;
}

/** Uniform dispose shape used across the package. */
export interface Disposable {
  dispose(): void;
}

/**
 * UtilityProcess-side host. Wraps a single native `FileExplorer` and
 * multiplexes zero-or-more renderer sessions over attached MessagePorts.
 * Mirrors api.d.ts.
 */
export interface FileExplorerHost {
  /**
   * Attach a renderer session over a MessagePort. Equivalent to
   * `attachChannel(createMessagePortHostChannel(port))` — the session gets
   * local-admin permissions, matching pre-channel behavior.
   */
  attachPort(port: MessagePortLike): Disposable;
  /**
   * Attach a session over any `ExplorerChannel`. This is the transport-neutral
   * entry point: a MessagePort locally, a framed Node Duplex (and through it a
   * Truffle mesh socket) remotely. Remote callers must pass an explicit
   * context — omitting it defaults to local-admin, which is only correct
   * in-process.
   */
  attachChannel(channel: ExplorerHostChannel, context?: ExplorerSessionContext): Disposable;
  readonly sessionCount: number;
  readonly local: FileExplorer;
  /**
   * Flag a subtree root as coarse (e.g. watcher Overflow). The next
   * tick's delta will include the root in its `coarseSubtrees` field so
   * attached sessions can invalidate just that subtree rather than the
   * whole mirror. Wired to the native watcher in Phase 5.
   */
  markSubtreeCoarse(rootId: number): void;
  /**
   * Flag a subtree as volatile-dirty. The next tick's delta rides a
   * single `subtreeDirty: [rootId]` marker; per-descendant events are
   * suppressed on the wire until `markSubtreeResynced` fires. Per SPEC
   * §4.9.10 — absorbs npm-install / cargo-build event storms.
   */
  markSubtreeDirty(rootId: number): void;
  /**
   * Flag a subtree as volatile-resynced. Clears any pending dirty flag
   * on the same root (mutually exclusive transitions) and emits a
   * single `subtreeResynced: [rootId]` marker on the next delta.
   * Consumers re-query the subtree's children to pick up post-storm
   * state.
   */
  markSubtreeResynced(rootId: number): void;
  /**
   * Register an in-process decoration provider directly against the
   * host's DecorationStore. Fires fan out to every attached session
   * via the existing §4.9.11 delta pipeline. Prefer this over
   * `host.local.registerDecorationProvider` when the host is the
   * origin of the decorations — `host.local`'s store is independent
   * and never reaches attached clients.
   */
  registerDecorationProvider(provider: unknown): Disposable;
  dispose(): Promise<void>;
}
