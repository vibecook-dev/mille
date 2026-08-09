import { hostname } from 'node:os';
import { join } from 'node:path';

import {
  app,
  BrowserWindow,
  dialog,
  ipcMain,
  MessageChannelMain,
  shell,
  utilityProcess,
  type UtilityProcess,
} from 'electron';

import type { ConnectPeerRequest, MeshDemoState, StartShareRequest } from '../shared/types';

const EMPTY_REMOTE: MeshDemoState['remote'] = {
  status: 'idle',
  peerRef: null,
  peerName: null,
  access: null,
  workspaceInstanceId: null,
  maxFileBytes: null,
  error: null,
};

let window: BrowserWindow | null = null;
let rendererReady = false;
let meshProcess: UtilityProcess | null = null;
let state: MeshDemoState = {
  mesh: 'starting',
  deviceName: hostname(),
  dnsName: null,
  authUrl: null,
  error: null,
  peers: [],
  share: null,
  remote: EMPTY_REMOTE,
};

let requestSequence = 0;
const pending = new Map<
  number,
  {
    resolve(value: unknown): void;
    reject(error: Error): void;
    timer: ReturnType<typeof setTimeout>;
  }
>();

interface ExplorerReady {
  readonly generation: number;
  readonly workspaceInstanceId: string;
  readonly peerName: string;
}

let pendingExplorer: ExplorerReady | null = null;

interface UtilityResponse {
  readonly type: 'response';
  readonly requestId: number;
  readonly ok: boolean;
  readonly result?: unknown;
  readonly error?: string;
}

function publishState(next: MeshDemoState): void {
  state = next;
  if (next.remote.status === 'idle') pendingExplorer = null;
  if (window !== null && !window.isDestroyed()) {
    window.webContents.send('mesh-state', state);
  }
}

function deliverExplorerPort(): void {
  const ready = pendingExplorer;
  const proc = meshProcess;
  const win = window;
  if (ready === null || proc === null || win === null || win.isDestroyed() || !rendererReady) {
    return;
  }
  pendingExplorer = null;
  const { port1, port2 } = new MessageChannelMain();
  proc.postMessage({ type: 'attach-browser-port', generation: ready.generation }, [port1]);
  win.webContents.postMessage(
    'mesh-explorer-port',
    {
      workspaceInstanceId: ready.workspaceInstanceId,
      peerName: ready.peerName,
    },
    [port2],
  );
}

function failPending(reason: string): void {
  for (const request of pending.values()) {
    clearTimeout(request.timer);
    request.reject(new Error(reason));
  }
  pending.clear();
}

function callUtility<T = void>(type: string, payload?: unknown): Promise<T> {
  const proc = meshProcess;
  if (proc === null) return Promise.reject(new Error('mesh process is not running'));
  const requestId = ++requestSequence;
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(requestId);
      reject(new Error(`${type} timed out`));
    }, 30_000);
    pending.set(requestId, {
      resolve: (value) => resolve(value as T),
      reject,
      timer,
    });
    proc.postMessage({ type, requestId, payload });
  });
}

function handleUtilityMessage(message: unknown): void {
  if (typeof message !== 'object' || message === null) return;
  const item = message as { type?: unknown };

  if (item.type === 'state') {
    const next = (message as { state?: unknown }).state;
    if (typeof next === 'object' && next !== null) publishState(next as MeshDemoState);
    return;
  }

  if (item.type === 'response') {
    const response = message as UtilityResponse;
    const request = pending.get(response.requestId);
    if (request === undefined) return;
    pending.delete(response.requestId);
    clearTimeout(request.timer);
    if (response.ok) request.resolve(response.result);
    else request.reject(new Error(response.error ?? 'mesh operation failed'));
    return;
  }

  if (item.type === 'explorer-ready') {
    pendingExplorer = message as ExplorerReady;
    deliverExplorerPort();
  }
}

function startMeshProcess(): void {
  const proc = utilityProcess.fork(join(__dirname, 'mesh-host.mjs'), [], {
    serviceName: 'mille-mesh-host',
    stdio: 'pipe',
    env: {
      ...process.env,
      MILLE_MESH_STATE_DIR: join(app.getPath('userData'), 'truffle'),
      MILLE_MESH_DEVICE_NAME: process.env.MILLE_DEMO_DEVICE_NAME ?? hostname(),
    },
  });
  meshProcess = proc;
  proc.stdout?.on('data', (chunk) => process.stdout.write(`[mesh-host] ${chunk}`));
  proc.stderr?.on('data', (chunk) => process.stderr.write(`[mesh-host] ${chunk}`));
  proc.on('message', handleUtilityMessage);
  proc.on('exit', (code) => {
    if (proc !== meshProcess) return;
    meshProcess = null;
    failPending(`mesh process exited (${code ?? 'unknown'})`);
    publishState({
      ...state,
      mesh: 'error',
      error: `Mesh helper exited (${code ?? 'unknown'}). Restart the app to retry.`,
      remote: { ...state.remote, status: 'error', error: 'Mesh helper exited.' },
    });
  });
}

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1180,
    height: 760,
    minWidth: 860,
    minHeight: 560,
    title: 'Mille Mesh',
    backgroundColor: '#0d1117',
    webPreferences: {
      preload: join(__dirname, '../preload/index.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  window = win;
  rendererReady = false;
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('did-start-loading', () => {
    rendererReady = false;
  });
  win.webContents.on('did-finish-load', () => {
    rendererReady = true;
    deliverExplorerPort();
  });
  win.on('closed', () => {
    if (window === win) {
      window = null;
      rendererReady = false;
      pendingExplorer = null;
      void callUtility('disconnect-peer').catch(() => {});
    }
  });

  if (process.env.ELECTRON_RENDERER_URL !== undefined) {
    void win.loadURL(process.env.ELECTRON_RENDERER_URL);
  } else {
    void win.loadFile(join(__dirname, '../renderer/index.html'));
  }
}

ipcMain.handle('mesh-get-state', () => state);

ipcMain.handle('mesh-open-auth', async () => {
  if (state.authUrl === null) throw new Error('No authentication URL is available');
  const url = new URL(state.authUrl);
  if (url.protocol !== 'https:' || !url.hostname.endsWith('.tailscale.com')) {
    throw new Error('Refusing to open an unexpected authentication URL');
  }
  await shell.openExternal(url.toString());
});

ipcMain.handle('mesh-start-sharing', async (_event, request: StartShareRequest) => {
  const picked = await dialog.showOpenDialog({
    title: 'Choose a folder to share over Mille Mesh',
    properties: ['openDirectory'],
  });
  const root = picked.filePaths[0];
  if (picked.canceled || root === undefined) return null;
  await callUtility('start-sharing', { ...request, root });
  return root;
});

ipcMain.handle('mesh-stop-sharing', () => callUtility('stop-sharing'));
ipcMain.handle('mesh-connect-peer', (_event, request: ConnectPeerRequest) =>
  callUtility('connect-peer', request),
);
ipcMain.handle('mesh-disconnect-peer', () => callUtility('disconnect-peer'));

app.whenReady().then(() => {
  createWindow();
  startMeshProcess();
});

app.on('activate', () => {
  if (BrowserWindow.getAllWindows().length === 0) createWindow();
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});

app.on('before-quit', () => {
  const proc = meshProcess;
  meshProcess = null;
  if (proc !== null) {
    try {
      proc.postMessage({ type: 'shutdown' });
    } catch {
      proc.kill();
    }
  }
  failPending('application is quitting');
});
