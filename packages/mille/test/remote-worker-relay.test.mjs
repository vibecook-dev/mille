// M7 — the remote client and its MessagePort relay run inside a Worker.
//
// The shape VibeField builds: a renderer Worker holds the WebSocket session
// (opened with a ticket) and hands an isolated consumer only a MessagePort,
// on which the consumer runs `connectFileExplorer()`. Here a worker_threads
// Worker stands in for the renderer Worker: it loads mille's `./remote` and
// `./port` entries under a resolve hook that refuses any Node builtin they
// might import, and relays a live session across a MessageChannel to this
// thread. The close paths are checked too, because a MessagePort cannot
// carry a close on its own.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MessageChannel, Worker } from 'node:worker_threads';

import { WebSocketServer, WebSocket as WsClient } from 'ws';

import { createFileExplorerHost } from '../dist/host.js';
import { connectFileExplorer } from '../dist/client-port.js';
import { serveWebSocketSession } from '../dist/node.js';
import { connectWebSocketChannel, relayExplorerChannel } from '../dist/remote-client.js';

const TICKET = 'tkt_worker_relay_0b1e';
const DIST_URL = new URL('../dist/', import.meta.url).href;
const HOOK_URL = new URL('./fixtures/renderer-world-hook.mjs', import.meta.url).href;
const WORKER_URL = new URL('./fixtures/remote-relay-worker.mjs', import.meta.url);

function seedTree() {
  const dir = mkdtempSync(join(tmpdir(), 'mille-relay-'));
  mkdirSync(join(dir, 'src'));
  writeFileSync(join(dir, 'src', 'module.rs'), '');
  writeFileSync(join(dir, 'README.md'), '# hello\n');
  return dir;
}

async function serve(host) {
  const wss = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(wss, 'listening');
  const sessions = [];
  wss.on('connection', (socket) =>
    sessions.push(
      serveWebSocketSession(socket, {
        authorize: ({ credential }) =>
          credential === TICKET
            ? { host, policy: { access: 'read-only', allowSearch: true } }
            : null,
      }),
    ),
  );
  return {
    url: `ws://127.0.0.1:${wss.address().port}`,
    sessions,
    async close() {
      for (const client of wss.clients) client.terminate();
      await new Promise((resolve) => wss.close(() => resolve()));
    },
  };
}

function nextWorkerMessage(worker, type) {
  return new Promise((resolve, reject) => {
    const onMessage = (message) => {
      if (message.type === 'failed') {
        worker.off('message', onMessage);
        reject(new Error(`worker failed: ${message.code} ${message.message}`));
        return;
      }
      if (message.type !== type) return;
      worker.off('message', onMessage);
      resolve(message);
    };
    worker.on('message', onMessage);
    worker.once('error', reject);
  });
}

/** Reject instead of hanging: the point of a close path is that nothing waits forever. */
function within(promise, ms, label) {
  let timer;
  return Promise.race([
    promise.finally(() => clearTimeout(timer)),
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} did not settle within ${ms}ms`)), ms);
    }),
  ]);
}

async function rowsAfterExpand(fx) {
  const deadline = Date.now() + 5000;
  while (fx.getSnapshot().roots().length === 0) {
    if (Date.now() > deadline) throw new Error('no roots');
    await new Promise((r) => setTimeout(r, 15));
  }
  const [root] = fx.getSnapshot().roots();
  await fx.setExpanded({ add: [root.id], remove: [] });
  fx.setViewport({ offset: 0, limit: 50, overscan: 8 });
  const expanded = new Set([root.id]);
  let rows = [];
  while (rows.length < 3) {
    if (Date.now() > deadline) throw new Error(`rows: ${rows.map((r) => r.name)}`);
    await new Promise((r) => setTimeout(r, 15));
    rows = fx.getSnapshot().visibleRows({ offset: 0, limit: 50, expanded });
  }
  return rows.map((r) => r.name);
}

const runtimes = [['ws package', true]];
if (typeof globalThis.WebSocket === 'function') runtimes.push(['global WebSocket', false]);

for (const [label, useWsPackage] of runtimes) {
  test(`a Worker (${label}) relays a ticketed session across a MessageChannel`, async () => {
    const dir = seedTree();
    const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
    const server = await serve(host);
    const worker = new Worker(WORKER_URL, {
      workerData: {
        url: server.url,
        ticket: TICKET,
        distUrl: DIST_URL,
        hookUrl: HOOK_URL,
        useWsPackage,
      },
    });
    let fx;
    try {
      const loaded = await nextWorkerMessage(worker, 'loaded');
      assert.equal(loaded.controlRefused, true, 'the hook refuses native.js, so it is live');

      const { port1, port2 } = new MessageChannel();
      const relaying = nextWorkerMessage(worker, 'relaying');
      worker.postMessage({ type: 'attach', port: port2 }, [port2]);
      assert.equal((await relaying).access, 'read-only');

      // This thread holds nothing but a MessagePort.
      fx = await connectFileExplorer(port1);
      const names = await rowsAfterExpand(fx);
      assert.ok(names.includes('src') && names.includes('README.md'), names.join(', '));
      const hits = await fx.search('README');
      assert.equal(hits[0].entry.name, 'README.md');
      // Search covers what the engine holds: on a roots-only host nobody has
      // opened src/, so its file is not found yet.
      assert.equal((await fx.search('module')).length, 0);

      // The upstream session ends: the port's client hears why, and later
      // calls fail at once instead of posting into a dead port.
      const closed = new Promise((resolve) =>
        fx.onConnection((event) => event.state === 'closed' && resolve(event)),
      );
      const relayClosed = nextWorkerMessage(worker, 'relay-closed');
      server.sessions[0].close('the host ended the session');
      const event = await within(closed, 3000, 'the close announcement');
      assert.equal(event.reason.code, 'REMOTE_CLOSE');
      assert.equal((await relayClosed).code, 'REMOTE_CLOSE');
      await within(assert.rejects(fx.call('getTreeVersion')), 1000, 'a call after the close');
    } finally {
      await fx?.dispose();
      await worker.terminate();
      await server.close();
      await host.dispose();
      removeTempDir(dir);
    }
  });
}

test('closing the port ends the upstream session; disposing the relay tells the port', async () => {
  const dir = seedTree();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  const server = await serve(host);
  try {
    // Port side goes away → the WebSocket session is closed for it.
    {
      const conn = await connectWebSocketChannel(server.url, {
        exportId: 'proj',
        credential: TICKET,
        WebSocket: WsClient,
      });
      const { port1, port2 } = new MessageChannel();
      relayExplorerChannel(conn.channel, port2);
      const fx = await connectFileExplorer(port1);
      await rowsAfterExpand(fx);
      port1.close();
      await within(server.sessions[0].closed, 3000, 'the upstream session');
    }
    // The relay is disposed → the port's client is told before the port closes.
    {
      const conn = await connectWebSocketChannel(server.url, {
        exportId: 'proj',
        credential: TICKET,
        WebSocket: WsClient,
      });
      const { port1, port2 } = new MessageChannel();
      let relayEvent;
      const relay = relayExplorerChannel(conn.channel, port2, {
        onClose: (event) => {
          relayEvent = event;
        },
      });
      const fx = await connectFileExplorer(port1);
      const closed = new Promise((resolve) =>
        fx.onConnection((event) => event.state === 'closed' && resolve(event)),
      );
      relay.dispose();
      const event = await within(closed, 3000, 'the dispose announcement');
      assert.equal(event.reason.code, 'LOCAL_CLOSE');
      assert.equal(relayEvent.reason, 'relay disposed');
      await within(server.sessions[1].closed, 3000, 'the upstream session');
      await fx.dispose();
    }
  } finally {
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});
