// Remote-workspace open handshake (SPEC §13).
//
// This runs as the first semantic messages on the framed stream, before the
// existing Mille handshake. Keeping workspace selection and authorization
// out of the explorer protocol is deliberate: mille's protocol has no
// concept of "which workspace" or "may I", and teaching it one would couple
// the local and remote cases for no benefit.
//
// The vocabulary itself now lives in `@vibecook/mille` (`src/remote/
// protocol.ts`), because a WebSocket session speaks it too and a browser
// client has to import it without this package's Node dependencies. It is
// re-exported here unchanged, so every name and import path this package
// published keeps working. Two additions arrived with the move, both
// invisible to this server: an optional `credential` on the open request
// (a ticket for hosts that authorize on one — this server authorizes on the
// socket's verified peer id and ignores it) and an optional
// `client.milleTruffleVersion`.

export {
  REMOTE_SERVICE,
  REMOTE_SERVICE_VERSION,
  isRemoteServiceMessage,
  parseOpenRequest,
  pong,
  rejection,
  type OpenWorkspaceAccepted,
  type OpenWorkspaceLimits,
  type OpenWorkspaceRejectCode,
  type OpenWorkspaceRejected,
  type OpenWorkspaceRequest,
  type RemoteAccess,
  type RemotePing,
  type RemotePong,
  type RemoteServiceMessage,
  type RequestProblem,
} from '@vibecook/mille/node';
