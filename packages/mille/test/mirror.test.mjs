// Unit tests for mirror working-state helpers — Phase 8 commit 8.1.
//
// The working state itself is a plain object of Maps/Sets; these tests
// lock in constructor defaults and the shallow-clone contract the
// reducer relies on (mutating a cloned map MUST NOT leak back into the
// source).

import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { createMirror, cloneMirror } from '../dist/mirror.js';
import { ClientMirrorSnapshot } from '../dist/mirror-snapshot.js';
import {
  applySnapshot,
  applyDelta,
  applyDirectoryLoad,
  evictToCap,
  DEFAULT_MIRROR_CAP,
} from '../dist/mirror-reducer.js';

function entry(overrides = {}) {
  return {
    id: 0,
    parentId: null,
    name: '',
    kind: 0,
    size: 0,
    mtimeMs: 0,
    ctimeMs: 0,
    symlinkTargetIsDir: null,
    pathSegments: null,
    isIgnored: false,
    isReadonly: false,
    isHidden: false,
    ...overrides,
  };
}

test('createMirror returns empty working state', () => {
  const m = createMirror();
  assert.equal(m.byId.size, 0);
  assert.equal(m.children.size, 0);
  assert.equal(m.orderedChildren.size, 0);
  assert.equal(m.directChildCounts.size, 0);
  assert.equal(m.pendingExpansions.size, 0);
  assert.equal(m.expanded.size, 0);
  assert.equal(m.viewportIds.size, 0);
  assert.deepEqual(m.roots, []);
  assert.equal(m.treeVersion, 0);
  assert.equal(m.projectionVersion, 0);
  assert.equal(m.decorationVersion, 0);
  assert.equal(m.volatileSubtrees.size, 0);
});

test('cloneMirror returns independent map references', () => {
  const src = createMirror();
  src.byId.set(1, entry({ id: 1, name: 'root', kind: 1 }));
  src.directChildCounts.set(1, 3);
  src.orderedChildren.add(1);
  src.pendingExpansions.add(1);
  src.directoryLoads.set(1, { generation: 3, state: 'loading' });
  src.expanded.add(1);
  src.viewportIds.add(1);
  src.roots.push(1);
  src.volatileSubtrees.add(1);
  src.treeVersion = 5;
  src.projectionVersion = 4;
  src.decorationVersion = 2;

  const clone = cloneMirror(src);

  // Mutations to the clone must not leak back.
  clone.byId.set(2, entry({ id: 2, parentId: 1, name: 'child', kind: 0, size: 10 }));
  clone.directChildCounts.set(2, 0);
  clone.orderedChildren.add(2);
  clone.pendingExpansions.add(2);
  clone.directoryLoads.set(2, { generation: 4, state: 'loading' });
  clone.expanded.add(2);
  clone.viewportIds.add(2);
  clone.roots.push(2);
  clone.volatileSubtrees.add(2);
  clone.treeVersion = 99;
  clone.projectionVersion = 98;
  clone.decorationVersion = 42;

  assert.equal(src.byId.size, 1, 'byId on src untouched');
  assert.equal(src.directChildCounts.size, 1, 'directChildCounts on src untouched');
  assert.equal(src.orderedChildren.size, 1, 'orderedChildren on src untouched');
  assert.equal(src.pendingExpansions.size, 1, 'pendingExpansions on src untouched');
  assert.equal(src.directoryLoads.size, 1, 'directoryLoads on src untouched');
  assert.equal(src.expanded.size, 1, 'expanded on src untouched');
  assert.equal(src.viewportIds.size, 1, 'viewportIds on src untouched');
  assert.deepEqual(src.roots, [1], 'roots on src untouched');
  assert.equal(src.volatileSubtrees.size, 1, 'volatileSubtrees on src untouched');
  assert.equal(src.treeVersion, 5, 'treeVersion on src untouched');
  assert.equal(src.projectionVersion, 4, 'projectionVersion on src untouched');
  assert.equal(src.decorationVersion, 2, 'decorationVersion on src untouched');

  // Clone carries forward the initial content.
  assert.ok(clone.byId.has(1));
  assert.equal(clone.directChildCounts.get(1), 3);
  assert.equal(clone.projectionVersion, 98);
  assert.ok(clone.orderedChildren.has(1));
  assert.ok(clone.pendingExpansions.has(1));
  assert.ok(clone.expanded.has(1));
  assert.ok(clone.viewportIds.has(1));
  assert.ok(clone.volatileSubtrees.has(1));
});

// ─── ClientMirrorSnapshot (8.2) ──────────────────────────────────────

test('ClientMirrorSnapshot exposes treeVersion + decorationVersion', () => {
  const m = createMirror();
  m.treeVersion = 7;
  m.decorationVersion = 3;
  const snap = new ClientMirrorSnapshot(m);
  assert.equal(snap.treeVersion, 7);
  assert.equal(snap.decorationVersion, 3);
});

test('ClientMirrorSnapshot is frozen', () => {
  const snap = new ClientMirrorSnapshot(createMirror());
  assert.equal(Object.isFrozen(snap), true);
});

test('ClientMirrorSnapshot.roots() returns entries in stored order', () => {
  const m = createMirror();
  m.byId.set(10, entry({ id: 10, name: 'a', kind: 1 }));
  m.byId.set(20, entry({ id: 20, name: 'b', kind: 1 }));
  m.roots.push(10, 20);
  const snap = new ClientMirrorSnapshot(m);
  const roots = snap.roots();
  assert.equal(roots.length, 2);
  assert.equal(roots[0].id, 10);
  assert.equal(roots[0].name, 'a');
  assert.equal(roots[1].id, 20);
});

test('ClientMirrorSnapshot.roots() skips ids missing from byId', () => {
  const m = createMirror();
  m.byId.set(10, entry({ id: 10, name: 'a', kind: 1 }));
  m.roots.push(10, 999);
  const snap = new ClientMirrorSnapshot(m);
  const roots = snap.roots();
  assert.equal(roots.length, 1);
  assert.equal(roots[0].id, 10);
});

test('ClientMirrorSnapshot.getById returns null for unknown ids', () => {
  const m = createMirror();
  const snap = new ClientMirrorSnapshot(m);
  assert.equal(snap.getById(42), null);
});

test('ClientMirrorSnapshot.getById translates null holes to undefined', () => {
  const m = createMirror();
  m.byId.set(1, entry({ id: 1, name: 'x', kind: 0 }));
  const snap = new ClientMirrorSnapshot(m);
  const e = snap.getById(1);
  assert.ok(e);
  assert.equal(e.id, 1);
  // Public Entry uses undefined-holes, not null.
  assert.equal(e.symlinkTargetIsDir, undefined);
  assert.equal(e.pathSegments, undefined);
});

test('ClientMirrorSnapshot.getById preserves optional fields when present', () => {
  const m = createMirror();
  m.byId.set(
    1,
    entry({
      id: 1,
      name: 'lnk',
      kind: 2,
      symlinkTargetIsDir: true,
      pathSegments: ['a', 'b', 'c'],
    }),
  );
  const snap = new ClientMirrorSnapshot(m);
  const e = snap.getById(1);
  assert.ok(e);
  assert.equal(e.symlinkTargetIsDir, true);
  assert.deepEqual(e.pathSegments, ['a', 'b', 'c']);
});

test('ClientMirrorSnapshot.directChildCount returns null when unknown', () => {
  const m = createMirror();
  const snap = new ClientMirrorSnapshot(m);
  assert.equal(snap.directChildCount(1), null);
  assert.equal(snap.directoryChildrenLoaded(1), false);
});

test('ClientMirrorSnapshot.directChildCount returns cached value', () => {
  const m = createMirror();
  m.directChildCounts.set(1, 4);
  m.directChildCounts.set(2, 0);
  const snap = new ClientMirrorSnapshot(m);
  assert.equal(snap.directChildCount(1), 4);
  assert.equal(snap.directChildCount(2), 0);
  assert.equal(snap.directoryChildrenLoaded(1), true);
  assert.equal(snap.directoryChildrenLoaded(2), true, 'zero children is still loaded');
});

test('ClientMirrorSnapshot.hasChildren only true for >0 count', () => {
  const m = createMirror();
  m.directChildCounts.set(1, 0);
  m.directChildCounts.set(2, 3);
  const snap = new ClientMirrorSnapshot(m);
  assert.equal(snap.hasChildren(1), false);
  assert.equal(snap.hasChildren(2), true);
  assert.equal(snap.hasChildren(999), false);
});

test('directory load state distinguishes loading, error, and completion', () => {
  let state = createMirror();
  state = applyDirectoryLoad(state, { id: 7, generation: 1, state: 'loading' });
  assert.deepEqual(new ClientMirrorSnapshot(state).directoryLoadState(7), { state: 'loading' });
  assert.ok(state.pendingExpansions.has(7));

  state = applyDirectoryLoad(state, {
    id: 7,
    generation: 1,
    state: 'error',
    error: { code: 'EACCES', message: 'permission denied' },
  });
  assert.deepEqual(new ClientMirrorSnapshot(state).directoryLoadState(7), {
    state: 'error',
    code: 'EACCES',
    message: 'permission denied',
  });
  assert.equal(state.pendingExpansions.has(7), false);

  state.directChildCounts.set(7, 0);
  state = applyDirectoryLoad(state, { id: 7, generation: 1, state: 'complete' });
  assert.deepEqual(new ClientMirrorSnapshot(state).directoryLoadState(7), { state: 'complete' });
});

test('stale directory load generations cannot overwrite a newer retry', () => {
  let state = createMirror();
  state = applyDirectoryLoad(state, { id: 7, generation: 2, state: 'loading' });
  const current = state;
  state = applyDirectoryLoad(state, {
    id: 7,
    generation: 1,
    state: 'error',
    error: { code: 'EIO', message: 'stale failure' },
  });
  assert.equal(state, current, 'stale completion state preserves snapshot identity');
  assert.deepEqual(new ClientMirrorSnapshot(state).directoryLoadState(7), { state: 'loading' });
  assert.ok(state.pendingExpansions.has(7));
});

test('partial child pages remain pending until the final count arrives', () => {
  const state = createMirror();
  state.pendingExpansions.add(1);
  state.directoryLoads.set(1, { generation: 1, state: 'loading' });
  const partial = applyDelta(state, {
    version: 1,
    changedIds: [],
    removedIds: [],
    directChildCounts: {},
    childSetChanged: [1],
    childLists: { 1: [2] },
    coarseSubtrees: [],
    subtreeDirty: [],
    subtreeResynced: [],
  });
  assert.ok(partial.pendingExpansions.has(1), 'partial page keeps spinner active');

  const complete = applyDelta(partial, {
    version: 2,
    changedIds: [],
    removedIds: [],
    directChildCounts: { 1: 1 },
    childSetChanged: [1],
    childLists: { 1: [2] },
    coarseSubtrees: [],
    subtreeDirty: [],
    subtreeResynced: [],
  });
  assert.equal(complete.pendingExpansions.has(1), false);
});

test('ClientMirrorSnapshot.getDecorations returns empty array', () => {
  const snap = new ClientMirrorSnapshot(createMirror());
  assert.deepEqual(snap.getDecorations(1), []);
});

// visibleRows lands in 8.4 — coverage in test/visible-rows.test.mjs.
// visibleRowCount lands in 8.5 — coverage in test/visible-row-count.test.mjs.

// ─── 8.8: mirrorCap eviction ──────────────────────────────────────────

test('DEFAULT_MIRROR_CAP is 4096', () => {
  assert.equal(DEFAULT_MIRROR_CAP, 4096);
});

test('applySnapshot respects mirrorCap — 5000 entries trimmed to cap', () => {
  const entries = [];
  for (let i = 1; i <= 5000; i++) {
    entries.push(entry({ id: i, name: `f${i}`, kind: 0 }));
  }
  const next = applySnapshot(createMirror(), {
    version: 1,
    roots: [],
    entriesJson: JSON.stringify(entries),
    directChildCounts: {},
    visibleCount: 5000,
  });
  assert.ok(next.byId.size <= 4096, `byId.size=${next.byId.size}`);
});

test('applySnapshot: roots are pinned and never evicted', () => {
  // 100 non-root entries + 2 roots; cap at 10. The two roots must
  // survive even though they'd otherwise get evicted by touch order.
  const entries = [entry({ id: 1, name: 'root-a', kind: 1 })];
  entries.push(entry({ id: 2, name: 'root-b', kind: 1 }));
  for (let i = 10; i < 110; i++) {
    entries.push(entry({ id: i, name: `f${i}`, kind: 0 }));
  }
  const next = applySnapshot(
    createMirror(),
    {
      version: 1,
      roots: [1, 2],
      entriesJson: JSON.stringify(entries),
      directChildCounts: {},
      visibleCount: entries.length,
    },
    10,
  );
  assert.ok(next.byId.size <= 10);
  assert.ok(next.byId.has(1), 'root-a retained');
  assert.ok(next.byId.has(2), 'root-b retained');
});

test('applyDelta: pendingExpansions are pinned across cap eviction', () => {
  const state = createMirror();
  state.pendingExpansions.add(500);
  // Seed with an old entry for id 500.
  state.byId.set(500, entry({ id: 500, name: 'pinned', kind: 1 }));
  state.lruTouch.set(500, 1);
  state.lruCounter = 1;
  // Now flood with 50 fresh entries at cap=10.
  const entries = [];
  for (let i = 600; i < 650; i++) {
    entries.push(entry({ id: i, name: `f${i}`, kind: 0 }));
  }
  const next = applyDelta(
    state,
    {
      version: 2,
      changedIds: entries.map((e) => e.id),
      entriesJson: JSON.stringify(entries),
      removedIds: [],
      directChildCounts: {},
      coarseSubtrees: [],
      subtreeDirty: [],
      subtreeResynced: [],
    },
    10,
  );
  assert.ok(next.byId.size <= 10);
  assert.ok(next.byId.has(500), 'pending-expansion target retained');
});

test('applyDelta: expanded folders are pinned across cap eviction', () => {
  const state = createMirror();
  state.expanded.add(500);
  state.byId.set(500, entry({ id: 500, name: 'expanded', kind: 1 }));
  state.lruTouch.set(500, 1);
  state.lruCounter = 1;
  const entries = [];
  for (let i = 600; i < 650; i++) {
    entries.push(entry({ id: i, name: `f${i}`, kind: 0 }));
  }
  const next = applyDelta(
    state,
    {
      version: 2,
      changedIds: entries.map((e) => e.id),
      entriesJson: JSON.stringify(entries),
      removedIds: [],
      directChildCounts: {},
      coarseSubtrees: [],
      subtreeDirty: [],
      subtreeResynced: [],
    },
    10,
  );
  assert.ok(next.byId.size <= 10);
  assert.ok(next.byId.has(500), 'expanded folder retained');
});

test('applyDelta: viewport patch pins incoming rows and releases the old window', () => {
  const state = createMirror();
  state.roots.push(1);
  state.viewportIds.add(2);
  state.children.set(1, [2, 3, 4, 5]);
  for (const id of [1, 2, 3]) {
    state.byId.set(id, entry({ id, parentId: id === 1 ? null : 1, name: `f${id}` }));
    state.lruTouch.set(id, id);
  }
  state.lruCounter = 3;

  const incoming = [
    entry({ id: 4, parentId: 1, name: 'f4' }),
    entry({ id: 5, parentId: 1, name: 'f5' }),
  ];
  const next = applyDelta(
    state,
    {
      version: 2,
      changedIds: [],
      entriesJson: JSON.stringify(incoming),
      viewportIds: [4, 5],
      removedIds: [],
      directChildCounts: {},
      coarseSubtrees: [],
      subtreeDirty: [],
      subtreeResynced: [],
    },
    3,
  );

  assert.deepEqual([...next.viewportIds], [4, 5]);
  assert.deepEqual(
    [...next.byId.keys()].sort((a, b) => a - b),
    [1, 4, 5],
  );
  assert.deepEqual(next.children.get(1), [2, 3, 4, 5], 'viewport refill keeps structural order');
});

test('applyDelta: explicit mirrorCap honored', () => {
  const entries = [];
  for (let i = 1; i <= 200; i++) {
    entries.push(entry({ id: i, name: `f${i}`, kind: 0 }));
  }
  const next = applyDelta(
    createMirror(),
    {
      version: 1,
      changedIds: entries.map((e) => e.id),
      entriesJson: JSON.stringify(entries),
      removedIds: [],
      directChildCounts: {},
      coarseSubtrees: [],
      subtreeDirty: [],
      subtreeResynced: [],
    },
    50,
  );
  assert.ok(next.byId.size <= 50, `byId.size=${next.byId.size}`);
});

test('evictToCap: no-op when under cap', () => {
  const m = createMirror();
  m.byId.set(1, entry({ id: 1, name: 'a' }));
  m.lruTouch.set(1, 1);
  evictToCap(m, 10, new Set());
  assert.equal(m.byId.size, 1);
});

test('evictToCap: oldest touches evicted first', () => {
  const m = createMirror();
  m.byId.set(1, entry({ id: 1, name: 'oldest' }));
  m.byId.set(2, entry({ id: 2, name: 'mid' }));
  m.byId.set(3, entry({ id: 3, name: 'newest' }));
  m.lruTouch.set(1, 1);
  m.lruTouch.set(2, 2);
  m.lruTouch.set(3, 3);
  evictToCap(m, 2, new Set());
  assert.equal(m.byId.size, 2);
  assert.equal(m.byId.has(1), false, 'oldest evicted');
  assert.ok(m.byId.has(3), 'newest retained');
});

test('evictToCap: purges aliased caches', () => {
  const m = createMirror();
  m.byId.set(1, entry({ id: 1, name: 'a' }));
  m.byId.set(2, entry({ id: 2, name: 'b' }));
  m.children.set(1, [10, 11]);
  m.directChildCounts.set(1, 2);
  m.lruTouch.set(1, 1);
  m.lruTouch.set(2, 2);
  evictToCap(m, 1, new Set());
  assert.equal(m.children.has(1), false);
  assert.equal(m.directChildCounts.has(1), false);
  assert.equal(m.lruTouch.has(1), false);
});
