// The `mille.remote` service vocabulary (SPEC §13), shared by every transport.
//
// These messages ride the same channel as explorer frames, before them: a
// client names an export and presents a credential, the server answers
// `accepted` or `rejected`, and only then does the explorer protocol start.
// Heartbeats (`ping`/`pong`) share the namespace and never reach a host.
//
// This was `@vibecook/mille-truffle`'s handshake module. It lives here now so
// a WebSocket session and a Truffle session speak one definition, and so a
// browser client can import it: it is pure, side-effect free and imports
// nothing. mille-truffle re-exports it unchanged.
//
// Additions over the original v1 shape, both backward compatible because an
// older parser builds its result field by field and drops what it does not
// know: `credential` (a bearer ticket the server's authorize hook checks) and
// an optional `client.milleTruffleVersion`.

export const REMOTE_SERVICE = 'mille.remote';
export const REMOTE_SERVICE_VERSION = 1;

export type RemoteAccess = 'read-only' | 'read-write';

/** Longest credential an open request may carry. A ticket is far shorter. */
export const MAX_REMOTE_CREDENTIAL_LENGTH = 4096;

export interface OpenWorkspaceRequest {
  readonly service: typeof REMOTE_SERVICE;
  readonly version: 1;
  readonly type: 'open';
  readonly requestId: string;
  readonly exportId: string;
  readonly requestedAccess: RemoteAccess;
  /**
   * A bearer credential — typically a short-lived ticket — for the server's
   * authorize hook. A secret: servers must never log it or echo it back.
   * Absent when the transport itself authenticates the peer (Truffle's
   * WhoIs identity) or the upgrade already did.
   */
  readonly credential?: string;
  readonly client: {
    readonly instanceId: string;
    readonly name?: string;
    readonly milleVersion: string;
    readonly milleTruffleVersion?: string;
  };
}

export interface OpenWorkspaceLimits {
  readonly maxMetadataBytes: number;
  readonly maxAttachments: number;
  readonly maxFrameBytes: number;
  readonly maxFileBytes: number;
  readonly heartbeatMs: number;
  readonly idleTimeoutMs: number;
}

export interface OpenWorkspaceAccepted {
  readonly service: typeof REMOTE_SERVICE;
  readonly version: 1;
  readonly type: 'accepted';
  readonly requestId: string;
  readonly sessionId: string;
  readonly workspaceInstanceId: string;
  readonly export: {
    readonly id: string;
    readonly label: string;
    readonly access: RemoteAccess;
    readonly rootCount: number;
  };
  readonly limits: OpenWorkspaceLimits;
}

export type OpenWorkspaceRejectCode =
  | 'ACCESS_DENIED'
  | 'VERSION_UNSUPPORTED'
  | 'INVALID_REQUEST'
  | 'LIMIT_EXCEEDED'
  | 'SERVER_SHUTTING_DOWN';

export interface OpenWorkspaceRejected {
  readonly service: typeof REMOTE_SERVICE;
  readonly version: 1;
  readonly type: 'rejected';
  readonly requestId?: string;
  readonly code: OpenWorkspaceRejectCode;
  readonly message: string;
}

export interface RemotePing {
  readonly service: typeof REMOTE_SERVICE;
  readonly version: 1;
  readonly type: 'ping';
  readonly nonce: string;
  readonly sentAtMs: number;
}

export interface RemotePong {
  readonly service: typeof REMOTE_SERVICE;
  readonly version: 1;
  readonly type: 'pong';
  readonly nonce: string;
  readonly sentAtMs: number;
}

export type RemoteServiceMessage =
  | OpenWorkspaceRequest
  | OpenWorkspaceAccepted
  | OpenWorkspaceRejected
  | RemotePing
  | RemotePong;

/**
 * Is this frame addressed to the remote-workspace service rather than to the
 * explorer protocol?
 *
 * Heartbeats are service-layer and must never reach `FileExplorerHost`
 * (§13.6), so every inbound frame is checked against this before dispatch.
 */
export function isRemoteServiceMessage(value: unknown): value is RemoteServiceMessage {
  if (typeof value !== 'object' || value === null) return false;
  return (value as { service?: unknown }).service === REMOTE_SERVICE;
}

/** Why a candidate open request is not one. */
export interface RequestProblem {
  readonly code: OpenWorkspaceRejectCode;
  readonly message: string;
}

const EXPORT_ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

function isNonEmptyString(v: unknown, max = 256): v is string {
  return typeof v === 'string' && v.length > 0 && v.length <= max;
}

/**
 * Validate an inbound open request.
 *
 * Returns the typed request or the reason it is not one. Note the version
 * check runs before anything else: a peer speaking a future service version
 * may legitimately send fields we would otherwise call invalid, and telling
 * it `VERSION_UNSUPPORTED` is more useful than `INVALID_REQUEST`.
 *
 * No message here ever quotes a field's value — in particular never the
 * credential, which a caller may log alongside the problem.
 */
export function parseOpenRequest(value: unknown): OpenWorkspaceRequest | RequestProblem {
  if (typeof value !== 'object' || value === null) {
    return { code: 'INVALID_REQUEST', message: 'open request must be an object' };
  }
  const v = value as Record<string, unknown>;

  if (v.service !== REMOTE_SERVICE) {
    return { code: 'INVALID_REQUEST', message: 'not a mille.remote message' };
  }
  if (v.version !== REMOTE_SERVICE_VERSION) {
    return {
      code: 'VERSION_UNSUPPORTED',
      message: `service version ${String(v.version)} is not supported`,
    };
  }
  if (v.type !== 'open') {
    return { code: 'INVALID_REQUEST', message: `expected an open request, got ${String(v.type)}` };
  }
  if (!isNonEmptyString(v.requestId, 128)) {
    return { code: 'INVALID_REQUEST', message: 'requestId must be a non-empty string' };
  }
  if (!isNonEmptyString(v.exportId, 64) || !EXPORT_ID_RE.test(v.exportId)) {
    // Deliberately does not say whether the export exists (SEC-006).
    return { code: 'INVALID_REQUEST', message: 'exportId is malformed' };
  }
  if (v.requestedAccess !== 'read-only' && v.requestedAccess !== 'read-write') {
    return { code: 'INVALID_REQUEST', message: 'requestedAccess must be read-only or read-write' };
  }
  if (v.credential !== undefined && !isNonEmptyString(v.credential, MAX_REMOTE_CREDENTIAL_LENGTH)) {
    return {
      code: 'INVALID_REQUEST',
      message: `credential must be a non-empty string of at most ${MAX_REMOTE_CREDENTIAL_LENGTH} characters`,
    };
  }

  const client = v.client;
  if (typeof client !== 'object' || client === null) {
    return { code: 'INVALID_REQUEST', message: 'client block is required' };
  }
  const c = client as Record<string, unknown>;
  if (!isNonEmptyString(c.instanceId, 128)) {
    return { code: 'INVALID_REQUEST', message: 'client.instanceId is required' };
  }
  if (!isNonEmptyString(c.milleVersion, 64)) {
    return { code: 'INVALID_REQUEST', message: 'client version strings are required' };
  }
  if (c.milleTruffleVersion !== undefined && !isNonEmptyString(c.milleTruffleVersion, 64)) {
    return { code: 'INVALID_REQUEST', message: 'client version strings are required' };
  }
  if (c.name !== undefined && !isNonEmptyString(c.name, 128)) {
    return { code: 'INVALID_REQUEST', message: 'client.name must be a non-empty string' };
  }

  return {
    service: REMOTE_SERVICE,
    version: 1,
    type: 'open',
    requestId: v.requestId,
    exportId: v.exportId,
    requestedAccess: v.requestedAccess,
    ...(v.credential === undefined ? null : { credential: v.credential as string }),
    client: {
      instanceId: c.instanceId,
      ...(c.name === undefined ? null : { name: c.name as string }),
      milleVersion: c.milleVersion,
      ...(c.milleTruffleVersion === undefined
        ? null
        : { milleTruffleVersion: c.milleTruffleVersion as string }),
    },
  };
}

export function rejection(
  code: OpenWorkspaceRejectCode,
  message: string,
  requestId?: string,
): OpenWorkspaceRejected {
  return {
    service: REMOTE_SERVICE,
    version: 1,
    type: 'rejected',
    ...(requestId === undefined ? null : { requestId }),
    code,
    message,
  };
}

export function pong(ping: RemotePing): RemotePong {
  return {
    service: REMOTE_SERVICE,
    version: 1,
    type: 'pong',
    nonce: ping.nonce,
    sentAtMs: ping.sentAtMs,
  };
}

export function ping(nonce: string, sentAtMs: number): RemotePing {
  return { service: REMOTE_SERVICE, version: 1, type: 'ping', nonce, sentAtMs };
}

// ─── Errors ─────────────────────────────────────────────────────────────

/**
 * Why an open did not produce a session. A server's refusal carries its own
 * code; the rest describe how the attempt ended without one.
 */
export type RemoteOpenErrorCode =
  | OpenWorkspaceRejectCode
  /** No answer (client) or no open request (server) within the deadline. */
  | 'TIMEOUT'
  /** The channel closed before the open completed. */
  | 'CLOSED'
  /** The caller's AbortSignal fired. */
  | 'ABORTED';

export class RemoteOpenError extends Error {
  override readonly name = 'RemoteOpenError';
  readonly code: RemoteOpenErrorCode;

  constructor(code: RemoteOpenErrorCode, message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.code = code;
  }
}

// ─── Identifiers ────────────────────────────────────────────────────────

/**
 * A random identifier for requests, sessions and client instances. These
 * are labels, not secrets: `crypto.randomUUID` is preferred, and the
 * fallback covers a renderer that is not a secure context, where the
 * standard leaves `randomUUID` undefined.
 */
export function randomId(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (typeof c?.randomUUID === 'function') return c.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 14)}`;
}
