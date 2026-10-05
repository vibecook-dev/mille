// Client-side WebSocket adapter — exported from `@vibecook/mille/remote`.
//
// Takes a standard `WebSocket` — a browser's, a renderer Worker's, Node 22's
// global, or the `ws` package's client — and presents it as the
// ExplorerClientChannel that `connectFileExplorerChannel()` consumes. This
// module and everything it imports is free of `node:*` and of the native
// addon, so it bundles into a renderer or a Worker unchanged.

import type { ClientToHostMessage, HostToClientMessage } from '../protocol.js';
import {
  WebSocketFramedChannel,
  type WebSocketChannelOptions,
  type WebSocketExplorerChannel,
  type WebSocketTransport,
} from './channel.js';

/**
 * The standard `WebSocket` surface mille needs. Declared structurally so the
 * package does not depend on the DOM library or on Node's global typings.
 */
export interface WebSocketLike {
  readonly readyState: number;
  readonly bufferedAmount: number;
  binaryType: string;
  send(data: Uint8Array<ArrayBuffer>): void;
  close(code?: number, reason?: string): void;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  addEventListener(type: string, listener: (event: any) => void): void;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  removeEventListener(type: string, listener: (event: any) => void): void;
}

export type WebSocketClientChannel = WebSocketExplorerChannel<
  ClientToHostMessage,
  HostToClientMessage
>;

interface MessageEventLike {
  readonly data?: unknown;
}

interface CloseEventLike {
  readonly code?: unknown;
  readonly reason?: unknown;
}

function standardTransport(socket: WebSocketLike): WebSocketTransport {
  // A browser defaults to 'blob', which can only be read asynchronously and
  // would reorder messages behind their reads. ArrayBuffers arrive in order.
  socket.binaryType = 'arraybuffer';
  return {
    get readyState() {
      return socket.readyState;
    },
    get bufferedAmount() {
      return socket.bufferedAmount;
    },
    send(frame, onSent) {
      // The standard API accepts synchronously into `bufferedAmount` and
      // reports nothing further; drain() polls the buffer from here on.
      socket.send(frame);
      onSent();
    },
    close(code, reason) {
      socket.close(code, reason);
    },
    listen(handlers, lateError) {
      const onOpen = (): void => handlers.open();
      const onMessage = (event: MessageEventLike): void => handlers.message(event.data, undefined);
      const onClose = (event: CloseEventLike): void =>
        handlers.close(
          typeof event.code === 'number' ? event.code : 1005,
          typeof event.reason === 'string' ? event.reason : '',
        );
      // A browser's error event carries no detail; `ws` puts it on `.error`.
      const onError = (event: unknown): void => {
        const detail = (event as { error?: unknown } | null)?.error;
        handlers.error(detail ?? event);
      };
      socket.addEventListener('open', onOpen);
      socket.addEventListener('message', onMessage);
      socket.addEventListener('close', onClose);
      socket.addEventListener('error', onError);
      return () => {
        // The `ws` client is an EventEmitter underneath its event-target
        // methods, and throws an unheard 'error'. Keep one attached.
        socket.addEventListener('error', lateError);
        socket.removeEventListener('open', onOpen);
        socket.removeEventListener('message', onMessage);
        socket.removeEventListener('close', onClose);
        socket.removeEventListener('error', onError);
      };
    },
  };
}

/**
 * Wrap a standard WebSocket as a client channel.
 *
 * The socket may still be CONNECTING: frames sent before it opens are held
 * (and counted in `bufferedBytes`) and go out in order once it does. A socket
 * that is already closing or closed is refused with a throw.
 */
export function createWebSocketClientChannel(
  socket: WebSocketLike,
  options?: WebSocketChannelOptions,
): WebSocketClientChannel {
  return new WebSocketFramedChannel<ClientToHostMessage, HostToClientMessage>(
    standardTransport(socket),
    options,
  );
}
