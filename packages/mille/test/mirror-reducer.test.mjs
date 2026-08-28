// Unit tests for the ViewportMirror delta reducer — Phase 8 commit 8.3.
//
// Locks in the invariants called out in mirror-reducer.ts:
//   - snapshot replaces state wholesale, delta merges incrementally
//   - entries prefer binary payloads and retain `entriesJson` fallback
//   - removedIds drops aliased caches (children, directChildCounts,
//     pendingExpansions, volatileSubtrees)
//   - directChildCounts merge: new keys + updates to existing
//   - coarseSubtrees: drops known child list + marks pending
//   - subtreeDirty / subtreeResynced flip the volatileSubtrees flag

import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { createMirror } from '../dist/mirror.js';
import { applySnapshot, applyDelta, hydrateLookupEntries } from '../dist/mirror-reducer.js';
import { encodeChildLists } from '../dist/child-list-codec.js';
import { encodeClientEntries } from '../dist/entry-codec.js';

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

function emptyDelta(overrides = {}) {
  return {
    version: 0,
    changedIds: [],
    removedIds: [],
    directChildCounts: {},
    coarseSubtrees: [],
    subtreeDirty: [],
    subtreeResynced: [],
    ...overrides,
  };
}

// ─── applySnapshot ──────────────────────────────────────────────────

test('applySnapshot replaces state wholesale', () => {
  const prev = createMirror();
  // Seed stale state to prove it gets wiped.
  prev.byId.set(42, entry({ id: 42, name: 'stale' }));
  prev.directChildCounts.set(42, 7);
  prev.pendingExpansions.add(42);
  prev.volatileSubtrees.add(42);
  prev.treeVersion = 3;

  const next = applySnapshot(prev, {
    version: 10,
    roots: [1, 2],
    entriesJson: JSON.stringify([
      entry({ id: 1, name: 'a', kind: 1 }),
      entry({ id: 2, name: 'b', kind: 1 }),
    ]),
    directChildCounts: { 1: 3, 2: 0 },
    visibleCount: 2,
  });

  assert.equal(next.treeVersion, 10);
  assert.deepEqual(next.roots, [1, 2]);
  assert.equal(next.byId.size, 2);
  assert.equal(next.byId.get(1).name, 'a');
  assert.equal(next.directChildCounts.get(1), 3);
  assert.equal(next.directChildCounts.get(2), 0);
  // Stale state is gone.
  assert.equal(next.byId.has(42), false);
  assert.equal(next.directChildCounts.has(42), false);
  assert.equal(next.pendingExpansions.has(42), false);
  assert.equal(next.volatileSubtrees.has(42), false);
});

test('applySnapshot tolerates missing entriesJson', () => {
  const next = applySnapshot(createMirror(), {
    version: 1,
    roots: [],
    directChildCounts: {},
    visibleCount: 0,
  });
  assert.equal(next.treeVersion, 1);
  assert.equal(next.byId.size, 0);
});

test('applySnapshot carries host visibility policy into the mirror', () => {
  const next = applySnapshot(createMirror(), {
    version: 1,
    roots: [],
    directChildCounts: {},
    visibleCount: 0,
    visibility: {
      showHiddenFiles: false,
      showIgnoredFiles: true,
    },
  });
  assert.equal(next.showHiddenFiles, false);
  assert.equal(next.showIgnoredFiles, true);
});

test('applySnapshot tolerates empty-string entriesJson', () => {
  const next = applySnapshot(createMirror(), {
    version: 1,
    roots: [],
    entriesJson: '',
    directChildCounts: {},
    visibleCount: 0,
  });
  assert.equal(next.byId.size, 0);
});

test('applySnapshot decodes binary mirror entries', () => {
  const next = applySnapshot(createMirror(), {
    version: 2,
    roots: [7],
    mirror: encodeClientEntries([entry({ id: 7, name: 'binary-root', kind: 1 })]),
    directChildCounts: { 7: 0 },
    visibleCount: 1,
  });
  assert.equal(next.byId.get(7).name, 'binary-root');
});

// ─── applyDelta: entries ────────────────────────────────────────────

test('applyDelta adds entries from entriesJson', () => {
  const state = createMirror();
  const next = applyDelta(
    state,
    emptyDelta({
      version: 5,
      changedIds: [1, 2],
      entriesJson: JSON.stringify([entry({ id: 1, name: 'a' }), entry({ id: 2, name: 'b' })]),
    }),
  );
  assert.equal(next.treeVersion, 5);
  assert.equal(next.byId.get(1).name, 'a');
  assert.equal(next.byId.get(2).name, 'b');
});

test('applyDelta decodes binary viewport entries', () => {
  const next = applyDelta(
    createMirror(),
    emptyDelta({
      version: 6,
      changedIds: [8],
      viewportPatch: encodeClientEntries([entry({ id: 8, name: 'binary-row' })]),
      viewportIds: [8],
    }),
  );
  assert.equal(next.byId.get(8).name, 'binary-row');
  assert.deepEqual([...next.viewportIds], [8]);
});

test('applyDelta retains packed authoritative child order without copying', () => {
  const childListsBin = encodeChildLists(new Map([[7, [11, 9, 13]]]));
  const next = applyDelta(
    createMirror(),
    emptyDelta({
      version: 6,
      childSetChanged: [7],
      childListsBin,
    }),
  );
  const ids = next.children.get(7);
  assert.ok(ids instanceof Uint32Array);
  assert.deepEqual(Array.from(ids), [11, 9, 13]);
  assert.equal(ids.buffer, childListsBin);
  assert.ok(next.orderedChildren.has(7));
});

test('applyDelta updates existing entries (changedIds)', () => {
  const state = createMirror();
  state.byId.set(1, entry({ id: 1, name: 'old' }));
  const next = applyDelta(
    state,
    emptyDelta({
      version: 2,
      changedIds: [1],
      entriesJson: JSON.stringify([entry({ id: 1, name: 'new' })]),
    }),
  );
  assert.equal(next.byId.get(1).name, 'new');
  // Source untouched.
  assert.equal(state.byId.get(1).name, 'old');
});

test('applyDelta replaces root order without mutating the retained mirror', () => {
  const state = createMirror();
  state.roots = [1, 2, 3];
  const next = applyDelta(
    state,
    emptyDelta({
      version: 2,
      roots: [3, 1, 2],
    }),
  );

  assert.deepEqual(next.roots, [3, 1, 2]);
  assert.deepEqual(state.roots, [1, 2, 3]);
});

test('applyDelta atomically replaces the projection policy', () => {
  const state = createMirror();
  const initialProjectionVersion = state.projectionVersion;
  const next = applyDelta(
    state,
    emptyDelta({
      version: 2,
      visibility: {
        showHiddenFiles: false,
        showIgnoredFiles: false,
        compactFolders: true,
      },
    }),
  );

  assert.equal(next.showHiddenFiles, false);
  assert.equal(next.showIgnoredFiles, false);
  assert.equal(next.compactFolders, true);
  assert.equal(next.projectionVersion, initialProjectionVersion + 1);
  assert.equal(state.showHiddenFiles, true, 'source policy remains immutable');
  assert.equal(state.compactFolders, false, 'source policy remains immutable');
  assert.equal(state.projectionVersion, initialProjectionVersion);
});

test('applyDelta removes entries and purges aliased caches', () => {
  const state = createMirror();
  state.byId.set(1, entry({ id: 1 }));
  state.children.set(1, [2, 3]);
  state.directChildCounts.set(1, 2);
  state.pendingExpansions.add(1);
  state.volatileSubtrees.add(1);

  const next = applyDelta(
    state,
    emptyDelta({
      version: 2,
      removedIds: [1],
    }),
  );

  assert.equal(next.byId.has(1), false);
  assert.equal(next.children.has(1), false);
  assert.equal(next.directChildCounts.has(1), false);
  assert.equal(next.pendingExpansions.has(1), false);
  assert.equal(next.volatileSubtrees.has(1), false);
});

// ─── applyDelta: childSetChanged without a child list ──────────────
//
// The mirror is viewport-bounded and the host ships a child LIST only for
// the parents this session has expanded; every other parent whose child set
// moved arrives as a bare id in `childSetChanged`. Rebuilding such a parent
// from the mirror's own `byId` yields `[]` — which is a fact about this
// mirror, not about the filesystem. Writing it down as an authoritative
// empty list is what used to strip a collapsed folder's chevron for good:
// no chevron → no `setExpanded` → no walk → no children, forever.

test('applyDelta leaves a collapsed parent unknown rather than claiming it is empty', () => {
  const state = createMirror();
  state.byId.set(1, entry({ id: 1, name: 'root', kind: 1 }));
  state.byId.set(2, entry({ id: 2, parentId: 1, name: 'a', kind: 1 }));
  state.children.set(1, [2]);
  state.orderedChildren.add(1);

  // The host says "a's child set moved" but ships no list for it — `a` is
  // collapsed in this session, and none of its children are in the mirror.
  const next = applyDelta(
    state,
    emptyDelta({ version: 2, childSetChanged: [2], directChildCounts: { 2: 3 } }),
  );

  assert.equal(next.children.has(2), false, 'unknown, not empty');
  assert.equal(next.directChildCounts.get(2), 3, 'the authoritative count still lands');
});

test('applyDelta keeps a rebuilt non-empty child list', () => {
  const state = createMirror();
  state.byId.set(1, entry({ id: 1, name: 'root', kind: 1 }));
  state.byId.set(2, entry({ id: 2, parentId: 1, name: 'a', kind: 1 }));
  state.byId.set(3, entry({ id: 3, parentId: 2, name: 'b', kind: 1 }));
  state.children.set(2, [3]);

  const next = applyDelta(state, emptyDelta({ version: 2, childSetChanged: [2] }));

  assert.deepEqual([...(next.children.get(2) ?? [])], [3]);
});

test('applyDelta honours an authoritative empty child list', () => {
  const state = createMirror();
  state.byId.set(1, entry({ id: 1, name: 'root', kind: 1 }));
  state.byId.set(2, entry({ id: 2, parentId: 1, name: 'empty-dir', kind: 1 }));

  // An EXPANDED folder the host walked and found empty: the list ships, so
  // `[]` is a real claim and the mirror must record it.
  const next = applyDelta(
    state,
    emptyDelta({ version: 2, childSetChanged: [2], childLists: { 2: [] } }),
  );

  assert.deepEqual([...(next.children.get(2) ?? ['unset'])], []);
  assert.equal(next.orderedChildren.has(2), true);
});

// ─── applyDelta: directChildCounts ─────────────────────────────────

test('applyDelta merges new directChildCounts keys', () => {
  const state = createMirror();
  state.directChildCounts.set(1, 5);
  const next = applyDelta(
    state,
    emptyDelta({
      version: 2,
      directChildCounts: { 2: 3 },
    }),
  );
  assert.equal(next.directChildCounts.get(1), 5, 'existing keys preserved');
  assert.equal(next.directChildCounts.get(2), 3, 'new key added');
});

test('applyDelta overwrites directChildCounts on update', () => {
  const state = createMirror();
  state.directChildCounts.set(1, 5);
  const next = applyDelta(
    state,
    emptyDelta({
      version: 2,
      directChildCounts: { 1: 9 },
    }),
  );
  assert.equal(next.directChildCounts.get(1), 9);
});

// ─── applyDelta: coarseSubtrees ────────────────────────────────────

test('applyDelta coarse subtree clears children + marks pending', () => {
  const state = createMirror();
  state.children.set(7, [8, 9]);
  const next = applyDelta(
    state,
    emptyDelta({
      version: 3,
      coarseSubtrees: [7],
    }),
  );
  assert.equal(next.children.has(7), false, 'cached child list dropped');
  assert.equal(next.pendingExpansions.has(7), true, 'marked pending for re-query');
});

// ─── applyDelta: volatile flags ────────────────────────────────────

test('applyDelta subtreeDirty adds to volatileSubtrees', () => {
  const state = createMirror();
  const next = applyDelta(
    state,
    emptyDelta({
      version: 1,
      subtreeDirty: [42],
    }),
  );
  assert.equal(next.volatileSubtrees.has(42), true);
});

test('applyDelta subtreeResynced clears from volatileSubtrees', () => {
  const state = createMirror();
  state.volatileSubtrees.add(42);
  const next = applyDelta(
    state,
    emptyDelta({
      version: 2,
      subtreeResynced: [42],
    }),
  );
  assert.equal(next.volatileSubtrees.has(42), false);
});

test('applyDelta dirty+resynced on different roots each take effect', () => {
  const state = createMirror();
  state.volatileSubtrees.add(100);
  const next = applyDelta(
    state,
    emptyDelta({
      version: 3,
      subtreeDirty: [200],
      subtreeResynced: [100],
    }),
  );
  assert.equal(next.volatileSubtrees.has(100), false);
  assert.equal(next.volatileSubtrees.has(200), true);
});

// ─── applyDelta: treeVersion propagation ───────────────────────────

test('applyDelta propagates treeVersion even on empty body', () => {
  const state = createMirror();
  state.treeVersion = 1;
  const next = applyDelta(state, emptyDelta({ version: 99 }));
  assert.equal(next.treeVersion, 99);
});

// ─── immutability of the source state ──────────────────────────────

test('applyDelta does not mutate the source state', () => {
  const state = createMirror();
  state.byId.set(1, entry({ id: 1, name: 'orig' }));
  state.directChildCounts.set(1, 2);
  state.volatileSubtrees.add(5);

  applyDelta(
    state,
    emptyDelta({
      version: 7,
      changedIds: [1],
      entriesJson: JSON.stringify([entry({ id: 1, name: 'changed' })]),
      removedIds: [],
      directChildCounts: { 2: 4 },
      coarseSubtrees: [5],
      subtreeDirty: [9],
      subtreeResynced: [5],
    }),
  );

  assert.equal(state.treeVersion, 0, 'source treeVersion untouched');
  assert.equal(state.byId.get(1).name, 'orig', 'source entry untouched');
  assert.equal(state.directChildCounts.size, 1, 'source counts untouched');
  assert.equal(state.volatileSubtrees.has(5), true, 'source flags untouched');
  assert.equal(state.volatileSubtrees.has(9), false, 'source flags untouched');
});

test('hydrateLookupEntries merges and pins a path chain without inventing child lists', () => {
  const state = createMirror();
  state.byId.set(99, entry({ id: 99, name: 'cold' }));
  const chain = [
    entry({ id: 3, parentId: 2, name: 'target.txt' }),
    entry({ id: 2, parentId: 1, name: 'nested', kind: 1 }),
    entry({ id: 1, parentId: null, name: 'root', kind: 1 }),
  ];

  const next = hydrateLookupEntries(state, chain, 7, 2);
  assert.equal(next.treeVersion, 7);
  assert.equal(next.byId.get(3).name, 'target.txt');
  assert.equal(next.byId.get(2).parentId, 1);
  assert.equal(next.byId.has(99), false, 'cold entry is evicted before the reveal chain');
  assert.equal(next.children.size, 0, 'partial path must not masquerade as a directory listing');
  assert.equal(state.byId.has(3), false, 'source state remains immutable');
});

// A mirror's treeVersion must never move backwards.
//
// `tick()` emits a delta for marker-only reasons too — subtree resynced/dirty,
// root changes, decorations — and such a delta reports the version of an empty
// ChangeSet, which lags whatever the mirror has already applied. Assigning it
// dragged an up-to-date mirror backwards, and the mirror then acked the older
// version. That is what made `resync`'s synchronization guarantee fall through
// to its 1 s timeout roughly one run in twenty: the host waited for an ack at
// version N while the client, freshly regressed, kept insisting it was at 0.
test('applyDelta never moves treeVersion backwards', () => {
  const prev = createMirror();
  prev.treeVersion = 8;

  // Marker-only delta: no entries, no changed ids — exactly the shape a
  // resync marker produces once the periodic tick has drained the ChangeSet.
  const next = applyDelta(prev, {
    version: 0,
    changedIds: [],
    childSetChanged: [],
    removedIds: [],
    directChildCounts: {},
    newVisibleCount: 0,
    coarseSubtrees: [],
    subtreeDirty: [],
    subtreeResynced: [1],
  });

  assert.equal(next.treeVersion, 8, 'a stale marker delta must not regress the mirror');
});

test('applyDelta still advances treeVersion for newer deltas', () => {
  const prev = createMirror();
  prev.treeVersion = 8;

  const next = applyDelta(prev, {
    version: 12,
    changedIds: [],
    childSetChanged: [],
    removedIds: [],
    directChildCounts: {},
    newVisibleCount: 0,
    coarseSubtrees: [],
    subtreeDirty: [],
    subtreeResynced: [],
  });

  assert.equal(next.treeVersion, 12, 'monotonic must not mean frozen');
});
