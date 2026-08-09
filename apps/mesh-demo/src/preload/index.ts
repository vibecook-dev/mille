import { contextBridge, ipcRenderer } from 'electron';

import type {
  ConnectPeerRequest,
  MeshDemoApi,
  MeshDemoState,
  StartShareRequest,
} from '../shared/types';

ipcRenderer.on(
  'mesh-explorer-port',
  (event, metadata: { workspaceInstanceId: string; peerName: string }) => {
    window.postMessage(
      { type: 'mesh-explorer-port', ...metadata },
      '*',
      event.ports as unknown as Transferable[],
    );
  },
);

const api: MeshDemoApi = {
  getState: () => ipcRenderer.invoke('mesh-get-state'),
  openAuth: () => ipcRenderer.invoke('mesh-open-auth'),
  startSharing: (request: StartShareRequest) => ipcRenderer.invoke('mesh-start-sharing', request),
  stopSharing: () => ipcRenderer.invoke('mesh-stop-sharing'),
  connectPeer: (request: ConnectPeerRequest) => ipcRenderer.invoke('mesh-connect-peer', request),
  disconnectPeer: () => ipcRenderer.invoke('mesh-disconnect-peer'),
  onState(listener: (state: MeshDemoState) => void) {
    const handler = (_event: Electron.IpcRendererEvent, next: MeshDemoState): void => {
      listener(next);
    };
    ipcRenderer.on('mesh-state', handler);
    return () => ipcRenderer.off('mesh-state', handler);
  },
};

contextBridge.exposeInMainWorld('milleMesh', api);
