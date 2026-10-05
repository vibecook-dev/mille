// A `mille.remote` session over a WebSocket, against a real host.
//
// The ticket model VibeField builds on: a client presents a credential in
// its open request, the server's authorize hook checks it, and the session's
// policy comes from the grant — never from what the client asked for. No
// host session exists until authorize has said yes, and the credential never
// reaches a log.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { WebSocketServer, WebSocket as WsClient } from 'ws';

import { createFileExplorerHost } from '../dist/host.js';
import { connectFileExplorerChannel } from '../dist/client-port.js';
import { serveWebSocketSession, DEFAULT_REMOTE_MAX_FILE_BYTES } from '../dist/node.js';
import {
  connectWebSocketChannel,
  createWebSocketClientChannel,
  openRemoteChannel,
  RemoteOpenError,
} from '../dist/remote-client.js';

const TICKET = 'tkt_3q9ZK2mVd-SECRET-7f1c0e88b2a4';

function tempRoot() {
  const dir = mkdtempSync(join(tmpdir(), 'mille-remote-'));
  mkdirSync(join(dir, 'src'));
  writeFileSync(join(dir, 'src', 'index.ts'), 'export const a = 1;\n');
  writeFileSync(join(dir, 'README.md'), '# hello\n');
  return dir;
}

/** Every argument any logger received, for the never-logged assertion. */
function capturingLogger(sink) {
  const record =
    (level) =>
    (...args) =>
      sink.push({ level, args });
  return { info: record('info'), warn: record('warn'), debug: record('debug') };
}

function assertNeverLogged(sink, secret) {
  const text = JSON.stringify(sink, (_key, value) =>
    value instanceof Error ? { name: value.name, message: value.message } : value,
  );
  assert.ok(!text.includes(secret), `a log line carried the credential: ${text}`);
}

/** A loopback ws server that serves every connection as a mille session. */
async function startServer(sessionOptions) {
  const wss = new WebSocketServer({ host: '127.0.0.1', port: 0, maxPayload: 32 * 1024 * 1024 });
  await once(wss, 'listening');
  const sessions = [];
  wss.on('connection', (socket) => sessions.push(serveWebSocketSession(socket, sessionOptions)));
  return {
    url: `ws://127.0.0.1:${wss.address().port}`,
    sessions,
    async close() {
      for (const client of wss.clients) client.terminate();
      await new Promise((resolve) => wss.close(() => resolve()));
    },
  };
}

async function until(predicate, label, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}`);
    await new Promise((r) => setTimeout(r, 15));
  }
}

async function rootsOf(fx) {
  await until(() => fx.getSnapshot().roots().length > 0, 'roots');
  return fx.getSnapshot().roots();
}

test('a ticketed client opens an export over a WebSocket and browses it', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  const calls = [];
  const logs = [];
  const server = await startServer({
    logger: capturingLogger(logs),
    authorize: (ctx) => {
      calls.push(ctx);
      if (ctx.exportId !== 'proj' || ctx.credential !== TICKET) return null;
      return { host, policy: { access: 'read-write' }, label: 'Project' };
    },
  });
  let fx;
  try {
    const conn = await connectWebSocketChannel(server.url, {
      exportId: 'proj',
      credential: TICKET,
      access: 'read-write',
      WebSocket: WsClient,
    });
    assert.equal(calls.length, 1);
    assert.equal(calls[0].credential, TICKET, 'authorize reads the credential');
    assert.equal(calls[0].requestedAccess, 'read-write');
    assert.equal(conn.accepted.export.label, 'Project');
    assert.equal(conn.accepted.export.access, 'read-write');
    assert.equal(conn.accepted.export.rootCount, 1);
    assert.equal(conn.accepted.limits.maxFileBytes, DEFAULT_REMOTE_MAX_FILE_BYTES);

    fx = await connectFileExplorerChannel(conn.channel);
    assert.equal(host.sessionCount, 1);
    const [root] = await rootsOf(fx);
    await fx.setExpanded({ add: [root.id], remove: [] });
    fx.setViewport({ offset: 0, limit: 50, overscan: 8 });
    const expanded = new Set([root.id]);
    await until(
      () => fx.getSnapshot().visibleRows({ offset: 0, limit: 50, expanded }).length > 1,
      'expanded rows',
    );
    const names = fx
      .getSnapshot()
      .visibleRows({ offset: 0, limit: 50, expanded })
      .map((r) => r.name);
    assert.ok(names.includes('src') && names.includes('README.md'), names.join(', '));

    const session = server.sessions[0];
    assert.equal(session.state, 'open');
    assert.equal((await session.opened).exportId, 'proj');

    await fx.dispose();
    fx = undefined;
    await session.closed;
    await until(() => host.sessionCount === 0, 'the host session to detach');
    assertNeverLogged(logs, TICKET);
  } finally {
    await fx?.dispose();
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('the session runs under the grant: asking can narrow it, never widen it', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  let grant = 'read-only';
  const server = await startServer({
    authorize: () => ({ host, policy: { access: grant } }),
  });
  const opened = [];
  try {
    // A read-only grant refuses writes host-side.
    let conn = await connectWebSocketChannel(server.url, { exportId: 'proj', WebSocket: WsClient });
    let fx = await connectFileExplorerChannel(conn.channel);
    opened.push(fx);
    let [root] = await rootsOf(fx);
    await assert.rejects(fx.create(root.id, 'nope.txt', 0), (err) => err.code === 'EROFS');

    // A read-write grant, but the client asked for read-only: it gets read-only.
    grant = 'read-write';
    conn = await connectWebSocketChannel(server.url, {
      exportId: 'proj',
      access: 'read-only',
      WebSocket: WsClient,
    });
    assert.equal(conn.accepted.export.access, 'read-only');
    fx = await connectFileExplorerChannel(conn.channel);
    opened.push(fx);
    [root] = await rootsOf(fx);
    await assert.rejects(fx.create(root.id, 'nope.txt', 0), (err) => err.code === 'EROFS');

    // Asking for read-write on a read-only grant is refused outright.
    grant = 'read-only';
    await assert.rejects(
      connectWebSocketChannel(server.url, {
        exportId: 'proj',
        access: 'read-write',
        WebSocket: WsClient,
      }),
      (err) => err instanceof RemoteOpenError && err.code === 'ACCESS_DENIED',
    );
  } finally {
    for (const fx of opened) await fx.dispose();
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('a wrong credential is refused before any host session exists', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  let peakSessions = 0;
  const server = await startServer({
    authorize: ({ credential }) => {
      peakSessions = Math.max(peakSessions, host.sessionCount);
      return credential === TICKET ? { host, policy: { access: 'read-only' } } : null;
    },
  });
  try {
    const raw = new WsClient(server.url);
    const channel = createWebSocketClientChannel(raw);
    const closedWith = new Promise((resolve) => channel.onClose(resolve));
    await assert.rejects(
      openRemoteChannel(channel, { exportId: 'proj', credential: 'tkt_forged' }),
      (err) =>
        err instanceof RemoteOpenError &&
        err.code === 'ACCESS_DENIED' &&
        err.message === 'access denied',
    );
    await closedWith;
    const session = server.sessions[0];
    await assert.rejects(session.opened, (err) => err.code === 'ACCESS_DENIED');
    const event = await session.closed;
    assert.equal(event.code, 'AUTH_REJECTED', 'the server closed with 4008');
    assert.equal(host.sessionCount, 0);
    assert.equal(peakSessions, 0);
  } finally {
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('a throwing authorize is a denial, and the credential is scrubbed from every log', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  const logs = [];
  const server = await startServer({
    logger: capturingLogger(logs),
    // Disclosure on, so the scrubbed reason reaches the client too.
    diagnosticDisclosure: true,
    authorize: ({ credential }) => {
      throw new Error(`ticket ${credential} has expired`);
    },
  });
  try {
    const err = await connectWebSocketChannel(server.url, {
      exportId: 'proj',
      credential: TICKET,
      WebSocket: WsClient,
    }).catch((e) => e);
    assert.ok(err instanceof RemoteOpenError, String(err));
    assert.equal(err.code, 'ACCESS_DENIED');
    assert.match(err.message, /ticket \[credential\] has expired/);
    assert.ok(!err.message.includes(TICKET));
    await server.sessions[0].closed;
    assert.ok(
      logs.some((l) => l.args[0] === 'open_rejected'),
      'the refusal was logged',
    );
    assertNeverLogged(logs, TICKET);
    assert.equal(host.sessionCount, 0);
  } finally {
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('a refusal keeps its code: LIMIT_EXCEEDED reaches the client as itself', async () => {
  const server = await startServer({
    authorize: () => ({ refuse: 'LIMIT_EXCEEDED', reason: 'four sessions already' }),
  });
  try {
    await assert.rejects(
      connectWebSocketChannel(server.url, { exportId: 'proj', WebSocket: WsClient }),
      (err) => err.code === 'LIMIT_EXCEEDED' && err.message === 'session limit reached',
    );
  } finally {
    await server.close();
  }
});

test('heartbeats are answered and never reach the host', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  const server = await startServer({
    heartbeatMs: 30,
    idleTimeoutMs: 5000,
    authorize: () => ({ host, policy: { access: 'read-only' } }),
  });
  let fx;
  try {
    const raw = new WsClient(server.url);
    const channel = createWebSocketClientChannel(raw);
    let pings = 0;
    channel.onMessage((m) => {
      if (m?.service === 'mille.remote' && m.type === 'ping') pings += 1;
    });
    const conn = await openRemoteChannel(channel, { exportId: 'proj' });
    const errors = [];
    conn.channel.onMessage((m) => {
      if (m?.type === 'error') errors.push(m.body);
      assert.notEqual(m?.service, 'mille.remote', 'service frames are filtered');
    });
    fx = await connectFileExplorerChannel(conn.channel);
    // Stay idle long enough for several ping/pong rounds.
    await until(() => pings >= 4, 'four pings', 3000);
    // A pong that leaked into the host would come back as EINVAL here.
    assert.deepEqual(errors, []);
    assert.equal(server.sessions[0].state, 'open', 'pongs count as liveness');
    assert.equal(typeof (await fx.call('getTreeVersion')), 'number');
  } finally {
    await fx?.dispose();
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('a client that never opens is closed at the deadline and touches nothing', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  let authorized = 0;
  const server = await startServer({
    openTimeoutMs: 100,
    authorize: () => {
      authorized += 1;
      return { host, policy: { access: 'read-only' } };
    },
  });
  try {
    const raw = new WsClient(server.url);
    await once(raw, 'open');
    const [code] = await once(raw, 'close');
    assert.equal(code, 1000);
    await assert.rejects(server.sessions[0].opened, (err) => err.code === 'TIMEOUT');
    assert.equal(authorized, 0);
    assert.equal(host.sessionCount, 0);
  } finally {
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('an explorer frame ahead of the open closes the session with PROTOCOL_ERROR', async () => {
  let authorized = 0;
  const server = await startServer({
    authorize: () => {
      authorized += 1;
      return null;
    },
  });
  try {
    const channel = createWebSocketClientChannel(new WsClient(server.url));
    const closed = new Promise((resolve) => channel.onClose(resolve));
    channel.send({ v: 1, type: 'handshake', body: { version: 1, clientId: 'x', options: {} } });
    const event = await closed;
    assert.equal(event.code, 'PROTOCOL_ERROR');
    assert.equal(authorized, 0);
  } finally {
    await server.close();
  }
});

test('a client that stops answering is closed after the idle timeout', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  const server = await startServer({
    heartbeatMs: 30,
    idleTimeoutMs: 200,
    authorize: () => ({ host, policy: { access: 'read-only' } }),
  });
  try {
    const raw = new WsClient(server.url);
    await openRemoteChannel(createWebSocketClientChannel(raw), { exportId: 'proj' });
    raw.pause(); // reads nothing, so answers no ping
    const event = await server.sessions[0].closed;
    assert.equal(event.code, 'LOCAL_CLOSE');
    assert.equal(event.reason, 'idle timeout');
  } finally {
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('a malformed frame retires one WebSocket session, not the host or its neighbours', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  const server = await startServer({
    authorize: () => ({ host, policy: { access: 'read-only' } }),
  });
  let healthy;
  try {
    const good = await connectWebSocketChannel(server.url, {
      exportId: 'proj',
      WebSocket: WsClient,
    });
    healthy = await connectFileExplorerChannel(good.channel);

    const badRaw = new WsClient(server.url);
    const bad = await openRemoteChannel(createWebSocketClientChannel(badRaw), { exportId: 'proj' });
    const badFx = await connectFileExplorerChannel(bad.channel);
    assert.equal(host.sessionCount, 2);

    badRaw.send(new Uint8Array(64).fill(0x41));
    await server.sessions[1].closed;
    await until(() => host.sessionCount === 1, 'the bad session to detach');
    await assert.rejects(badFx.call('getTreeVersion'));
    assert.equal(typeof (await healthy.call('getTreeVersion')), 'number');
  } finally {
    await healthy?.dispose();
    await server.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('the file cap always fits in one frame, whatever the grant says', async () => {
  const dir = tempRoot();
  const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
  const wss = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(wss, 'listening');
  wss.on('connection', (socket) =>
    serveWebSocketSession(socket, {
      limits: { maxFrameBytes: 1024 * 1024 },
      authorize: () => ({ host, policy: { access: 'read-only', maxFileBytes: 2 ** 30 } }),
    }),
  );
  try {
    const conn = await connectWebSocketChannel(`ws://127.0.0.1:${wss.address().port}`, {
      exportId: 'proj',
      WebSocket: WsClient,
    });
    assert.equal(conn.accepted.limits.maxFrameBytes, 1024 * 1024);
    assert.ok(conn.accepted.limits.maxFileBytes < 1024 * 1024, 'capped below one frame');
    conn.close();
  } finally {
    for (const client of wss.clients) client.terminate();
    await new Promise((resolve) => wss.close(() => resolve()));
    await host.dispose();
    removeTempDir(dir);
  }
});
