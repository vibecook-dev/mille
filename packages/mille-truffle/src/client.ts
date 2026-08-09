// connectMille and the RemoteFileExplorer facade (SPEC §14.2, §18).
//
// The facade owns one live PortFileExplorer at a time and swaps it on
// reconnect. Callers hold the facade, not the session, so a dropped
// connection does not invalidate their reference.
//
// Two behaviours are load-bearing and easy to get wrong:
//
//   * A dropped connection leaves the last mirror snapshot readable. A stale
//     tree beats a blank one — the user was looking at something (§18.3).
//   * Queued mutations are never replayed. A write that did not return a
//     result frame did not happen as far as the caller is concerned, and
//     re-issuing it after a gap could duplicate a rename or clobber a file
//     changed in the interim (FR-008).

import { randomUUID } from 'node:crypto';

import type { PortFileExplorer } from '@vibecook/mille';
import { connectFileExplorerChannel } from '@vibecook/mille';
import { createFramedStreamClientChannel } from '@vibecook/mille/node';
import type { ExplorerClientChannel } from '@vibecook/mille';

import { RemoteExplorerError } from './errors.js';
import {
  REMOTE_SERVICE,
  type OpenWorkspaceAccepted,
  type OpenWorkspaceRejected,
  type RemoteAccess,
  type RemotePing,
} from './handshake.js';
import {
  backoffDelay,
  resolveReconnect,
  shouldRetry,
  type ReconnectOptions,
  type RemoteConnectionState,
} from './state.js';
import type { MilleRemoteLogger } from './types.js';
import type { TruffleSocketLike } from './server.js';

/** Minimal structural view of the mesh, mirroring the server side. */
export interface MeshConnectLike {
  readonly net: {
    connect(options: { peer?: unknown; host?: unknown; port: number }): TruffleSocketLike;
  };
}

export interface ConnectMilleOptions {
  /** Peer handle or query string, passed through to `mesh.net.connect`. */
  readonly peer: unknown;
  readonly port?: number;
  readonly exportId: string;
  readonly access?: RemoteAccess;
  readonly clientName?: string;
  readonly reconnect?: ReconnectOptions | false;
  readonly signal?: AbortSignal;
  readonly logger?: MilleRemoteLogger;
  /** Deadline for the open handshake reply. Default 15 s. */
  readonly openTimeoutMs?: number;
  /** Retry when the server reported it was shutting down. Default false. */
  readonly retryServerShutdown?: boolean;
}

/** Options for one authenticated channel without the reconnecting facade. */
export type ConnectMilleChannelOptions = Omit<
  ConnectMilleOptions,
  'reconnect' | 'retryServerShutdown'
>;

/**
 * A negotiated remote workspace channel. The channel carries only mille
 * explorer frames; service heartbeats are answered and filtered internally.
 */
export interface MilleChannelConnection {
  readonly accepted: OpenWorkspaceAccepted;
  readonly channel: ExplorerClientChannel;
  close(reason?: string): void;
}

export interface RemoteConnectionEvent {
  readonly state: RemoteConnectionState;
  readonly attempt: number;
  readonly workspaceInstanceId?: string | undefined;
  readonly error?: RemoteExplorerError | undefined;
}

export interface RemoteIdentityResetEvent {
  readonly previousWorkspaceInstanceId: string;
  readonly workspaceInstanceId: string;
}

export interface Disposable {
  dispose(): void;
}

export interface RemoteFileExplorer {
  readonly state: RemoteConnectionState;
  readonly exportId: string;
  readonly workspaceInstanceId: string | undefined;
  /** The live session. Throws when offline. */
  readonly explorer: PortFileExplorer;
  /** Last published snapshot, readable even while stale. */
  getSnapshot(): unknown;
  ready(): Promise<void>;
  close(): Promise<void>;
  on(event: 'connection', listener: (e: RemoteConnectionEvent) => void): Disposable;
  on(event: 'identityReset', listener: (e: RemoteIdentityResetEvent) => void): Disposable;
}

const DEFAULT_PORT = 9451;
const DEFAULT_OPEN_TIMEOUT_MS = 15_000;

/**
 * Service frames ride the same channel as explorer frames, which is typed to
 * the explorer protocol. Narrowing goes through `unknown` on purpose — these
 * two unions genuinely do not overlap, and a direct cast would be asserting
 * something false.
 */
function asService<T>(raw: unknown): T {
  return raw as unknown as T;
}

/**
 * Map a server rejection onto a client-facing error code.
 *
 * They are deliberately different vocabularies: the server says why it
 * refused, the client says what kind of failure this is. `INVALID_REQUEST`
 * in particular means *we* sent something malformed, which is a protocol
 * mismatch from the caller's point of view, not a transient fault — getting
 * this wrong would make a permanent bug look retryable.
 */
function rejectCodeToError(
  code: OpenWorkspaceRejected['code'],
): RemoteExplorerError['code'] {
  switch (code) {
    case 'VERSION_UNSUPPORTED':
    case 'INVALID_REQUEST':
      return 'PROTOCOL_MISMATCH';
    case 'ACCESS_DENIED':
      return 'ACCESS_DENIED';
    case 'LIMIT_EXCEEDED':
      return 'LIMIT_EXCEEDED';
    case 'SERVER_SHUTTING_DOWN':
      return 'SERVER_SHUTTING_DOWN';
  }
}

interface OpenResult {
  readonly accepted: OpenWorkspaceAccepted;
  readonly channel: ExplorerClientChannel;
  readonly explorer: PortFileExplorer;
}

interface OpenChannelResult {
  readonly accepted: OpenWorkspaceAccepted;
  readonly channel: ExplorerClientChannel;
}

function filterServiceFrames(
  raw: ExplorerClientChannel,
  logger: MilleRemoteLogger | undefined,
): ExplorerClientChannel {
  const listeners = new Set<Parameters<ExplorerClientChannel['onMessage']>[0]>();

  raw.onMessage((message) => {
    const candidate = message as unknown;
    if (typeof candidate === 'object' && candidate !== null) {
      const service = candidate as { service?: unknown; type?: unknown };
      if (service.service === REMOTE_SERVICE) {
        if (service.type === 'ping') {
          try {
            const ping = asService<RemotePing>(candidate);
            raw.send({
              service: REMOTE_SERVICE,
              version: 1,
              type: 'pong',
              nonce: ping.nonce,
              sentAtMs: ping.sentAtMs,
            } as unknown as never);
          } catch {
            /* closing */
          }
        }
        return;
      }
    }

    for (const listener of [...listeners]) {
      try {
        listener(message);
      } catch (error) {
        logger?.warn?.('explorer channel listener threw', { error });
      }
    }
  });

  return {
    get state() {
      return raw.state;
    },
    get bufferedBytes() {
      return raw.bufferedBytes;
    },
    send: (message) => raw.send(message),
    drain: () => raw.drain(),
    onMessage(listener) {
      listeners.add(listener);
      return { dispose: () => listeners.delete(listener) };
    },
    onClose: (listener) => raw.onClose(listener),
    close: (reason) => raw.close(reason),
    dispose() {
      raw.close('disposed');
    },
  };
}

/**
 * Dial, run the open handshake, then hand the same channel to mille.
 *
 * Service frames and explorer frames share one channel; the service listener
 * installed here filters its own and lets everything else through to the
 * mille client (§13.6 — heartbeats never reach the explorer).
 */
async function openOnce(
  mesh: MeshConnectLike,
  options: ConnectMilleChannelOptions,
  signal: AbortSignal | undefined,
): Promise<OpenChannelResult> {
  const port = options.port ?? DEFAULT_PORT;
  const timeoutMs = options.openTimeoutMs ?? DEFAULT_OPEN_TIMEOUT_MS;

  if (signal?.aborted === true) {
    throw new RemoteExplorerError('OFFLINE', 'aborted before connect');
  }

  const socket = mesh.net.connect({ peer: options.peer, port });
  await new Promise<void>((resolve, reject) => {
    const onConnect = (): void => {
      cleanup();
      resolve();
    };
    const onError = (err: Error): void => {
      cleanup();
      socket.destroy();
      reject(new RemoteExplorerError('TRANSPORT_ERROR', err.message, { cause: err }));
    };
    const onAbort = (): void => {
      cleanup();
      socket.destroy();
      reject(new RemoteExplorerError('OFFLINE', 'aborted before connect'));
    };
    const cleanup = (): void => {
      socket.off('connect', onConnect);
      socket.off('error', onError);
      signal?.removeEventListener('abort', onAbort);
    };
    socket.once('connect', onConnect);
    socket.once('error', onError);
    signal?.addEventListener('abort', onAbort, { once: true });
    if (signal?.aborted === true) onAbort();
  });

  const channel = createFramedStreamClientChannel(socket);

  const accepted = await new Promise<OpenWorkspaceAccepted>((resolve, reject) => {
    const timer = setTimeout(() => {
      finish();
      channel.close('open timeout');
      reject(new RemoteExplorerError('TIMEOUT', `no open reply within ${timeoutMs}ms`));
    }, timeoutMs);
    (timer as unknown as { unref?: () => void }).unref?.();

    const sub = channel.onMessage((raw) => {
      if (typeof raw !== 'object' || raw === null) return;
      const msg = raw as { service?: unknown; type?: unknown };
      if (msg.service !== REMOTE_SERVICE) return;

      if (msg.type === 'ping') {
        // Answer heartbeats even before acceptance — the server starts its
        // idle clock at accept, not at open.
        try {
          const p = asService<RemotePing>(raw);
          channel.send({
            service: REMOTE_SERVICE,
            version: 1,
            type: 'pong',
            nonce: p.nonce,
            sentAtMs: p.sentAtMs,
          } as unknown as never);
        } catch {
          /* closing */
        }
        return;
      }
      if (msg.type === 'accepted') {
        finish();
        resolve(asService<OpenWorkspaceAccepted>(raw));
        return;
      }
      if (msg.type === 'rejected') {
        const r = asService<OpenWorkspaceRejected>(raw);
        finish();
        channel.close('rejected');
        reject(new RemoteExplorerError(rejectCodeToError(r.code), r.message));
      }
    });

    const closeSub = channel.onClose((event) => {
      finish();
      reject(
        new RemoteExplorerError('TRANSPORT_ERROR', `channel closed during open: ${event.code}`),
      );
    });

    const onAbort = (): void => {
      finish();
      channel.close('aborted during open');
      reject(new RemoteExplorerError('OFFLINE', 'aborted during open'));
    };
    signal?.addEventListener('abort', onAbort, { once: true });
    if (signal?.aborted === true) onAbort();

    function finish(): void {
      clearTimeout(timer);
      sub.dispose();
      closeSub.dispose();
      signal?.removeEventListener('abort', onAbort);
    }

    channel.send({
      service: REMOTE_SERVICE,
      version: 1,
      type: 'open',
      requestId: randomUUID(),
      exportId: options.exportId,
      requestedAccess: options.access ?? 'read-only',
      client: {
        instanceId: randomUUID(),
        ...(options.clientName === undefined ? null : { name: options.clientName }),
        milleVersion: '0.3.0',
        milleTruffleVersion: '0.1.0',
      },
    } as unknown as never);
  });

  return { accepted, channel: filterServiceFrames(channel, options.logger) };
}

/**
 * Negotiate one remote export and return its renderer-safe explorer channel.
 * This is the bridge used by Electron main/utility processes: they can relay
 * the semantic messages to a MessagePort without exposing a mesh socket or
 * Node primitives to the renderer.
 */
export async function connectMilleChannel(
  mesh: MeshConnectLike,
  options: ConnectMilleChannelOptions,
): Promise<MilleChannelConnection> {
  const result = await openOnce(mesh, options, options.signal);
  return {
    accepted: result.accepted,
    channel: result.channel,
    close: (reason) => result.channel.close(reason),
  };
}

export async function connectMille(
  mesh: MeshConnectLike,
  options: ConnectMilleOptions,
): Promise<RemoteFileExplorer> {
  const logger = options.logger;
  const reconnect = resolveReconnect(options.reconnect);

  let state: RemoteConnectionState = 'connecting';
  let current: OpenResult | null = null;
  let workspaceInstanceId: string | undefined;
  let lastSnapshot: unknown;
  let closed = false;
  let attempt = 0;
  let terminalFailure: RemoteExplorerError | null = null;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;

  const connectionListeners = new Set<(e: RemoteConnectionEvent) => void>();
  const identityListeners = new Set<(e: RemoteIdentityResetEvent) => void>();

  const emitConnection = (e: RemoteConnectionEvent): void => {
    for (const l of [...connectionListeners]) {
      try {
        l(e);
      } catch {
        /* a bad listener must not derail reconnection */
      }
    }
  };

  const setState = (next: RemoteConnectionState, error?: RemoteExplorerError): void => {
    state = next;
    emitConnection({ state: next, attempt, workspaceInstanceId, error });
  };

  let readyResolve!: () => void;
  let readyReject!: (err: unknown) => void;
  let ready = new Promise<void>((res, rej) => {
    readyResolve = res;
    readyReject = rej;
  });
  // A later failure must not surface as an unhandled rejection on a promise
  // nobody is awaiting any more.
  void ready.catch(() => {});

  function adopt(result: OpenResult): void {
    const previous = workspaceInstanceId;
    current = result;
    workspaceInstanceId = result.accepted.workspaceInstanceId;
    attempt = 0;

    if (previous !== undefined && previous !== workspaceInstanceId) {
      // The engine restarted: every EntryId the caller holds now refers to
      // nothing, or worse, to something else. Say so loudly (§18.5).
      for (const l of [...identityListeners]) {
        try {
          l({ previousWorkspaceInstanceId: previous, workspaceInstanceId });
        } catch {
          /* ignore */
        }
      }
      log('identity_reset', { previous, current: workspaceInstanceId });
    }

    result.explorer.onConnection?.((ev) => {
      if (ev.state === 'closed') onSessionLost();
    });
    setState('online');
    readyResolve();
  }

  function log(event: string, fields?: Record<string, unknown>): void {
    logger?.info?.(event, fields);
  }

  function onSessionLost(): void {
    if (closed) return;
    // Retain the last snapshot before dropping the session — this is what
    // keeps the tree on screen while offline.
    try {
      lastSnapshot = current?.explorer.getSnapshot() ?? lastSnapshot;
    } catch {
      /* keep whatever we had */
    }
    current = null;
    if (reconnect === null) {
      setState('closed');
      return;
    }
    setState('stale');
    scheduleRetry();
  }

  function scheduleRetry(): void {
    if (closed || reconnect === null) return;
    const delay = backoffDelay(attempt, reconnect);
    attempt += 1;
    log('reconnect_attempt', { attempt, delay });
    retryTimer = setTimeout(() => {
      void attemptConnect();
    }, delay);
    (retryTimer as unknown as { unref?: () => void }).unref?.();
  }

  async function attemptConnect(): Promise<void> {
    if (closed) return;
    setState(attempt === 0 ? 'connecting' : 'reconnecting');
    try {
      const opened = await openOnce(mesh, options, options.signal);
      const explorer = await connectFileExplorerChannel(opened.channel);
      const result: OpenResult = { ...opened, explorer };
      if (closed) {
        result.channel.close('closed during connect');
        return;
      }
      adopt(result);
    } catch (err) {
      const error =
        err instanceof RemoteExplorerError
          ? err
          : new RemoteExplorerError('TRANSPORT_ERROR', String((err as Error)?.message ?? err));
      const decision = shouldRetry(error.code, {
        retryServerShutdown: options.retryServerShutdown === true,
      });
      log('connect_failed', { code: error.code, retry: decision.retry, reason: decision.reason });

      if (!decision.retry || reconnect === null || closed) {
        terminalFailure = error;
        setState('closed', error);
        readyReject(error);
        return;
      }
      setState('stale', error);
      scheduleRetry();
    }
  }

  options.signal?.addEventListener(
    'abort',
    () => {
      void facade.close();
    },
    { once: true },
  );

  const facade: RemoteFileExplorer = {
    get state() {
      return state;
    },
    exportId: options.exportId,
    get workspaceInstanceId() {
      return workspaceInstanceId;
    },
    get explorer() {
      if (current === null) {
        throw new RemoteExplorerError('OFFLINE', `not connected (state: ${state})`);
      }
      return current.explorer;
    },
    getSnapshot() {
      // Deliberately does not throw offline: the whole point of §18.3 is
      // that the last tree stays readable while the connection is gone.
      if (current !== null) {
        lastSnapshot = current.explorer.getSnapshot();
      }
      return lastSnapshot;
    },
    ready() {
      return ready;
    },
    async close() {
      if (closed) return;
      closed = true;
      if (retryTimer !== undefined) clearTimeout(retryTimer);
      const session = current;
      current = null;
      if (session !== null) {
        try {
          lastSnapshot = session.explorer.getSnapshot();
        } catch {
          /* ignore */
        }
        await session.explorer.dispose();
      }
      setState('closed');
      connectionListeners.clear();
      identityListeners.clear();
    },
    on(event: 'connection' | 'identityReset', listener: (e: never) => void): Disposable {
      if (event === 'connection') {
        const l = listener as unknown as (e: RemoteConnectionEvent) => void;
        connectionListeners.add(l);
        return { dispose: () => connectionListeners.delete(l) };
      }
      const l = listener as unknown as (e: RemoteIdentityResetEvent) => void;
      identityListeners.add(l);
      return { dispose: () => identityListeners.delete(l) };
    },
  };

  await attemptConnect();
  // Surface a terminal first-attempt failure to the caller of connectMille
  // rather than handing back a facade that will never come up.
  if (terminalFailure !== null) throw terminalFailure;
  return facade;
}
