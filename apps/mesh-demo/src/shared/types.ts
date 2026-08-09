export type ShareAccess = 'read-only' | 'read-write';

export interface PeerView {
  readonly ref: string;
  readonly tailscaleId: string;
  readonly displayName: string;
  readonly deviceName: string | null;
  readonly hostname: string;
  readonly ip: string;
  readonly os: string | null;
  readonly online: boolean;
}

export interface ShareView {
  readonly root: string;
  readonly label: string;
  readonly access: ShareAccess;
  readonly allowedPeerIds: readonly string[];
  readonly maxFileBytes: number;
}

export interface RemoteView {
  readonly status: 'idle' | 'connecting' | 'online' | 'reconnecting' | 'error';
  readonly peerRef: string | null;
  readonly peerName: string | null;
  readonly access: ShareAccess | null;
  readonly workspaceInstanceId: string | null;
  readonly maxFileBytes: number | null;
  readonly error: string | null;
}

export interface MeshDemoState {
  readonly mesh: 'starting' | 'auth-required' | 'online' | 'error';
  readonly deviceName: string;
  readonly dnsName: string | null;
  readonly authUrl: string | null;
  readonly error: string | null;
  readonly peers: readonly PeerView[];
  readonly share: ShareView | null;
  readonly remote: RemoteView;
}

export interface StartShareRequest {
  readonly access: ShareAccess;
  readonly allowedPeerIds: readonly string[];
}

export interface ConnectPeerRequest {
  readonly peerRef: string;
  readonly access: ShareAccess;
}

export interface MeshDemoApi {
  getState(): Promise<MeshDemoState>;
  openAuth(): Promise<void>;
  startSharing(request: StartShareRequest): Promise<string | null>;
  stopSharing(): Promise<void>;
  connectPeer(request: ConnectPeerRequest): Promise<void>;
  disconnectPeer(): Promise<void>;
  onState(listener: (state: MeshDemoState) => void): () => void;
}
