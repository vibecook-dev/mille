// serveMille — remote-workspace PR 4 (SPEC §24.4, §24.5).
//
// Driven by a fake MeshNode that hands out Duplex pairs, so the whole
// listener, handshake, authorization and host-cache path is exercised with
// no tailnet and no native Truffle addon. The real `mesh.net` was verified
// separately against a live tailnet; what needs repeatable coverage is the
// decision logic, especially the denials.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, mkdirSync, writeFileSync, chmodSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { PassThrough, Duplex } from 'node:stream';
import { EventEmitter } from 'node:events';

import { serveMille, ExportConfigError, resolveExport } from '../dist/index.js';
import {
  createFramedStreamClientChannel,
} from '../../mille/dist/node.js';

// ─── fake mesh ──────────────────────────────────────────────────────────

/** A Duplex pair plus the identity the server should observe on accept. */
function socketPair({ peerId = 'nPEER0001CNTRL', peerName = 'test peer' } = {}) {
  const a2b = new PassThrough();
  const b2a = new PassThrough();
  const server = Duplex.from({ readable: a2b, writable: b2a });
  const client = Duplex.from({ readable: b2a, writable: a2b });
  for (const s of [server, client, a2b, b2a]) s.on('error', () => {});
  // Mirrors TruffleSocket: identity readable synchronously at accept time.
  if (peerId !== null) server.remotePeerId = peerId;
  if (peerName !== null) server.remotePeerName = peerName;
  server.remoteAddress = '100.64.0.1:40000';
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
  /** Test hook: deliver an inbound connection. */
  accept(socket) {
    this.#listener(socket);
  }
}

function fakeMesh() {
  const mesh = { net: {}, servers: [] };
  mesh.net.createServer = (listener) => {
    const s = new FakeMeshServer(listener);
    mesh.servers.push(s);
    return s;
  };
  return mesh;
}

function tempRoot(name = 'mille-mt-') {
  const dir = mkdtempSync(join(tmpdir(), name));
  mkdirSync(join(dir, 'src'));
  writeFileSync(join(dir, 'src', 'a.ts'), 'export const a = 1;\n');
  writeFileSync(join(dir, 'README.md'), '# export\n');
  return dir;
}

const openRequest = (overrides = {}) => ({
  service: 'mille.remote',
  version: 1,
  type: 'open',
  requestId: 'req-1',
  exportId: 'work',
  requestedAccess: 'read-only',
  client: {
    instanceId: 'client-1',
    name: 'tester',
    milleVersion: '0.3.0',
    milleTruffleVersion: '0.1.0',
  },
  ...overrides,
});

/** Open a connection and return the first service reply. */
async function openAndAwaitReply(mesh, socketOpts, request = openRequest()) {
  const { server, client } = socketPair(socketOpts);
  const channel = createFramedStreamClientChannel(client);
  const replies = [];
  channel.onMessage((m) => replies.push(m));
  mesh.servers[0].accept(server);
  channel.send(request);

  const deadline = Date.now() + 5000;
  while (replies.length === 0 && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 10));
  }
  return { reply: replies[0], channel, replies };
}

// ─── export validation (SPEC §15.1) ─────────────────────────────────────

test('export validation rejects malformed configuration', () => {
  const dir = tempRoot();
  try {
    const base = { label: 'Work', roots: [dir], access: 'read-only' };
    assert.throws(() => resolveExport('bad id!', base), ExportConfigError, 'export id charset');
    assert.throws(
      () => resolveExport('work', { ...base, roots: ['relative/path'] }),
      /must be absolute/,
    );
    assert.throws(
      () => resolveExport('work', { ...base, roots: [join(dir, 'does-not-exist')] }),
      /cannot be resolved/,
    );
    assert.throws(() => resolveExport('work', { ...base, roots: [dir, dir] }), /duplicate/);
    assert.throws(
      () => resolveExport('work', { ...base, followSymlinks: true }),
      /followSymlinks must be false/,
      'SEC-003',
    );
    assert.throws(() => resolveExport('work', { ...base, access: 'rw' }), /access must be/);
    // A valid one resolves and gets a fingerprint.
    const ok = resolveExport('work', base);
    assert.equal(ok.id, 'work');
    assert.equal(ok.roots.length, 1);
    assert.match(ok.fingerprint, /^[0-9a-f]{32}$/);
  } finally {
    removeTempDir(dir);
  }
});

test('the host-cache fingerprint tracks roots and options, not identity', () => {
  const a = tempRoot();
  const b = tempRoot();
  try {
    const one = resolveExport('w', { label: 'W', roots: [a], access: 'read-only' });
    const same = resolveExport('w', {
      label: 'different label',
      roots: [a],
      access: 'read-only',
      allowedPeerIds: ['nSOMEONE'],
    });
    const other = resolveExport('w', { label: 'W', roots: [b], access: 'read-only' });
    assert.equal(one.fingerprint, same.fingerprint, 'label and allow-list are not engine inputs');
    assert.notEqual(one.fingerprint, other.fingerprint, 'different roots, different engine');
  } finally {
    removeTempDir(a);
    removeTempDir(b);
  }
});

// ─── the open handshake ─────────────────────────────────────────────────

test('an authorized peer is accepted and gets a workspace instance', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-write' } },
    });
    assert.equal(server.port, 9451, 'defaults to 9451');

    const { reply } = await openAndAwaitReply(mesh);
    assert.equal(reply?.type, 'accepted', JSON.stringify(reply));
    assert.equal(reply.export.id, 'work');
    assert.equal(reply.export.rootCount, 1);
    assert.equal(reply.export.access, 'read-only', 'granted the access requested, not more');
    assert.match(reply.workspaceInstanceId, /^[0-9a-f-]{36}$/);
    assert.equal(reply.limits.maxFileBytes, 16 * 1024 * 1024);
    assert.equal(server.listSessions().length, 1);
    assert.equal(server.hostCount, 1);
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('a served export actually populates — the tree is not empty', async () => {
  // Regression: serveMille created the host with mille's default
  // `initialWalk: 'full'`, which is a no-op meaning "the consumer calls
  // populateFromRoots itself". An in-process embedder does; a remote peer
  // cannot, so every served workspace sat empty forever. Caught only by
  // driving a real client — the fake mesh never looked at the tree.
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    const { reply, channel } = await openAndAwaitReply(mesh);
    assert.equal(reply.type, 'accepted');

    const { connectFileExplorerChannel } = await import('../../mille/dist/client-port.js');
    const client = await connectFileExplorerChannel(channel);
    try {
      const deadline = Date.now() + 5000;
      while (client.getSnapshot().roots().length === 0 && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 20));
      }
      assert.equal(client.getSnapshot().roots().length, 1, 'the served root reached the client');
    } finally {
      await client.dispose();
    }
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('an export may still override the walk policy', async () => {
  const dir = tempRoot();
  try {
    const ex = resolveExport('w', {
      label: 'W',
      roots: [dir],
      access: 'read-only',
      explorer: { initialWalk: 'none' },
    });
    assert.equal(ex.explorer.initialWalk, 'none', 'the default does not clobber an explicit choice');
  } finally {
    removeTempDir(dir);
  }
});

test('SEC-006: an unknown export and a forbidden one are indistinguishable', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: {
        work: { label: 'Work', roots: [dir], access: 'read-only', allowedPeerIds: ['nALLOWED'] },
      },
    });

    const missing = await openAndAwaitReply(mesh, {}, openRequest({ exportId: 'nosuch' }));
    const forbidden = await openAndAwaitReply(mesh, { peerId: 'nDENIED01CNTRL' });

    assert.equal(missing.reply.type, 'rejected');
    assert.equal(forbidden.reply.type, 'rejected');
    assert.equal(missing.reply.code, forbidden.reply.code, 'same code');
    assert.equal(missing.reply.message, forbidden.reply.message, 'same message');
    assert.equal(missing.reply.code, 'ACCESS_DENIED');
    assert.equal(server.hostCount, 0, 'no host was created for either');
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('no host is created before authorization passes', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    let authorizeCalls = 0;
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      authorize: () => {
        authorizeCalls += 1;
        return false;
      },
    });

    const { reply } = await openAndAwaitReply(mesh);
    assert.equal(reply.type, 'rejected');
    assert.equal(authorizeCalls, 1, 'the callback ran');
    // SEC-001 — the ordering is the security property.
    assert.equal(server.hostCount, 0, 'a denied peer never caused an engine to exist');
    assert.equal(server.listSessions().length, 0);
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('an authorize callback that throws is a denial', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      authorize: () => {
        throw new Error('policy service unreachable');
      },
    });
    const { reply } = await openAndAwaitReply(mesh);
    assert.equal(reply.type, 'rejected', 'a throwing policy must not fail open');
    assert.equal(server.hostCount, 0);
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('a socket with no verified peer id is refused', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    // Truffle documents remotePeerId as nullable and says never to gate on
    // it — absence must not read as permission.
    const { reply } = await openAndAwaitReply(mesh, { peerId: null, peerName: null });
    assert.equal(reply.type, 'rejected');
    assert.equal(server.hostCount, 0);
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('read-write on a read-only export is refused, not downgraded', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    const { reply } = await openAndAwaitReply(
      mesh,
      {},
      openRequest({ requestedAccess: 'read-write' }),
    );
    assert.equal(reply.type, 'rejected', 'a silent downgrade would hide a misconfiguration');
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('a malformed or wrong-version open request is rejected with a useful code', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });

    const badVersion = await openAndAwaitReply(mesh, {}, openRequest({ version: 99 }));
    assert.equal(badVersion.reply.code, 'VERSION_UNSUPPORTED');

    const noClient = await openAndAwaitReply(mesh, {}, openRequest({ client: undefined }));
    assert.equal(noClient.reply.code, 'INVALID_REQUEST');

    const badAccess = await openAndAwaitReply(mesh, {}, openRequest({ requestedAccess: 'root' }));
    assert.equal(badAccess.reply.code, 'INVALID_REQUEST');

    assert.equal(server.hostCount, 0);
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

// ─── host cache and limits (SPEC §15.2, §20.2) ──────────────────────────

test('two authorized sessions share one host and one workspace instance', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });

    const first = await openAndAwaitReply(mesh, { peerId: 'nPEER-A01CNTRL' });
    const second = await openAndAwaitReply(mesh, { peerId: 'nPEER-B01CNTRL' });

    assert.equal(first.reply.type, 'accepted');
    assert.equal(second.reply.type, 'accepted');
    assert.equal(server.hostCount, 1, 'one engine, not two');
    assert.equal(
      first.reply.workspaceInstanceId,
      second.reply.workspaceInstanceId,
      'same instance, so EntryIds mean the same thing to both',
    );
    assert.equal(server.listSessions().length, 2);
    assert.notEqual(first.reply.sessionId, second.reply.sessionId);
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('per-peer and per-export session limits are enforced', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only', maxSessions: 3 } },
      maxSessionsPerPeer: 2,
    });

    const a1 = await openAndAwaitReply(mesh, { peerId: 'nSAME0001CNTRL' });
    const a2 = await openAndAwaitReply(mesh, { peerId: 'nSAME0001CNTRL' });
    const a3 = await openAndAwaitReply(mesh, { peerId: 'nSAME0001CNTRL' });
    assert.equal(a1.reply.type, 'accepted');
    assert.equal(a2.reply.type, 'accepted');
    assert.equal(a3.reply.code, 'LIMIT_EXCEEDED', 'third from one peer refused');

    const b1 = await openAndAwaitReply(mesh, { peerId: 'nOTHER001CNTRL' });
    assert.equal(b1.reply.type, 'accepted', 'a different peer has its own budget');

    const b2 = await openAndAwaitReply(mesh, { peerId: 'nTHIRD001CNTRL' });
    assert.equal(b2.reply.code, 'LIMIT_EXCEEDED', 'export cap of 3 reached');
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('closing a session releases the host after the idle lease', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      hostIdleTimeoutMs: 50,
    });

    const { reply, channel } = await openAndAwaitReply(mesh);
    assert.equal(reply.type, 'accepted');
    assert.equal(server.hostCount, 1);

    channel.close();
    const deadline = Date.now() + 3000;
    while (server.hostCount > 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 20));
    }
    assert.equal(server.hostCount, 0, 'host disposed after the lease expired');
    assert.equal(server.listSessions().length, 0, 'session registry is clean');
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('a reconnect inside the idle lease keeps the same workspace instance', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      hostIdleTimeoutMs: 10_000,
    });

    const first = await openAndAwaitReply(mesh);
    const instance = first.reply.workspaceInstanceId;
    first.channel.close();
    await new Promise((r) => setTimeout(r, 50));

    const second = await openAndAwaitReply(mesh);
    assert.equal(second.reply.type, 'accepted');
    assert.equal(
      second.reply.workspaceInstanceId,
      instance,
      'same instance — the client’s cached EntryIds remain valid',
    );
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('heartbeat pings are answered and never reach the explorer host', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
    });
    const { channel, replies } = await openAndAwaitReply(mesh);

    channel.send({
      service: 'mille.remote',
      version: 1,
      type: 'ping',
      nonce: 'abc123',
      sentAtMs: 1,
    });
    const deadline = Date.now() + 3000;
    while (!replies.some((m) => m?.type === 'pong') && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 10));
    }
    const reply = replies.find((m) => m?.type === 'pong');
    assert.ok(reply, 'got a pong');
    assert.equal(reply.nonce, 'abc123', 'nonce echoed');
    // The host never saw it. Had it, the host would have answered the ping as
    // a malformed explorer frame — an EINVAL error, which a real client turns
    // into a rejection of every pending request.
    await new Promise((r) => setTimeout(r, 50));
    assert.deepEqual(
      replies.filter((m) => m?.type === 'error'),
      [],
      'no explorer error frame answered the ping',
    );
    assert.equal(server.listSessions().length, 1);
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});

test('close() disposes every host and session, leaving nothing behind', async () => {
  const dir = tempRoot();
  const mesh = fakeMesh();
  const server = await serveMille(mesh, {
    exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
  });
  try {
    await openAndAwaitReply(mesh, { peerId: 'nA0000001CNTRL' });
    await openAndAwaitReply(mesh, { peerId: 'nB0000001CNTRL' });
    assert.equal(server.listSessions().length, 2);
    assert.equal(server.hostCount, 1);

    await server.close();
    assert.equal(server.listSessions().length, 0);
    assert.equal(server.hostCount, 0);
    assert.equal(mesh.servers[0].closed, true, 'the mesh listener was closed');
    await server.close(); // idempotent
  } finally {
    removeTempDir(dir);
  }
});

test('the server refuses new opens once shutting down', async () => {
  const dir = tempRoot();
  const mesh = fakeMesh();
  const server = await serveMille(mesh, {
    exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
  });
  try {
    await server.close();
    const { server: sock } = socketPair();
    mesh.servers[0].accept(sock);
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(server.listSessions().length, 0, 'no session was created after shutdown');
  } finally {
    removeTempDir(dir);
  }
});

test('diagnosticDisclosure surfaces the real reason when explicitly enabled', async () => {
  const dir = tempRoot();
  let server;
  try {
    const mesh = fakeMesh();
    server = await serveMille(mesh, {
      exports: { work: { label: 'Work', roots: [dir], access: 'read-only' } },
      diagnosticDisclosure: true,
    });
    const { reply } = await openAndAwaitReply(mesh, {}, openRequest({ exportId: 'ghost' }));
    assert.equal(reply.code, 'ACCESS_DENIED');
    assert.match(reply.message, /no such export ghost/, 'admin opted into the detail');
  } finally {
    if (server) await server.close();
    removeTempDir(dir);
  }
});
