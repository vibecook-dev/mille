// The client half of a `mille.remote` session — browser-safe.
//
// `openRemoteChannel()` runs the open handshake on any client channel and
// hands back one that carries only explorer frames: heartbeats are answered
// and filtered here, so `connectFileExplorerChannel()` (or a MessagePort
// relay) never sees a service frame. `connectWebSocketChannel()` dials a
// WebSocket first. Neither imports `node:*` or the native addon; this is the
// code a renderer Worker runs.

import type { Disposable } from '../types.js';
import { ListenerSet, NOOP_LOGGER } from '../channel/emitter.js';
import type {
  ExplorerChannelCloseEvent,
  ExplorerChannelLogger,
  ExplorerChannelState,
  ExplorerClientChannel,
} from '../channel/types.js';
import {
  PROTOCOL_VERSION,
  type ClientToHostMessage,
  type HostToClientMessage,
} from '../protocol.js';
import { WIRE_MAJOR, WIRE_MINOR } from '../stream/limits.js';
import type { WebSocketChannelOptions } from '../websocket/channel.js';
import { createWebSocketClientChannel, type WebSocketLike } from '../websocket/client.js';
import {
  isRemoteServiceMessage,
  pong,
  randomId,
  RemoteOpenError,
  REMOTE_SERVICE,
  type OpenWorkspaceAccepted,
  type OpenWorkspaceRejected,
  type OpenWorkspaceRequest,
  type RemoteAccess,
  type RemotePing,
  type RemoteServiceMessage,
} from './protocol.js';

export interface OpenRemoteChannelOptions {
  /** The export to open. Clients name an export, never a path. */
  readonly exportId: string;
  /** What to ask for. The server's grant decides; asking can only narrow it. Default read-only. */
  readonly access?: RemoteAccess;
  /** A bearer credential (a ticket) for the server's authorize hook. Never logged. */
  readonly credential?: string;
  /** A label for the server's logs. */
  readonly clientName?: string;
  /** Deadline for the server's answer. Default 15 s. */
  readonly openTimeoutMs?: number;
  readonly signal?: AbortSignal;
  readonly logger?: ExplorerChannelLogger;
}

export interface RemoteChannelConnection {
  readonly accepted: OpenWorkspaceAccepted;
  /** Explorer frames only; heartbeats are answered and filtered inside. */
  readonly channel: ExplorerClientChannel;
  close(reason?: string): void;
}

const DEFAULT_OPEN_TIMEOUT_MS = 15_000;

/**
 * What this client reports as `client.milleVersion`. A bundled client cannot
 * know its package version without build machinery, and what a server can
 * act on is what the client speaks — so it reports exactly that.
 */
const CLIENT_SPEAKS = `wire/${WIRE_MAJOR}.${WIRE_MINOR} protocol/${PROTOCOL_VERSION}`;

/**
 * A client channel with the service layer peeled off. Every inbound frame
 * is checked: `mille.remote` frames go to the open handshake (or are
 * answered, for a ping), everything else to the explorer's listeners.
 */
class RemoteClientChannel implements ExplorerClientChannel {
  readonly #raw: ExplorerClientChannel;
  readonly #messages: ListenerSet<HostToClientMessage>;
  readonly #onService: (message: RemoteServiceMessage) => void;

  constructor(
    raw: ExplorerClientChannel,
    logger: ExplorerChannelLogger,
    onService: (message: RemoteServiceMessage) => void,
  ) {
    this.#raw = raw;
    this.#onService = onService;
    this.#messages = new ListenerSet<HostToClientMessage>('channel message', logger);
    raw.onMessage((message) => {
      const candidate = message as unknown;
      if (isRemoteServiceMessage(candidate)) {
        this.#service(candidate);
        return;
      }
      this.#messages.emit(message);
    });
    raw.onClose(() => this.#messages.clear());
  }

  #service(message: RemoteServiceMessage): void {
    if (message.type === 'ping') {
      // Answered even before acceptance: the server's idle clock does not
      // care which phase the client is in.
      try {
        this.#raw.send(pong(message as RemotePing) as unknown as ClientToHostMessage);
      } catch {
        /* closing */
      }
      return;
    }
    this.#onService(message);
  }

  get state(): ExplorerChannelState {
    return this.#raw.state;
  }

  get bufferedBytes(): number {
    return this.#raw.bufferedBytes;
  }

  send(message: ClientToHostMessage): void {
    this.#raw.send(message);
  }

  drain(): Promise<void> {
    return this.#raw.drain();
  }

  onMessage(listener: (message: HostToClientMessage) => void): Disposable {
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

/**
 * Open one export on an already-connected client channel.
 *
 * Resolves once the server accepts, with a channel ready for
 * `connectFileExplorerChannel()` or `relayExplorerChannel()`. Rejects with a
 * `RemoteOpenError` — the server's own code for a refusal (`ACCESS_DENIED`
 * for a bad credential), or `TIMEOUT` / `CLOSED` / `ABORTED` — and closes
 * the channel. Mutations are never part of an open, so retrying after any
 * rejection is safe.
 */
export function openRemoteChannel(
  raw: ExplorerClientChannel,
  options: OpenRemoteChannelOptions,
): Promise<RemoteChannelConnection> {
  const logger = options.logger ?? NOOP_LOGGER;
  const timeoutMs = options.openTimeoutMs ?? DEFAULT_OPEN_TIMEOUT_MS;
  const signal = options.signal;

  if (signal?.aborted === true) {
    raw.close('aborted before open');
    return Promise.reject(new RemoteOpenError('ABORTED', 'aborted before open'));
  }

  return new Promise<RemoteChannelConnection>((resolve, reject) => {
    let settled = false;

    const channel = new RemoteClientChannel(raw, logger, (message) => {
      if (settled) return;
      if (message.type === 'accepted') {
        finish();
        const accepted = message as OpenWorkspaceAccepted;
        resolve({ accepted, channel, close: (reason) => channel.close(reason) });
        return;
      }
      if (message.type === 'rejected') {
        const refusal = message as OpenWorkspaceRejected;
        finish();
        raw.close('rejected');
        reject(new RemoteOpenError(refusal.code, refusal.message));
      }
    });

    const closeSub = raw.onClose((event) => {
      if (settled) return;
      finish();
      // A server that closes with AUTH_REJECTED refused the credential even
      // if its `rejected` frame was lost on the way.
      const code = event.code === 'AUTH_REJECTED' ? 'ACCESS_DENIED' : 'CLOSED';
      reject(
        new RemoteOpenError(code, `channel closed during open: ${event.code}`, { cause: event }),
      );
    });

    const timer = setTimeout(() => {
      if (settled) return;
      finish();
      raw.close('open timeout');
      reject(new RemoteOpenError('TIMEOUT', `no open reply within ${timeoutMs}ms`));
    }, timeoutMs);
    (timer as { unref?: () => void }).unref?.();

    const onAbort = (): void => {
      if (settled) return;
      finish();
      raw.close('aborted during open');
      reject(new RemoteOpenError('ABORTED', 'aborted during open'));
    };
    signal?.addEventListener('abort', onAbort, { once: true });

    function finish(): void {
      settled = true;
      clearTimeout(timer);
      closeSub.dispose();
      signal?.removeEventListener('abort', onAbort);
    }

    const request: OpenWorkspaceRequest = {
      service: REMOTE_SERVICE,
      version: 1,
      type: 'open',
      requestId: randomId(),
      exportId: options.exportId,
      requestedAccess: options.access ?? 'read-only',
      ...(options.credential === undefined ? null : { credential: options.credential }),
      client: {
        instanceId: randomId(),
        ...(options.clientName === undefined ? null : { name: options.clientName }),
        milleVersion: CLIENT_SPEAKS,
      },
    };
    try {
      raw.send(request as unknown as ClientToHostMessage);
    } catch (err) {
      if (settled) return;
      finish();
      reject(new RemoteOpenError('CLOSED', 'could not send the open request', { cause: err }));
    }
  });
}

/** The standard WebSocket constructor, for runtimes without a global one. */
export type WebSocketConstructorLike = new (
  url: string,
  protocols?: string | string[],
) => WebSocketLike;

export interface ConnectWebSocketChannelOptions
  extends OpenRemoteChannelOptions, Omit<WebSocketChannelOptions, 'logger'> {
  /**
   * Subprotocols to offer during the upgrade, for a server that authorizes
   * there instead of (or as well as) on the open request.
   */
  readonly protocols?: string | readonly string[];
  /**
   * The constructor to dial with. Defaults to the global `WebSocket`, which
   * browsers, Workers and Node >= 22 have; on Node 20, pass `ws`'s.
   */
  readonly WebSocket?: WebSocketConstructorLike;
}

/**
 * Dial (or adopt) a WebSocket and open one export on it.
 *
 * `target` is a URL, or a socket the caller already created — connecting or
 * open. The credential travels in the open request, inside the encrypted
 * channel when the URL is `wss:`, never in the URL: a query string ends up in
 * access logs.
 */
export function connectWebSocketChannel(
  target: string | URL | WebSocketLike,
  options: ConnectWebSocketChannelOptions,
): Promise<RemoteChannelConnection> {
  let socket: WebSocketLike;
  if (typeof target === 'string' || target instanceof URL) {
    const Ctor =
      options.WebSocket ??
      (globalThis as { WebSocket?: WebSocketConstructorLike }).WebSocket ??
      undefined;
    if (Ctor === undefined) {
      return Promise.reject(
        new TypeError(
          'no WebSocket constructor: this runtime has no global one; pass options.WebSocket',
        ),
      );
    }
    const protocols =
      options.protocols === undefined
        ? undefined
        : typeof options.protocols === 'string'
          ? options.protocols
          : [...options.protocols];
    socket =
      protocols === undefined ? new Ctor(String(target)) : new Ctor(String(target), protocols);
  } else {
    socket = target;
  }

  const { logger } = options;
  const channel = createWebSocketClientChannel(socket, {
    ...(options.maxMetadataBytes === undefined
      ? null
      : { maxMetadataBytes: options.maxMetadataBytes }),
    ...(options.maxAttachments === undefined ? null : { maxAttachments: options.maxAttachments }),
    ...(options.maxFrameBytes === undefined ? null : { maxFrameBytes: options.maxFrameBytes }),
    ...(options.outboundSoftBytes === undefined
      ? null
      : { outboundSoftBytes: options.outboundSoftBytes }),
    ...(options.outboundHardBytes === undefined
      ? null
      : { outboundHardBytes: options.outboundHardBytes }),
    ...(logger === undefined ? null : { logger }),
  });
  return openRemoteChannel(channel, options);
}
