// A stand-in for a renderer Worker: it loads only mille's renderer-facing
// entries (`./remote`, `./port`) under a hook that refuses Node builtins,
// dials a WebSocket with a ticket, and relays the session into a
// MessagePort it was handed. The worker scaffolding itself (parentPort,
// module.register) is Node's; nothing mille provides here touches it.

import { register } from 'node:module';
import { parentPort, workerData } from 'node:worker_threads';

register(workerData.hookUrl, { data: { guarded: [workerData.distUrl] } });

// Control: the module that loads the native addon must now be refused, or
// the hook is not doing anything and the rest of this proves nothing.
let controlRefused = false;
try {
  await import(new URL('native.js', workerData.distUrl).href);
} catch (err) {
  controlRefused = /renderer-world module .* imported /.test(String(err?.message));
}

const remote = await import(new URL('remote-client.js', workerData.distUrl).href);
await import(new URL('client-port.js', workerData.distUrl).href);

let WebSocketCtor = globalThis.WebSocket;
if (workerData.useWsPackage || typeof WebSocketCtor !== 'function') {
  WebSocketCtor = (await import('ws')).WebSocket;
}

let relay = null;
parentPort.postMessage({ type: 'loaded', controlRefused });

parentPort.on('message', async (message) => {
  if (message.type === 'attach') {
    try {
      const conn = await remote.connectWebSocketChannel(workerData.url, {
        exportId: 'proj',
        credential: workerData.ticket,
        WebSocket: WebSocketCtor,
      });
      relay = remote.relayExplorerChannel(conn.channel, message.port, {
        transfer: true,
        onClose: (event) => parentPort.postMessage({ type: 'relay-closed', code: event.code }),
      });
      parentPort.postMessage({ type: 'relaying', access: conn.accepted.export.access });
    } catch (err) {
      parentPort.postMessage({ type: 'failed', code: err?.code, message: String(err?.message) });
    }
  } else if (message.type === 'dispose') {
    relay?.dispose();
  }
});
