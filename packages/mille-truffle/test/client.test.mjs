// connectMille + reconnect facade — remote-workspace PR 5 (SPEC §24.4).
//
// The backoff and retry policy are pure functions, so they are asserted
// directly rather than observed through a network — an unbounded loop or a
// terminal error being retried forever is exactly the kind of bug that hides
// behind "it worked when I tried it".
//
// The facade itself runs against a real serveMille over an in-process fake
// mesh, so the open handshake, session swap, stale snapshot and identity
// reset are all exercised end to end without a tailnet.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { PassThrough, Duplex } from 'node:stream';
import { EventEmitter } from 'node:events';

import {
  serveMille,
  connectMille,
  connectMilleChannel,
  backoffDelay,
  resolveReconnect,
  shouldRetry,
  DEFAULT_RECONNECT,
  RemoteExplorerError,
} from '../dist/index.js';
import { connectFileExplorerChannel } from '../../mille/dist/index.js';

// ─── pure policy (SPEC §18.4) ───────────────────────────────────────────

test('backoff grows geometrically and is clamped to maxDelayMs', () => {
  const r = resolveReconnect({ jitter: 0 });
  const seen = [0, 1, 2, 3, 4, 5, 6, 7, 8].map((n) => backoffDelay(n, r));
  assert.equal(seen[0], 500, 'first retry is minDelayMs');
  for (let i = 1; i < seen.length; i += 1) {
    assert.ok(seen[i] >= seen[i - 1], `attempt ${i} does not go backwards`);
    assert.ok(seen[i] <= r.maxDelayMs, `attempt ${i} respects the ceiling`);
  }
  assert.equal(seen.at(-1), r.maxDelayMs, 'saturates rather than growing forever');
});

test('jitter stays inside the configured bounds', () => {
  const r = resolveReconnect({ jitter: 1 });
  // Extreme random values must not escape the clamp in either direction.
  for (const rand of [() => 0, () => 1, () => 0.5]) {
    for (let attempt = 0; attempt < 10; attempt += 1) {
      const d = backoffDelay(attempt, r, rand);
      assert.ok(d >= r.minDelayMs, `attempt ${attempt} not below the floor (${d})`);
      assert.ok(d <= r.maxDelayMs, `attempt ${attempt} not above the ceiling (${d})`);
    }
  }
});

test('reconnect options are validated', () => {
  assert.equal(resolveReconnect(false), null, 'false disables reconnection');
  assert.deepEqual(resolveReconnect(), DEFAULT_RECONNECT);
  assert.throws(() => resolveReconnect({ minDelayMs: 0 }), RangeError);
  assert.throws(() => resolveReconnect({ minDelayMs: 900, maxDelayMs: 100 }), RangeError);
  assert.throws(() => resolveReconnect({ multiplier: 0.5 }), RangeError);
  assert.throws(() => resolveReconnect({ jitter: 2 }), RangeError);
});

test('terminal failures are not retried', () => {
  // Retrying these is worse than useless: the answer will not change, and a
  // client redialling a denial looks like a brute-force attempt in the log.
  for (const code of ['ACCESS_DENIED', 'PROTOCOL_MISMATCH', 'LIMIT_EXCEEDED']) {
    assert.equal(shouldRetry(code).retry, false, `${code} is terminal`);
  }
  for (const code of ['OFFLINE', 'TIMEOUT', 'TRANSPORT_ERROR', 'BACKPRESSURE']) {
    assert.equal(shouldRetry(code).retry, true, `${code} may be transient`);
  }
  assert.equal(shouldRetry('SERVER_SHUTTING_DOWN').retry, false, 'default is not to retry');
  assert.equal(
    shouldRetry('SERVER_SHUTTING_DOWN', { retryServerShutdown: true }).retry,
    true,
    'opt-in when the caller expects a restart',
  );
});

// ─── fake mesh wiring ───────────────────────────────────────────────────

function socketPair(peerId = 'nCLIENT01CNTRL') {
  const a2b = new PassThrough();
  const b2a = new PassThrough();
  const server = Duplex.from({ readable: a2b, writable: b2a });
  const client = Duplex.from({ readable: b2a, writable: a2b });
  for (const s of [server, client, a2b, b2a]) s.on('error', () => {});
  server.remotePeerId = peerId;
  server.remotePeerName = 'fake peer';
  server.remoteAddress = '100.64.0.9:1234';
  return { server, client };
}

class FakeMeshServer extends EventEmitter {
  #listener;
  port;
  constructor(listener) {
    super();
    this.#listener = listener;
  }
  listen(port) {
    this.port = port;
    queueMicrotask(() => this.emit('listening'));
    return this;
  }
  close(cb) {
    this.closed = true;
    if (cb) queueMicrotask(cb);
    return this;
  }
  accept(socket) {
    this.#listener(socket);
  }
}

/** One object playing both mesh roles, wired to itself in-process. */
function loopbackMesh() {
  const mesh = { net: {}, servers: [], sockets: [] };
  mesh.net.createServer = (listener) => {
    const s = new FakeMeshServer(listener);
    mesh.servers.push(s);
    return s;
  };
  mesh.net.connect = () => {
    const { server, client } = socketPair();
    mesh.sockets.push({ server, client });
    // `connect` returns synchronously and emits later, like TruffleSocket.
    queueMicrotask(() => {
      if (mesh.servers[0] && !mesh.servers[0].closed) mesh.servers[0].accept(server);
      client.emit('connect');
    });
    return client;
  };
  return mesh;
}

function tempRoot() {
  const dir = mkdtempSync(join(tmpdir(), 'mille-mtc-'));
  mkdirSync(join(dir, 'src'));
  writeFileSync(join(dir, 'src', 'a.ts'), 'export const a = 1;\n');
  return dir;
}

// ─── the facade ─────────────────────────────────────────────────────────

test('connectMille opens a workspace and exposes a live explorer', async () => {
  const dir = tempRoot();
  let server;
  let remote;
  try {
    const mesh = loopbackMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-write' } },
    });
    remote = await connectMille(mesh, {
      peer: 'fake',
      exportId: 'work',
      access: 'read-write',
      reconnect: false,
    });

    assert.equal(remote.state, 'online');
    assert.equal(remote.exportId, 'work');
    assert.match(remote.workspaceInstanceId, /^[0-9a-f-]{36}$/);

    const deadline = Date.now() + 5000;
    while (remote.explorer.getSnapshot().roots().length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 20));
    }
    assert.equal(remote.explorer.getSnapshot().roots().length, 1, 'the tree arrived');
  } finally {
    if (remote) await remote.close();
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('connectMilleChannel exposes only explorer frames for an Electron relay', async () => {
  const dir = tempRoot();
  let server;
  let opened;
  let explorer;
  try {
    const mesh = loopbackMesh();
    server = await serveMille(mesh, {
      heartbeatMs: 25,
      idleTimeoutMs: 1000,
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    opened = await connectMilleChannel(mesh, {
      peer: 'fake',
      exportId: 'work',
      access: 'read-only',
    });

    const leakedServiceFrames = [];
    const observer = opened.channel.onMessage((message) => {
      if (message?.service === 'mille.remote') leakedServiceFrames.push(message);
    });
    explorer = await connectFileExplorerChannel(opened.channel);

    const deadline = Date.now() + 5000;
    while (explorer.getSnapshot().roots().length === 0 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert.equal(explorer.getSnapshot().roots().length, 1, 'renderer handshake completed');
    const rootId = explorer.getSnapshot().roots()[0].id;
    await explorer.setExpanded({ add: [rootId], remove: [] });
    explorer.setViewport({ offset: 0, limit: 100, overscan: 8 });
    while (
      explorer
        .getSnapshot()
        .visibleRows({ offset: 0, limit: 100, expanded: new Set([rootId]) }).length < 2 &&
      Date.now() < deadline
    ) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert.ok(
      explorer
        .getSnapshot()
        .visibleRows({ offset: 0, limit: 100, expanded: new Set([rootId]) })
        .some((row) => row.name === 'src'),
      'lazy expansion crossed the raw channel',
    );
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.deepEqual(leakedServiceFrames, [], 'service heartbeats stayed below the relay');
    assert.equal(opened.accepted.limits.maxFileBytes, 16 * 1024 * 1024);
    observer.dispose();
  } finally {
    if (explorer) await explorer.dispose();
    else opened?.close('test cleanup');
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('connectMilleChannel aborts while waiting for the export handshake', async () => {
  const mesh = loopbackMesh();
  const controller = new AbortController();
  const opening = connectMilleChannel(mesh, {
    peer: 'fake',
    exportId: 'work',
    signal: controller.signal,
    openTimeoutMs: 10_000,
  });
  setTimeout(() => controller.abort(), 10);

  await assert.rejects(opening, (error) => {
    assert.ok(error instanceof RemoteExplorerError);
    assert.equal(error.code, 'OFFLINE');
    return true;
  });
  assert.equal(
    mesh.sockets.at(-1).client.writableEnded,
    true,
    'the aborted transport was gracefully ended',
  );
});

test('a denied open rejects connectMille and does not retry', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = loopbackMesh();
    let attempts = 0;
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      authorize: () => {
        attempts += 1;
        return false;
      },
    });

    await assert.rejects(
      connectMille(mesh, {
        peer: 'fake',
        exportId: 'work',
        // Reconnect enabled — a terminal denial must still not loop.
        reconnect: { minDelayMs: 10, maxDelayMs: 20 },
      }),
      (err) => {
        assert.ok(err instanceof RemoteExplorerError);
        assert.equal(err.code, 'ACCESS_DENIED');
        return true;
      },
    );

    await new Promise((r) => setTimeout(r, 200));
    assert.equal(attempts, 1, 'a denial was not retried');
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('an unknown export rejects with ACCESS_DENIED, not a retry loop', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = loopbackMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    await assert.rejects(
      connectMille(mesh, { peer: 'fake', exportId: 'ghost', reconnect: false }),
      (err) => err.code === 'ACCESS_DENIED',
    );
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('SPEC §18.3: a dropped connection leaves the last snapshot readable', async () => {
  const dir = tempRoot();
  let server;
  let remote;
  try {
    const mesh = loopbackMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    remote = await connectMille(mesh, { peer: 'fake', exportId: 'work', reconnect: false });

    const deadline = Date.now() + 5000;
    while (remote.explorer.getSnapshot().roots().length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 20));
    }
    const before = remote.getSnapshot().roots().length;
    assert.equal(before, 1);

    const events = [];
    remote.on('connection', (e) => events.push(e.state));

    // Kill the transport under the session.
    mesh.sockets.at(-1).server.destroy();
    const closedBy = Date.now() + 3000;
    while (remote.state === 'online' && Date.now() < closedBy) {
      await new Promise((r) => setTimeout(r, 20));
    }
    assert.notEqual(remote.state, 'online', 'the facade noticed');

    // The tree is still there. That is the whole point.
    assert.equal(remote.getSnapshot().roots().length, before, 'stale snapshot still readable');
    // But new work fails fast rather than hanging.
    assert.throws(() => remote.explorer, /not connected/);
  } finally {
    if (remote) await remote.close();
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('reconnect re-opens and keeps the same workspace instance', async () => {
  const dir = tempRoot();
  let server;
  let remote;
  try {
    const mesh = loopbackMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      hostIdleTimeoutMs: 30_000,
    });
    remote = await connectMille(mesh, {
      peer: 'fake',
      exportId: 'work',
      reconnect: { minDelayMs: 20, maxDelayMs: 60, jitter: 0 },
      openTimeoutMs: 500,
    });
    const first = remote.workspaceInstanceId;

    const identityResets = [];
    remote.on('identityReset', (e) => identityResets.push(e));

    mesh.sockets.at(-1).server.destroy();

    const backBy = Date.now() + 8000;
    while (remote.state !== 'online' && Date.now() < backBy) {
      await new Promise((r) => setTimeout(r, 25));
    }
    assert.equal(remote.state, 'online', 'came back up');
    assert.equal(remote.workspaceInstanceId, first, 'same host, same instance');
    assert.equal(identityResets.length, 0, 'no identity reset when the host survived');
  } finally {
    if (remote) await remote.close();
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('a replaced host emits identityReset so stale EntryIds are not trusted', async () => {
  const dir = tempRoot();
  let serverA;
  let serverB;
  let remote;
  try {
    const mesh = loopbackMesh();
    serverA = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      hostIdleTimeoutMs: 1,
    });
    remote = await connectMille(mesh, {
      peer: 'fake',
      exportId: 'work',
      reconnect: { minDelayMs: 30, maxDelayMs: 80, jitter: 0 },
      // There is a window below where the old server is gone and the
      // replacement is not up yet. A reconnect landing in it gets a socket
      // nobody accepts, and the default open deadline is 15 s (SPEC §13.5) —
      // long enough to outlast this test's wait on a loaded runner, which is
      // how it failed on Windows CI while passing locally. Fail an
      // unanswered attempt fast so backoff can try again.
      openTimeoutMs: 500,
    });
    const first = remote.workspaceInstanceId;

    const resets = [];
    remote.on('identityReset', (e) => resets.push(e));

    // Replace the server entirely: a new host means new EntryIds.
    await serverA.close();
    mesh.servers.length = 0;
    serverB = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });

    const backBy = Date.now() + 25_000;
    while (remote.state !== 'online' && Date.now() < backBy) {
      await new Promise((r) => setTimeout(r, 25));
    }
    assert.equal(remote.state, 'online', 'reconnected to the replacement');
    assert.notEqual(remote.workspaceInstanceId, first, 'a different instance');
    assert.equal(resets.length, 1, 'exactly one identityReset');
    assert.equal(resets[0].previousWorkspaceInstanceId, first);
    assert.equal(resets[0].workspaceInstanceId, remote.workspaceInstanceId);
  } finally {
    if (remote) await remote.close();
    if (serverB) await serverB.close();
    removeTempDir(dir);
  }
});

test('close() stops reconnection and is idempotent', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = loopbackMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    const remote = await connectMille(mesh, {
      peer: 'fake',
      exportId: 'work',
      reconnect: { minDelayMs: 20, maxDelayMs: 40, jitter: 0 },
    });

    await remote.close();
    assert.equal(remote.state, 'closed');

    const sessionsAfter = server.listSessions().length;
    mesh.sockets.at(-1).server.destroy();
    await new Promise((r) => setTimeout(r, 300));
    assert.equal(remote.state, 'closed', 'no reconnection after an explicit close');
    assert.ok(server.listSessions().length <= sessionsAfter, 'no new session was opened');

    await remote.close();
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});
