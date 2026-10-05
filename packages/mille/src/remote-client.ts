// Browser-safe remote client entry point — `@vibecook/mille/remote`.
//
// Everything a renderer, a renderer Worker or a phone's web view needs to
// reach a mille host over the network: the WebSocket channel adapter, the
// `mille.remote` open handshake (with its credential), and the framing
// limits. Pair it with `@vibecook/mille/port`, whose
// `connectFileExplorerChannel()` turns the opened channel into a live
// explorer.
//
// This entry's import graph is free of `node:*` modules and of the native
// addon, and `test/remote-entry-graph.test.mjs` holds it to that: a Worker
// bundle that imports it must never acquire a Node polyfill.

export {
  createWebSocketClientChannel,
  type WebSocketClientChannel,
  type WebSocketLike,
} from './websocket/client.js';
export type { WebSocketChannelOptions, WebSocketExplorerChannel } from './websocket/channel.js';
export {
  WEBSOCKET_CLOSE,
  explorerCloseCodeFor,
  webSocketCloseCodeFor,
} from './websocket/close-codes.js';
export {
  DEFAULT_LIMITS,
  FrameProtocolError,
  resolveLimits,
  type FramedStreamLimits,
  type FramedStreamLimitsInput,
} from './stream/limits.js';
export type {
  ExplorerChannel,
  ExplorerChannelCloseCode,
  ExplorerChannelCloseEvent,
  ExplorerChannelLogger,
  ExplorerChannelState,
  ExplorerClientChannel,
} from './channel/types.js';
export {
  connectWebSocketChannel,
  openRemoteChannel,
  type ConnectWebSocketChannelOptions,
  type OpenRemoteChannelOptions,
  type RemoteChannelConnection,
  type WebSocketConstructorLike,
} from './remote/client.js';
export {
  MAX_REMOTE_CREDENTIAL_LENGTH,
  REMOTE_SERVICE,
  REMOTE_SERVICE_VERSION,
  RemoteOpenError,
  isRemoteServiceMessage,
  parseOpenRequest,
  ping,
  pong,
  rejection,
  type OpenWorkspaceAccepted,
  type OpenWorkspaceLimits,
  type OpenWorkspaceRejectCode,
  type OpenWorkspaceRejected,
  type OpenWorkspaceRequest,
  type RemoteAccess,
  type RemoteOpenErrorCode,
  type RemotePing,
  type RemotePong,
  type RemoteServiceMessage,
  type RequestProblem,
} from './remote/protocol.js';
export { relayExplorerChannel, type RelayExplorerChannelOptions } from './remote/relay.js';
