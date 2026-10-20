// WebSocket close codes ⇄ ExplorerChannelCloseCode.
//
// A WebSocket close frame carries a 16-bit code, and the channel contract
// carries a reason union. The two have to agree in both directions: the side
// that shuts a session down picks a code that says why, and the side that
// receives it has to land on the same reason, or a client cannot tell "the
// host refused my ticket" from "the network dropped".
//
// The outbound table uses the application range (4000–4999) for everything
// but a normal close, for one hard reason: a browser's `WebSocket.close()`
// throws `InvalidAccessError` for any code other than 1000 or 3000–4999. The
// renderer-side adapter must be able to say PROTOCOL_ERROR too, and both ends
// speaking one table beats a server-only dialect. Each application code is
// 4000 plus the standard code it stands for, so 4002 reads as 1002 (protocol
// error) and 4008 as 1008 (policy violation).
//
// The inbound table also accepts the standard codes, because the transport
// itself can close a socket on our behalf: `ws` closes with 1009 when a
// message exceeds `maxPayload`, and a dropped connection surfaces as 1006.

import type { ExplorerChannelCloseCode } from '../channel/types.js';

export const WEBSOCKET_CLOSE = {
  /** A deliberate, orderly close (RFC 6455 §7.4.1). */
  NORMAL: 1000,
  /** A frame that is not a well-formed mille frame. */
  PROTOCOL_ERROR: 4002,
  /** A text message; the mille protocol is binary only. */
  UNSUPPORTED_DATA: 4003,
  /** The open request was refused — a bad ticket, an unknown export. */
  AUTH_REJECTED: 4008,
  /** A message larger than the negotiated frame limit. */
  MESSAGE_TOO_BIG: 4009,
  /** The local side gave up on a transport it could no longer use. */
  TRANSPORT_ERROR: 4011,
  /** The peer stopped reading and the outbound queue hit its hard limit. */
  BACKPRESSURE: 4013,
} as const;

/** Which flavour of protocol error caused a close, for a more precise code. */
export type ProtocolErrorDetail = 'malformed' | 'text' | 'oversize';

/** The code this side sends when it shuts a session down for `reason`. */
export function webSocketCloseCodeFor(
  reason: ExplorerChannelCloseCode,
  detail?: ProtocolErrorDetail,
): number {
  switch (reason) {
    case 'LOCAL_CLOSE':
    case 'REMOTE_CLOSE':
      return WEBSOCKET_CLOSE.NORMAL;
    case 'PROTOCOL_ERROR':
      if (detail === 'text') return WEBSOCKET_CLOSE.UNSUPPORTED_DATA;
      if (detail === 'oversize') return WEBSOCKET_CLOSE.MESSAGE_TOO_BIG;
      return WEBSOCKET_CLOSE.PROTOCOL_ERROR;
    case 'AUTH_REJECTED':
      return WEBSOCKET_CLOSE.AUTH_REJECTED;
    case 'BACKPRESSURE':
      return WEBSOCKET_CLOSE.BACKPRESSURE;
    case 'TRANSPORT_ERROR':
      return WEBSOCKET_CLOSE.TRANSPORT_ERROR;
  }
}

/** The channel reason a received close code stands for. */
export function explorerCloseCodeFor(code: number): ExplorerChannelCloseCode {
  switch (code) {
    // 1002 protocol error · 1003 unsupported data · 1007 invalid payload ·
    // 1009 too big · 1010 missing extension — all "the bytes were wrong".
    case 1002:
    case 1003:
    case 1007:
    case 1009:
    case 1010:
    case WEBSOCKET_CLOSE.PROTOCOL_ERROR:
    case WEBSOCKET_CLOSE.UNSUPPORTED_DATA:
    case WEBSOCKET_CLOSE.MESSAGE_TOO_BIG:
      return 'PROTOCOL_ERROR';
    case 1008:
    case WEBSOCKET_CLOSE.AUTH_REJECTED:
      return 'AUTH_REJECTED';
    case WEBSOCKET_CLOSE.BACKPRESSURE:
      return 'BACKPRESSURE';
    // 1006 is never sent: it is what an endpoint reports when the connection
    // dropped without a close frame. 1011/1014/1015 are server or TLS faults.
    case 1006:
    case 1011:
    case 1014:
    case 1015:
    case WEBSOCKET_CLOSE.TRANSPORT_ERROR:
      return 'TRANSPORT_ERROR';
    // 1000 normal · 1001 going away · 1005 no status · 1012 restart ·
    // 1013 try later, and any application code we do not define: the peer
    // chose to close, which is what REMOTE_CLOSE means.
    default:
      return 'REMOTE_CLOSE';
  }
}

/** RFC 6455 §5.5: a control frame's payload is at most 125 bytes, two of them the code. */
const MAX_CLOSE_REASON_BYTES = 123;

/**
 * Trim a close reason to what a close frame can carry.
 *
 * Both `ws` and browsers *throw* on a longer reason, and a throw at shutdown
 * would leave the socket open. Cuts on a code-point boundary so the peer
 * never receives invalid UTF-8 (which `ws` answers with 1007).
 */
export function closeReasonForWire(reason: string | undefined): string {
  if (reason === undefined || reason.length === 0) return '';
  const encoder = new TextEncoder();
  if (encoder.encode(reason).byteLength <= MAX_CLOSE_REASON_BYTES) return reason;
  let out = '';
  let bytes = 0;
  for (const ch of reason) {
    const size = encoder.encode(ch).byteLength;
    if (bytes + size > MAX_CLOSE_REASON_BYTES) break;
    out += ch;
    bytes += size;
  }
  return out;
}
