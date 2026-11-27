// Phase 6 commit 6.7 — integration tests for the typed FileExplorer
// wrapper in src/client.ts.
//
// Exercises the wrapper end-to-end against the live .node binding:
//   - construction routes ExplorerOptions through to the native
//   - capabilities bitmask surfaces
//   - getSnapshot() returns a MirrorSnapshot with tree/decoration versions
//   - visibleRows + visibleRowCount return sensible empty results
//   - on('ready') subscribes through the correct native channel and
//     disposes cleanly
//   - dispose() resolves without throwing
//
// Tests that depend on walker-populated entries (create under a real
// parent_id, readFile round-trip) are explicitly skipped pending
// Phase 7, where the walker integration lands.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';

import { FileExplorer, MirrorSnapshot } from '../dist/index.js';

function tempRoot() {
  return mkdtempSync(join(tmpdir(), 'mille-client-'));
}

test('FileExplorer constructor accepts an absolute root path', () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [dir] });
    assert.equal(typeof fx.capabilities, 'number');
    assert.equal(typeof fx.getTreeVersion(), 'number');
  } finally {
    removeTempDir(dir);
  }
});

test('FileExplorer constructor accepts a Uri-shaped root', () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [{ scheme: 'file', path: dir }] });
    assert.equal(typeof fx.capabilities, 'number');
  } finally {
    removeTempDir(dir);
  }
});

test('FileExplorer constructor forwards followSymlinks booleans + smart', () => {
  const dir = tempRoot();
  try {
    // All three variants should construct without throwing.
    for (const v of [true, false, 'smart']) {
      const fx = new FileExplorer({ roots: [dir], followSymlinks: v });
      assert.equal(typeof fx.capabilities, 'number');
    }
  } finally {
    removeTempDir(dir);
  }
});

test('getSnapshot returns MirrorSnapshot with tree/decoration versions', () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [dir] });
    const snap = fx.getSnapshot();
    assert.ok(snap instanceof MirrorSnapshot);
    assert.equal(typeof snap.treeVersion, 'number');
    assert.equal(typeof snap.decorationVersion, 'number');
    assert.ok(Array.isArray(snap.roots()));
  } finally {
    removeTempDir(dir);
  }
});

test('MirrorSnapshot.getById / hasChildren / directChildCount for unknown id', () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [dir] });
    const snap = fx.getSnapshot();
    assert.equal(snap.getById(99999), null);
    assert.equal(snap.hasChildren(99999), false);
    assert.equal(snap.directChildCount(99999), null);
    // Decorations land in Phase 9 — wrapper returns an empty array.
    assert.deepEqual(snap.getDecorations(99999), []);
  } finally {
    removeTempDir(dir);
  }
});

test('visibleRows + visibleRowCount return empty on a pristine tree', () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [dir] });
    const snap = fx.getSnapshot();
    const rows = snap.visibleRows({ expanded: new Set(), offset: 0, limit: 100 });
    assert.ok(Array.isArray(rows));
    assert.equal(rows.length, 0);
    const count = snap.visibleRowCount(new Set());
    assert.equal(count.known, 0);
    assert.ok(count.pendingExpansions instanceof Set);
    assert.equal(count.pendingExpansions.size, 0);
    assert.equal(snap.visibleRowIndex(99999, new Set()), null);
  } finally {
    removeTempDir(dir);
  }
});

test('visibleRowsBulk decodes the bincode payload (empty case)', () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [dir] });
    const snap = fx.getSnapshot();
    const rows = snap.visibleRowsBulk({ expanded: new Set(), offset: 0, limit: 100 });
    assert.ok(Array.isArray(rows));
    assert.equal(rows.length, 0);
  } finally {
    removeTempDir(dir);
  }
});

test(
  'ready fires after population, including for late listeners, once per subscription',
  { timeout: 5000 },
  async () => {
    const dir = tempRoot();
    const fx = new FileExplorer({ roots: [dir] });
    try {
      let calls = 0;
      const observed = new Promise((resolve) =>
        fx.on('ready', () => {
          calls++;
          resolve(fx.getSnapshot().roots().length);
        }),
      );
      await Promise.all([fx.populateFromRoots(), fx.populateFromRoots()]);
      assert.equal(await observed, 1);
      let cancelledCalls = 0;
      const cancelled = fx.on('ready', () => cancelledCalls++);
      cancelled.dispose();
      await new Promise((resolve) => fx.on('ready', resolve));
      assert.equal(calls, 1);
      assert.equal(cancelledCalls, 0);
    } finally {
      await fx.dispose();
      removeTempDir(dir);
    }
  },
);

test('on() rejects unknown event names', () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [dir] });
    assert.throws(() => fx.on('bogus', () => {}), /unknown event channel/);
  } finally {
    removeTempDir(dir);
  }
});

test('dispose() resolves without throwing', async () => {
  const dir = tempRoot();
  try {
    const fx = new FileExplorer({ roots: [dir] });
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

// ─── Phase 7.10 — populateFromRoots + walker-seeded round-trips ──────

/**
 * After populateFromRoots, the walk root itself is the entry with
 * parent_id = null. Its direct children (a.txt, b.txt, sub) are
 * addressable via snap.directChildCount/hasChildren but not via roots()
 * — roots() only returns top-level parent-less entries. Walk the tree
 * via visibleRows with the walk root expanded to reach children.
 */
function findChildByName(fx, parentName, childName) {
  const snap = fx.getSnapshot();
  const rootEntry = snap.roots().find((r) => r.name === parentName);
  if (!rootEntry) return null;
  const rows = snap.visibleRows({ expanded: [rootEntry.id], offset: 0, limit: 1000 });
  return rows.find((r) => r.name === childName) ?? null;
}

test('populateFromRoots seeds the store with entries', async () => {
  const dir = tempRoot();
  try {
    writeFileSync(join(dir, 'a.txt'), 'a');
    writeFileSync(join(dir, 'b.txt'), 'b');
    mkdirSync(join(dir, 'sub'));
    writeFileSync(join(dir, 'sub/c.txt'), 'c');

    const fx = new FileExplorer({ roots: [dir] });
    const n = await fx.populateFromRoots();
    // 1 walk root + 3 top-level (a.txt, b.txt, sub) + 1 nested (sub/c.txt) = 5.
    assert.ok(n >= 4, `expected >=4 entries, got ${n}`);

    const snap = fx.getSnapshot();
    // include_root: true means the walk root itself is a root entry.
    assert.equal(snap.roots().length, 1);
    const walkRoot = snap.roots()[0];
    // Three children: a.txt, b.txt, sub.
    assert.equal(snap.directChildCount(walkRoot.id), 3);
    assert.equal(snap.visibleRowIndex(walkRoot.id, new Set()), 0);
    const expanded = new Set([walkRoot.id]);
    const visible = snap.visibleRows({ expanded, offset: 0, limit: 100 });
    assert.deepEqual(
      snap.visibleRowIds({ expanded, offset: 0, limit: 100 }),
      visible.map((row) => row.id),
    );
    for (let index = 0; index < visible.length; index += 1) {
      assert.equal(snap.visibleRowIndex(visible[index].id, expanded), index);
    }
    const aTxt = visible.find((row) => row.name === 'a.txt');
    assert.ok(aTxt);
    assert.equal(await fx.findVisiblePrefix('A.', null, false, expanded), aTxt.id);
    assert.equal(await fx.resolvePath('a.txt'), aTxt.id);
    assert.equal(await fx.resolvePath(`${walkRoot.name}/a.txt`), aTxt.id);
    assert.equal(await fx.resolvePath('missing.txt'), null);
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('resolvePath hydrates an unindexed path without a full workspace walk', async () => {
  const dir = tempRoot();
  try {
    mkdirSync(join(dir, 'deep', 'nested'), { recursive: true });
    writeFileSync(join(dir, 'deep', 'nested', 'target.txt'), 'target');
    writeFileSync(join(dir, 'cold.txt'), 'cold');
    const fx = new FileExplorer({ roots: [dir] });

    const id = await fx.resolvePath('deep/nested/target.txt');
    assert.equal(typeof id, 'number');
    assert.equal(await fx.resolvePath(join(dir, 'deep', 'nested', 'target.txt')), id);
    const snapshot = fx.getSnapshot();
    assert.equal(snapshot.getById(id).name, 'target.txt');
    assert.equal(snapshot.roots().length, 1);
    assert.equal(snapshot.directChildCount(snapshot.roots()[0].id), 1);
    assert.equal(await fx.resolvePath('../outside.txt'), null);
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('rename round-trip against a populated entry', async () => {
  const dir = tempRoot();
  try {
    writeFileSync(join(dir, 'orig.txt'), 'hi');
    const fx = new FileExplorer({ roots: [dir] });
    await fx.populateFromRoots();
    const rootName = basename(dir);
    const orig = findChildByName(fx, rootName, 'orig.txt');
    assert.ok(orig, 'orig.txt should be seeded');
    const renamed = await fx.rename(orig.id, 'renamed.txt');
    assert.equal(renamed.name, 'renamed.txt');
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('readFile round-trip against a populated entry', async () => {
  const dir = tempRoot();
  try {
    writeFileSync(join(dir, 'hello.txt'), 'hello world');
    const fx = new FileExplorer({ roots: [dir] });
    await fx.populateFromRoots();
    const rootName = basename(dir);
    const file = findChildByName(fx, rootName, 'hello.txt');
    assert.ok(file, 'hello.txt should be seeded');
    const data = await fx.readFile(file.id);
    assert.equal(Buffer.from(data).toString('utf8'), 'hello world');
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});
