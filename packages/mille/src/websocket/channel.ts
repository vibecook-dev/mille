// ExplorerChannel over a WebSocket — one codec frame per binary message.
//
// The framed stream channel (../stream/framed-channel.ts) has to find frame
// boundaries in a byte stream, because a Duplex splits wherever it likes. A
// WebSocket already delivers whole messages, so this channel maps one mille
// frame onto exactly one binary message and needs no incremental decoder:
// a message either is a complete, valid frame or the session ends. The
// codec, the header validation and the limits are the same ones the stream
// uses, so the wire format does not fork — only its envelope differs.
//
// Nothing here imports `node:*`. The same class serves the host (wrapping
// the `ws` package's server-side socket, see ./host.ts) and the renderer
// (wrapping a standard browser or Worker `WebSocket`, see ./client.ts); each
// adapter only normalizes event subscription into a `WebSocketTransport`.
//
// Backpressure. A WebSocket has no `'drain'` event; `bufferedAmount` is the
// only signal on both platforms. So the hard limit is checked against
// `bufferedAmount` before every send, and `drain()` polls it — a short timer
// that exists only while someone is waiting. On `ws` the per-send callback
// re-checks immediately, so a host rarely waits for the timer.

import type { Disposable } from '../types.js';
import { ListenerSet, NOOP_LOGGER } from '../channel/emitter.js';
import type {
  ExplorerChannel,
  ExplorerChannelCloseCode,
  ExplorerChannelCloseEvent,
  ExplorerChannelLogger,
  ExplorerChannelState,
} from '../channel/types.js';
import { decodeFrame, encodeFrame, parseHeader } from '../stream/codec.js';
import {
  FrameProtocolError,
  HEADER_BYTES,
  resolveLimits,
  type FramedStreamLimits,
  type FramedStreamLimitsInput,
} from '../stream/limits.js';
import {
  closeReasonForWire,
  explorerCloseCodeFor,
  webSocketCloseCodeFor,
  type ProtocolErrorDetail,
} from './close-codes.js';

/** `WebSocket.CONNECTING` / `OPEN` — the same numbers on `ws` and in browsers. */
const CONNECTING = 0;
const OPEN = 1;

/** How often `drain()` re-reads `bufferedAmount` while a caller is waiting. */
const DRAIN_POLL_MS = 8;

export interface WebSocketChannelOptions extends FramedStreamLimitsInput {
  readonly logger?: ExplorerChannelLogger;
}

/**
 * An ExplorerChannel over a WebSocket, plus one capability the generic
 * contract lacks: closing *with a reason the peer can read*. `close()` always
 * means an orderly 1000; `closeAs('AUTH_REJECTED')` tells the client its
 * credential was refused (4008) even if the refusal frame never arrives.
 */
export interface WebSocketExplorerChannel<TOutbound, TInbound> extends ExplorerChannel<
  TOutbound,
  TInbound
> {
  closeAs(code: ExplorerChannelCloseCode, reason?: string): void;
}

/** What an adapter hands the channel once it has normalized a socket. */
export interface WebSocketTransport {
  readonly readyState: number;
  readonly bufferedAmount: number;
  /** Queue one binary message. `onSent` fires once it left the local queue, or failed. */
  // `ArrayBuffer`-backed, because the standard `WebSocket.send` refuses a
  // view that could sit on a SharedArrayBuffer. `encodeFrame` allocates one.
  send(frame: Uint8Array<ArrayBuffer>, onSent: (error?: unknown) => void): void;
  close(code: number, reason: string): void;
  /** Drop the connection without a closing handshake, where the platform can. */
  terminate?(): void;
  /**
   * Attach the handlers. The returned function detaches them, and must leave
   * `lateError` attached in their place: `ws` sockets are EventEmitters, and
   * an `'error'` with no listener is thrown — tearing a socket down is
   * exactly when one tends to arrive.
   */
  listen(handlers: WebSocketTransportHandlers, lateError: (error: unknown) => void): () => void;
}

export interface WebSocketTransportHandlers {
  open(): void;
  /** `isBinary` is known on `ws`; a browser says the same thing with the data's type. */
  message(data: unknown, isBinary: boolean | undefined): void;
  close(code: number, reason: string): void;
  error(error: unknown): void;
}

interface DrainWaiter {
  /** `#sentTotal` at the time of the call. */
  readonly mark: number;
  readonly resolve: () => void;
}

interface ShutdownOptions {
  /** The socket is already closed (or closing) by the peer; do not close it again. */
  readonly peerClosed?: boolean;
  readonly detail?: ProtocolErrorDetail;
}

/** A complete binary message as bytes, `'text'` for a text message, `null` for anything else. */
function messageBytes(data: unknown, isBinary: boolean | undefined): Uint8Array | 'text' | null {
  if (typeof data === 'string' || isBinary === false) return 'text';
  if (data instanceof Uint8Array) return data;
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  }
  if (
    data instanceof ArrayBuffer ||
    Object.prototype.toString.call(data) === '[object ArrayBuffer]'
  ) {
    return new Uint8Array(data as ArrayBuffer);
  }
  // `ws` with binaryType 'fragments' hands over the frame's pieces.
  if (Array.isArray(data) && data.every((part) => part instanceof Uint8Array)) {
    const parts = data as Uint8Array[];
    let total = 0;
    for (const part of parts) total += part.byteLength;
    const out = new Uint8Array(total);
    let offset = 0;
    for (const part of parts) {
      out.set(part, offset);
      offset += part.byteLength;
    }
    return out;
  }
  return null;
}

/**
 * Decode one message that must be exactly one frame.
 *
 * The header is validated against the limits before the body is touched,
 * exactly as the stream decoder does, and a length that disagrees with the
 * message's real size is rejected: trailing bytes or a short body mean the
 * sender is not framing the way this transport requires.
 */
function decodeMessageFrame(bytes: Uint8Array, limits: FramedStreamLimits): unknown {
  if (bytes.byteLength < HEADER_BYTES) {
    throw new FrameProtocolError(`message of ${bytes.byteLength} bytes is shorter than a header`);
  }
  const header = parseHeader(bytes.subarray(0, HEADER_BYTES), limits);
  if (header.totalLength !== bytes.byteLength) {
    throw new FrameProtocolError(
      `message is ${bytes.byteLength} bytes but its frame declares ${header.totalLength}`,
    );
  }
  return decodeFrame(bytes, header);
}

function describe(error: unknown, fallback: string): string {
  if (error instanceof Error && error.message.length > 0) return error.message;
  if (typeof error === 'object' && error !== null) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === 'string' && message.length > 0) return message;
  }
  return fallback;
}

export class WebSocketFramedChannel<TOutbound, TInbound> implements WebSocketExplorerChannel<
  TOutbound,
  TInbound
> {
  readonly #transport: WebSocketTransport;
  readonly #limits: FramedStreamLimits;
  readonly #logger: ExplorerChannelLogger;
  readonly #messages: ListenerSet<TInbound>;
  readonly #closes: ListenerSet<ExplorerChannelCloseEvent>;
  readonly #unlisten: () => void;

  /** Frames sent while the socket was still CONNECTING, flushed on open. */
  #pending: Uint8Array<ArrayBuffer>[] = [];
  #pendingBytes = 0;
  /** Cumulative bytes ever passed to `send()`, for drain() ordering. */
  #sentTotal = 0;
  #drainWaiters: DrainWaiter[] = [];
  #drainTimer: ReturnType<typeof setTimeout> | undefined;
  #softWarned = false;

  #state: ExplorerChannelState = 'open';
  #closeEvent: ExplorerChannelCloseEvent | undefined;

  constructor(transport: WebSocketTransport, options?: WebSocketChannelOptions) {
    const readyState = transport.readyState;
    if (readyState !== CONNECTING && readyState !== OPEN) {
      throw new Error(
        'WebSocket is closing or closed; a channel needs a connecting or open socket',
      );
    }
    this.#transport = transport;
    this.#limits = resolveLimits(options);
    this.#logger = options?.logger ?? NOOP_LOGGER;
    this.#messages = new ListenerSet<TInbound>('channel message', this.#logger);
    this.#closes = new ListenerSet<ExplorerChannelCloseEvent>('channel close', this.#logger);
    this.#unlisten = transport.listen(
      {
        open: this.#onOpen,
        message: this.#onMessage,
        close: this.#onClose,
        error: this.#onError,
      },
      this.#onLateError,
    );
  }

  get state(): ExplorerChannelState {
    return this.#state;
  }

  /** Bytes accepted by `send()` that the socket has not yet put on the wire. */
  get bufferedBytes(): number {
    if (this.#state !== 'open') return 0;
    return this.#pendingBytes + this.#transport.bufferedAmount;
  }

  /** True once the queue passes the soft watermark; callers should await drain(). */
  get overSoftWatermark(): boolean {
    return this.bufferedBytes >= this.#limits.outboundSoftBytes;
  }

  send(message: TOutbound): void {
    if (this.#state !== 'open') {
      throw new Error(`ExplorerChannel is ${this.#state}; cannot send`);
    }
    // Encoding failures are the caller's bug, not a transport fault: throw
    // without killing a channel that is still perfectly healthy.
    const frame = encodeFrame(message, this.#limits);

    const queued = this.bufferedBytes;
    if (queued + frame.byteLength > this.#limits.outboundHardBytes) {
      const err = new Error(
        `outbound queue would exceed ${this.#limits.outboundHardBytes} bytes; closing channel`,
      );
      this.#shutdown('BACKPRESSURE', 'outbound hard limit exceeded', err);
      throw err;
    }

    this.#sentTotal += frame.byteLength;
    if (this.#transport.readyState === CONNECTING) {
      this.#pending.push(frame);
      this.#pendingBytes += frame.byteLength;
    } else {
      const failure = this.#write(frame);
      // A socket that throws on send is gone. Close with the specific cause,
      // then rethrow so the caller's failure path (the host detaches the
      // session) still runs — the MessagePort adapter does the same.
      if (failure !== undefined) throw failure;
    }

    if (queued + frame.byteLength >= this.#limits.outboundSoftBytes) {
      if (!this.#softWarned) {
        this.#softWarned = true;
        this.#logger.warn('outbound queue passed the soft watermark', {
          queuedBytes: queued + frame.byteLength,
          softLimit: this.#limits.outboundSoftBytes,
        });
      }
    } else {
      this.#softWarned = false;
    }
  }

  /**
   * CH-004 over a WebSocket: resolves once everything sent before this call
   * has left the socket's buffer. With a FIFO buffer that is exactly when
   * what remains buffered is no more than what was sent *after* the call.
   * Framing overhead counted in `bufferedAmount` can only make this resolve
   * later than strictly necessary, never earlier.
   */
  drain(): Promise<void> {
    if (this.#state !== 'open' || this.bufferedBytes === 0) return Promise.resolve();
    return new Promise<void>((resolve) => {
      this.#drainWaiters.push({ mark: this.#sentTotal, resolve });
      this.#scheduleDrainCheck();
    });
  }

  onMessage(listener: (message: TInbound) => void): Disposable {
    return this.#messages.add(listener);
  }

  onClose(listener: (event: ExplorerChannelCloseEvent) => void): Disposable {
    if (this.#state === 'closed') {
      const event = this.#closeEvent ?? { code: 'LOCAL_CLOSE' as const };
      queueMicrotask(() => listener(event));
      return { dispose: () => {} };
    }
    return this.#closes.add(listener);
  }

  close(reason?: string): void {
    this.#shutdown('LOCAL_CLOSE', reason);
  }

  closeAs(code: ExplorerChannelCloseCode, reason?: string): void {
    // REMOTE_CLOSE describes the peer's decision; locally it is just a close.
    this.#shutdown(code === 'REMOTE_CLOSE' ? 'LOCAL_CLOSE' : code, reason);
  }

  dispose(): void {
    this.close('disposed');
  }

  // ─── internals ────────────────────────────────────────────────────────

  /** Hand one frame to the socket. Returns the error when the socket threw. */
  #write(frame: Uint8Array<ArrayBuffer>): unknown {
    try {
      this.#transport.send(frame, this.#onSent);
      return undefined;
    } catch (err) {
      this.#shutdown('TRANSPORT_ERROR', 'WebSocket send threw', err);
      return err ?? new Error('WebSocket send threw');
    }
  }

  readonly #onSent = (): void => {
    // A failed send is followed by the socket's own close/error event, which
    // carries the better reason. Here we only learn the buffer moved.
    this.#checkDrain();
  };

  #scheduleDrainCheck(): void {
    if (this.#drainTimer !== undefined || this.#drainWaiters.length === 0) return;
    this.#drainTimer = setTimeout(() => {
      this.#drainTimer = undefined;
      this.#checkDrain();
    }, DRAIN_POLL_MS);
    // Never keep a Node process alive just to poll a buffer.
    (this.#drainTimer as { unref?: () => void }).unref?.();
  }

  #checkDrain(): void {
    if (this.#drainWaiters.length === 0) return;
    const buffered = this.bufferedBytes;
    const ready = this.#drainWaiters.filter((w) => buffered <= this.#sentTotal - w.mark);
    if (ready.length > 0) {
      this.#drainWaiters = this.#drainWaiters.filter((w) => buffered > this.#sentTotal - w.mark);
      for (const w of ready) w.resolve();
    }
    this.#scheduleDrainCheck();
  }

  readonly #onOpen = (): void => {
    if (this.#state !== 'open') return;
    const frames = this.#pending;
    this.#pending = [];
    this.#pendingBytes = 0;
    for (const frame of frames) {
      if (this.#write(frame) !== undefined) return;
    }
  };

  readonly #onMessage = (data: unknown, isBinary: boolean | undefined): void => {
    if (this.#state !== 'open') return;
    const bytes = messageBytes(data, isBinary);
    if (bytes === 'text') {
      this.#shutdown('PROTOCOL_ERROR', 'text message; the mille protocol is binary', undefined, {
        detail: 'text',
      });
      return;
    }
    if (bytes === null) {
      this.#shutdown('PROTOCOL_ERROR', 'unsupported message payload');
      return;
    }
    if (bytes.byteLength > this.#limits.maxFrameBytes) {
      this.#shutdown(
        'PROTOCOL_ERROR',
        `message of ${bytes.byteLength} bytes exceeds limit ${this.#limits.maxFrameBytes}`,
        undefined,
        { detail: 'oversize' },
      );
      return;
    }
    let message: unknown;
    try {
      message = decodeMessageFrame(bytes, this.#limits);
    } catch (err) {
      // A malformed frame terminates this session only — never the shared
      // host (SPEC NFR-005) — and makes no attempt to resynchronize.
      this.#shutdown('PROTOCOL_ERROR', describe(err, 'malformed frame'), err);
      return;
    }
    this.#messages.emit(message as TInbound);
  };

  readonly #onClose = (code: number, reason: string): void => {
    this.#shutdown(
      explorerCloseCodeFor(code),
      reason.length > 0 ? reason : `peer closed with code ${code}`,
      { webSocketCloseCode: code },
      { peerClosed: true },
    );
  };

  readonly #onError = (error: unknown): void => {
    // `ws` reports a frame-level violation (an oversized message, invalid
    // UTF-8, a bad close code) as an error whose code starts with WS_ERR_,
    // and has already started the closing handshake with the right status.
    const code = (error as { code?: unknown } | null)?.code;
    if (typeof code === 'string' && code.startsWith('WS_ERR_')) {
      this.#shutdown('PROTOCOL_ERROR', describe(error, code), error, { peerClosed: true });
      return;
    }
    this.#shutdown('TRANSPORT_ERROR', describe(error, 'WebSocket error'), error);
  };

  readonly #onLateError = (error: unknown): void => {
    this.#logger.warn('WebSocket error after channel close', error);
  };

  #shutdown(
    code: ExplorerChannelCloseCode,
    reason?: string,
    cause?: unknown,
    options: ShutdownOptions = {},
  ): void {
    if (this.#state !== 'open') return;
    this.#state = 'closing';
    const event: ExplorerChannelCloseEvent = { code, reason, cause };
    this.#closeEvent = event;

    this.#unlisten();
    this.#pending = [];
    this.#pendingBytes = 0;

    if (options.peerClosed !== true) {
      try {
        if (code === 'TRANSPORT_ERROR' && this.#transport.terminate !== undefined) {
          // A transport we gave up on gets no closing handshake to hang on.
          this.#transport.terminate();
        } else {
          this.#transport.close(
            webSocketCloseCodeFor(code, options.detail),
            closeReasonForWire(reason),
          );
        }
      } catch (err) {
        this.#logger.warn('closing the WebSocket threw', err);
      }
    }

    this.#state = 'closed';
    if (this.#drainTimer !== undefined) {
      clearTimeout(this.#drainTimer);
      this.#drainTimer = undefined;
    }
    // Anything awaiting drain will never be satisfied now; release them
    // rather than leaving the promises dangling forever.
    const waiters = this.#drainWaiters;
    this.#drainWaiters = [];
    for (const w of waiters) w.resolve();

    this.#closes.emit(event);
    this.#closes.clear();
    this.#messages.clear();
  }
}
