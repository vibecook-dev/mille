// The server half of a `mille.remote` session, for any host channel.
//
// Shape of one connection (SPEC §13, the same ordering mille-truffle keeps):
//
//   channel accepted ──► an open request within openTimeoutMs, else close
//        ├─ parse it; a malformed one is refused, nothing else happens
//        ├─ authorize(exportId, requestedAccess, credential) → a grant
//        ├─ the session policy is the GRANT's, narrowed by what was asked
//        ├─ attach to the grant's host — the first time any host is touched
//        ├─ send `accepted`
//        └─ explorer frames flow; heartbeats are answered here
//
// The ordering is the security property: before authorize answers, the
// connection owns a timer and nothing else — no host session, no engine
// work, no filesystem access. And the policy is never read from the
// request: a client can ask for less than it was granted, never more.
//
// Service frames never reach the host. The host is attached to a view of
// the channel that carries only explorer frames, so a heartbeat `pong` is
// not mistaken for a malformed explorer frame (which the host would answer
// with an error that rejects the client's pending requests).

import type { Disposable } from '../types.js';
import { ListenerSet } from '../channel/emitter.js';
import type {
  ExplorerChannelCloseCode,
  ExplorerChannelCloseEvent,
  ExplorerChannelLogger,
  ExplorerChannelState,
  ExplorerHostChannel,
  ExplorerSessionAccess,
  ExplorerSessionContext,
  ExplorerSessionPolicy,
} from '../channel/types.js';
import type { ClientToHostMessage, HostToClientMessage } from '../protocol.js';
import {
  resolveLimits,
  type FramedStreamLimits,
  type FramedStreamLimitsInput,
} from '../stream/limits.js';
import {
  isRemoteServiceMessage,
  parseOpenRequest,
  ping,
  pong,
  randomId,
  rejection,
  RemoteOpenError,
  REMOTE_SERVICE,
  type OpenWorkspaceAccepted,
  type OpenWorkspaceRejectCode,
  type OpenWorkspaceRequest,
  type RemoteAccess,
  type RemotePing,
  type RemoteServiceMessage,
} from './protocol.js';

/** What a session attaches to. `FileExplorerHost` satisfies it. */
export interface RemoteSessionHost {
  attachChannel(channel: ExplorerHostChannel, context?: ExplorerSessionContext): Disposable;
}

/** What the authorize hook is told about an open request. */
export interface RemoteOpenContext {
  readonly exportId: string;
  readonly requestedAccess: RemoteAccess;
  /**
   * The credential the client presented, if any. A secret: check it, then
   * drop it. Never log it — mille never does, and never echoes it back.
   */
  readonly credential: string | undefined;
  /** The client's self-chosen instance id. A label for logs, not an identity. */
  readonly clientInstanceId: string;
  readonly clientName: string | undefined;
}

/** A yes: which host, under which policy. */
export interface RemoteSessionGrant {
  readonly host: RemoteSessionHost;
  /**
   * The session's policy, authoritative. The client's `requestedAccess` can
   * only narrow it; asking for read-write on a read-only grant is refused.
   * `maxFileBytes` defaults to `DEFAULT_REMOTE_MAX_FILE_BYTES` and is always
   * capped to what one frame can carry.
   */
  readonly policy: ExplorerSessionPolicy;
  /** Shown to the client in `accepted`. Defaults to the export id. */
  readonly label?: string | undefined;
  readonly rootCount?: number | undefined;
  /** A verified identity for the session context, when the embedder has one. */
  readonly peerId?: string | undefined;
  readonly peerName?: string | undefined;
  /**
   * Identifies the host instance, so a client knows its EntryIds survived a
   * reconnect. Defaults to an id minted once per host object.
   */
  readonly workspaceInstanceId?: string | undefined;
}

/** A no with a reason the client may act on. A bare `null`/`false` is ACCESS_DENIED. */
export interface RemoteSessionRefusal {
  readonly refuse: 'ACCESS_DENIED' | 'LIMIT_EXCEEDED' | 'SERVER_SHUTTING_DOWN';
  /** For the server's log; sent to the client only under `diagnosticDisclosure`. */
  readonly reason?: string | undefined;
}

export type RemoteAuthorizeResult =
  | RemoteSessionGrant
  | RemoteSessionRefusal
  | null
  | undefined
  | false;

export interface RemoteSessionLogger {
  info?(event: string, fields?: Record<string, unknown>): void;
  warn?(event: string, fields?: Record<string, unknown>): void;
  debug?(event: string, fields?: Record<string, unknown>): void;
}

export interface ServeRemoteSessionOptions {
  /**
   * Decide the open. Runs once per connection, before any host is touched.
   * A throw is a denial, never an accidental allow; its message is logged
   * with the credential scrubbed out.
   */
  authorize(context: RemoteOpenContext): RemoteAuthorizeResult | Promise<RemoteAuthorizeResult>;
  /** Deadline from accept to `accepted`, authorize included. Default 10 s. */
  readonly openTimeoutMs?: number;
  /** Ping after this much inbound silence. Default 20 s; 0 turns heartbeats off. */
  readonly heartbeatMs?: number;
  /** Close after this much inbound silence. Default 60 s. Ignored when heartbeats are off. */
  readonly idleTimeoutMs?: number;
  /**
   * Tell the client *why* an open was refused. Off by default: an unknown
   * export and a forbidden one must look the same from outside (SEC-006).
   */
  readonly diagnosticDisclosure?: boolean;
  readonly logger?: RemoteSessionLogger;
  /** The frame limits the channel runs under; reported in `accepted`. */
  readonly limits?: FramedStreamLimitsInput;
}

export interface RemoteSessionInfo {
  readonly sessionId: string;
  readonly exportId: string;
  readonly access: ExplorerSessionAccess;
  readonly workspaceInstanceId: string;
  readonly clientInstanceId: string;
  readonly peerId?: string | undefined;
  readonly peerName?: string | undefined;
  readonly openedAtMs: number;
}

export interface RemoteSession {
  readonly state: 'opening' | 'open' | 'closed';
  /** Set once the session is accepted. */
  readonly info: RemoteSessionInfo | undefined;
  /** Resolves on accept; rejects with a `RemoteOpenError` when the open fails. Never unhandled. */
  readonly opened: Promise<RemoteSessionInfo>;
  /** Resolves when the channel closes, for whatever reason. */
  readonly closed: Promise<ExplorerChannelCloseEvent>;
  close(reason?: string): void;
}

/** A remote session's per-file cap when its grant names none. Matches mille-truffle. */
export const DEFAULT_REMOTE_MAX_FILE_BYTES = 16 * 1024 * 1024;

/** Room a file payload's frame needs besides the bytes: header, JSON envelope, table. */
const FILE_FRAME_HEADROOM_BYTES = 64 * 1024;

const DEFAULTS = {
  openTimeoutMs: 10_000,
  heartbeatMs: 20_000,
  idleTimeoutMs: 60_000,
} as const;

const workspaceInstanceIds = new WeakMap<object, string>();

function workspaceInstanceIdFor(host: RemoteSessionHost): string {
  let id = workspaceInstanceIds.get(host);
  if (id === undefined) {
    id = randomId();
    workspaceInstanceIds.set(host, id);
  }
  return id;
}

/** `FileExplorerHost.local.getSnapshot().roots()`, when the host is one. */
function rootCountOf(host: RemoteSessionHost): number {
  try {
    const local = (host as { local?: { getSnapshot?: () => { roots?: () => unknown[] } } }).local;
    const roots = local?.getSnapshot?.().roots?.();
    return Array.isArray(roots) ? roots.length : 0;
  } catch {
    return 0;
  }
}

function isGrant(value: unknown): value is RemoteSessionGrant {
  return (
    typeof value === 'object' &&
    value !== null &&
    typeof (value as { host?: { attachChannel?: unknown } }).host?.attachChannel === 'function' &&
    typeof (value as { policy?: { access?: unknown } }).policy?.access === 'string'
  );
}

function isRefusal(value: unknown): value is RemoteSessionRefusal {
  return typeof value === 'object' && value !== null && 'refuse' in value;
}

/** Remove every occurrence of the credential from a string bound for a log. */
function scrub(text: string, credential: string | undefined): string {
  if (credential === undefined || credential.length === 0) return text;
  return text.split(credential).join('[credential]');
}

type PolicyOutcome =
  | { readonly ok: true; readonly policy: ExplorerSessionPolicy }
  | { readonly ok: false; readonly reason: string };

/**
 * The policy a session actually runs under: the grant's, narrowed by the
 * request, with a file cap that always fits in one frame.
 */
function effectivePolicy(
  granted: ExplorerSessionPolicy,
  requested: RemoteAccess,
  limits: FramedStreamLimits,
): PolicyOutcome {
  // A client that asks for writes on a read-only grant is refused, not
  // quietly downgraded: a UI built for writes would fail later and hide a
  // misconfiguration (mille-truffle's §16.1 rule).
  if (requested === 'read-write' && granted.access === 'read-only') {
    return { ok: false, reason: 'read-write requested on a read-only grant' };
  }
  const frameCap = Math.max(1, limits.maxFrameBytes - FILE_FRAME_HEADROOM_BYTES);
  const maxFileBytes = Math.min(granted.maxFileBytes ?? DEFAULT_REMOTE_MAX_FILE_BYTES, frameCap);
  if (requested === 'read-only' && granted.access !== 'read-only') {
    // Asking for less is allowed. A session that asked to be read-only gets
    // none of the host-global flags either: they all mutate shared state.
    return { ok: true, policy: { access: 'read-only', maxFileBytes } };
  }
  return { ok: true, policy: { ...granted, maxFileBytes } };
}

/** `accepted.export.access` speaks the request's vocabulary, which has no admin. */
function reportedAccess(access: ExplorerSessionAccess): RemoteAccess {
  return access === 'read-only' ? 'read-only' : 'read-write';
}

/** Close with a code the peer can read when the transport supports one. */
function closeChannelAs(
  channel: ExplorerHostChannel,
  code: ExplorerChannelCloseCode,
  reason: string,
): void {
  const closeAs = (channel as { closeAs?: (c: ExplorerChannelCloseCode, r?: string) => void })
    .closeAs;
  if (typeof closeAs === 'function') closeAs.call(channel, code, reason);
  else channel.close(reason);
}

/**
 * The channel the host is attached to: the same transport, but its message
 * listeners receive only explorer frames, and only once the session has
 * been accepted.
 */
class HostView implements ExplorerHostChannel {
  readonly #raw: ExplorerHostChannel;
  readonly #messages: ListenerSet<ClientToHostMessage>;

  constructor(raw: ExplorerHostChannel, logger: ExplorerChannelLogger) {
    this.#raw = raw;
    this.#messages = new ListenerSet<ClientToHostMessage>('channel message', logger);
    raw.onClose(() => this.#messages.clear());
  }

  deliver(message: ClientToHostMessage): void {
    this.#messages.emit(message);
  }

  get state(): ExplorerChannelState {
    return this.#raw.state;
  }

  get bufferedBytes(): number {
    return this.#raw.bufferedBytes;
  }

  send(message: HostToClientMessage): void {
    this.#raw.send(message);
  }

  drain(): Promise<void> {
    return this.#raw.drain();
  }

  onMessage(listener: (message: ClientToHostMessage) => void): Disposable {
    return this.#messages.add(listener);
  }

  onClose(listener: (event: ExplorerChannelCloseEvent) => void): Disposable {
    return this.#raw.onClose(listener);
  }

  close(reason?: string): void {
    this.#raw.close(reason);
  }

  dispose(): void {
    this.#raw.close('disposed');
  }
}

const GENERIC_MESSAGES: Readonly<Record<OpenWorkspaceRejectCode, string>> = {
  ACCESS_DENIED: 'access denied',
  LIMIT_EXCEEDED: 'session limit reached',
  SERVER_SHUTTING_DOWN: 'server is shutting down',
  INVALID_REQUEST: 'invalid open request',
  VERSION_UNSUPPORTED: 'service version not supported',
};

/**
 * Serve one `mille.remote` session on a host channel.
 *
 * Works on any channel — a WebSocket (`serveWebSocketSession` wraps this), a
 * framed Duplex, a MessagePort. The returned handle reports the session's
 * life; the channel is closed for you on refusal, timeout or idleness.
 */
export function serveRemoteSession(
  channel: ExplorerHostChannel,
  options: ServeRemoteSessionOptions,
): RemoteSession {
  const logger = options.logger;
  const limits = resolveLimits(options.limits);
  const openTimeoutMs = options.openTimeoutMs ?? DEFAULTS.openTimeoutMs;
  const heartbeatMs = options.heartbeatMs ?? DEFAULTS.heartbeatMs;
  const idleTimeoutMs = options.idleTimeoutMs ?? DEFAULTS.idleTimeoutMs;
  const connectionId = randomId().slice(0, 8);

  const log = (
    level: 'info' | 'warn' | 'debug',
    event: string,
    fields?: Record<string, unknown>,
  ): void => {
    try {
      logger?.[level]?.(event, { connectionId, ...fields });
    } catch {
      /* a logger must not take the session down */
    }
  };
  const channelLogger: ExplorerChannelLogger = {
    warn: (message) => log('warn', 'channel_warning', { message }),
  };

  let state: 'opening' | 'open' | 'closed' = 'opening';
  let info: RemoteSessionInfo | undefined;
  let attachment: Disposable | undefined;
  let openSeen = false;
  /** Set when the session was refused, so `opened` can say why. */
  let refusal: RemoteOpenError | undefined;
  let lastInboundMs = Date.now();

  let resolveOpened!: (value: RemoteSessionInfo) => void;
  let rejectOpened!: (reason: unknown) => void;
  const opened = new Promise<RemoteSessionInfo>((resolve, reject) => {
    resolveOpened = resolve;
    rejectOpened = reject;
  });
  // Callers that only watch `closed` must not see an unhandled rejection.
  opened.catch(() => {});
  let resolveClosed!: (event: ExplorerChannelCloseEvent) => void;
  const closed = new Promise<ExplorerChannelCloseEvent>((resolve) => {
    resolveClosed = resolve;
  });

  const hostView = new HostView(channel, channelLogger);

  const openTimer = setTimeout(() => {
    if (state !== 'opening') return;
    log('warn', 'open_timeout', { openSeen });
    refusal = new RemoteOpenError('TIMEOUT', `no session within ${openTimeoutMs}ms`);
    channel.close('open timeout');
  }, openTimeoutMs);
  (openTimer as { unref?: () => void }).unref?.();

  const heartbeat =
    heartbeatMs > 0
      ? setInterval(
          () => {
            if (state !== 'open') return;
            const idleFor = Date.now() - lastInboundMs;
            if (idleFor >= idleTimeoutMs) {
              log('warn', 'idle_timeout', { sessionId: info?.sessionId, idleFor });
              channel.close('idle timeout');
              return;
            }
            if (idleFor >= heartbeatMs) {
              try {
                channel.send(ping(randomId(), Date.now()) as unknown as HostToClientMessage);
              } catch {
                /* closing */
              }
            }
          },
          Math.max(10, Math.floor(heartbeatMs / 2)),
        )
      : undefined;
  (heartbeat as { unref?: () => void } | undefined)?.unref?.();

  const messageSub = channel.onMessage((message) => {
    lastInboundMs = Date.now();
    const candidate = message as unknown;
    if (isRemoteServiceMessage(candidate)) {
      handleService(candidate);
      return;
    }
    if (state !== 'open') {
      // Nothing may reach a host before authorization, and a client that
      // pipelines explorer frames ahead of `accepted` would otherwise lose
      // them silently and hang.
      log('warn', 'frame_before_accept', {});
      closeChannelAs(channel, 'PROTOCOL_ERROR', 'explorer frame before the session was accepted');
      return;
    }
    hostView.deliver(message);
  });
  const closeSub = channel.onClose((event) => teardown(event));

  function handleService(message: RemoteServiceMessage): void {
    switch (message.type) {
      case 'ping':
        try {
          channel.send(pong(message as RemotePing) as unknown as HostToClientMessage);
        } catch {
          /* closing */
        }
        return;
      case 'open':
        if (openSeen) {
          log('debug', 'duplicate_open', {});
          return;
        }
        openSeen = true;
        void open(message);
        return;
      default:
        // pong, or a client echoing server-only types: nothing to do.
        return;
    }
  }

  function refuse(code: OpenWorkspaceRejectCode, internalReason: string, requestId?: string): void {
    if (state !== 'opening') return;
    log('warn', 'open_rejected', { code, reason: internalReason });
    const message = options.diagnosticDisclosure === true ? internalReason : GENERIC_MESSAGES[code];
    refusal = new RemoteOpenError(code, message);
    try {
      channel.send(rejection(code, message, requestId) as unknown as HostToClientMessage);
    } catch {
      /* already closing */
    }
    const closeCode: ExplorerChannelCloseCode =
      code === 'ACCESS_DENIED'
        ? 'AUTH_REJECTED'
        : code === 'INVALID_REQUEST' || code === 'VERSION_UNSUPPORTED'
          ? 'PROTOCOL_ERROR'
          : 'LOCAL_CLOSE';
    closeChannelAs(channel, closeCode, `rejected: ${code}`);
  }

  async function open(raw: OpenWorkspaceRequest | RemoteServiceMessage): Promise<void> {
    const parsed = parseOpenRequest(raw);
    if ('code' in parsed) {
      refuse(parsed.code, parsed.message);
      return;
    }
    const credential = parsed.credential;

    let verdict: RemoteAuthorizeResult;
    try {
      verdict = await options.authorize({
        exportId: parsed.exportId,
        requestedAccess: parsed.requestedAccess,
        credential,
        clientInstanceId: parsed.client.instanceId,
        clientName: parsed.client.name,
      });
    } catch (err) {
      // §17.3 — an exception is a denial. Its message is the embedder's and
      // may quote what it was checking; scrub the credential before logging.
      const detail = err instanceof Error ? err.message : String(err);
      refuse('ACCESS_DENIED', scrub(`authorize threw: ${detail}`, credential), parsed.requestId);
      return;
    }
    // The client may have gone while authorize ran.
    if (state !== 'opening') return;

    if (!isGrant(verdict)) {
      if (isRefusal(verdict)) {
        refuse(
          verdict.refuse,
          scrub(verdict.reason ?? verdict.refuse, credential),
          parsed.requestId,
        );
      } else {
        refuse('ACCESS_DENIED', 'authorize denied the open', parsed.requestId);
      }
      return;
    }

    const effective = effectivePolicy(verdict.policy, parsed.requestedAccess, limits);
    if (!effective.ok) {
      refuse('ACCESS_DENIED', effective.reason, parsed.requestId);
      return;
    }

    let attached: Disposable;
    try {
      // Authorized — only now does a host learn this connection exists.
      attached = verdict.host.attachChannel(hostView, {
        kind: 'remote',
        clientId: parsed.client.instanceId,
        peerId: verdict.peerId,
        peerName: verdict.peerName,
        exportId: parsed.exportId,
        policy: effective.policy,
      });
    } catch (err) {
      refuse(
        'SERVER_SHUTTING_DOWN',
        `host refused the attach: ${err instanceof Error ? err.message : String(err)}`,
        parsed.requestId,
      );
      return;
    }
    attachment = attached;
    state = 'open';
    clearTimeout(openTimer);

    const sessionId = randomId();
    const workspaceInstanceId = verdict.workspaceInstanceId ?? workspaceInstanceIdFor(verdict.host);
    info = {
      sessionId,
      exportId: parsed.exportId,
      access: effective.policy.access,
      workspaceInstanceId,
      clientInstanceId: parsed.client.instanceId,
      peerId: verdict.peerId,
      peerName: verdict.peerName,
      openedAtMs: Date.now(),
    };
    const accepted: OpenWorkspaceAccepted = {
      service: REMOTE_SERVICE,
      version: 1,
      type: 'accepted',
      requestId: parsed.requestId,
      sessionId,
      workspaceInstanceId,
      export: {
        id: parsed.exportId,
        label: verdict.label ?? parsed.exportId,
        access: reportedAccess(effective.policy.access),
        rootCount: verdict.rootCount ?? rootCountOf(verdict.host),
      },
      limits: {
        maxMetadataBytes: limits.maxMetadataBytes,
        maxAttachments: limits.maxAttachments,
        maxFrameBytes: limits.maxFrameBytes,
        maxFileBytes: effective.policy.maxFileBytes ?? DEFAULT_REMOTE_MAX_FILE_BYTES,
        heartbeatMs,
        idleTimeoutMs,
      },
    };
    try {
      channel.send(accepted as unknown as HostToClientMessage);
    } catch {
      return; // the close handler tears the session down
    }
    log('info', 'open_accepted', {
      sessionId,
      exportId: parsed.exportId,
      access: effective.policy.access,
      workspaceInstanceId,
    });
    resolveOpened(info);
  }

  function teardown(event: ExplorerChannelCloseEvent): void {
    if (state === 'closed') return;
    const wasOpen = state === 'open';
    state = 'closed';
    clearTimeout(openTimer);
    if (heartbeat !== undefined) clearInterval(heartbeat);
    messageSub.dispose();
    closeSub.dispose();
    attachment?.dispose();
    if (!wasOpen) {
      rejectOpened(
        refusal ??
          new RemoteOpenError('CLOSED', `channel closed before the session opened: ${event.code}`, {
            cause: event,
          }),
      );
    }
    log(wasOpen ? 'info' : 'debug', 'session_closed', {
      sessionId: info?.sessionId,
      code: event.code,
    });
    resolveClosed(event);
  }

  return {
    get state() {
      return state;
    },
    get info() {
      return info;
    },
    opened,
    closed,
    close: (reason) => channel.close(reason ?? 'closed by server'),
  };
}
