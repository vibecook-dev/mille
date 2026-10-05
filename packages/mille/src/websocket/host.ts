// Host-side WebSocket adapter — exported from `@vibecook/mille/node`.
//
// Takes the server-side socket the `ws` package hands to a
// `WebSocketServer`'s `'connection'` listener (or anything shaped like it)
// and presents it as an ExplorerHostChannel. mille never imports `ws`: the
// shape below is structural, so an embedder brings its own server, its own
// upgrade handling and its own `ws` version.
//
// Set `maxPayload` on the server to the frame limit (the default is
// `DEFAULT_LIMITS.maxFrameBytes`, 32 MiB). `ws` buffers a whole message
// before this adapter sees it, so only the server can refuse an oversized
// message before it is allocated; the adapter still rejects one that gets
// through, with close code 4009.

import type { ClientToHostMessage, HostToClientMessage } from '../protocol.js';
import {
  WebSocketFramedChannel,
  type WebSocketChannelOptions,
  type WebSocketExplorerChannel,
  type WebSocketTransport,
} from './channel.js';

/**
 * The server-side socket shape mille needs: the `ws` package's `WebSocket`
 * satisfies it as-is. `terminate` is optional; without it a transport fault
 * closes with 4011 instead of dropping the connection outright.
 */
export interface WebSocketServerSocketLike {
  readonly readyState: number;
  readonly bufferedAmount: number;
  send(data: Uint8Array, options: { binary: boolean }, callback?: (error?: Error) => void): void;
  close(code?: number, reason?: string): void;
  terminate?(): void;
  // `any[]` because `ws` declares these listeners with exact tuple types per
  // event; a structural `unknown[]` would not accept its overloads.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  on(event: string, listener: (...args: any[]) => void): unknown;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  off?(event: string, listener: (...args: any[]) => void): unknown;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  removeListener?(event: string, listener: (...args: any[]) => void): unknown;
}

export type WebSocketHostChannel = WebSocketExplorerChannel<
  HostToClientMessage,
  ClientToHostMessage
>;

function reasonText(reason: unknown): string {
  if (typeof reason === 'string') return reason;
  if (reason instanceof Uint8Array) return new TextDecoder().decode(reason);
  return '';
}

function serverTransport(socket: WebSocketServerSocketLike): WebSocketTransport {
  const off = (event: string, listener: (...args: unknown[]) => void): void => {
    if (typeof socket.off === 'function') socket.off(event, listener);
    else socket.removeListener?.(event, listener);
  };
  return {
    get readyState() {
      return socket.readyState;
    },
    get bufferedAmount() {
      return socket.bufferedAmount;
    },
    send(frame, onSent) {
      socket.send(frame, { binary: true }, onSent);
    },
    close(code, reason) {
      socket.close(code, reason);
    },
    ...(typeof socket.terminate === 'function'
      ? { terminate: () => (socket.terminate as () => void).call(socket) }
      : {}),
    listen(handlers, lateError) {
      const onOpen = (): void => handlers.open();
      const onMessage = (data: unknown, isBinary: unknown): void =>
        handlers.message(data, typeof isBinary === 'boolean' ? isBinary : undefined);
      const onClose = (code: unknown, reason: unknown): void =>
        handlers.close(typeof code === 'number' ? code : 1005, reasonText(reason));
      const onError = (error: unknown): void => handlers.error(error);
      socket.on('open', onOpen);
      socket.on('message', onMessage);
      socket.on('close', onClose);
      socket.on('error', onError);
      return () => {
        // Swap, never drop: the socket must keep an error listener.
        socket.on('error', lateError);
        off('open', onOpen);
        off('message', onMessage as (...args: unknown[]) => void);
        off('close', onClose as (...args: unknown[]) => void);
        off('error', onError as (...args: unknown[]) => void);
      };
    },
  };
}

/**
 * Wrap an accepted server-side WebSocket as a host channel.
 *
 * The channel speaks only the framing; it authorizes nothing. Hand it to
 * `serveRemoteSession()` (or `serveWebSocketSession()`, which does both) so
 * no host session exists until the client's credential has been checked.
 */
export function createWebSocketHostChannel(
  socket: WebSocketServerSocketLike,
  options?: WebSocketChannelOptions,
): WebSocketHostChannel {
  return new WebSocketFramedChannel<HostToClientMessage, ClientToHostMessage>(
    serverTransport(socket),
    options,
  );
}
