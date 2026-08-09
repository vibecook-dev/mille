// Transport-neutral channel contracts — remote-workspace PR 1 (SPEC §9).
//
// The host and client have always talked over a MessagePort. That works
// in-process (Electron UtilityProcess ↔ renderer) and nowhere else. This
// module names the shape they actually depend on — an ordered, reliable,
// message-oriented pipe with a close notification — so a second transport
// (a Node Duplex, and through it a Truffle mesh socket) can be dropped in
// without the host or client learning anything about it.
//
// Nothing here imports `node:*` or any transport. The MessagePort adapter
// lives in ./message-port.ts; the framed stream channel lands in PR 2 under
// ../stream/ and is exported only from `@vibecook/mille/node`.

import type { Disposable } from '../types.js';
import type { ClientToHostMessage, HostToClientMessage } from '../protocol.js';

export type ExplorerChannelState = 'open' | 'closing' | 'closed';

/**
 * Why a channel stopped. The most specific code available at the first
 * transition away from `open` wins; later failures are logged, not
 * re-emitted (see CH-005).
 *
 * `REMOTE_CLOSE` is not reachable over a bare MessagePort — the
 * `MessagePortLike` surface has no close event — so the MessagePort
 * adapter only ever produces `LOCAL_CLOSE` or `TRANSPORT_ERROR`.
 */
export type ExplorerChannelCloseCode =
  | 'LOCAL_CLOSE'
  | 'REMOTE_CLOSE'
  | 'TRANSPORT_ERROR'
  | 'PROTOCOL_ERROR'
  | 'BACKPRESSURE'
  | 'AUTH_REJECTED';

export interface ExplorerChannelCloseEvent {
  readonly code: ExplorerChannelCloseCode;
  readonly reason?: string | undefined;
  readonly cause?: unknown;
}

/**
 * Where a channel reports problems it deliberately does not throw on —
 * a listener that threw (CH-008), or a transport error after close.
 * Defaults to a no-op so embedders opt in rather than inherit console noise.
 */
export interface ExplorerChannelLogger {
  warn(message: string, detail?: unknown): void;
}

/**
 * An ordered, reliable, message-oriented connection between one host
 * session and one client.
 *
 * Required semantics (SPEC §9.2), restated because implementations must
 * hold them and tests assert them:
 *
 * - CH-001 receive order matches `send()` order.
 * - CH-002 `send()` confirms local enqueue only — never remote receipt.
 *   Mille's existing ack/result frames retain that role.
 * - CH-003 `send()` throws when the channel is not open, or when accepting
 *   the message would exceed the hard outbound limit.
 * - CH-004 `drain()` resolves only once everything queued before the call
 *   has been accepted by the transport.
 * - CH-005 `onClose` fires exactly once.
 * - CH-006 no messages are delivered after close.
 * - CH-007 listener disposal is idempotent.
 * - CH-008 a throwing listener is reported to the logger and does not kill
 *   the channel or stop the remaining listeners.
 */
export interface ExplorerChannel<TOutbound, TInbound> extends Disposable {
  readonly state: ExplorerChannelState;

  /**
   * Bytes queued by this adapter but not yet accepted by the transport.
   *
   * Always `0` on the MessagePort adapter: `postMessage` accepts
   * synchronously and structured-cloned objects have no byte count we can
   * observe. The framed stream channel reports real queued bytes.
   */
  readonly bufferedBytes: number;

  /** Queue one ordered message. Throws per CH-003. */
  send(message: TOutbound): void;

  /** Resolves once everything queued before this call has been accepted. */
  drain(): Promise<void>;

  onMessage(listener: (message: TInbound) => void): Disposable;
  onClose(listener: (event: ExplorerChannelCloseEvent) => void): Disposable;

  /** Idempotent. Emits exactly one close event locally. */
  close(reason?: string): void;
}

export type ExplorerHostChannel = ExplorerChannel<HostToClientMessage, ClientToHostMessage>;

export type ExplorerClientChannel = ExplorerChannel<ClientToHostMessage, HostToClientMessage>;

// ─── Session context and policy (SPEC §12.1) ────────────────────────────
//
// Declared here because they are part of the `attachChannel` contract.
// PR 1 stores the context and masks nothing; the enforcement tables land
// in PR 3. Shipping the types now means the Truffle server can be written
// against a stable signature.

export type ExplorerSessionAccess = 'admin' | 'read-write' | 'read-only';

/**
 * What a session is permitted to do. Every `allow*` flag defaults to
 * denied for remote sessions and allowed for `admin`; the operations they
 * gate are host-global today (undo, projection settings, workspace roots),
 * so a remote peer must not reach them.
 */
export interface ExplorerSessionPolicy {
  readonly access: ExplorerSessionAccess;
  /** Maximum bytes a remote session may read or write in one file operation. */
  readonly maxFileBytes?: number;
  readonly allowClientDecorations?: boolean;
  readonly allowProjectionMutation?: boolean;
  readonly allowWorkspaceRootMutation?: boolean;
  readonly allowExternalImport?: boolean;
  readonly allowUndo?: boolean;
  readonly allowWorkspaceResync?: boolean;
}

/**
 * Who is on the other end of a channel.
 *
 * `peerId` must be a transport-verified identity, never a value the client
 * supplied about itself. Over Truffle that is the inbound socket's
 * `remotePeerId` (a Tailscale WhoIs node id).
 */
export interface ExplorerSessionContext {
  readonly kind?: 'local' | 'remote' | undefined;
  readonly clientId?: string | undefined;
  // `| undefined` throughout, not just `?`. Under exactOptionalPropertyTypes
  // the two differ, and every real caller builds this from values that are
  // already `string | undefined` — a Truffle socket's `remotePeerId` is
  // exactly that. Without it, constructing a context requires conditional
  // spreads at every call site.
  readonly peerId?: string | undefined;
  readonly peerName?: string | undefined;
  readonly exportId?: string | undefined;
  readonly policy?: ExplorerSessionPolicy | undefined;
}

/** A context with the defaults applied, as stored on a live session. */
export interface ResolvedSessionContext {
  readonly kind: 'local' | 'remote';
  readonly clientId?: string | undefined;
  readonly peerId?: string | undefined;
  readonly peerName?: string | undefined;
  readonly exportId?: string | undefined;
  readonly policy: Required<Pick<ExplorerSessionPolicy, 'access'>> & ExplorerSessionPolicy;
}

/**
 * The policy an in-process consumer gets when it calls `attachPort()` with
 * no context: full admin, matching v0.3 behavior exactly. Remote callers
 * must always pass an explicit context.
 */
export const LOCAL_ADMIN_POLICY: ExplorerSessionPolicy = {
  access: 'admin',
  allowClientDecorations: true,
  allowProjectionMutation: true,
  allowWorkspaceRootMutation: true,
  allowExternalImport: true,
  allowUndo: true,
  allowWorkspaceResync: true,
};

export function resolveSessionContext(context?: ExplorerSessionContext): ResolvedSessionContext {
  const kind = context?.kind ?? 'local';
  const policy =
    context?.policy ?? (kind === 'local' ? LOCAL_ADMIN_POLICY : { access: 'read-only' });
  return {
    kind,
    clientId: context?.clientId,
    peerId: context?.peerId,
    peerName: context?.peerName,
    exportId: context?.exportId,
    policy,
  };
}
