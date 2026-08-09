import { basename } from 'node:path';

import type { ExplorerClientChannel } from '@vibecook/mille';
import {
  connectMilleChannel,
  isRemoteExplorerError,
  serveMille,
  shouldRetry,
  type MilleChannelConnection,
  type MilleRemoteServer,
} from '@vibecook/mille-truffle';
import { createMeshNode, type MeshNode, type Peer } from '@vibecook/truffle';
import type { MessagePortMain } from 'electron';

import type {
  ConnectPeerRequest,
  MeshDemoState,
  PeerView,
  ShareAccess,
  StartShareRequest,
} from '../shared/types';
import { relayExplorerChannel, type RelayHandle } from './relay';

const PORT = 9451;
const EXPORT_ID = 'shared';
const MAX_FILE_BYTES = 16 * 1024 * 1024;
const deviceName = process.env.MILLE_MESH_DEVICE_NAME ?? 'mille-device';
const configuredStateDir = process.env.MILLE_MESH_STATE_DIR;
const parent = process.parentPort;

if (parent === undefined) throw new Error('mesh-host must run as an Electron UtilityProcess');
if (configuredStateDir === undefined) throw new Error('MILLE_MESH_STATE_DIR is required');
const stateDir: string = configuredStateDir;

const EMPTY_REMOTE: MeshDemoState['remote'] = {
  status: 'idle',
  peerRef: null,
  peerName: null,
  access: null,
  workspaceInstanceId: null,
  maxFileBytes: null,
  error: null,
};

let state: MeshDemoState = {
  mesh: 'starting',
  deviceName,
  dnsName: null,
  authUrl: null,
  error: null,
  peers: [],
  share: null,
  remote: EMPTY_REMOTE,
};

let mesh: MeshNode | null = null;
let server: MilleRemoteServer | null = null;
let connection: MilleChannelConnection | null = null;
let relay: RelayHandle | null = null;
let closeSubscription: { dispose(): void } | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempt = 0;
let intentGeneration = 0;
let channelGeneration = 0;
let shuttingDown = false;

interface ConnectionIntent {
  readonly tailscaleId: string;
  readonly peerName: string;
  readonly access: ShareAccess;
}

let intent: ConnectionIntent | null = null;

function publish(next: MeshDemoState): void {
  state = next;
  parent.postMessage({ type: 'state', state });
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function peerView(peer: Peer): PeerView {
  return {
    ref: peer.ref,
    tailscaleId: peer.tailscaleId,
    displayName: peer.displayName,
    deviceName: peer.deviceName,
    hostname: peer.hostname,
    ip: peer.ip,
    os: peer.os ?? null,
    online: peer.online,
  };
}

async function refreshPeers(): Promise<Peer[]> {
  const current = mesh;
  if (current === null) return [];
  const peers = await current.getPeers();
  publish({ ...state, peers: peers.map(peerView) });
  return peers;
}

async function startSharing(payload: StartShareRequest & { root: string }): Promise<void> {
  const current = mesh;
  if (current === null || state.mesh !== 'online') throw new Error('mesh is not online');
  if (payload.access !== 'read-only' && payload.access !== 'read-write') {
    throw new Error('invalid share access');
  }
  const knownIds = new Set((await current.getPeers()).map((peer) => peer.tailscaleId));
  const allowedPeerIds = [...new Set(payload.allowedPeerIds)].filter((id) => knownIds.has(id));
  if (allowedPeerIds.length === 0) {
    throw new Error('select at least one known peer before sharing');
  }

  const previous = server;
  server = null;
  if (previous !== null) await previous.close();

  try {
    const next = await serveMille(current, {
      port: PORT,
      exports: {
        [EXPORT_ID]: {
          label: basename(payload.root),
          roots: [payload.root],
          access: payload.access,
          followSymlinks: false,
          allowedPeerIds,
          maxFileBytes: MAX_FILE_BYTES,
          explorer: {
            respectIgnore: true,
            initialWalk: 'roots-only',
            watchDebounceMs: 75,
          },
        },
      },
    });
    server = next;
    publish({
      ...state,
      share: {
        root: payload.root,
        label: basename(payload.root),
        access: payload.access,
        allowedPeerIds,
        maxFileBytes: MAX_FILE_BYTES,
      },
    });
  } catch (error) {
    publish({ ...state, share: null });
    throw error;
  }
}

async function stopSharing(): Promise<void> {
  const current = server;
  server = null;
  publish({ ...state, share: null });
  if (current !== null) await current.close();
}

function clearReconnect(): void {
  if (reconnectTimer !== null) clearTimeout(reconnectTimer);
  reconnectTimer = null;
}

function releaseConnection(reason: string): void {
  closeSubscription?.dispose();
  closeSubscription = null;
  relay?.dispose();
  relay = null;
  const current = connection;
  connection = null;
  current?.close(reason);
}

function scheduleReconnect(generation: number): void {
  if (shuttingDown || intent === null || generation !== intentGeneration) return;
  clearReconnect();
  const delay = Math.min(10_000, 500 * 2 ** Math.min(reconnectAttempt, 5));
  reconnectAttempt += 1;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    void openIntent(generation, true);
  }, delay);
}

function connectionLost(generation: number): void {
  if (generation !== channelGeneration || connection === null) return;
  closeSubscription?.dispose();
  closeSubscription = null;
  relay?.dispose();
  relay = null;
  connection = null;
  if (intent === null || shuttingDown) return;
  publish({
    ...state,
    remote: {
      ...state.remote,
      status: 'reconnecting',
      workspaceInstanceId: null,
      maxFileBytes: null,
      error: 'Connection lost. Retrying…',
    },
  });
  scheduleReconnect(intentGeneration);
}

async function openIntent(generation: number, reconnecting: boolean): Promise<void> {
  const currentMesh = mesh;
  const wanted = intent;
  if (currentMesh === null || wanted === null || generation !== intentGeneration) return;

  publish({
    ...state,
    remote: {
      ...state.remote,
      status: reconnecting ? 'reconnecting' : 'connecting',
      error: null,
    },
  });

  try {
    const peers = await refreshPeers();
    const peer = peers.find((candidate) => candidate.tailscaleId === wanted.tailscaleId);
    if (peer === undefined) throw new Error(`${wanted.peerName} is not currently discoverable`);
    const opened = await connectMilleChannel(currentMesh, {
      peer,
      port: PORT,
      exportId: EXPORT_ID,
      access: wanted.access,
      clientName: deviceName,
    });
    if (generation !== intentGeneration || shuttingDown) {
      opened.close('connection superseded');
      return;
    }

    releaseConnection('replaced');
    connection = opened;
    reconnectAttempt = 0;
    const liveGeneration = ++channelGeneration;
    closeSubscription = opened.channel.onClose(() => connectionLost(liveGeneration));
    const view = peerView(peer);
    publish({
      ...state,
      remote: {
        status: 'online',
        peerRef: view.ref,
        peerName: view.displayName,
        access: opened.accepted.export.access,
        workspaceInstanceId: opened.accepted.workspaceInstanceId,
        maxFileBytes: opened.accepted.limits.maxFileBytes,
        error: null,
      },
    });
    parent.postMessage({
      type: 'explorer-ready',
      generation: liveGeneration,
      workspaceInstanceId: opened.accepted.workspaceInstanceId,
      peerName: view.displayName,
    });
  } catch (error) {
    if (generation !== intentGeneration || shuttingDown) return;
    const message = errorMessage(error);
    publish({
      ...state,
      remote: {
        ...state.remote,
        status: reconnecting ? 'reconnecting' : 'error',
        error: message,
      },
    });
    const retry = !isRemoteExplorerError(error) || shouldRetry(error.code).retry;
    if (reconnecting && retry) scheduleReconnect(generation);
    else if (reconnecting) intent = null;
    else throw error;
  }
}

async function connectPeer(request: ConnectPeerRequest): Promise<void> {
  const current = mesh;
  if (current === null) throw new Error('mesh is not online');
  if (request.access !== 'read-only' && request.access !== 'read-write') {
    throw new Error('invalid requested access');
  }
  const peer = (await current.getPeers()).find((candidate) => candidate.ref === request.peerRef);
  if (peer === undefined) throw new Error('peer is no longer available');

  clearReconnect();
  intentGeneration += 1;
  releaseConnection('connecting to another peer');
  intent = {
    tailscaleId: peer.tailscaleId,
    peerName: peer.displayName,
    access: request.access,
  };
  publish({
    ...state,
    remote: {
      status: 'connecting',
      peerRef: peer.ref,
      peerName: peer.displayName,
      access: request.access,
      workspaceInstanceId: null,
      maxFileBytes: null,
      error: null,
    },
  });
  await openIntent(intentGeneration, false);
}

function disconnectPeer(): void {
  intentGeneration += 1;
  intent = null;
  reconnectAttempt = 0;
  clearReconnect();
  releaseConnection('user disconnected');
  publish({ ...state, remote: EMPTY_REMOTE });
}

function attachBrowserPort(generation: number, port: MessagePortMain): void {
  const current = connection;
  if (current === null || generation !== channelGeneration) {
    port.close();
    return;
  }
  relay?.dispose();
  relay = relayExplorerChannel(current.channel as ExplorerClientChannel, port, () => {
    current.close('renderer port closed');
  });
}

async function shutdown(): Promise<void> {
  if (shuttingDown) return;
  shuttingDown = true;
  disconnectPeer();
  await stopSharing();
  const current = mesh;
  mesh = null;
  if (current !== null) await current.stop();
}

parent.on('message', (event) => {
  const message = event.data as {
    type?: string;
    requestId?: number;
    payload?: unknown;
    generation?: number;
  };
  if (message.type === 'attach-browser-port') {
    const port = event.ports[0];
    if (port !== undefined && typeof message.generation === 'number') {
      attachBrowserPort(message.generation, port);
    }
    return;
  }
  if (message.type === 'shutdown') {
    void shutdown().finally(() => process.exit(0));
    return;
  }
  if (typeof message.requestId !== 'number') return;

  const run = async (): Promise<unknown> => {
    switch (message.type) {
      case 'start-sharing':
        return startSharing(message.payload as StartShareRequest & { root: string });
      case 'stop-sharing':
        return stopSharing();
      case 'connect-peer':
        return connectPeer(message.payload as ConnectPeerRequest);
      case 'disconnect-peer':
        return disconnectPeer();
      default:
        throw new Error(`unknown mesh request: ${message.type ?? '<missing>'}`);
    }
  };
  void run().then(
    (result) =>
      parent.postMessage({ type: 'response', requestId: message.requestId, ok: true, result }),
    (error) =>
      parent.postMessage({
        type: 'response',
        requestId: message.requestId,
        ok: false,
        error: errorMessage(error),
      }),
  );
});

async function bootstrap(): Promise<void> {
  try {
    const created = await createMeshNode({
      appId: 'mille-demo',
      deviceName,
      stateDir,
      autoAuth: false,
      onAuthRequired: (url) => {
        publish({ ...state, mesh: 'auth-required', authUrl: url, error: null });
      },
      onPeerChange: () => {
        void refreshPeers().catch((error) => {
          console.warn('[mesh-host] peer refresh failed:', errorMessage(error));
        });
      },
    });
    mesh = created;
    publish({
      ...state,
      mesh: 'online',
      dnsName: created.dnsName,
      authUrl: null,
      error: null,
    });
    await refreshPeers().catch((error) => {
      console.warn('[mesh-host] initial peer refresh failed:', errorMessage(error));
    });
  } catch (error) {
    publish({
      ...state,
      mesh: 'error',
      error: errorMessage(error),
      authUrl: null,
    });
  }
}

void bootstrap();
