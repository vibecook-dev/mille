import { removeTempDir } from '../../../scripts/test-temp.mjs';
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import test from 'node:test';

import { FileExplorer } from '../dist/index.js';

async function waitFor(read, predicate, message, timeoutMs = 4_000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = read();
    if (predicate(last)) return last;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  assert.fail(`${message}; last=${JSON.stringify(last)}`);
}

function childByName(fx, rootId, name) {
  const snap = fx.getSnapshot();
  for (const id of snap.childrenOf(rootId)) {
    const entry = snap.getById(id);
    if (entry?.name === name) return entry;
  }
  return null;
}

test('live watcher reconciles external create/modify/rename/delete', async () => {
  const root = await mkdtemp(join(tmpdir(), 'mille-watch-live-'));
  const fx = new FileExplorer({ roots: [root], watchDebounceMs: 40 });
  try {
    await fx.populateFromRoots();
    const rootEntry = fx.getSnapshot().roots()[0];
    assert.ok(rootEntry);

    const createdPath = join(root, 'external.txt');
    await writeFile(createdPath, 'a');
    const created = await waitFor(
      () => childByName(fx, rootEntry.id, 'external.txt'),
      Boolean,
      'external create did not reach the snapshot',
    );
    const stableId = created.id;

    await writeFile(createdPath, 'a much larger payload');
    const modified = await waitFor(
      () => childByName(fx, rootEntry.id, 'external.txt'),
      (entry) => entry?.size === 21,
      'external modify did not refresh metadata',
    );
    assert.equal(modified.id, stableId, 'modify must preserve EntryId');

    const renamedPath = join(root, 'renamed.txt');
    await rename(createdPath, renamedPath);
    await waitFor(
      () => childByName(fx, rootEntry.id, 'renamed.txt'),
      Boolean,
      'external rename did not reach the snapshot',
    );
    assert.equal(childByName(fx, rootEntry.id, 'external.txt'), null);

    await rm(renamedPath);
    await waitFor(
      () => childByName(fx, rootEntry.id, 'renamed.txt'),
      (entry) => entry === null,
      'external delete did not leave the snapshot',
    );
  } finally {
    await fx.dispose();
    removeTempDir(root);
  }
});

test('typed watcher channels report file creation and removal', async () => {
  const root = await mkdtemp(join(tmpdir(), 'mille-watch-events-'));
  const deletedPath = join(root, 'deleted.txt');
  await writeFile(deletedPath, 'already present when watching starts');
  const fx = new FileExplorer({ roots: [root], watchDebounceMs: 40 });
  const subscriptions = [];
  try {
    await fx.populateFromRoots();
    const observedKinds = [];
    const batchKinds = [];
    let treeChangeCount = 0;
    const targetKind = (event) => {
      const name = event.path && basename(event.path);
      if (event.kind === 'created' && name === 'created.txt') return 'created';
      // The native debouncer can reclassify a removal as a metadata hint.
      // In that case the public event has a path but no current entry.
      // Exact raw delete classification is covered by the Rust watcher tests.
      if (
        name === 'deleted.txt' &&
        (event.kind === 'deleted' || (event.kind === 'changed' && !event.entry))
      ) {
        return 'removed';
      }
      return null;
    };
    subscriptions.push(
      fx.on('event', (event) => {
        const kind = targetKind(event);
        if (kind) observedKinds.push(kind);
      }),
      fx.on('batch', (batch) => batchKinds.push(...batch.map(targetKind).filter(Boolean))),
      fx.on('change:tree', () => {
        treeChangeCount += 1;
      }),
    );

    // Use separate paths so the OS and debouncer cannot combine a rapid
    // create/rename/delete into a transient file. The
    // reconciliation test above covers that lifecycle through snapshots.
    await writeFile(join(root, 'created.txt'), 'events');
    await waitFor(
      () => ({ observedKinds, batchKinds, treeChangeCount }),
      (state) =>
        state.observedKinds.includes('created') &&
        state.batchKinds.includes('created') &&
        state.treeChangeCount > 0,
      'create did not reach every typed watcher channel',
    );
    const changesAfterCreate = treeChangeCount;

    await rm(deletedPath);
    await waitFor(
      () => ({ observedKinds, batchKinds, treeChangeCount }),
      (state) =>
        state.observedKinds.includes('removed') &&
        state.batchKinds.includes('removed') &&
        state.treeChangeCount > changesAfterCreate,
      'delete did not reach every typed watcher channel',
    );
  } finally {
    for (const subscription of subscriptions) subscription.dispose();
    await fx.dispose();
    removeTempDir(root);
  }
});

test('watcher echo preserves a library-mutated entry', async () => {
  const root = await mkdtemp(join(tmpdir(), 'mille-watch-echo-'));
  const fx = new FileExplorer({ roots: [root], watchDebounceMs: 40 });
  try {
    await fx.populateFromRoots();
    const rootEntry = fx.getSnapshot().roots()[0];
    const created = await fx.create(rootEntry.id, 'owned.txt', 0);
    await fx.writeFile(created.id, Buffer.from('updated payload'));
    assert.equal(fx.getSnapshot().getById(created.id)?.size, 15);

    const renamed = await fx.rename(created.id, 'renamed-owned.txt');
    assert.equal(renamed.id, created.id);
    await new Promise((resolve) => setTimeout(resolve, 250));
    const matching = fx
      .getSnapshot()
      .childrenOf(rootEntry.id)
      .map((id) => fx.getSnapshot().getById(id))
      .filter((entry) => entry?.name === 'renamed-owned.txt');
    assert.equal(matching.length, 1);
    assert.equal(matching[0].id, created.id);
    assert.equal(matching[0].size, 15);
  } finally {
    await fx.dispose();
    removeTempDir(root);
  }
});

test('external directory rename/delete never leaves dangling descendants', async () => {
  const root = await mkdtemp(join(tmpdir(), 'mille-watch-directory-'));
  const oldDir = join(root, 'old-dir');
  await mkdir(oldDir);
  await writeFile(join(oldDir, 'child.txt'), 'child');
  const fx = new FileExplorer({ roots: [root], watchDebounceMs: 40 });
  try {
    await fx.populateFromRoots();
    const rootEntry = fx.getSnapshot().roots()[0];
    const oldEntry = childByName(fx, rootEntry.id, 'old-dir');
    const oldChildId = fx.getSnapshot().childrenOf(oldEntry.id)[0];

    const newDir = join(root, 'new-dir');
    await rename(oldDir, newDir);
    const renamed = await waitFor(
      () => childByName(fx, rootEntry.id, 'new-dir'),
      Boolean,
      'external directory rename did not reconcile',
    );
    const renamedChild = childByName(fx, renamed.id, 'child.txt');
    assert.ok(renamedChild, 'known descendants must remain visible after directory rename');
    if (renamed.id === oldEntry.id) {
      assert.equal(renamedChild.id, oldChildId, 'paired rename must preserve descendant EntryId');
    } else {
      // Some platforms report directory rename as delete + create. In that
      // event shape ids are reallocated, but the removed subtree must not
      // leave aliases or dangling descendants.
      assert.equal(fx.getSnapshot().getById(oldEntry.id), null);
      assert.equal(fx.getSnapshot().getById(oldChildId), null);
    }

    await rm(newDir, { recursive: true });
    await waitFor(
      () => fx.getSnapshot().getById(renamed.id),
      (entry) => entry === null,
      'external recursive directory delete did not reconcile',
    );
  } finally {
    await fx.dispose();
    removeTempDir(root);
  }
});

test('dispose stops watcher delivery', async () => {
  const root = await mkdtemp(join(tmpdir(), 'mille-watch-dispose-'));
  const fx = new FileExplorer({ roots: [root], watchDebounceMs: 30 });
  try {
    await fx.populateFromRoots();
    const version = fx.getTreeVersion();
    await fx.dispose();
    await writeFile(join(root, 'after-dispose.txt'), 'ignored');
    await new Promise((resolve) => setTimeout(resolve, 200));
    assert.equal(fx.getTreeVersion(), version);
  } finally {
    await fx.dispose();
    removeTempDir(root);
  }
});
