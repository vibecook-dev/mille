// Phase B2 — lazy expansion integration test.
//
// B1 unblocked the handshake by shipping roots via delta. B2 moves the
// walk itself off the handshake path: the host no longer requires the
// consumer to `populateFromRoots()` upfront. Instead, `initialWalk:
// 'roots-only'` seeds just the root Entry records at attach time, and
// the host's `setExpanded` handler fires a depth-1 walk per newly
// expanded folder. Children land in the next delta.
//
// This test exercises that end-to-end:
//
//   1. Create a temp dir with nested folders + files.
//   2. Construct host with `initialWalk: 'roots-only'`.
//   3. Attach client, handshake.
//   4. Assert snapshot carries roots but no children.
//   5. Client sends `setExpanded({ add: [rootId] })`.
//   6. Wait for the next delta — assert the root's direct children appear.
//   7. Second scenario: re-expanding a known folder doesn't re-trigger a walk.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MessageChannel } from 'node:worker_threads';

import {
  FileExplorer,
  FileSystemError,
  createFileExplorerHost,
  connectFileExplorer,
} from '../dist/index.js';

function tempRoot() {
  return mkdtempSync(join(tmpdir(), 'mille-lazy-expand-'));
}

/**
 * Poll `predicate` every 10ms up to `timeoutMs`. Ready when the predicate
 * returns a non-nullish value — `0` counts as ready (EntryIds start at 0,
 * so truthy checks would spin forever waiting for the first-allocated id).
 */
async function waitFor(predicate, { timeoutMs = 2000, stepMs = 10 } = {}) {
  const start = Date.now();
  for (;;) {
    const result = predicate();
    if (result !== null && result !== undefined && result !== false) return result;
    if (Date.now() - start > timeoutMs) {
      throw new Error(`waitFor: predicate timed out after ${timeoutMs}ms`);
    }
    await new Promise((r) => setTimeout(r, stepMs));
  }
}

test('initialWalk=roots-only seeds root; setExpanded triggers child walk via delta', async () => {
  const dir = tempRoot();
  try {
    // Structure: root/
    //              hello.txt
    //              sub/
    //                 deep.txt
    //                 deeper/
    //                        nested.txt
    writeFileSync(join(dir, 'hello.txt'), 'world');
    mkdirSync(join(dir, 'sub'));
    writeFileSync(join(dir, 'sub', 'deep.txt'), 'x');
    mkdirSync(join(dir, 'sub', 'deeper'));
    writeFileSync(join(dir, 'sub', 'deeper', 'nested.txt'), 'y');

    const host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
    });

    // Root identity is published synchronously, before any filesystem I/O or
    // client handshake. Descendants remain lazy.
    assert.equal(
      host.local.getSnapshot().roots().length,
      1,
      'pre-attach: configured root is already visible',
    );

    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);

    // Tee deltas to observe what crosses the wire.
    const observedDeltaFrames = [];
    port2.on('message', (raw) => {
      if (raw && raw.type === 'delta') observedDeltaFrames.push(raw.body);
    });

    const client = await connectFileExplorer(port2);

    // The root is part of the initial snapshot; it must never depend on a
    // later delta or watcher startup.
    const rootId = await waitFor(() => {
      const roots = client.getSnapshot().roots();
      return roots.length === 1 ? roots[0].id : null;
    });

    const rootsAfter = client.getSnapshot().roots();
    assert.equal(rootsAfter.length, 1, 'client received one root at handshake');
    assert.equal(rootsAfter[0].id, rootId, 'root id matches');

    // Critical: the root should be visible, but its CHILDREN should
    // NOT be in the client mirror — that's the point of roots-only.
    // childrenOf the root should be empty until we expand.
    const snapBeforeExpand = client.getSnapshot();
    const kidsBeforeExpand = snapBeforeExpand.childrenOf ? snapBeforeExpand.childrenOf(rootId) : [];
    assert.equal(
      kidsBeforeExpand.length,
      0,
      'roots-only: root has no children in client mirror before expand',
    );

    // Now expand the root. This should trigger a depth-1 walk on the
    // host side (handleSetExpanded auto-prefetches) and ship the
    // children via the next delta.
    const beforeExpandFrameCount = observedDeltaFrames.length;
    client.setExpanded({ add: [rootId] });

    // Wait for the delta that brings children. Children land in the
    // async walker's ChangeSet → delta path, not the synchronous
    // setExpanded reply (which ships nothing new when the store was
    // empty). Give the walker a moment.
    await waitFor(() => {
      const s = client.getSnapshot();
      const kidCount = s.directChildCount(rootId) ?? 0;
      return kidCount >= 2 ? kidCount : null;
    });

    // Verify via the host's native snapshot (richer API with
    // `childrenOf` — client-side mirror only exposes `directChildCount`
    // + `hasChildren` + `getById`).
    const hostSnap = host.local.getSnapshot();
    const kidsAfterExpand = hostSnap.childrenOf(rootId);
    assert.ok(
      kidsAfterExpand.length >= 2,
      `after expand: root has ${kidsAfterExpand.length} children (expected >= 2)`,
    );
    const kidNames = new Set(
      kidsAfterExpand
        .map((id) => hostSnap.getById(id))
        .filter(Boolean)
        .map((e) => e.name),
    );
    assert.ok(kidNames.has('hello.txt'), 'direct children include hello.txt');
    assert.ok(kidNames.has('sub'), 'direct children include sub');

    // With depth=1, grandchildren should NOT be walked yet.
    const subId = [...kidsAfterExpand].find((id) => {
      const e = hostSnap.getById(id);
      return e && e.name === 'sub';
    });
    assert.ok(subId !== undefined, 'found sub id');
    const grandkids = hostSnap.childrenOf(subId);
    assert.equal(
      grandkids.length,
      0,
      'depth=1 walk does not surface grandchildren (sub/deep.txt, sub/deeper)',
    );

    // At least one delta after the expand should carry entries.
    const expansionDeltas = observedDeltaFrames.slice(beforeExpandFrameCount);
    const deltaWithEntries = expansionDeltas.find(
      (b) => b.viewportPatch instanceof ArrayBuffer && b.viewportPatch.byteLength > 1,
    );
    assert.ok(
      deltaWithEntries !== undefined,
      'at least one post-expand delta carried children entries',
    );

    await client.dispose();
    await host.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('empty directory expansion publishes loaded-empty completion without a second toggle', async () => {
  const dir = tempRoot();
  try {
    const host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
    });
    const rootBeforeAttach = host.local.getSnapshot().roots()[0];
    assert.ok(rootBeforeAttach, 'root seeded synchronously');
    assert.equal(
      host.local.getSnapshot().directoryChildrenLoaded(rootBeforeAttach.id),
      false,
      'placeholder root is pending before expansion',
    );

    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    const client = await connectFileExplorer(port2);
    const rootId = client.getSnapshot().roots()[0]?.id;
    assert.equal(rootId, rootBeforeAttach.id, 'handshake preserves seeded identity');

    client.setExpanded({ add: [rootId] });
    await waitFor(() => client.getSnapshot().directChildCount(rootId) === 0);

    assert.equal(
      host.local.getSnapshot().directoryChildrenLoaded(rootId),
      true,
      'one expansion completed the directory listing',
    );
    assert.equal(
      client.getSnapshot().directoryChildrenLoaded(rootId),
      true,
      'loaded-empty completion reached the client mirror',
    );
    assert.equal(client.getSnapshot().hasChildren(rootId), false, 'loaded empty is a leaf');

    await client.dispose();
    await host.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('wide directories publish usable partial rows before authoritative completion', async () => {
  const dir = tempRoot();
  let host;
  let client;
  let sub;
  let resumeRead;
  const readMayFinish = new Promise((resolve) => {
    resumeRead = resolve;
  });
  try {
    const total = 512;
    for (let i = 0; i < total; i++) {
      writeFileSync(join(dir, `file-${String(i).padStart(4, '0')}.txt`), 'x');
    }
    host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
      compactFolders: false,
      directoryBatchSize: 16,
    });
    // Finish watch registration before expanding, so its separate gap-repair
    // resync cannot complete the deliberately held prefix behind the test.
    const startWatching = host.local.startWatching.bind(host.local);
    let watchRegistration;
    host.local.startWatching = () => (watchRegistration = startWatching());
    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    client = await connectFileExplorer(port2, { prefetchRows: 128 });
    await watchRegistration;
    host.local.startWatching = startWatching;
    const rootId = client.getSnapshot().roots()[0].id;

    // An optimized 512-file walk can finish between polling turns. Hold a
    // native cache prefix until the mirror consumes it: resolvePath inserts
    // real entries without marking their parent's listing authoritative.
    // The real progressive read finishes the listing after the assertions.
    const resyncProgressive = host.local.resyncProgressive.bind(host.local);
    host.local.resyncProgressive = async (id, options) => {
      for (let i = 0; i < 16; i++) {
        await host.local.resolvePath(`file-${String(i).padStart(4, '0')}.txt`);
      }
      await readMayFinish;
      return resyncProgressive(id, options);
    };

    let clientObservedPartial = false;
    sub = client.on('change', () => {
      const snapshot = client.getSnapshot();
      const rows = snapshot.visibleRows({
        expanded: new Set([rootId]),
        offset: 0,
        limit: 128,
      });
      if (
        rows.length > 1 &&
        snapshot.directChildCount(rootId) === null &&
        snapshot.directoryLoadState(rootId).state === 'loading'
      ) {
        clientObservedPartial = true;
      }
    });

    client.setExpanded({ add: [rootId] });
    await waitFor(() => {
      const snapshot = host.local.getSnapshot();
      const count = snapshot.childrenOf(rootId).length;
      return count > 0 && count < total && !snapshot.directoryChildrenLoaded(rootId);
    });
    await waitFor(() => clientObservedPartial);
    assert.ok(
      client
        .getSnapshot()
        .visibleRowCount(new Set([rootId]))
        .pendingExpansions.has(rootId),
      'partial rows retain the loading indicator',
    );

    resumeRead();
    await waitFor(() => client.getSnapshot().directChildCount(rootId) === total, {
      timeoutMs: 5000,
    });
    await waitFor(() => client.getSnapshot().directoryLoadState(rootId).state === 'complete');
    const page = await host.local.list(rootId, { offset: 10, limit: 5, sort: 'name' });
    assert.equal(page.total, total);
    assert.equal(page.entries.length, 5);
    assert.equal(page.entries[0].name, 'file-0010.txt');
    assert.equal(page.hasMore, true);
  } finally {
    resumeRead();
    sub?.dispose();
    await client?.dispose();
    await host?.dispose();
    removeTempDir(dir);
  }
});

test('collapsing cancels an obsolete wide-directory read and re-expand retries cleanly', async () => {
  const dir = tempRoot();
  try {
    const total = 2048;
    for (let i = 0; i < total; i++) {
      writeFileSync(join(dir, `cancel-${String(i).padStart(5, '0')}.txt`), 'x');
    }
    const host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
      compactFolders: false,
      directoryBatchSize: 16,
    });
    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    const client = await connectFileExplorer(port2);
    const rootId = client.getSnapshot().roots()[0].id;

    client.setExpanded({ add: [rootId] });
    await waitFor(() => {
      const snapshot = host.local.getSnapshot();
      return snapshot.childrenOf(rootId).length > 0 && !snapshot.directoryChildrenLoaded(rootId);
    });
    client.setExpanded({ remove: [rootId] });
    await new Promise((resolve) => setTimeout(resolve, 40));
    assert.equal(
      host.local.getSnapshot().directoryChildrenLoaded(rootId),
      false,
      'cancelled prefix is never promoted to an authoritative listing',
    );

    client.setExpanded({ add: [rootId] });
    await waitFor(() => client.getSnapshot().directChildCount(rootId) === total, {
      timeoutMs: 8000,
    });
    await waitFor(() => client.getSnapshot().directoryLoadState(rootId).state === 'complete');

    await client.dispose();
    await host.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('one collapsed window does not cancel a progressive load still used by another', async () => {
  const dir = tempRoot();
  try {
    const total = 1024;
    for (let i = 0; i < total; i++) {
      writeFileSync(join(dir, `shared-${String(i).padStart(5, '0')}.txt`), 'x');
    }
    const host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
      compactFolders: false,
      directoryBatchSize: 16,
    });
    const a = new MessageChannel();
    const b = new MessageChannel();
    host.attachPort(a.port1);
    host.attachPort(b.port1);
    const clientA = await connectFileExplorer(a.port2);
    const clientB = await connectFileExplorer(b.port2);
    const rootId = clientA.getSnapshot().roots()[0].id;
    assert.equal(clientB.getSnapshot().roots()[0].id, rootId);

    clientA.setExpanded({ add: [rootId] });
    clientB.setExpanded({ add: [rootId] });
    await waitFor(() => {
      const snapshot = host.local.getSnapshot();
      return snapshot.childrenOf(rootId).length > 0 && !snapshot.directoryChildrenLoaded(rootId);
    });
    await waitFor(
      () =>
        clientA.getSnapshot().directoryLoadState(rootId).state === 'loading' &&
        clientB.getSnapshot().directoryLoadState(rootId).state === 'loading',
    );

    clientA.setExpanded({ remove: [rootId] });
    await waitFor(() => clientA.getSnapshot().directoryLoadState(rootId).state === 'idle');
    await waitFor(() => clientB.getSnapshot().directChildCount(rootId) === total, {
      timeoutMs: 8000,
    });
    await waitFor(() => clientB.getSnapshot().directoryLoadState(rootId).state === 'complete');
    assert.equal(host.local.getSnapshot().directoryChildrenLoaded(rootId), true);

    // Once the shared authoritative read is cached, the collapsed peer can
    // re-expand immediately without starting a second filesystem scan.
    clientA.setExpanded({ add: [rootId] });
    await waitFor(() => clientA.getSnapshot().directChildCount(rootId) === total);
    await waitFor(() => clientA.getSnapshot().directoryLoadState(rootId).state === 'complete');

    await clientA.dispose();
    await clientB.dispose();
    await host.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('local progressive resync honors AbortSignal without marking a partial page complete', async () => {
  const dir = tempRoot();
  const fx = new FileExplorer({ roots: [dir] });
  try {
    const total = 1024;
    for (let i = 0; i < total; i++) {
      writeFileSync(join(dir, `abort-${String(i).padStart(5, '0')}.txt`), 'x');
    }
    fx.seedWorkspaceRoots();
    const rootId = fx.getSnapshot().roots()[0].id;
    const controller = new AbortController();
    const pending = fx.resyncProgressive(rootId, {
      operationId: 'test-progressive-abort',
      batchSize: 16,
      signal: controller.signal,
    });
    await waitFor(() => {
      const snapshot = fx.getSnapshot();
      return snapshot.childrenOf(rootId).length > 0 && !snapshot.directoryChildrenLoaded(rootId);
    });
    controller.abort();
    await assert.rejects(pending, (error) => error?.code === 'ECANCELED');
    assert.equal(
      fx.getSnapshot().directoryChildrenLoaded(rootId),
      false,
      'aborted prefix remains retryable rather than masquerading as complete',
    );
  } finally {
    await fx.dispose();
    removeTempDir(dir);
  }
});

test('progressive reads can be cancelled immediately and release their operation id', async () => {
  const dir = tempRoot();
  const fx = new FileExplorer({ roots: [dir], compactFolders: false });
  let pending;
  try {
    for (let i = 0; i < 512; i++) writeFileSync(join(dir, `entry-${i}.txt`), '');
    fx.seedWorkspaceRoots();
    const rootId = fx.getSnapshot().roots()[0].id;
    const operationId = 'immediate-directory-cancel';
    pending = fx.resyncProgressive(rootId, { operationId, batchSize: 16 });
    const rejected = assert.rejects(pending, (error) => error?.code === 'ECANCELED');
    const cancelled = fx.cancelOperation(operationId);
    await rejected;
    assert.equal(cancelled, true, 'registered before the call returns');
    assert.equal(fx.cancelOperation(operationId), false, 'settled operations release their ids');
    assert.equal(fx.getSnapshot().directoryChildrenLoaded(rootId), false);

    const controller = new AbortController();
    pending = fx.resyncProgressive(rootId, {
      operationId,
      batchSize: 16,
      signal: controller.signal,
    });
    controller.abort();
    await assert.rejects(pending, (error) => error?.code === 'ECANCELED');
    assert.equal(fx.cancelOperation(operationId), false);
    assert.equal(fx.getSnapshot().directoryChildrenLoaded(rootId), false);

    await fx.resyncProgressive(rootId, { operationId, batchSize: 16 });
    assert.equal(fx.getSnapshot().directoryChildrenLoaded(rootId), true);
    assert.equal(fx.getSnapshot().childrenOf(rootId).length, 512);
    assert.equal(fx.cancelOperation(operationId), false);
  } finally {
    await pending?.catch(() => {});
    await fx.dispose();
    removeTempDir(dir);
  }
});

test('duplicate progressive operation ids reject without cancelling the original read', async () => {
  const dir = tempRoot();
  const fx = new FileExplorer({ roots: [dir], compactFolders: false });
  let pending;
  try {
    for (let i = 0; i < 512; i++) writeFileSync(join(dir, `entry-${i}.txt`), '');
    fx.seedWorkspaceRoots();
    const rootId = fx.getSnapshot().roots()[0].id;
    const options = { operationId: 'duplicate-directory-read', batchSize: 16 };
    pending = fx.resyncProgressive(rootId, options);
    void pending.catch(() => {});
    const controller = new AbortController();
    const duplicate = fx.resyncProgressive(rootId, { ...options, signal: controller.signal });
    controller.abort();
    await assert.rejects(duplicate, (error) => error?.code === 'EINVAL');
    await pending;
    assert.equal(fx.getSnapshot().directoryChildrenLoaded(rootId), true);
    assert.equal(fx.cancelOperation(options.operationId), false);
  } finally {
    await pending?.catch(() => {});
    await fx.dispose();
    removeTempDir(dir);
  }
});

test('progressive setup failures leave no registered operation behind', async () => {
  const dir = tempRoot();
  const fx = new FileExplorer({ roots: [dir] });
  try {
    fx.seedWorkspaceRoots();
    const rootId = fx.getSnapshot().roots()[0].id;
    const options = { operationId: 'failed-directory-setup' };
    const controller = new AbortController();
    controller.abort();
    await assert.rejects(
      fx.resyncProgressive(rootId, { ...options, signal: controller.signal }),
      (error) => error?.code === 'ECANCELED',
    );
    assert.equal(fx.cancelOperation(options.operationId), false);
    await assert.rejects(
      fx.resyncProgressive(9_999_999, options),
      (error) => error instanceof FileSystemError && error.code === 'EINVAL',
    );
    assert.equal(fx.cancelOperation(options.operationId), false);

    // Disposal is checked by the worker, after synchronous registration.
    // Its rejection must release the id just like cancellation and success.
    await fx.dispose();
    await assert.rejects(fx.resyncProgressive(rootId, options), /disposed/);
    assert.equal(fx.cancelOperation(options.operationId), false);
    assert.equal(fx.getSnapshot().directoryChildrenLoaded(rootId), false);
  } finally {
    await fx.dispose();
    removeTempDir(dir);
  }
});

test('compact-chain failures stay visible and retry without collapsing the expanded parent', async () => {
  const dir = tempRoot();
  mkdirSync(join(dir, 'a', 'b'), { recursive: true });
  const host = await createFileExplorerHost({
    roots: [dir],
    initialWalk: 'roots-only',
    compactFolders: true,
  });
  const resync = host.local.resyncProgressive.bind(host.local);
  let rejectChild;
  let client;
  let injected = false;
  host.local.resyncProgressive = (id, options) => {
    if (!injected && host.local.getSnapshot().getById(id)?.name === 'a') {
      injected = true;
      return new Promise((_, reject) => {
        rejectChild = reject;
      });
    }
    return resync(id, options);
  };
  try {
    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    client = await connectFileExplorer(port2);
    const rootId = client.getSnapshot().roots()[0].id;
    client.setExpanded({ add: [rootId] });
    await waitFor(
      () => rejectChild !== undefined && client.getSnapshot().directChildCount(rootId) === 1,
    );
    assert.equal(client.getSnapshot().directoryLoadState(rootId).state, 'loading');
    assert.ok(
      client
        .getSnapshot()
        .visibleRowCount(new Set([rootId]))
        .pendingExpansions.has(rootId),
    );

    rejectChild(new FileSystemError('EACCES', 'compact child is inaccessible'));
    await waitFor(() => client.getSnapshot().directoryLoadState(rootId).state === 'error');
    assert.deepEqual(client.getSnapshot().directoryLoadState(rootId), {
      state: 'error',
      code: 'EACCES',
      message: 'compact child is inaccessible',
    });
    assert.equal(
      client
        .getSnapshot()
        .visibleRowCount(new Set([rootId]))
        .pendingExpansions.has(rootId),
      false,
    );

    client.setExpanded({ add: [rootId] });
    assert.equal(client.getSnapshot().directoryLoadState(rootId).state, 'loading');
    await waitFor(() => client.getSnapshot().directoryLoadState(rootId).state === 'complete');
    const rows = client
      .getSnapshot()
      .visibleRows({ expanded: new Set([rootId]), offset: 0, limit: 10 });
    assert.equal(rows[1]?.name, 'b');
    assert.deepEqual(rows[1]?.pathSegments, ['a', 'b']);
  } finally {
    rejectChild?.(new FileSystemError('ECANCELED', 'test cleanup'));
    await client?.dispose();
    await host.dispose();
    removeTempDir(dir);
  }
});

test('host disposal cancels and joins an in-flight progressive directory read', async () => {
  const dir = tempRoot();
  let host;
  let client;
  try {
    for (let i = 0; i < 1024; i++) {
      writeFileSync(join(dir, `dispose-${String(i).padStart(5, '0')}.txt`), 'x');
    }
    host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
      compactFolders: false,
      directoryBatchSize: 16,
    });
    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    client = await connectFileExplorer(port2);
    const rootId = client.getSnapshot().roots()[0].id;
    client.setExpanded({ add: [rootId] });
    await waitFor(() => {
      const snapshot = host.local.getSnapshot();
      return snapshot.childrenOf(rootId).length > 0 && !snapshot.directoryChildrenLoaded(rootId);
    });

    await host.dispose();
    host = undefined;
  } finally {
    await client?.dispose().catch(() => {});
    await host?.dispose().catch(() => {});
    removeTempDir(dir);
  }
});

test('invalid expansion surfaces structured retryable error state', async () => {
  const dir = tempRoot();
  try {
    const host = await createFileExplorerHost({ roots: [dir], initialWalk: 'roots-only' });
    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    const client = await connectFileExplorer(port2);
    const invalidId = 9_999_999;
    client.setExpanded({ add: [invalidId] });
    const state = await waitFor(() => {
      const current = client.getSnapshot().directoryLoadState(invalidId);
      return current.state === 'error' ? current : null;
    });
    assert.equal(state.code, 'EINVAL');
    assert.match(state.message, /not in the current snapshot/);
    await client.dispose();
    await host.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('re-expanding an already-walked folder does not re-trigger a walk', async () => {
  // Guard against accidental re-walks: authoritative native listing state
  // makes `setExpanded({add:[id]})` after completion a no-op on the native
  // side. We observe the explicit load-state channel rather than inferring a
  // scan from timing-sensitive viewport refill frames.
  const dir = tempRoot();
  let host;
  let client;
  try {
    mkdirSync(join(dir, 'a'));
    writeFileSync(join(dir, 'a', 'x.txt'), '1');

    host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
    });

    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);

    const observedLoadFrames = [];
    port2.on('message', (raw) => {
      if (raw && raw.type === 'directoryLoad') observedLoadFrames.push(raw.body);
    });

    client = await connectFileExplorer(port2);

    const rootId = await waitFor(() => {
      const roots = client.getSnapshot().roots();
      return roots.length === 1 ? roots[0].id : null;
    });

    // First expand — triggers walk.
    client.setExpanded({ add: [rootId] });
    await waitFor(() => client.getSnapshot().directoryLoadState(rootId).state === 'complete');
    const loadsAfterFirstExpand = observedLoadFrames.length;

    // Collapse + re-expand. The collapse is a pure session-state change
    // on the client (setExpanded({remove}) ships to the host, which
    // removes from its Session.expanded set; no walk is triggered).
    client.setExpanded({ remove: [rootId] });
    client.setExpanded({ add: [rootId] });

    // The ordered channel delivers any loading transition before completion.
    // Wait for that boundary instead of guessing how many tick windows suffice.
    await waitFor(() =>
      observedLoadFrames.slice(loadsAfterFirstExpand).some((body) => body.state === 'complete'),
    );

    const secondLoadFrames = observedLoadFrames.slice(loadsAfterFirstExpand);
    assert.equal(
      secondLoadFrames.filter((body) => body.state === 'loading').length,
      0,
      're-expand of an authoritative folder must not start another native read',
    );
    assert.ok(
      secondLoadFrames.some((body) => body.state === 'complete'),
      'host reports cached completion immediately',
    );
  } finally {
    await client?.dispose();
    await host?.dispose();
    removeTempDir(dir);
  }
});

test('initialWalk=none leaves store empty until explicit populateFromRoots', async () => {
  // The escape hatch: consumers who want full manual control set
  // initialWalk: 'none' and drive hydration themselves.
  const dir = tempRoot();
  try {
    writeFileSync(join(dir, 'a.txt'), 'a');
    const host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'none',
    });

    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    const client = await connectFileExplorer(port2);

    // No roots seed — give the host a few ticks to prove it really
    // doesn't walk on its own.
    await new Promise((r) => setTimeout(r, 80));
    assert.equal(
      client.getSnapshot().roots().length,
      0,
      'initialWalk=none: no roots seeded automatically',
    );

    // Now walk manually — B1 delta-roots path picks it up.
    await host.local.populateFromRoots();
    await waitFor(() => client.getSnapshot().roots().length === 1);

    assert.equal(client.getSnapshot().roots().length, 1, 'manual walk seeded one root');

    await client.dispose();
    await host.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('expanding a folder that lazy hydration touched still walks its whole child list', async () => {
  // Under lazy expansion a folder can hold children WITHOUT having been
  // walked: `getByUri` hydrates one ancestor chain at a time, and the SCM
  // decoration companion resolves every dirty path that way. Reading "has
  // children in the store" as "was walked" froze such a folder at whatever
  // subset the hydration happened to create — expand `packages` and you saw
  // the one package with an edit in it, never the rest.
  const dir = tempRoot();
  try {
    // packages/{design-kit,shell-ui,fieldd}; only design-kit is on the
    // hydrated chain.
    mkdirSync(join(dir, 'packages', 'design-kit'), { recursive: true });
    mkdirSync(join(dir, 'packages', 'shell-ui'), { recursive: true });
    mkdirSync(join(dir, 'packages', 'fieldd'), { recursive: true });
    writeFileSync(join(dir, 'packages', 'design-kit', 'tokens.css'), ':root{}');
    writeFileSync(join(dir, 'packages', 'shell-ui', 'index.ts'), 'export {};');
    writeFileSync(join(dir, 'packages', 'fieldd', 'index.ts'), 'export {};');

    const host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
      compactFolders: false,
    });
    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);
    const client = await connectFileExplorer(port2);

    const rootId = await waitFor(() => {
      const roots = client.getSnapshot().roots();
      return roots.length === 1 ? roots[0].id : null;
    });
    client.setExpanded({ add: [rootId] });
    const packagesId = await waitFor(() => {
      const hostSnap = host.local.getSnapshot();
      for (const id of hostSnap.childrenOf(rootId)) {
        if (hostSnap.getById(id)?.name === 'packages') return id;
      }
      return null;
    });

    // What the SCM companion does for a dirty file: resolve its path, which
    // hydrates the ancestor chain and nothing else.
    await host.local.getByUri({
      scheme: 'file',
      path: join(dir, 'packages', 'design-kit', 'tokens.css'),
    });
    assert.equal(
      host.local.getSnapshot().childrenOf(packagesId).length,
      1,
      'hydration left `packages` holding exactly the chain child',
    );

    // A partial child list may be rendered immediately, but it must remain
    // explicitly pending until the authoritative directory read completes.
    const expansionFrames = [];
    port2.on('message', (raw) => {
      if (raw && raw.type === 'delta') expansionFrames.push(raw.body);
    });
    await new Promise((resolve) => setTimeout(resolve, 30));
    expansionFrames.length = 0;
    client.setExpanded({ add: [packagesId] });
    const provisional = await waitFor(() =>
      expansionFrames.find((body) => body.childSetChanged?.includes(packagesId)),
    );
    assert.equal(
      Object.prototype.hasOwnProperty.call(provisional.directChildCounts, String(packagesId)),
      false,
      'provisional children do not claim listing completion',
    );

    const names = await waitFor(() => {
      const hostSnap = host.local.getSnapshot();
      const kids = hostSnap
        .childrenOf(packagesId)
        .map((id) => hostSnap.getById(id)?.name)
        .filter(Boolean);
      return kids.length >= 3 ? new Set(kids) : null;
    });
    assert.ok(names.has('design-kit'), 'expand keeps the hydrated child');
    assert.ok(names.has('shell-ui'), 'expand adds the sibling hydration never saw');
    assert.ok(names.has('fieldd'), 'expand adds every sibling, not just the first');
    await waitFor(() => client.getSnapshot().directoryChildrenLoaded(packagesId));

    await client.dispose();
    await host.dispose();
  } finally {
    removeTempDir(dir);
  }
});
