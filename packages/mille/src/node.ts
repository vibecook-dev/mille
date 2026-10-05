// Node-only entry point — `@vibecook/mille/node`.
//
// The framed stream channel imports `node:stream`, so it must not be
// reachable from the package root: a renderer or browser bundle that pulls
// in `@vibecook/mille` should never acquire a Node polyfill for it
// (SPEC NFR-007, §11.8). Everything here is additive; the root entry is
// unchanged and stays browser-safe.
//
// Pair this with any Duplex — a PassThrough in tests, a TCP socket, or a
// Truffle mesh socket, which is a `stream.Duplex` with a tailnet-verified
// peer identity attached.

export {
  createFramedStreamHostChannel,
  createFramedStreamClientChannel,
  type FramedStreamChannelOptions,
} from './stream/framed-channel.js';

export {
  DEFAULT_LIMITS,
  FrameProtocolError,
  HEADER_BYTES,
  WIRE_MAGIC,
  WIRE_MAJOR,
  WIRE_MINOR,
  resolveLimits,
  type FramedStreamLimits,
  type FramedStreamLimitsInput,
} from './stream/limits.js';

// The same framing over a WebSocket: one codec frame per binary message.
// The host side lives here because it pairs with `createFileExplorerHost`;
// the browser-safe client side is `@vibecook/mille/remote`.
export {
  createWebSocketHostChannel,
  serveWebSocketSession,
  type WebSocketHostChannel,
  type WebSocketServerSocketLike,
} from './websocket/host.js';

// A `mille.remote` session on any host channel: the open request, the
// credential check, the policy from the grant, heartbeats kept off the host.
export { DEFAULT_REMOTE_MAX_VIEWPORT_ROWS } from './channel/policy.js';
export {
  DEFAULT_REMOTE_MAX_FILE_BYTES,
  serveRemoteSession,
  type RemoteAuthorizeResult,
  type RemoteOpenContext,
  type RemoteSession,
  type RemoteSessionGrant,
  type RemoteSessionHost,
  type RemoteSessionInfo,
  type RemoteSessionLogger,
  type RemoteSessionRefusal,
  type ServeRemoteSessionOptions,
} from './remote/server.js';
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
export type { WebSocketChannelOptions, WebSocketExplorerChannel } from './websocket/channel.js';
export {
  WEBSOCKET_CLOSE,
  explorerCloseCodeFor,
  webSocketCloseCodeFor,
} from './websocket/close-codes.js';

// Exported for tests and for anyone building a transport of their own on the
// same wire format. Not part of the supported surface — the framing is
// versioned, but these signatures are not. A WebSocket no longer needs them:
// use the adapters above.
export { encodeFrame, decodeFrame, parseHeader, type FrameHeader } from './stream/codec.js';
export { FrameDecoder } from './stream/decoder.js';
