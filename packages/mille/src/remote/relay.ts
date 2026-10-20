// Relay an explorer channel into a MessagePort — browser-safe (M7).
//
// The shape this exists for: a renderer Worker holds the network session
// (a WebSocket opened with a ticket) and hands an isolated consumer — an
// iframe, a sandboxed room, the main thread — nothing but a MessagePort.
// The consumer runs `connectFileExplorer(port)` exactly as it would against
// an in-process host; it never sees the socket, the credential, or a Node
// primitive. mesh-demo's utility-process relay did this for Electron's
// `MessagePortMain`; this one takes any standard or Node `MessagePort`.
//
// Messages are relayed verbatim — the relay neither parses nor filters
// explorer frames (service frames were already peeled off by
// `openRemoteChannel`). One addition: a MessagePort has no close event a
// client can rely on, so when the upstream channel ends the relay posts a
// fatal `error` frame first. `PortFileExplorer` closes on it, so pending
// calls reject with the upstream reason and later calls fail fast.

import type { Disposable, MessagePortLike } from '../types.js';
import { NOOP_LOGGER } from '../channel/emitter.js';
import { adaptPort } from '../channel/message-port.js';
import type {
  ExplorerChannelCloseEvent,
  ExplorerChannelLogger,
  ExplorerClientChannel,
} from '../channel/types.js';
import { PROTOCOL_VERSION, type ClientToHostMessage } from '../protocol.js';

export interface RelayExplorerChannelOptions {
  /**
   * Transfer each binary payload's ArrayBuffer to the port instead of
   * copying it. Only when the relay is the channel's sole message consumer:
   * a transferred buffer is detached for every other listener.
   */
  readonly transfer?: boolean;
  /** Called once when the relay stops, with why. */
  readonly onClose?: (event: ExplorerChannelCloseEvent) => void;
  readonly logger?: ExplorerChannelLogger;
}

/** Event subscription a raw port may offer for its close notification. */
interface ClosablePort {
  addEventListener?(type: 'close', listener: () => void): void;
  removeEventListener?(type: 'close', listener: () => void): void;
  on?(event: 'close', listener: () => void): unknown;
  off?(event: 'close', listener: () => void): unknown;
}

/**
 * Every ArrayBuffer in `message` that a view owns outright. A view onto part
 * of a larger buffer is left to be copied: transferring would ship (and
 * detach) bytes that are not the payload's.
 */
function transferablesOf(message: unknown): ArrayBuffer[] {
  const out = new Set<ArrayBuffer>();
  const walk = (value: unknown, depth: number): void => {
    if (depth > 64 || value === null || typeof value !== 'object') return;
    if (ArrayBuffer.isView(value)) {
      const { buffer, byteOffset, byteLength } = value;
      if (buffer instanceof ArrayBuffer && byteOffset === 0 && byteLength === buffer.byteLength) {
        out.add(buffer);
      }
      return;
    }
    if (Array.isArray(value)) {
      for (const item of value) walk(item, depth + 1);
      return;
    }
    for (const item of Object.values(value)) walk(item, depth + 1);
  };
  walk(message, 0);
  return [...out];
}

/**
 * Pump `channel` ⇄ `port` until either side ends.
 *
 * - upstream closes → a fatal `error` frame to the port, then the port closes;
 * - the port closes (where the platform reports it) → the channel closes;
 * - `dispose()` → both close, and the port's client is told first.
 */
export function relayExplorerChannel(
  channel: ExplorerClientChannel,
  port: MessagePortLike,
  options: RelayExplorerChannelOptions = {},
): Disposable {
  const logger = options.logger ?? NOOP_LOGGER;
  const target = adaptPort(port);
  const closable = port as unknown as ClosablePort;
  const transfer = options.transfer === true;
  let active = true;

  const post = (message: unknown): void => {
    if (transfer) {
      const list = transferablesOf(message);
      target.postMessage(message, list);
    } else {
      target.postMessage(message);
    }
  };

  const announceClose = (event: ExplorerChannelCloseEvent): void => {
    try {
      post({
        v: PROTOCOL_VERSION,
        type: 'error',
        body: {
          code: 'ECANCELED',
          message: `connection closed: ${event.code}`,
          fatal: true,
          closeCode: event.code,
        },
      });
    } catch {
      /* the port is already gone; nobody is left to tell */
    }
  };

  const fromChannel = channel.onMessage((message) => {
    if (!active) return;
    try {
      post(message);
    } catch (err) {
      logger.warn('relay could not post to the port; closing the channel', err);
      finish(
        { code: 'TRANSPORT_ERROR', reason: 'relay port rejected a message', cause: err },
        {
          closeChannel: true,
          closePort: true,
        },
      );
    }
  });

  const channelClosed = channel.onClose((event) => {
    if (!active) return;
    announceClose(event);
    finish(event, { closeChannel: false, closePort: true });
  });

  const onPortMessage = (event: { data: unknown }): void => {
    if (!active) return;
    try {
      channel.send(event.data as ClientToHostMessage);
    } catch (err) {
      // A closed channel reports its own close; anything else is logged.
      if (channel.state === 'open') logger.warn('relay could not forward to the channel', err);
    }
  };

  const onPortClose = (): void => {
    if (!active) return;
    finish(
      { code: 'LOCAL_CLOSE', reason: 'relay port closed' },
      { closeChannel: true, closePort: false },
    );
  };

  target.addEventListener('message', onPortMessage);
  // Node's MessagePort and recent browsers report a closed peer; older
  // browsers do not, and then only the channel side ends the relay.
  if (typeof closable.addEventListener === 'function') {
    closable.addEventListener('close', onPortClose);
  } else {
    closable.on?.('close', onPortClose);
  }
  target.start?.();

  function finish(
    event: ExplorerChannelCloseEvent,
    { closeChannel, closePort }: { closeChannel: boolean; closePort: boolean },
  ): void {
    if (!active) return;
    active = false;
    fromChannel.dispose();
    channelClosed.dispose();
    target.removeEventListener('message', onPortMessage);
    if (typeof closable.removeEventListener === 'function') {
      closable.removeEventListener('close', onPortClose);
    } else {
      closable.off?.('close', onPortClose);
    }
    if (closeChannel) channel.close(event.reason);
    if (closePort) {
      try {
        target.close?.();
      } catch {
        /* already closed */
      }
    }
    try {
      options.onClose?.(event);
    } catch (err) {
      logger.warn('relay onClose listener threw', err);
    }
  }

  return {
    dispose: () => {
      if (!active) return;
      const event: ExplorerChannelCloseEvent = { code: 'LOCAL_CLOSE', reason: 'relay disposed' };
      announceClose(event);
      finish(event, { closeChannel: true, closePort: true });
    },
  };
}
