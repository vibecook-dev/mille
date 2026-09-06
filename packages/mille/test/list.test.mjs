import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { DEFAULT_EXPLORER_SETTINGS, FileExplorer } from '../dist/index.js';

test('list paginates physical children independently of compaction and file nesting', async () => {
  const root = mkdtempSync(join(tmpdir(), 'mille-list-'));
  mkdirSync(join(root, 'a', 'b'), { recursive: true });
  writeFileSync(join(root, 'a', 'b', 'leaf.txt'), '');
  for (const name of ['bundle.js', 'bundle.js.map', 'notes.txt']) {
    writeFileSync(join(root, name), '');
  }
  const fx = new FileExplorer({
    roots: [root],
    settings: {
      ...DEFAULT_EXPLORER_SETTINGS,
      compactFolders: true,
      fileNestingPatterns: { '*.js': ['${capture}.js.map'] },
    },
  });
  try {
    await fx.populateFromRoots();
    const snapshot = fx.getSnapshot();
    const rootId = snapshot.roots()[0].id;
    assert.deepEqual(
      snapshot.projectedChildrenOf(rootId).map((id) => snapshot.getById(id).name),
      ['b', 'bundle.js', 'notes.txt'],
      'the fixture exercises both projections',
    );
    const all = await fx.list(rootId, { sort: 'name' });
    assert.deepEqual(
      all.entries.map((entry) => entry.name),
      ['a', 'bundle.js', 'bundle.js.map', 'notes.txt'],
    );
    assert.ok(all.entries.every((entry) => entry.parentId === rootId));
    assert.equal(all.total, 4);
    const first = await fx.list(rootId, { sort: 'name', limit: 2 });
    const second = await fx.list(rootId, { sort: 'name', offset: 2, limit: 2 });
    assert.deepEqual([...first.entries, ...second.entries], all.entries);
    assert.equal(first.total, 4);
    assert.equal(first.hasMore, true);
    assert.equal(second.total, 4);
    assert.equal(second.hasMore, false);
    const descending = await fx.list(rootId, { sort: 'name', sortDir: 'desc', limit: 2 });
    assert.deepEqual(
      descending.entries.map((entry) => entry.name),
      ['notes.txt', 'bundle.js.map'],
    );
  } finally {
    await fx.dispose();
    removeTempDir(root);
  }
});

test('list applies visibility before pagination and honors includeIgnored', async () => {
  const root = mkdtempSync(join(tmpdir(), 'mille-list-visibility-'));
  for (const name of ['.DS_Store', '.env', 'desktop.ini', 'excluded.txt', 'visible.txt']) {
    writeFileSync(join(root, name), '');
  }
  const fx = new FileExplorer({
    roots: [root],
    settings: {
      ...DEFAULT_EXPLORER_SETTINGS,
      showHiddenFiles: false,
      showIgnoredFiles: false,
      excludeGlobs: ['excluded.txt'],
    },
  });
  try {
    fx.seedWorkspaceRoots();
    const rootId = fx.getSnapshot().roots()[0].id;
    const visible = await fx.list(rootId, { sort: 'name', limit: 1 });
    assert.deepEqual(
      visible.entries.map((entry) => entry.name),
      ['visible.txt'],
    );
    assert.equal(visible.total, 1);
    assert.equal(visible.hasMore, false);
    const all = await fx.list(rootId, { includeIgnored: true, sort: 'name', limit: 2 });
    assert.deepEqual(
      all.entries.map((entry) => entry.name),
      ['.DS_Store', '.env'],
    );
    assert.equal(all.total, 5);
    assert.equal(all.hasMore, true);
  } finally {
    await fx.dispose();
    removeTempDir(root);
  }
});
