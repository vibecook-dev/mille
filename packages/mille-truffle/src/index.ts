// @vibecook/mille-truffle — serve a mille workspace over a tailnet.
//
// Node-only: it builds framed stream channels over `mesh.net` sockets.

export { serveMille } from './server.js';
export type { MeshLike, MeshNetLike, MeshServerLike, TruffleSocketLike } from './server.js';

export { connectMille, connectMilleChannel } from './client.js';
export type {
  ConnectMilleChannelOptions,
  ConnectMilleOptions,
  MeshConnectLike,
  MilleChannelConnection,
  RemoteConnectionEvent,
  RemoteFileExplorer,
  RemoteIdentityResetEvent,
} from './client.js';

export { DEFAULT_RECONNECT, backoffDelay, resolveReconnect, shouldRetry } from './state.js';
export type {
  ReconnectOptions,
  RemoteConnectionState,
  ResolvedReconnect,
  RetryDecision,
} from './state.js';

export { RemoteExplorerError, isRemoteExplorerError } from './errors.js';
export type { RemoteExplorerErrorCode } from './errors.js';

export { ExportConfigError, resolveExport, resolveExports } from './exports.js';

export { authorizePeer, sessionPolicyFor } from './authorize.js';
export type { AuthorizeInput, AuthorizeOutcome } from './authorize.js';

export {
  REMOTE_SERVICE,
  REMOTE_SERVICE_VERSION,
  isRemoteServiceMessage,
  parseOpenRequest,
} from './handshake.js';
export type {
  OpenWorkspaceAccepted,
  OpenWorkspaceLimits,
  OpenWorkspaceRejectCode,
  OpenWorkspaceRejected,
  OpenWorkspaceRequest,
  RemoteAccess,
  RemotePing,
  RemotePong,
  RemoteServiceMessage,
} from './handshake.js';

export type {
  AuthorizeMillePeerContext,
  MilleExportConfig,
  MilleRemoteLogger,
  MilleRemoteServer,
  RemoteSessionInfo,
  ResolvedExport,
  ServeMilleOptions,
} from './types.js';
