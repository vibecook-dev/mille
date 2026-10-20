// A non-admin session's viewport is capped (policy `maxViewportRows`).
//
// A viewport ships one full entry record per row in a single delta, built on
// the host's thread. Without a cap, a remote client could ask for every row
// of a huge tree in one request — a long stall for every session and, past
// the frame limit, a delta that cannot be sent at all. These tests check the
// request is clamped rather than refused or fatal, that the client is told,
// that admin sessions are untouched, and that the cap is advertised.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MessageChannel } from 'node:worker_threads';

import { WebSocketServer, WebSocket as WsClient } from 'ws';

import { createFileExplorerHost } from '../dist/host.js';
import { connectFileExplorer, connectFileExplorerChannel } from '../dist/client-port.js';
import { createMessagePortHostChannel } from '../dist/channel/message-port.js';
import { DEFAULT_REMOTE_MAX_VIEWPORT_ROWS, serveWebSocketSession } from '../dist/node.js';
import { connectWebSocketChannel } from '../dist/remote-client.js';

const FILES = 3000;

let fixtureDir;
let host;

test.before(async () => {
  fixtureDir = mkdtempSync(join(tmpdir(), 'mille-viewport-cap-'));
  const root = join(fixtureDir, 'tree');
  mkdirSync(root);
  for (let i = 0; i < FILES; i += 1) {
    writeFileSync(join(root, `f-${String(i).padStart(5, '0')}.txt`), '');
  }
  host = await createFileExplorerHost({ roots: [root], watchDebounceMs: 60_000 });
  await host.local.populateFromRoots();
});

test.after(async () => {
  await host?.dispose();
  if (fixtureDir) removeTempDir(fixtureDir);
});

/** A port client under `context` (admin when omitted), collecting warnings. */
async function attach(context, clientOptions = {}) {
  const { port1, port2 } = new MessageChannel();
  if (context === undefined) host.attachPort(port1);
  else host.attachChannel(createMessagePortHostChannel(port1), context);
  const fx = await connectFileExplorer(port2, { mirrorCap: 10_000, ...clientOptions });
  const warnings = [];
  fx.on('warning', (payload) => warnings.push(payload));
  return {
    fx,
    warnings,
    close: async () => {
      await fx.dispose();
      port2.close();
    },
  };
}

async function until(predicate, label, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}`);
    await new Promise((r) => setTimeout(r, 10));
  }
}

async function expandRoot(fx) {
  await until(() => fx.getSnapshot().roots().length > 0, 'roots');
  const [root] = fx.getSnapshot().roots();
  await fx.setExpanded({ add: [root.id], remove: [] });
  await until(() => fx.getSnapshot().directChildCount(root.id) === FILES, 'child ids');
  return new Set([root.id]);
}

function hydratedCount(fx, expanded, limit) {
  return fx
    .getSnapshot()
    .visibleRows({ offset: 0, limit, expanded })
    .filter((row) => row.pending !== true).length;
}

const clampWarnings = (warnings) =>
  warnings.filter((w) => w.code === 'VIEWPORT_CLAMPED').map((w) => JSON.parse(w.detail));

test('a remote session asking for an oversized viewport is clamped, told, and keeps working', async () => {
  const s = await attach({ kind: 'remote', policy: { access: 'read-only' } });
  try {
    const expanded = await expandRoot(s.fx);
    s.fx.setViewport({ offset: 0, limit: FILES + 1, overscan: 500 });
    await until(() => clampWarnings(s.warnings).length > 0, 'the clamp warning');
    const [clamp] = clampWarnings(s.warnings);
    assert.equal(clamp.maxViewportRows, DEFAULT_REMOTE_MAX_VIEWPORT_ROWS);
    assert.deepEqual(clamp.requested, { offset: 0, limit: FILES + 1, overscan: 500 });
    assert.deepEqual(clamp.granted, {
      offset: 0,
      limit: DEFAULT_REMOTE_MAX_VIEWPORT_ROWS,
      overscan: 0,
    });

    await until(
      () => hydratedCount(s.fx, expanded, FILES + 1) >= DEFAULT_REMOTE_MAX_VIEWPORT_ROWS,
      'the granted rows',
    );
    // Rows past the cap were never shipped.
    await new Promise((r) => setTimeout(r, 100));
    assert.equal(hydratedCount(s.fx, expanded, FILES + 1), DEFAULT_REMOTE_MAX_VIEWPORT_ROWS);
    // The session is alive and answering.
    assert.equal(typeof (await s.fx.call('getTreeVersion')), 'number');
  } finally {
    await s.close();
  }
});

test('an explicit cap bounds limit plus overscan on both sides', async () => {
  const s = await attach({ kind: 'remote', policy: { access: 'read-only', maxViewportRows: 100 } });
  try {
    await expandRoot(s.fx);
    s.fx.setViewport({ offset: 10, limit: 200, overscan: 0 });
    s.fx.setViewport({ offset: 10, limit: 80, overscan: 50 });
    // Within the cap: no warning for this one.
    s.fx.setViewport({ offset: 10, limit: 60, overscan: 20 });
    await until(() => clampWarnings(s.warnings).length >= 2, 'two clamp warnings');
    await new Promise((r) => setTimeout(r, 50));
    const clamps = clampWarnings(s.warnings);
    assert.equal(clamps.length, 2);
    assert.deepEqual(clamps[0].granted, { offset: 10, limit: 100, overscan: 0 });
    assert.deepEqual(clamps[1].granted, { offset: 10, limit: 80, overscan: 10 });
  } finally {
    await s.close();
  }
});

test('admin sessions are not capped', async () => {
  const s = await attach(undefined);
  try {
    const expanded = await expandRoot(s.fx);
    s.fx.setViewport({ offset: 0, limit: FILES + 1, overscan: 0 });
    await until(() => hydratedCount(s.fx, expanded, FILES + 1) === FILES + 1, 'every row');
    assert.deepEqual(clampWarnings(s.warnings), []);
  } finally {
    await s.close();
  }
});

test("the handshake's prefetchRows answers to the same cap", async () => {
  const s = await attach(
    { kind: 'remote', policy: { access: 'read-only', maxViewportRows: 50 } },
    { prefetchRows: 100_000 },
  );
  try {
    // A headless client expands before it sets a viewport: the prefetch is the window.
    const expanded = await expandRoot(s.fx);
    await until(() => clampWarnings(s.warnings).length > 0, 'the prefetch clamp warning');
    assert.deepEqual(clampWarnings(s.warnings)[0].granted, { prefetchRows: 50 });
    await until(() => hydratedCount(s.fx, expanded, 200) >= 50, 'the prefetched rows');
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(hydratedCount(s.fx, expanded, 200), 50);
  } finally {
    await s.close();
  }
});

async function wsServer(sessionOptions) {
  const wss = new WebSocketServer({ host: '127.0.0.1', port: 0 });
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

test('a WebSocket session advertises its cap in accepted.limits', async () => {
  let policy = { access: 'read-only' };
  const server = await wsServer({ authorize: () => ({ host, policy }) });
  try {
    let conn = await connectWebSocketChannel(server.url, { exportId: 'x', WebSocket: WsClient });
    assert.equal(conn.accepted.limits.maxViewportRows, DEFAULT_REMOTE_MAX_VIEWPORT_ROWS);
    conn.close();

    // A narrowed read-write grant keeps the cap it named.
    policy = { access: 'read-write', maxViewportRows: 500 };
    conn = await connectWebSocketChannel(server.url, {
      exportId: 'x',
      access: 'read-only',
      WebSocket: WsClient,
    });
    assert.equal(conn.accepted.export.access, 'read-only');
    assert.equal(conn.accepted.limits.maxViewportRows, 500);
    conn.close();
  } finally {
    await server.close();
  }
});

test('a delta that still cannot be framed closes the session with its reason', async () => {
  // A grant whose cap admits the whole folder on a channel whose frame cannot
  // carry it: the last-resort path, which must say why it ended.
  const server = await wsServer({
    limits: { maxFrameBytes: 64 * 1024 },
    authorize: () => ({ host, policy: { access: 'read-only', maxViewportRows: 100_000 } }),
  });
  try {
    const conn = await connectWebSocketChannel(server.url, { exportId: 'x', WebSocket: WsClient });
    const fx = await connectFileExplorerChannel(conn.channel, { mirrorCap: 10_000 });
    const closed = new Promise((resolve) =>
      fx.onConnection((event) => event.state === 'closed' && resolve(event)),
    );
    await expandRoot(fx);
    fx.setViewport({ offset: 0, limit: FILES + 1, overscan: 0 });
    const event = await closed;
    assert.match(event.reason.reason ?? '', /could not send delta/);
    const serverEvent = await server.sessions[0].closed;
    assert.equal(serverEvent.code, 'LOCAL_CLOSE');
    await fx.dispose();
  } finally {
    await server.close();
  }
});
