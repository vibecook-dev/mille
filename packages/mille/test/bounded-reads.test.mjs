// M10: a native read that blocks in the kernel answers for its own folder and
// the engine moves on.
//
// An iCloud-evicted folder blocks `readdir` in the kernel while macOS
// downloads it (VibeField measured a Keynote package holding one read for
// minutes); a hung network mount does the same. CI cannot evict a folder, so
// these tests use the read probe a `build:napi:probe` binary carries:
// `__readProbePark(path)` parks every native read of that path on the helper
// thread — exactly where the kernel would block — until it is released. The
// probe is absent from published artifacts, and the suite skips without it.

import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MessageChannel } from 'node:worker_threads';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import {
  buildIdentity,
  connectFileExplorer,
  createFileExplorerHost,
  FileExplorer,
  isFileSystemError,
} from '../dist/index.js';

const require = createRequire(import.meta.url);
const native = require(buildIdentity().resolvedPath);
const probe = typeof native.__readProbePark === 'function' ? native : null;
const skip = probe === null ? 'needs the read probe (build:napi:probe)' : false;

function fixture(extra = []) {
  const root = mkdtempSync(join(tmpdir(), 'mille-bounded-'));
  for (const dir of ['slow/sub', 'fast', ...extra]) mkdirSync(join(root, dir), { recursive: true });
  writeFileSync(join(root, 'slow', 'inside.txt'), 's');
  writeFileSync(join(root, 'fast', 'a.txt'), 'a');
  return root;
}

async function rejection(promise) {
  try {
    await promise;
  } catch (error) {
    return error;
  }
  assert.fail('expected the operation to reject');
}

async function eventually(predicate, limitMs = 5_000) {
  const deadline = Date.now() + limitMs;
  for (;;) {
    if (await predicate()) return;
    if (Date.now() > deadline) assert.fail('condition not reached in time');
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

/** Seed the root, list it, and return the ids of its children by name. */
async function open(fx, root) {
  const rootId = await fx.resolvePath(root);
  const page = await fx.list(rootId);
  return {
    rootId,
    ids: Object.fromEntries(page.entries.map((entry) => [entry.name, entry.id])),
  };
}

test(
  'a stalled folder answers ETIMEDOUT on time and other folders keep loading',
  { skip },
  async () => {
    probe.__readProbeReset();
    const root = fixture();
    const fx = new FileExplorer({ roots: [root], readTimeoutMs: 400 });
    try {
      const { ids } = await open(fx, root);
      probe.__readProbePark(join(root, 'slow'));

      const started = Date.now();
      const stalled = rejection(fx.list(ids.slow));
      const fast = await fx.list(ids.fast);
      assert.deepEqual(
        fast.entries.map((entry) => entry.name),
        ['a.txt'],
      );
      const error = await stalled;
      const waited = Date.now() - started;
      assert.ok(isFileSystemError(error), String(error));
      assert.equal(error.code, 'ETIMEDOUT');
      assert.equal(error.path, join(root, 'slow'));
      assert.ok(waited >= 400, `answered before the deadline (${waited} ms)`);
      assert.ok(waited < 5_000, `answered ${waited} ms after the request`);
      assert.equal(fx.getSnapshot().directoryChildrenLoaded(ids.slow), false);

      // Still stuck: a repeat answers at once instead of parking another
      // thread, and the rest of the engine serves normally.
      const again = Date.now();
      const repeat = await rejection(fx.list(ids.slow));
      assert.equal(repeat.code, 'ETIMEDOUT');
      assert.ok(Date.now() - again < 400, `repeat waited ${Date.now() - again} ms`);
      assert.equal((await fx.list(ids.fast)).entries.length, 1);
      assert.equal(probe.__readProbeStats().inside, 1, 'one kernel call for the stuck folder');

      // The folder "materializes": the parked call returns, and a retry
      // succeeds and marks the listing complete.
      probe.__readProbeRelease(join(root, 'slow'));
      await eventually(async () => {
        try {
          return (await fx.list(ids.slow)).entries.length === 2;
        } catch (error) {
          if (isFileSystemError(error) && error.code === 'ETIMEDOUT') return false;
          throw error;
        }
      });
      assert.deepEqual(
        (await fx.list(ids.slow, { sort: 'name' })).entries.map((entry) => entry.name),
        ['inside.txt', 'sub'],
      );
      assert.equal(fx.getSnapshot().directoryChildrenLoaded(ids.slow), true);
    } finally {
      probe.__readProbeReset();
      await fx.dispose();
      removeTempDir(root);
    }
  },
);

test(
  'a deeper walk keeps everything else and leaves the stalled folder not loaded',
  { skip },
  async () => {
    probe.__readProbeReset();
    const root = fixture(['fast/deeper']);
    const fx = new FileExplorer({ roots: [root], readTimeoutMs: 300 });
    try {
      const { rootId } = await open(fx, root);
      probe.__readProbePark(join(root, 'slow'), 'list');
      await fx.prefetch(rootId, { depth: 3 });
      const snapshot = fx.getSnapshot();
      const byName = (parent) =>
        Object.fromEntries(
          snapshot.childrenOf(parent).map((id) => [snapshot.getById(id).name, id]),
        );
      const top = byName(rootId);
      assert.deepEqual(Object.keys(byName(top.fast)).sort(), ['a.txt', 'deeper']);
      assert.equal(snapshot.directoryChildrenLoaded(top.fast), true);
      assert.deepEqual(Object.keys(byName(top.slow)), []);
      assert.equal(snapshot.directoryChildrenLoaded(top.slow), false);

      probe.__readProbeRelease(join(root, 'slow'), 'list');
      await eventually(async () => {
        await fx.prefetch(rootId, { depth: 3 });
        return fx.getSnapshot().directoryChildrenLoaded(top.slow);
      });
    } finally {
      probe.__readProbeReset();
      await fx.dispose();
      removeTempDir(root);
    }
  },
);

test('the stuck cap holds under many stuck folders', { skip }, async () => {
  probe.__readProbeReset();
  const names = Array.from({ length: 12 }, (_, i) => `stuck-${String(i).padStart(2, '0')}`);
  const root = fixture(names);
  // Two healthy + two stuck threads at most.
  const fx = new FileExplorer({
    roots: [root],
    readTimeoutMs: 200,
    maxStuckReads: 2,
    walkerConcurrency: 2,
  });
  try {
    const { ids } = await open(fx, root);
    for (const name of names) probe.__readProbePark(join(root, name));
    const started = Date.now();
    const errors = await Promise.all(names.map((name) => rejection(fx.list(ids[name]))));
    for (const error of errors) {
      assert.ok(isFileSystemError(error), String(error));
      assert.equal(error.code, 'ETIMEDOUT');
    }
    assert.ok(Date.now() - started < 8_000, `took ${Date.now() - started} ms`);
    const stats = probe.__readProbeStats();
    assert.ok(stats.peak <= 4, `peak ${stats.peak} reads parked at once`);
    // The budget is spent: a healthy folder is refused rather than queued.
    const refused = await rejection(fx.list(ids.fast));
    assert.equal(refused.code, 'ETIMEDOUT');

    probe.__readProbeReset();
    await eventually(async () => {
      try {
        return (await fx.list(ids.fast)).entries.length === 1;
      } catch {
        return false;
      }
    });
  } finally {
    probe.__readProbeReset();
    await fx.dispose();
    removeTempDir(root);
  }
});

test('dispose completes while a read is still stuck', { skip }, async () => {
  probe.__readProbeReset();
  const root = fixture();
  const fx = new FileExplorer({ roots: [root], readTimeoutMs: 60_000 });
  try {
    const { ids } = await open(fx, root);
    probe.__readProbePark(join(root, 'slow'));
    const pending = rejection(fx.list(ids.slow));
    await eventually(() => probe.__readProbeStats().inside === 1);

    const started = Date.now();
    await fx.dispose();
    assert.ok(Date.now() - started < 2_000, `dispose took ${Date.now() - started} ms`);
    const error = await pending;
    assert.ok(isFileSystemError(error), String(error));
    assert.equal(error.code, 'ECANCELED');
    assert.ok(Date.now() - started < 4_000, 'the stuck operation was not released');
    // The helper is still "in the kernel"; it exits once released.
    assert.equal(probe.__readProbeStats().inside, 1);
  } finally {
    probe.__readProbeReset();
    removeTempDir(root);
  }
});

test('reveal into a stalled folder answers ETIMEDOUT', { skip }, async () => {
  probe.__readProbeReset();
  const root = fixture();
  const fx = new FileExplorer({ roots: [root], readTimeoutMs: 300 });
  try {
    await open(fx, root);
    const target = join(root, 'slow', 'inside.txt');
    probe.__readProbePark(target, 'stat');
    const error = await rejection(fx.resolvePath(target));
    assert.equal(error.code, 'ETIMEDOUT');
    probe.__readProbeRelease(target, 'stat');
    await eventually(async () => {
      try {
        return (await fx.resolvePath(target)) !== null;
      } catch {
        return false;
      }
    });
  } finally {
    probe.__readProbeReset();
    await fx.dispose();
    removeTempDir(root);
  }
});

test(
  'an expanded stalled folder reports a directory-load error and other expansions complete',
  { skip },
  async () => {
    probe.__readProbeReset();
    const root = fixture();
    const host = await createFileExplorerHost({
      roots: [root],
      initialWalk: 'roots-only',
      readTimeoutMs: 300,
    });
    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    const client = await connectFileExplorer(port2);
    try {
      const rootId = client.getSnapshot().roots()[0].id;
      client.setExpanded({ add: [rootId] });
      await eventually(() => client.getSnapshot().directoryLoadState(rootId).state === 'complete');
      const snapshot = host.local.getSnapshot();
      const ids = Object.fromEntries(
        snapshot.childrenOf(rootId).map((id) => [snapshot.getById(id).name, id]),
      );

      probe.__readProbePark(join(root, 'slow'));
      client.setExpanded({ add: [ids.slow, ids.fast] });
      const state = (id) => client.getSnapshot().directoryLoadState(id);
      await eventually(
        () => state(ids.slow).state === 'error' && state(ids.fast).state === 'complete',
      );
      assert.equal(state(ids.slow).code, 'ETIMEDOUT');

      // Collapse, let the folder answer, expand again: it loads.
      client.setExpanded({ remove: [ids.slow] });
      probe.__readProbeRelease(join(root, 'slow'));
      await eventually(() => probe.__readProbeStats().inside === 0);
      client.setExpanded({ add: [ids.slow] });
      await eventually(() => state(ids.slow).state === 'complete');
      const after = host.local.getSnapshot();
      assert.deepEqual(
        after
          .childrenOf(ids.slow)
          .map((id) => after.getById(id).name)
          .sort(),
        ['inside.txt', 'sub'],
      );
    } finally {
      probe.__readProbeReset();
      await client.dispose();
      await host.dispose();
      removeTempDir(root);
    }
  },
);
