// Search over the channel (M6): the host's engine, bounded and policy-gated.
//
// In-process `FileExplorer.search` has always existed; a port client could
// not reach it. These tests drive `PortFileExplorer.search` against a real
// host — over a MessagePort, a remote-policy channel, and a WebSocket — and
// check the bounds a remote caller must not be able to get around: the page
// limit and its cap, the `clipped` flag, the `allowSearch` gate, and the
// symlink boundary resolvePath already keeps.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import { MessageChannel } from 'node:worker_threads';

import { WebSocketServer, WebSocket as WsClient } from 'ws';

import { createFileExplorerHost } from '../dist/host.js';
import { connectFileExplorer, connectFileExplorerChannel } from '../dist/client-port.js';
import { createMessagePortHostChannel } from '../dist/channel/message-port.js';
import { serveWebSocketSession } from '../dist/node.js';
import { connectWebSocketChannel } from '../dist/remote-client.js';

function seedTree() {
  const dir = mkdtempSync(join(tmpdir(), 'mille-port-search-'));
  mkdirSync(join(dir, 'src'));
  mkdirSync(join(dir, 'src', 'srcgen'));
  writeFileSync(join(dir, 'src', 'module.rs'), '');
  for (let i = 0; i < 30; i += 1) {
    writeFileSync(join(dir, `file_${String(i).padStart(2, '0')}.txt`), '');
  }
  return dir;
}

/** A walked host: search covers what the engine holds, so walk it all. */
async function walkedHost(dir, options = {}) {
  const host = await createFileExplorerHost({ roots: [dir], ...options });
  await host.local.populateFromRoots();
  return host;
}

/** A port client attached with `context` (admin when omitted). */
async function portClient(host, context) {
  const { port1, port2 } = new MessageChannel();
  if (context === undefined) host.attachPort(port1);
  else host.attachChannel(createMessagePortHostChannel(port1), context);
  const client = await connectFileExplorer(port2);
  return { client, close: () => client.dispose().finally(() => port2.close()) };
}

test('search over a port returns score-ordered hits with their paths', async () => {
  const dir = seedTree();
  const host = await walkedHost(dir);
  const { client, close } = await portClient(host);
  try {
    const hits = await client.search('module.rs');
    assert.ok(Array.isArray(hits), 'an array, as FileTree consumes it');
    assert.equal(hits.clipped, false);
    assert.equal(typeof hits.treeVersion, 'number');
    assert.equal(hits[0].entry.name, 'module.rs');
    assert.deepEqual(hits[0].path, [basename(dir), 'src', 'module.rs']);
    for (let i = 1; i < hits.length; i += 1) assert.ok(hits[i - 1].score >= hits[i].score);
    // The hit and its ancestors are in the mirror, so the tree can reveal it.
    assert.ok(client.getSnapshot().getById(hits[0].entry.id) !== null);
  } finally {
    await close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('the page is bounded and clipped says when the engine had more', async () => {
  const dir = seedTree();
  const host = await walkedHost(dir);
  const { client, close } = await portClient(host);
  try {
    const page = await client.search('file_', { limit: 10 });
    assert.equal(page.length, 10);
    assert.equal(page.clipped, true);

    const all = await client.search('file_', { limit: 100 });
    assert.equal(all.length, 30);
    assert.equal(all.clipped, false);

    // Over the cap is clamped, not refused.
    const huge = await client.search('file_', { limit: 1_000_000 });
    assert.equal(huge.length, 30);
  } finally {
    await close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('kinds narrows in the engine, before the limit', async () => {
  const dir = seedTree();
  const host = await walkedHost(dir);
  const { client, close } = await portClient(host);
  try {
    const dirs = await client.search('src', { kinds: [1], limit: 1 });
    assert.equal(dirs.length, 1);
    assert.equal(dirs[0].entry.kind, 1, 'a one-hit directory search returns a directory');
    const many = await client.search('src', { kinds: [1], limit: 10 });
    assert.ok(
      many.every((h) => h.entry.kind === 1),
      'only directories match',
    );
    const names = many.map((h) => h.entry.name);
    // The temp root's own name may fuzzy-match too; it is a directory as well.
    assert.ok(names.includes('src') && names.includes('srcgen'), names.join(', '));
    assert.ok(!names.includes('module.rs'));
  } finally {
    await close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('a remote session searches only when its policy allows it', async () => {
  const dir = seedTree();
  const host = await walkedHost(dir);
  const denied = await portClient(host, { kind: 'remote', policy: { access: 'read-only' } });
  const allowed = await portClient(host, {
    kind: 'remote',
    policy: { access: 'read-only', allowSearch: true },
  });
  try {
    await assert.rejects(denied.client.search('module'), (err) => err.code === 'EACCES');
    const hits = await allowed.client.search('module');
    assert.equal(hits[0].entry.name, 'module.rs');
  } finally {
    await denied.close();
    await allowed.close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('a restricted session finds nothing beyond a symlink another session expanded', async () => {
  const dir = seedTree();
  const outside = mkdtempSync(join(tmpdir(), 'mille-port-search-outside-'));
  writeFileSync(join(outside, 'secret_payroll.csv'), '');
  symlinkSync(outside, join(dir, 'linked'), 'dir');
  // A host that does not follow links (the remote-export setting) keeps the
  // link as a symlink entry; an admin session expanding it hydrates what
  // lies beyond into the shared store.
  const host = await walkedHost(dir, { followSymlinks: false });
  const admin = await portClient(host);
  const remote = await portClient(host, {
    kind: 'remote',
    policy: { access: 'read-only', allowSearch: true },
  });
  try {
    const snapshot = host.local.getSnapshot();
    const [root] = snapshot.roots();
    const link = [...snapshot.projectedChildrenOf(root.id)]
      .map((id) => snapshot.getById(id))
      .find((entry) => entry.name === 'linked');
    assert.equal(link.kind, 2, 'the link is recorded as a symlink');
    await admin.client.setExpanded({ add: [root.id, link.id], remove: [] });
    admin.client.setViewport({ offset: 0, limit: 100 });
    const deadline = Date.now() + 5000;
    while (host.local.search('secret_payroll').length === 0) {
      if (Date.now() > deadline) throw new Error('the expansion never hydrated the link');
      await new Promise((r) => setTimeout(r, 15));
    }

    const seen = await admin.client.search('secret_payroll');
    assert.equal(seen.length, 1, 'the admin session sees through its own link');
    const hidden = await remote.client.search('secret_payroll');
    assert.equal(hidden.length, 0, 'a restricted session does not');
  } finally {
    await admin.close();
    await remote.close();
    await host.dispose();
    removeTempDir(dir);
    removeTempDir(outside);
  }
});

test('the AbortSignal stays local; an aborted search rejects with ECANCELED', async () => {
  const dir = seedTree();
  const host = await walkedHost(dir);
  const { client, close } = await portClient(host);
  try {
    // mille-ui passes a signal; structured clone cannot carry one.
    const live = new AbortController();
    const hits = await client.search('module', { limit: 5, signal: live.signal });
    assert.equal(hits[0].entry.name, 'module.rs');

    const gone = new AbortController();
    gone.abort();
    await assert.rejects(
      client.search('module', { signal: gone.signal }),
      (err) => err.code === 'ECANCELED',
    );
  } finally {
    await close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('malformed search arguments are refused with EINVAL', async () => {
  const dir = seedTree();
  const host = await walkedHost(dir);
  const { client, close } = await portClient(host);
  try {
    const einval = (err) => err.code === 'EINVAL';
    await assert.rejects(client.call('search', [42]), einval);
    await assert.rejects(client.call('search', ['x', { limit: 0 }]), einval);
    await assert.rejects(client.call('search', ['x', { kinds: [9] }]), einval);
    await assert.rejects(client.call('search', ['x', { includeIgnored: 'yes' }]), einval);
    await assert.rejects(client.call('search', ['x'.repeat(2000)]), einval);
  } finally {
    await close();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('search crosses a WebSocket session whose grant allows it', async () => {
  const dir = seedTree();
  const host = await walkedHost(dir);
  const wss = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(wss, 'listening');
  wss.on('connection', (socket) =>
    serveWebSocketSession(socket, {
      authorize: () => ({ host, policy: { access: 'read-write', allowSearch: true } }),
    }),
  );
  let fx;
  try {
    // Asked for read-only on a read-write grant: narrowed, but search survives.
    const conn = await connectWebSocketChannel(`ws://127.0.0.1:${wss.address().port}`, {
      exportId: 'proj',
      access: 'read-only',
      WebSocket: WsClient,
    });
    assert.equal(conn.accepted.export.access, 'read-only');
    fx = await connectFileExplorerChannel(conn.channel);
    const hits = await fx.search('module', { limit: 3 });
    assert.equal(hits[0].entry.name, 'module.rs');
    assert.deepEqual(hits[0].path.slice(-2), ['src', 'module.rs']);
  } finally {
    await fx?.dispose();
    for (const client of wss.clients) client.terminate();
    await new Promise((resolve) => wss.close(() => resolve()));
    await host.dispose();
    removeTempDir(dir);
  }
});
