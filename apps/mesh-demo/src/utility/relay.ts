import type { ExplorerClientChannel } from '@vibecook/mille';
import type { MessagePortMain } from 'electron';

export interface RelayHandle {
  dispose(): void;
}

/** Relay semantic explorer frames without exposing the mesh socket to Chromium. */
export function relayExplorerChannel(
  channel: ExplorerClientChannel,
  port: MessagePortMain,
  onPortClosed: () => void,
): RelayHandle {
  let disposed = false;

  const fromChannel = channel.onMessage((message) => {
    if (!disposed) port.postMessage(message);
  });
  const channelClosed = channel.onClose(() => dispose());

  const fromPort = (event: Electron.MessageEvent): void => {
    if (disposed) return;
    try {
      channel.send(event.data as never);
    } catch {
      onPortClosed();
    }
  };
  const portClosed = (): void => {
    if (disposed) return;
    dispose(false);
    onPortClosed();
  };

  port.on('message', fromPort);
  port.once('close', portClosed);
  port.start();

  function dispose(closePort = true): void {
    if (disposed) return;
    disposed = true;
    fromChannel.dispose();
    channelClosed.dispose();
    port.off('message', fromPort);
    port.off('close', portClosed);
    if (closePort) port.close();
  }

  return { dispose };
}
