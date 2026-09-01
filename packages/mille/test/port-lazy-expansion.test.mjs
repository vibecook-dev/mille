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

import { createFileExplorerHost, connectFileExplorer } from '../dist/index.js';

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

test('re-expanding an already-walked folder does not re-trigger a walk', async () => {
  // Guard against accidental re-walks: authoritative native listing state
  // makes `setExpanded({add:[id]})` after completion a no-op on the native
  // side. We observe this by checking that no additional child-carrying
  // deltas fan out on the second expand.
  const dir = tempRoot();
  try {
    mkdirSync(join(dir, 'a'));
    writeFileSync(join(dir, 'a', 'x.txt'), '1');

    const host = await createFileExplorerHost({
      roots: [dir],
      initialWalk: 'roots-only',
    });

    const { port1, port2 } = new MessageChannel();
    host.attachPort(port1);

    const observedDeltaFrames = [];
    port2.on('message', (raw) => {
      if (raw && raw.type === 'delta') observedDeltaFrames.push(raw.body);
    });

    const client = await connectFileExplorer(port2);

    const rootId = await waitFor(() => {
      const roots = client.getSnapshot().roots();
      return roots.length === 1 ? roots[0].id : null;
    });

    // First expand — triggers walk.
    client.setExpanded({ add: [rootId] });
    await waitFor(() => {
      const s = client.getSnapshot();
      const kidCount = s.directChildCount(rootId) ?? 0;
      return kidCount >= 1 ? kidCount : null;
    });

    // Let any trailing deltas land before counting.
    await new Promise((r) => setTimeout(r, 50));
    const deltasAfterFirstExpand = observedDeltaFrames.length;

    // Collapse + re-expand. The collapse is a pure session-state change
    // on the client (setExpanded({remove}) ships to the host, which
    // removes from its Session.expanded set; no walk is triggered).
    client.setExpanded({ remove: [rootId] });
    await new Promise((r) => setTimeout(r, 40));
    client.setExpanded({ add: [rootId] });

    // Give the host a few tick windows. If the guard were broken, a
    // second prefetch would fire and potentially produce duplicate
    // child-insertion deltas. The walker's `populateFromPath` filter
    // (dedupe on path) means duplicates wouldn't actually land —
    // but firing the walk at all is wasted work.
    await new Promise((r) => setTimeout(r, 80));

    // Re-expansion does not walk again, but it does produce one bounded
    // viewport refill because collapsing changed the mounted host window to
    // the root alone.
    const entriesOnSecondExpand = observedDeltaFrames
      .slice(deltasAfterFirstExpand)
      .filter((b) => b.viewportPatch instanceof ArrayBuffer && b.viewportPatch.byteLength > 1);
    assert.equal(
      entriesOnSecondExpand.length,
      1,
      `re-expand should ship one viewport refill (saw ${entriesOnSecondExpand.length})`,
    );
    assert.ok(Array.isArray(entriesOnSecondExpand[0].viewportIds));

    await client.dispose();
    await host.dispose();
  } finally {
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
