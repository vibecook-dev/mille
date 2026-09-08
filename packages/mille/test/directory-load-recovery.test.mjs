import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MessageChannel } from 'node:worker_threads';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { createFileExplorerHost, connectFileExplorer, FileSystemError } from '../dist/index.js';

async function waitFor(predicate) {
  const deadline = Date.now() + 3_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.fail('directory recovery state did not arrive');
}

function delayedPort(port, delayMs, versionOnlyAck = false) {
  const listeners = new Map();
  const timers = new Set();
  return {
    postMessage(message, transfer) {
      const timer = setTimeout(() => {
        timers.delete(timer);
        port.postMessage(message, transfer);
      }, delayMs);
      timers.add(timer);
    },
    addEventListener(_type, listener) {
      const handler = (data) =>
        listener({
          data:
            versionOnlyAck && data.type === 'ack'
              ? { ...data, body: { version: data.body.version } }
              : data,
        });
      listeners.set(listener, handler);
      port.on('message', handler);
    },
    removeEventListener(_type, listener) {
      const handler = listeners.get(listener);
      if (handler) port.off('message', handler);
      listeners.delete(listener);
    },
    start: () => port.start(),
    close() {
      for (const timer of timers) clearTimeout(timer);
      port.close();
    },
  };
}

async function fixture({
  compactFolders = true,
  nested = false,
  slowPeer = false,
  versionOnlyAck = false,
} = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'mille-load-recovery-'));
  const root = join(dir, 'workspace');
  const otherRoot = join(dir, 'other');
  mkdirSync(join(root, 'a', 'b'), { recursive: true });
  writeFileSync(join(root, 'a', 'b', 'leaf.txt'), '');
  mkdirSync(otherRoot);
  writeFileSync(join(otherRoot, 'unrelated.txt'), '');
  const host = await createFileExplorerHost({
    roots: [root, otherRoot],
    initialWalk: 'roots-only',
    compactFolders,
  });
  const [rootEntry, otherEntry] = host.local.getSnapshot().roots();
  if (nested) await host.local.resync(rootEntry.id);
  const loadId = nested ? host.local.getSnapshot().childrenOf(rootEntry.id)[0] : rootEntry.id;
  const original = host.local.resyncProgressive.bind(host.local);
  const blocked = new Map();
  // Hold reads at the failure boundary so both windows join the same task.
  // Compact mode fails below an already-listed parent; ordinary mode fails
  // the expanded folder itself, before it has an authoritative child count.
  let intercept = (id) =>
    compactFolders ? host.local.pathOf(id) === join(root, 'a') : id === loadId;
  host.local.resyncProgressive = (id, options) => {
    if (intercept(id)) {
      return new Promise((resolve, reject) =>
        blocked.set(id, {
          reject,
          resume: () => resolve(original(id, options)),
        }),
      );
    }
    return original(id, options);
  };
  const clients = [];
  for (let i = 0; i < 2; i++) {
    const { port1, port2 } = new MessageChannel();
    host.attachPort(i === 1 && slowPeer ? delayedPort(port1, 40, versionOnlyAck) : port1);
    clients.push(await connectFileExplorer(port2));
  }
  const state = (client, id = loadId) => client.getSnapshot().directoryLoadState(id).state;
  return {
    host,
    clients,
    rootId: rootEntry.id,
    otherId: otherEntry.id,
    loadId,
    state,
    async fail() {
      for (const client of clients)
        client.setExpanded({ add: [...new Set([rootEntry.id, loadId])] });
      await waitFor(
        () => blocked.size > 0 && clients.every((client) => state(client) === 'loading'),
      );
      for (const read of blocked.values())
        read.reject(new FileSystemError('EACCES', 'transient read failure'));
      blocked.clear();
      await waitFor(() => clients.every((client) => state(client) === 'error'));
    },
    intercept(predicate) {
      intercept = predicate;
    },
    async waitForRead() {
      await waitFor(() => blocked.size > 0);
    },
    resume() {
      intercept = () => false;
      for (const read of blocked.values()) read.resume();
      blocked.clear();
    },
    reject(error) {
      for (const read of blocked.values()) read.reject(error);
      blocked.clear();
    },
    async dispose() {
      intercept = () => false;
      for (const read of blocked.values())
        read.reject(new FileSystemError('ECANCELED', 'test cleanup'));
      await Promise.all(clients.map((client) => client.dispose()));
      await host.dispose();
      removeTempDir(dir);
    },
  };
}

for (const mode of ['directory', 'recursive', 'workspace']) {
  test(`${mode} refresh clears failed loads in both mirrors before resolving`, async () => {
    const f = await fixture({ compactFolders: mode !== 'directory', slowPeer: true });
    try {
      await f.fail();
      f.intercept(() => false);
      if (mode === 'workspace') await f.clients[0].resyncWorkspace();
      else await f.clients[0].resync(f.rootId, { recursive: mode === 'recursive' });
      for (const client of f.clients) {
        assert.equal(f.state(client), 'complete');
        assert.equal(
          client
            .getSnapshot()
            .visibleRowCount(new Set([f.rootId]))
            .pendingExpansions.has(f.rootId),
          false,
        );
        const rows = client
          .getSnapshot()
          .visibleRows({ expanded: new Set([f.rootId]), offset: 0, limit: 10 });
        const child = rows[rows.findIndex((row) => row.id === f.rootId) + 1];
        assert.equal(child?.name, mode === 'directory' ? 'a' : 'b');
        if (mode !== 'directory') assert.deepEqual(child.pathSegments, ['a', 'b']);
      }
    } finally {
      await f.dispose();
    }
  });
}

test('a no-op refresh waits for recovery frames even when the tree version was already acknowledged', async () => {
  const f = await fixture({ slowPeer: true });
  try {
    await f.fail();
    // An independent native read can finish the tree without resolving the
    // failed session state. A refresh elsewhere acknowledges that version.
    await f.host.local.resync(f.rootId, { recursive: true });
    const version = await f.clients[0].resync(f.otherId, { recursive: true });
    assert.ok(f.clients.every((client) => f.state(client) === 'error'));
    assert.equal(await f.clients[0].resync(f.rootId, { recursive: true }), version);
    assert.ok(f.clients.every((client) => f.state(client) === 'complete'));
  } finally {
    await f.dispose();
  }
});

test('refreshing a compact descendant recovers the failed expansion that owns it', async () => {
  const f = await fixture();
  try {
    await f.fail();
    const childId = f.host.local.getSnapshot().childrenOf(f.rootId)[0];
    await f.clients[0].resync(childId, { recursive: true });
    for (const client of f.clients) {
      assert.equal(f.state(client), 'complete');
      const rows = client
        .getSnapshot()
        .visibleRows({ expanded: new Set([f.rootId]), offset: 0, limit: 10 });
      assert.ok(rows.some((row) => row.pathSegments?.join('/') === 'a/b'));
    }
  } finally {
    await f.dispose();
  }
});

test('refresh recovery accepts an older peer that only acknowledges tree versions', async () => {
  const f = await fixture({ slowPeer: true, versionOnlyAck: true });
  try {
    await f.fail();
    const started = Date.now();
    await f.clients[0].resync(f.rootId, { recursive: true });
    assert.ok(f.clients.every((client) => f.state(client) === 'complete'));
    assert.ok(Date.now() - started < 800, 'version-only peer incurred the 1000ms ack timeout');
  } finally {
    await f.dispose();
  }
});

test('retry from one window clears the error and refreshes compact rows in its peer', async () => {
  const f = await fixture();
  try {
    await f.fail();
    f.clients[0].setExpanded({ add: [f.loadId] });
    await f.waitForRead();
    await waitFor(() => f.clients.every((client) => f.state(client) === 'loading'));
    f.resume();
    await waitFor(() => f.clients.every((client) => f.state(client) === 'complete'));
    for (const client of f.clients) {
      const rows = client
        .getSnapshot()
        .visibleRows({ expanded: new Set([f.rootId]), offset: 0, limit: 10 });
      assert.ok(rows.some((row) => row.name === 'b' && row.pathSegments?.join('/') === 'a/b'));
    }
  } finally {
    await f.dispose();
  }
});

test('collapsing the retrying window keeps the expanded peer interested in the shared read', async () => {
  const f = await fixture();
  try {
    await f.fail();
    f.clients[0].setExpanded({ add: [f.rootId] });
    await f.waitForRead();
    await waitFor(() => f.clients.every((client) => f.state(client) === 'loading'));
    f.clients[0].setExpanded({ remove: [f.rootId] });
    await waitFor(() => f.state(f.clients[0]) !== 'loading');
    assert.equal(f.state(f.clients[1]), 'loading');
    f.resume();
    await waitFor(() => f.state(f.clients[1]) === 'complete');
  } finally {
    await f.dispose();
  }
});

test('retrying an already-cached directory also recovers its failed peer', async () => {
  const f = await fixture();
  try {
    await f.fail();
    await f.host.local.resync(f.rootId, { recursive: true });
    f.clients[0].setExpanded({ add: [f.rootId] });
    await waitFor(() => f.clients.every((client) => f.state(client) === 'complete'));
    for (const client of f.clients) {
      const rows = client
        .getSnapshot()
        .visibleRows({ expanded: new Set([f.rootId]), offset: 0, limit: 10 });
      assert.ok(rows.some((row) => row.pathSegments?.join('/') === 'a/b'));
    }
  } finally {
    await f.dispose();
  }
});

for (const mode of ['recursive', 'workspace']) {
  test(`${mode} refresh also recovers failed expanded descendants`, async () => {
    const f = await fixture({ compactFolders: false, nested: true });
    try {
      await f.fail();
      f.intercept(() => false);
      if (mode === 'workspace') await f.clients[0].resyncWorkspace();
      else await f.clients[0].resync(f.rootId, { recursive: true });
      for (const client of f.clients) {
        assert.equal(f.state(client), 'complete');
        assert.equal(client.getSnapshot().directChildCount(f.loadId), 1);
      }
    } finally {
      await f.dispose();
    }
  });
}

test('a shallow refresh preserves a compact-chain read that still needs its child listing', async () => {
  const f = await fixture();
  try {
    await f.fail();
    f.clients[0].setExpanded({ add: [f.rootId] });
    await f.waitForRead();
    await waitFor(() => f.clients.every((client) => f.state(client) === 'loading'));
    await f.clients[0].resync(f.rootId);
    for (const client of f.clients) {
      assert.equal(f.state(client), 'loading');
      assert.ok(
        client
          .getSnapshot()
          .visibleRowCount(new Set([f.rootId]))
          .pendingExpansions.has(f.rootId),
      );
    }
    f.resume();
    await waitFor(() => f.clients.every((client) => f.state(client) === 'complete'));
  } finally {
    await f.dispose();
  }
});

test('recursive refresh supersedes an active retry and ignores its late failure', async () => {
  const f = await fixture();
  try {
    await f.fail();
    f.clients[0].setExpanded({ add: [f.rootId] });
    await f.waitForRead();
    await waitFor(() => f.clients.every((client) => f.state(client) === 'loading'));
    await f.clients[0].resync(f.rootId, { recursive: true });
    assert.ok(f.clients.every((client) => f.state(client) === 'complete'));
    f.reject(new FileSystemError('EACCES', 'late failure from superseded retry'));
    await new Promise((resolve) => setTimeout(resolve, 30));
    assert.ok(f.clients.every((client) => f.state(client) === 'complete'));
  } finally {
    await f.dispose();
  }
});

test('refreshing another root leaves the failed folder retryable', async () => {
  const f = await fixture();
  try {
    await f.fail();
    await f.clients[0].resync(f.otherId, { recursive: true });
    assert.ok(f.clients.every((client) => f.state(client) === 'error'));
  } finally {
    await f.dispose();
  }
});
