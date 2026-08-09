// The Truffle-facing listener (SPEC §13, §14.1, §15.2).
//
// Shape of one accepted connection:
//
//   socket accepted ──► verified peerId available synchronously
//        │              (SEC-001: gate before any host exists)
//        ├─ open request within openTimeoutMs, else close
//        ├─ resolve export, authorize peer
//        ├─ acquire or reuse a host for that export
//        ├─ send `accepted`
//        └─ hand the remaining stream to FileExplorerHost.attachChannel
//
// The ordering is the security property: no host is created, no filesystem
// is touched, and no engine work happens until authorization has passed.
//
// Truffle is reached only through `mesh.net`. Nothing here imports the
// native addon, and the mesh node is borrowed — disposing this server never
// stops the caller's node (NFR-008).

import { randomUUID } from 'node:crypto';
import type { Duplex } from 'node:stream';

import type { FileExplorerHost, ExplorerHostChannel } from '@vibecook/mille';
import { createFileExplorerHost } from '@vibecook/mille';
import {
  createFramedStreamHostChannel,
  type FramedStreamChannelOptions,
} from '@vibecook/mille/node';

import { authorizePeer, sessionPolicyFor } from './authorize.js';
import { findOverlappingRoots, resolveExports } from './exports.js';
import {
  isRemoteServiceMessage,
  parseOpenRequest,
  pong,
  rejection,
  REMOTE_SERVICE,
  type OpenWorkspaceAccepted,
  type RemotePing,
} from './handshake.js';
import type {
  MilleRemoteLogger,
  MilleRemoteServer,
  RemoteSessionInfo,
  ResolvedExport,
  ServeMilleOptions,
} from './types.js';

/** Minimal structural view of what this server needs from a MeshNode. */
export interface MeshNetLike {
  createServer(listener: (socket: TruffleSocketLike) => void): MeshServerLike;
}
export interface MeshServerLike {
  listen(port: number, onListening?: () => void): unknown;
  close(callback?: () => void): unknown;
  once(event: string, listener: (...args: unknown[]) => void): unknown;
  readonly port?: number | undefined;
}
export interface TruffleSocketLike extends Duplex {
  readonly remotePeerId?: string | undefined;
  readonly remotePeerName?: string | undefined;
  readonly remoteAddress?: string | undefined;
}
export interface MeshLike {
  readonly net: MeshNetLike;
}

const DEFAULTS = {
  port: 9451,
  hostIdleTimeoutMs: 5 * 60_000,
  maxSessionsPerPeer: 4,
  heartbeatMs: 20_000,
  idleTimeoutMs: 60_000,
  openTimeoutMs: 10_000,
} as const;

interface HostCacheEntry {
  readonly exportId: string;
  readonly fingerprint: string;
  readonly workspaceInstanceId: string;
  readonly host: FileExplorerHost;
  sessionCount: number;
  idleTimer?: ReturnType<typeof setTimeout> | undefined;
}

interface LiveSession {
  readonly info: RemoteSessionInfo;
  readonly socket: TruffleSocketLike;
  readonly entry: HostCacheEntry;
  readonly detach: () => void;
  disposed: boolean;
}

function log(
  logger: MilleRemoteLogger | undefined,
  level: 'info' | 'warn' | 'debug',
  event: string,
  fields?: Record<string, unknown>,
): void {
  logger?.[level]?.(event, fields);
}

export async function serveMille(
  mesh: MeshLike,
  options: ServeMilleOptions,
): Promise<MilleRemoteServer> {
  const logger = options.logger;
  const exportsById = resolveExports(options.exports);
  for (const warning of findOverlappingRoots(exportsById)) {
    log(logger, 'warn', 'export_roots_overlap', { detail: warning });
  }

  const port = options.port ?? DEFAULTS.port;
  const hostIdleTimeoutMs = options.hostIdleTimeoutMs ?? DEFAULTS.hostIdleTimeoutMs;
  const maxSessionsPerPeer = options.maxSessionsPerPeer ?? DEFAULTS.maxSessionsPerPeer;
  const heartbeatMs = options.heartbeatMs ?? DEFAULTS.heartbeatMs;
  const idleTimeoutMs = options.idleTimeoutMs ?? DEFAULTS.idleTimeoutMs;
  const openTimeoutMs = options.openTimeoutMs ?? DEFAULTS.openTimeoutMs;

  const hosts = new Map<string, HostCacheEntry>();
  const sessions = new Map<string, LiveSession>();
  let shuttingDown = false;

  // ─── host cache (SPEC §15.2) ──────────────────────────────────────────

  async function acquireHost(ex: ResolvedExport): Promise<HostCacheEntry> {
    const existing = hosts.get(ex.fingerprint);
    if (existing !== undefined) {
      // A new session during the idle lease cancels disposal, so the peer
      // keeps the same EntryIds and workspace instance it had before.
      if (existing.idleTimer !== undefined) {
        clearTimeout(existing.idleTimer);
        existing.idleTimer = undefined;
      }
      existing.sessionCount += 1;
      return existing;
    }
    const host = await createFileExplorerHost({
      // `initialWalk` defaults to `'full'`, which is a *no-op* in the host —
      // it means "the consumer will call populateFromRoots itself". An
      // in-process embedder does; a remote peer cannot, so a served export
      // would sit empty forever. Seed roots lazily instead and let expansion
      // pull the rest, which is also the right shape for a large remote
      // workspace. An export may still override it explicitly.
      initialWalk: 'roots-only',
      ...ex.explorer,
      roots: [...ex.roots],
      followSymlinks: false,
    });
    const entry: HostCacheEntry = {
      exportId: ex.id,
      fingerprint: ex.fingerprint,
      workspaceInstanceId: randomUUID(),
      host,
      sessionCount: 1,
    };
    hosts.set(ex.fingerprint, entry);
    log(logger, 'info', 'host_created', {
      exportId: ex.id,
      workspaceInstanceId: entry.workspaceInstanceId,
    });
    return entry;
  }

  function releaseHost(entry: HostCacheEntry): void {
    entry.sessionCount -= 1;
    if (entry.sessionCount > 0 || shuttingDown) return;
    // Keep the engine warm briefly: a reconnect inside the lease keeps the
    // same workspace instance, so the client's cached EntryIds stay valid.
    entry.idleTimer = setTimeout(() => {
      if (entry.sessionCount > 0) return;
      hosts.delete(entry.fingerprint);
      void entry.host.dispose();
      log(logger, 'info', 'host_disposed', {
        exportId: entry.exportId,
        workspaceInstanceId: entry.workspaceInstanceId,
      });
    }, hostIdleTimeoutMs);
    entry.idleTimer.unref?.();
  }

  // ─── one accepted socket ──────────────────────────────────────────────

  function handleConnection(socket: TruffleSocketLike): void {
    const connectionId = randomUUID().slice(0, 8);
    // Identity is set synchronously on the accept path, so it is readable
    // here with no await in between (net.ts:107-110).
    const peerId = socket.remotePeerId ?? null;
    const peerName = socket.remotePeerName;

    log(logger, 'info', 'open_requested', {
      connectionId,
      peerId,
      remoteAddress: socket.remoteAddress,
    });

    if (shuttingDown) {
      refuse(socket, rejection('SERVER_SHUTTING_DOWN', 'server is shutting down'), connectionId);
      return;
    }

    // The open request must arrive promptly. Until it does this socket owns
    // nothing but a timer — no host, no engine, no filesystem handle.
    let settled = false;
    const openTimer = setTimeout(() => {
      if (settled) return;
      settled = true;
      log(logger, 'warn', 'open_timeout', { connectionId, peerId });
      socket.destroy();
    }, openTimeoutMs);
    openTimer.unref?.();

    // The open handshake rides the same framed codec as everything else, so
    // one channel serves both phases. Service frames are filtered out before
    // the explorer host ever sees them (§13.6).
    const channelOptions: FramedStreamChannelOptions = {};
    const channel = createFramedStreamHostChannel(socket, channelOptions);

    let attached: LiveSession | null = null;
    let lastInboundMs = Date.now();

    const heartbeat = setInterval(() => {
      const idleFor = Date.now() - lastInboundMs;
      if (idleFor >= idleTimeoutMs) {
        log(logger, 'warn', 'idle_timeout', { connectionId, idleFor });
        channel.close('idle timeout');
        return;
      }
      if (idleFor >= heartbeatMs) {
        try {
          channel.send({
            service: REMOTE_SERVICE,
            version: 1,
            type: 'ping',
            nonce: randomUUID(),
            sentAtMs: Date.now(),
          } as unknown as never);
        } catch {
          /* channel already closing */
        }
      }
    }, Math.max(1000, Math.floor(heartbeatMs / 2)));
    heartbeat.unref?.();

    const messageSub = channel.onMessage((raw) => {
      lastInboundMs = Date.now();
      if (!isRemoteServiceMessage(raw)) return; // explorer frame; host handles it
      const msg = raw as { type?: string };
      if (msg.type === 'ping') {
        try {
          channel.send(pong(raw as RemotePing) as unknown as never);
        } catch {
          /* closing */
        }
        return;
      }
      if (msg.type === 'pong') return;
      if (msg.type !== 'open') return;
      if (settled) return;
      settled = true;
      clearTimeout(openTimer);
      void openWorkspace(raw);
    });

    channel.onClose((event) => {
      clearTimeout(openTimer);
      clearInterval(heartbeat);
      messageSub.dispose();
      if (attached !== null && !attached.disposed) {
        attached.disposed = true;
        sessions.delete(attached.info.sessionId);
        attached.detach();
        releaseHost(attached.entry);
        log(logger, 'info', 'channel_closed', {
          connectionId,
          sessionId: attached.info.sessionId,
          code: event.code,
        });
      } else {
        log(logger, 'debug', 'channel_closed', { connectionId, code: event.code });
      }
    });

    async function openWorkspace(raw: unknown): Promise<void> {
      const parsed = parseOpenRequest(raw);
      if ('code' in parsed) {
        log(logger, 'warn', 'open_rejected', { connectionId, peerId, coarse: parsed.code });
        refuse(socket, rejection(parsed.code, parsed.message), connectionId, channel);
        return;
      }

      const ex = exportsById.get(parsed.exportId);
      // SEC-006 — an unknown export and a forbidden one must look identical
      // from outside, or the service enumerates its own exports.
      const denied = (internalReason: string): void => {
        log(logger, 'warn', 'open_rejected', {
          connectionId,
          peerId,
          exportId: parsed.exportId,
          reason: internalReason,
        });
        const message = options.diagnosticDisclosure === true ? internalReason : 'access denied';
        refuse(socket, rejection('ACCESS_DENIED', message, parsed.requestId), connectionId, channel);
      };

      if (ex === undefined) {
        denied(`no such export ${parsed.exportId}`);
        return;
      }

      const verdict = await authorizePeer({
        export: ex,
        peerId,
        peerName,
        requestedAccess: parsed.requestedAccess,
        authorize: options.authorize,
      });
      if (!verdict.ok) {
        denied(verdict.reason);
        return;
      }

      const forExport = [...sessions.values()].filter((s) => s.info.exportId === ex.id);
      if (forExport.length >= ex.maxSessions) {
        refuse(
          socket,
          rejection('LIMIT_EXCEEDED', 'export session limit reached', parsed.requestId),
          connectionId,
          channel,
        );
        return;
      }
      if (forExport.filter((s) => s.info.peerId === peerId).length >= maxSessionsPerPeer) {
        refuse(
          socket,
          rejection('LIMIT_EXCEEDED', 'peer session limit reached', parsed.requestId),
          connectionId,
          channel,
        );
        return;
      }

      // Authorized — only now does an engine come into existence.
      const entry = await acquireHost(ex);
      const sessionId = randomUUID();
      const detach = entry.host.attachChannel(channel as ExplorerHostChannel, {
        kind: 'remote',
        clientId: parsed.client.instanceId,
        peerId: peerId ?? undefined,
        peerName,
        exportId: ex.id,
        policy: sessionPolicyFor(verdict.access, ex.maxFileBytes),
      });

      const info: RemoteSessionInfo = {
        sessionId,
        exportId: ex.id,
        peerId: peerId as string,
        peerName,
        access: verdict.access,
        workspaceInstanceId: entry.workspaceInstanceId,
        openedAtMs: Date.now(),
      };
      attached = { info, socket, entry, detach: () => detach.dispose(), disposed: false };
      sessions.set(sessionId, attached);

      const accepted: OpenWorkspaceAccepted = {
        service: REMOTE_SERVICE,
        version: 1,
        type: 'accepted',
        requestId: parsed.requestId,
        sessionId,
        workspaceInstanceId: entry.workspaceInstanceId,
        export: {
          id: ex.id,
          label: ex.label,
          access: verdict.access,
          rootCount: ex.roots.length,
        },
        limits: {
          maxMetadataBytes: 4 * 1024 * 1024,
          maxAttachments: 32,
          maxFrameBytes: 32 * 1024 * 1024,
          maxFileBytes: ex.maxFileBytes,
          heartbeatMs,
          idleTimeoutMs,
        },
      };
      channel.send(accepted as unknown as never);
      log(logger, 'info', 'open_accepted', {
        connectionId,
        sessionId,
        exportId: ex.id,
        workspaceInstanceId: entry.workspaceInstanceId,
        access: verdict.access,
      });
    }
  }

  /** Send one rejection and close. Never leaves a half-open socket behind. */
  function refuse(
    socket: TruffleSocketLike,
    message: ReturnType<typeof rejection>,
    _connectionId: string,
    channel?: ExplorerHostChannel,
  ): void {
    try {
      if (channel !== undefined) {
        channel.send(message as unknown as never);
        channel.close('rejected');
        return;
      }
      socket.destroy();
    } catch {
      socket.destroy();
    }
  }

  // ─── listen ───────────────────────────────────────────────────────────

  const netServer = mesh.net.createServer(handleConnection);
  const boundPort = await new Promise<number>((resolve, reject) => {
    netServer.once('listening', () => resolve(netServer.port ?? port));
    netServer.once('error', (err) => reject(err as Error));
    netServer.listen(port);
  });
  log(logger, 'info', 'server_listening', { port: boundPort, exports: exportsById.size });

  async function close(): Promise<void> {
    if (shuttingDown) return;
    shuttingDown = true;
    await new Promise<void>((resolve) => netServer.close(() => resolve()));
    for (const session of [...sessions.values()]) {
      if (session.disposed) continue;
      session.disposed = true;
      session.detach();
      session.socket.destroy();
    }
    sessions.clear();
    for (const entry of hosts.values()) {
      if (entry.idleTimer !== undefined) clearTimeout(entry.idleTimer);
      await entry.host.dispose();
    }
    hosts.clear();
    log(logger, 'info', 'server_closed', { port: boundPort });
  }

  return {
    port: boundPort,
    listSessions: () => [...sessions.values()].map((s) => s.info),
    get hostCount() {
      return hosts.size;
    },
    close,
    [Symbol.asyncDispose]: close,
  };
}
