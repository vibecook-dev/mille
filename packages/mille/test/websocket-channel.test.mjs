// ExplorerChannel over a WebSocket — one codec frame per binary message.
//
// Both adapters run against a real loopback `ws` server: the host side wraps
// the server's accepted socket, the client side wraps a standard client
// socket. Where the runtime has a global WebSocket (Node >= 22) the client
// tests run against it too, since that is the API a renderer Worker holds.

import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { once } from 'node:events';

import { WebSocketServer, WebSocket as WsClient } from 'ws';

import { createWebSocketHostChannel, WEBSOCKET_CLOSE, encodeFrame } from '../dist/node.js';
import {
  createWebSocketClientChannel,
  DEFAULT_LIMITS,
  explorerCloseCodeFor,
  webSocketCloseCodeFor,
} from '../dist/remote-client.js';
import { closeReasonForWire } from '../dist/websocket/close-codes.js';

/** A loopback server whose accepted sockets are handed out in order. */
async function startServer(options = {}) {
  const wss = new WebSocketServer({ host: '127.0.0.1', port: 0, ...options });
  await once(wss, 'listening');
  const accepted = [];
  const waiters = [];
  wss.on('connection', (socket) => {
    const waiter = waiters.shift();
    if (waiter) waiter(socket);
    else accepted.push(socket);
  });
  const { port } = wss.address();
  return {
    url: `ws://127.0.0.1:${port}`,
    nextSocket() {
      const ready = accepted.shift();
      if (ready) return Promise.resolve(ready);
      return new Promise((resolve) => waiters.push(resolve));
    },
    async close() {
      for (const client of wss.clients) client.terminate();
      await new Promise((resolve) => wss.close(() => resolve()));
    },
  };
}

/** One host channel and one client channel joined by a real loopback socket. */
async function channelPair(server, { hostOptions, clientOptions, Client = WsClient } = {}) {
  const raw = new Client(server.url);
  const client = createWebSocketClientChannel(raw, clientOptions);
  const socket = await server.nextSocket();
  const host = createWebSocketHostChannel(socket, hostOptions);
  return { host, client, raw, socket };
}

function nextMessage(channel) {
  return new Promise((resolve) => {
    const sub = channel.onMessage((message) => {
      sub.dispose();
      resolve(message);
    });
  });
}

function nextClose(channel) {
  return new Promise((resolve) => channel.onClose(resolve));
}

/** A raw `ws` client that speaks bytes, not frames — for malformed input. */
async function rawClient(server) {
  const raw = new WsClient(server.url);
  await once(raw, 'open');
  const socket = await server.nextSocket();
  return { raw, socket };
}

function rawCloseCode(raw) {
  return new Promise((resolve) => raw.once('close', (code) => resolve(code)));
}

const clientRuntimes = [['ws client', WsClient]];
if (typeof globalThis.WebSocket === 'function') {
  clientRuntimes.push(['global WebSocket', globalThis.WebSocket]);
}

for (const [label, Client] of clientRuntimes) {
  test(`${label}: messages and binary attachments cross both ways`, async () => {
    const server = await startServer();
    try {
      const { host, client } = await channelPair(server, { Client });
      const backing = new Uint8Array([9, 9, 1, 2, 3, 9, 9]);
      const toHost = nextMessage(host);
      client.send({
        v: 1,
        type: 'call',
        body: { reqId: 1, method: 'x', args: [backing.subarray(2, 5)] },
      });
      const got = await toHost;
      assert.equal(got.type, 'call');
      // SPEC §11.5: a view ships its own bytes, never its whole backing buffer.
      assert.deepEqual([...got.body.args[0]], [1, 2, 3]);

      const toClient = nextMessage(client);
      host.send({ v: 1, type: 'callResult', body: { reqId: 1, result: new Uint8Array([4, 5]) } });
      const reply = await toClient;
      assert.deepEqual([...reply.body.result], [4, 5]);
      client.close();
      await nextClose(host);
    } finally {
      await server.close();
    }
  });

  test(`${label}: frames sent while CONNECTING go out in order on open`, async () => {
    const server = await startServer();
    try {
      const raw = new Client(server.url);
      const client = createWebSocketClientChannel(raw);
      // Nothing is connected yet; these are held, not dropped or thrown.
      for (let i = 0; i < 20; i += 1) client.send({ v: 1, type: 'n', body: { i } });
      assert.ok(client.bufferedBytes > 0, 'held frames count as buffered');
      const host = createWebSocketHostChannel(await server.nextSocket());
      const seen = [];
      await new Promise((resolve) => {
        host.onMessage((m) => {
          seen.push(m.body.i);
          if (seen.length === 20) resolve();
        });
      });
      assert.deepEqual(
        seen,
        Array.from({ length: 20 }, (_, i) => i),
      );
      client.close();
    } finally {
      await server.close();
    }
  });
}

test('CH-001: order is preserved across 5,000 messages', async () => {
  const server = await startServer();
  try {
    const { host, client } = await channelPair(server);
    const seen = [];
    const done = new Promise((resolve) => {
      host.onMessage((m) => {
        seen.push(m.body.i);
        if (seen.length === 5000) resolve();
      });
    });
    for (let i = 0; i < 5000; i += 1) client.send({ v: 1, type: 'n', body: { i } });
    await done;
    for (let i = 0; i < 5000; i += 1) assert.equal(seen[i], i);
    client.close();
  } finally {
    await server.close();
  }
});

test('CH-003 / CH-005 / CH-006: close semantics hold over a WebSocket', async () => {
  const server = await startServer();
  try {
    const { host, client } = await channelPair(server);
    let closes = 0;
    host.onClose(() => {
      closes += 1;
    });
    let delivered = 0;
    host.onMessage(() => {
      delivered += 1;
    });
    host.close('done');
    host.close('again');
    assert.equal(host.state, 'closed');
    assert.throws(() => host.send({ v: 1, type: 'x', body: {} }), /closed/);
    await nextClose(client);
    assert.equal(closes, 1);
    assert.equal(delivered, 0);
    // A late subscriber still learns the terminal state.
    const late = await nextClose(host);
    assert.equal(late.code, 'LOCAL_CLOSE');
  } finally {
    await server.close();
  }
});

test('close codes cross the wire: an orderly close is REMOTE_CLOSE on the peer', async () => {
  const server = await startServer();
  try {
    const { host, client } = await channelPair(server);
    const atClient = nextClose(client);
    host.close('bye');
    const event = await atClient;
    assert.equal(event.code, 'REMOTE_CLOSE');
    assert.equal(event.reason, 'bye');
    assert.equal(event.cause.webSocketCloseCode, WEBSOCKET_CLOSE.NORMAL);
  } finally {
    await server.close();
  }
});

test('closeAs(AUTH_REJECTED) reaches the client as AUTH_REJECTED (4008)', async () => {
  const server = await startServer();
  try {
    const { host, client } = await channelPair(server);
    const atClient = nextClose(client);
    host.closeAs('AUTH_REJECTED', 'access denied');
    const event = await atClient;
    assert.equal(event.code, 'AUTH_REJECTED');
    assert.equal(event.cause.webSocketCloseCode, 4008);
  } finally {
    await server.close();
  }
});

test('a dropped connection is TRANSPORT_ERROR on the client', async () => {
  const server = await startServer();
  try {
    const { client, socket } = await channelPair(server);
    // Let the client observe the open before the drop.
    await new Promise((resolve) => setTimeout(resolve, 20));
    const atClient = nextClose(client);
    socket.terminate();
    const event = await atClient;
    assert.equal(event.code, 'TRANSPORT_ERROR');
  } finally {
    await server.close();
  }
});

test('a malformed binary message closes the session with PROTOCOL_ERROR (4002)', async () => {
  const server = await startServer();
  try {
    const { raw, socket } = await rawClient(server);
    const host = createWebSocketHostChannel(socket);
    const hostClosed = nextClose(host);
    const code = rawCloseCode(raw);
    raw.send(new Uint8Array(64).fill(0x41));
    const event = await hostClosed;
    assert.equal(event.code, 'PROTOCOL_ERROR');
    assert.match(event.reason, /magic/);
    assert.equal(await code, WEBSOCKET_CLOSE.PROTOCOL_ERROR);
  } finally {
    await server.close();
  }
});

test('a text message closes with PROTOCOL_ERROR (4003, unsupported data)', async () => {
  const server = await startServer();
  try {
    const { raw, socket } = await rawClient(server);
    const host = createWebSocketHostChannel(socket);
    const hostClosed = nextClose(host);
    const code = rawCloseCode(raw);
    raw.send('{"v":1,"type":"handshake","body":{}}');
    assert.equal((await hostClosed).code, 'PROTOCOL_ERROR');
    assert.equal(await code, WEBSOCKET_CLOSE.UNSUPPORTED_DATA);
  } finally {
    await server.close();
  }
});

test('two frames in one message are refused: one frame per message', async () => {
  const server = await startServer();
  try {
    const { raw, socket } = await rawClient(server);
    const host = createWebSocketHostChannel(socket);
    let delivered = 0;
    host.onMessage(() => {
      delivered += 1;
    });
    const hostClosed = nextClose(host);
    const a = encodeFrame({ v: 1, type: 'a', body: {} }, DEFAULT_LIMITS);
    const b = encodeFrame({ v: 1, type: 'b', body: {} }, DEFAULT_LIMITS);
    const joined = new Uint8Array(a.byteLength + b.byteLength);
    joined.set(a, 0);
    joined.set(b, a.byteLength);
    raw.send(joined);
    const event = await hostClosed;
    assert.equal(event.code, 'PROTOCOL_ERROR');
    assert.match(event.reason, /declares/);
    assert.equal(delivered, 0, 'neither half is delivered');
  } finally {
    await server.close();
  }
});

test('a message over the frame limit closes with 4009 before it is decoded', async () => {
  const server = await startServer();
  try {
    const { raw, socket } = await rawClient(server);
    const host = createWebSocketHostChannel(socket, { maxFrameBytes: 4096 });
    const hostClosed = nextClose(host);
    const code = rawCloseCode(raw);
    raw.send(new Uint8Array(8192));
    const event = await hostClosed;
    assert.equal(event.code, 'PROTOCOL_ERROR');
    assert.match(event.reason, /exceeds limit 4096/);
    assert.equal(await code, WEBSOCKET_CLOSE.MESSAGE_TOO_BIG);
  } finally {
    await server.close();
  }
});

test("the server's maxPayload refuses an oversized message before allocating it", async () => {
  const server = await startServer({ maxPayload: 4096 });
  try {
    const { raw, socket } = await rawClient(server);
    const host = createWebSocketHostChannel(socket);
    const hostClosed = nextClose(host);
    const code = rawCloseCode(raw);
    raw.send(new Uint8Array(8192));
    assert.equal((await hostClosed).code, 'PROTOCOL_ERROR');
    // `ws` itself closes with the standard 1009; the client maps it the same way.
    assert.equal(await code, 1009);
    assert.equal(explorerCloseCodeFor(1009), 'PROTOCOL_ERROR');
  } finally {
    await server.close();
  }
});

test('the outbound hard limit closes with BACKPRESSURE and throws; the peer reads 4013', async () => {
  const server = await startServer();
  try {
    const { raw, socket } = await rawClient(server);
    const host = createWebSocketHostChannel(socket, {
      outboundSoftBytes: 1024 * 1024,
      outboundHardBytes: 4 * 1024 * 1024,
    });
    // A peer that stops reading: the kernel buffers fill, then ours does.
    raw.pause();
    const chunk = new Uint8Array(256 * 1024);
    let threw = null;
    for (let i = 0; i < 2000 && threw === null; i += 1) {
      try {
        host.send({ v: 1, type: 'blob', body: { chunk } });
      } catch (err) {
        threw = err;
      }
      if (i % 16 === 15) await new Promise((resolve) => setImmediate(resolve));
    }
    assert.ok(threw, 'a send past the hard limit throws');
    assert.match(threw.message, /hard limit|exceed/);
    assert.equal(host.state, 'closed');
    const code = rawCloseCode(raw);
    raw.resume();
    assert.equal(await code, WEBSOCKET_CLOSE.BACKPRESSURE);
  } finally {
    await server.close();
  }
});

test('drain() waits for the socket buffer and resolves once it has flushed', async () => {
  const server = await startServer();
  try {
    const { raw, socket } = await rawClient(server);
    const host = createWebSocketHostChannel(socket);
    raw.pause();
    const chunk = new Uint8Array(512 * 1024);
    // Enough to overflow the loopback kernel buffers so bytes stay with us.
    for (let i = 0; i < 24; i += 1) host.send({ v: 1, type: 'blob', body: { chunk } });
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.ok(host.bufferedBytes > 0, 'a paused peer leaves bytes buffered');
    let drained = false;
    const drainPromise = host.drain().then(() => {
      drained = true;
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.equal(drained, false, 'drain waits while the peer is not reading');
    raw.resume();
    await drainPromise;
    assert.equal(host.bufferedBytes, 0);
    host.close();
  } finally {
    await server.close();
  }
});

test('a channel cannot be built on a socket that is already closed', async () => {
  const server = await startServer();
  try {
    const { raw } = await rawClient(server);
    const closed = once(raw, 'close');
    raw.close();
    await closed;
    assert.throws(() => createWebSocketClientChannel(raw), /closing or closed/);
  } finally {
    await server.close();
  }
});

test('close codes map both ways and close reasons fit a control frame', () => {
  for (const code of [
    'LOCAL_CLOSE',
    'PROTOCOL_ERROR',
    'AUTH_REJECTED',
    'BACKPRESSURE',
    'TRANSPORT_ERROR',
  ]) {
    const wire = webSocketCloseCodeFor(code);
    // Browsers only accept 1000 and 3000-4999 from close(); the table must too.
    assert.ok(wire === 1000 || (wire >= 3000 && wire <= 4999), `${code} -> ${wire}`);
    const back = explorerCloseCodeFor(wire);
    assert.equal(back, code === 'LOCAL_CLOSE' ? 'REMOTE_CLOSE' : code);
  }
  assert.equal(explorerCloseCodeFor(1006), 'TRANSPORT_ERROR');
  assert.equal(explorerCloseCodeFor(1001), 'REMOTE_CLOSE');
  assert.equal(explorerCloseCodeFor(4999), 'REMOTE_CLOSE');

  const long = 'é'.repeat(100); // 200 UTF-8 bytes
  const trimmed = closeReasonForWire(long);
  assert.ok(new TextEncoder().encode(trimmed).byteLength <= 123);
  assert.equal(trimmed, 'é'.repeat(61));
  assert.equal(closeReasonForWire('short'), 'short');
});
