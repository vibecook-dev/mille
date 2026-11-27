import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import { test } from 'node:test';

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { DEFAULT_EXPLORER_SETTINGS, FileExplorer } from '../dist/index.js';

function workspace(t) {
  const base = mkdtempSync(join(tmpdir(), 'mille-lifecycle-'));
  const root = join(base, 'workspace');
  mkdirSync(root);
  const explorers = [];
  t.after(async () => {
    try {
      for (const fx of explorers) await fx.dispose();
    } finally {
      removeTempDir(base);
    }
  });
  return {
    base,
    root,
    open(options = {}) {
      const fx = new FileExplorer({ roots: [root], compactFolders: false, ...options });
      explorers.push(fx);
      return fx;
    },
  };
}

function indexed(snapshot) {
  const result = new Map();
  function visit(entry, path) {
    result.set(path, entry);
    for (const id of snapshot.childrenOf(entry.id)) {
      const child = snapshot.getById(id);
      visit(child, path ? path + '/' + child.name : child.name);
    }
  }
  for (const root of snapshot.roots()) visit(root, snapshot.roots().length === 1 ? '' : root.name);
  return result;
}

function nextEvent(fx, channel, matches = () => true) {
  return new Promise((resolve) => {
    const sub = fx.on(channel, (value) => {
      if (matches(value)) {
        sub.dispose();
        resolve(value);
      }
    });
  });
}

test(
  'wide directory pages obey the live cache limit, preserve totals and return usable ids',
  { timeout: 10000 },
  async (t) => {
    const { root, open } = workspace(t);
    for (let i = 0; i < 12; i++) writeFileSync(join(root, 'file' + i), String(i));
    writeFileSync(join(root, '.hidden'), '');
    const fx = open({
      maxCachedEntries: 4,
      settings: { ...DEFAULT_EXPLORER_SETTINGS, showHiddenFiles: false },
    });
    const warning = nextEvent(fx, 'warning', (event) => event.code === 'ENTRY_CAP_REACHED');
    await fx.populateFromRoots();
    await warning;
    const old = fx.getSnapshot();
    const rootId = old.roots()[0].id;
    assert.equal(indexed(old).size, 4);
    assert.equal(old.directoryChildrenLoaded(rootId), false);
    const names = [];
    for (let offset = 0; offset < 12; ) {
      const result = await fx.list(rootId, { offset, limit: 7, sort: 'name', sortDir: 'desc' });
      assert.equal(result.total, 12);
      assert.equal(result.entries.length, 3);
      assert.ok(indexed(fx.getSnapshot()).size <= 4);
      for (const row of result.entries) {
        assert.equal(fx.getSnapshot().getById(row.id).name, row.name);
        assert.equal(await fx.readText(row.id), row.name.slice('file'.length));
        names.push(row.name);
      }
      offset += result.entries.length;
      assert.equal(result.hasMore, offset < result.total);
    }
    assert.deepEqual(
      names,
      Array.from({ length: 12 }, (_, i) => 'file' + (11 - i)),
    );
    assert.equal(indexed(old).size, 4);
    const id = await fx.resolvePath('file10');
    assert.equal(await fx.readText(id), '10');
    assert.ok(indexed(fx.getSnapshot()).size <= 4);
  },
);

test('all roots retain a slot and operations report when a path cannot fit', async (t) => {
  const { root, base, open } = workspace(t);
  const second = join(base, 'other');
  mkdirSync(second);
  writeFileSync(join(root, 'file'), 'payload');
  assert.throws(() => open({ roots: [root, second], maxCachedEntries: 1 }), { code: 'EINVAL' });
  const fx = open({ roots: [root, second], maxCachedEntries: 2 });
  await fx.populateFromRoots();
  const snapshot = fx.getSnapshot();
  assert.equal(snapshot.roots().length, 2);
  assert.equal(indexed(snapshot).size, 2);
  const rootId = snapshot.roots().find((r) => r.name === basename(root)).id;
  assert.deepEqual(await fx.list(rootId, { limit: 0 }), { entries: [], total: 1, hasMore: true });
  await assert.rejects(fx.list(rootId), { code: 'ENOSPC' });
  await assert.rejects(fx.resolvePath('file'), { code: 'ENOSPC' });
  await assert.rejects(fx.create(rootId, 'new', 0), { code: 'ENOSPC' });
  assert.equal(indexed(fx.getSnapshot()).size, 2);
});

test(
  'snapshot resume preserves identities and reports offline changes through the public event stream',
  { timeout: 10000 },
  async (t) => {
    const { root, base, open } = workspace(t);
    const snapshotPath = join(root, '.explorer-state');
    const explicit = join(base, 'copy.snapshot');
    for (const name of ['middle', 'removed', 'unchanged'])
      writeFileSync(join(root, name), 'original');
    const first = open({ snapshotPath });
    await first.populateFromRoots();
    const before = indexed(first.getSnapshot());
    await first.writeSnapshot(explicit);
    await first.dispose();
    assert.ok(readFileSync(explicit).length > 0);
    writeFileSync(join(root, 'aaa-new'), 'new');
    writeFileSync(join(root, 'middle'), 'modified while closed');
    rmSync(join(root, 'removed'));
    const second = open({ snapshotPath });
    const warm = second.getSnapshot();
    assert.equal(indexed(warm).get('removed').id, before.get('removed').id);
    assert.equal(warm.directoryChildrenLoaded(warm.roots()[0].id), false);
    const offline = nextEvent(second, 'batch', (events) =>
      events.some((e) => e.kind === 'changedSinceSnapshot'),
    );
    await second.populateFromRoots();
    const after = indexed(second.getSnapshot());
    const events = (await offline).filter((e) => e.kind === 'changedSinceSnapshot');
    const details = new Map(events.map((e) => [basename(e.path), e.detail]));
    assert.equal(after.get('middle').id, before.get('middle').id);
    assert.equal(after.get('unchanged').id, before.get('unchanged').id);
    assert.equal(after.has('removed'), false);
    assert.ok(after.get('aaa-new').id > Math.max(...Array.from(before.values(), (e) => e.id)));
    assert.equal(details.get('aaa-new'), 'created');
    assert.equal(details.get('middle'), 'changed');
    assert.equal(details.get('removed'), 'deleted');
    assert.equal(details.has('unchanged'), false);
    assert.equal(after.has('.explorer-state'), false);
    assert.equal(await second.resolvePath(snapshotPath), null);
    assert.equal(details.has('.explorer-state'), false);
  },
);

test(
  'invalid checkpoints warn once and the successful cold walk replaces them',
  { timeout: 10000 },
  async (t) => {
    const { root, base, open } = workspace(t);
    const snapshotPath = join(base, 'state');
    writeFileSync(snapshotPath, 'incompatible data');
    writeFileSync(join(root, 'survives'), 'data');
    const fx = open({ snapshotPath });
    const stale = nextEvent(fx, 'warning', (event) => event.code === 'SNAPSHOT_STALE');
    await fx.populateFromRoots();
    await stale;
    const original = await fx.resolvePath('survives');
    await fx.dispose();
    const restored = open({ snapshotPath });
    await restored.populateFromRoots();
    assert.equal(await restored.resolvePath('survives'), original);
  },
);

test(
  'roots-only startup emits ready after seeding and checkpoints lazily indexed rows',
  { timeout: 10000 },
  async (t) => {
    const { root, base, open } = workspace(t);
    const snapshotPath = join(base, 'lazy.snapshot');
    writeFileSync(join(root, 'c'), 'cached');
    const first = open({ snapshotPath, initialWalk: 'roots-only' });
    const ready = nextEvent(first, 'ready');
    first.seedWorkspaceRoots();
    await ready;
    const id = await first.resolvePath('c');
    await first.dispose();

    writeFileSync(join(root, 'a'), 'outside the saved working set');
    const second = open({ snapshotPath, initialWalk: 'roots-only' });
    second.seedWorkspaceRoots();
    assert.equal(indexed(second.getSnapshot()).size, 1);
    const events = [];
    second.on('event', (event) => events.push(event));
    await second.populateFromRoots();
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(await second.resolvePath('c'), id);
    assert.equal(
      events.some((event) => event.kind === 'changedSinceSnapshot' && basename(event.path) === 'a'),
      false,
    );
  },
);

test('a failed checkpoint write can be retried without losing the saved state', async (t) => {
  const { root, base, open } = workspace(t);
  const destination = join(base, 'later');
  const snapshotPath = join(destination, 'state');
  writeFileSync(join(root, 'file'), 'before');
  const fx = open({ snapshotPath });
  await assert.rejects(fx.populateFromRoots(), { code: 'ENOENT' });
  mkdirSync(destination);
  await fx.populateFromRoots();
  const start = await fx.resolvePath('file');
  await fx.dispose();
  const next = open({ snapshotPath });
  assert.equal(indexed(next.getSnapshot()).get('file').id, start);
});

test(
  'smart follows unique directory targets, gives real paths priority and keeps link rows',
  { skip: process.platform === 'win32' },
  async (t) => {
    const { root, base, open } = workspace(t);
    const external = join(base, 'external');
    mkdirSync(external);
    writeFileSync(join(external, 'outside'), 'external');
    mkdirSync(join(root, 'z-real'));
    writeFileSync(join(root, 'z-real', 'inside'), 'real');
    symlinkSync('z-real', join(root, 'a-alias'));
    symlinkSync(root, join(root, 'cycle'));
    symlinkSync(external, join(root, 'ext-a'));
    symlinkSync(external, join(root, 'ext-b'));
    symlinkSync('missing', join(root, 'dangling'));
    symlinkSync(external, join(root, 'ignored'));
    writeFileSync(join(root, '.gitignore'), 'ignored/\n');
    for (const mode of [false, 'smart', true]) {
      const fx = open({ followSymlinks: mode });
      await fx.populateFromRoots();
      const files = indexed(fx.getSnapshot());
      assert.ok(files.has('z-real/inside'));
      assert.equal(files.has('a-alias/inside'), mode === true);
      assert.equal(files.has('ext-a/outside'), mode !== false);
      assert.equal(files.has('ext-b/outside'), mode === true);
      assert.equal(files.has('cycle/z-real'), false);
      assert.equal(files.has('ignored/outside'), false);
      if (mode === 'smart') {
        assert.equal(files.get('ext-a').kind, 2);
        assert.equal(files.get('ext-a').symlinkTargetIsDir, true);
        assert.equal(files.get('dangling').kind, 2);
        assert.equal(files.get('cycle').kind, 2);
      }
      await fx.dispose();
    }
  },
);

test(
  'smart lazy expansion cuts ancestor cycles and keeps one expanded external target',
  { skip: process.platform === 'win32' },
  async (t) => {
    const { root, base, open } = workspace(t);
    const external = join(base, 'external');
    mkdirSync(external);
    writeFileSync(join(external, 'outside'), 'external');
    symlinkSync(external, join(root, 'first'));
    symlinkSync(external, join(root, 'second'));
    symlinkSync(root, join(root, 'loop'));
    const fx = open({ followSymlinks: 'smart', initialWalk: 'roots-only' });
    fx.seedWorkspaceRoots();
    const rootId = fx.getSnapshot().roots()[0].id;
    const { entries } = await fx.list(rootId);
    const ids = new Map(entries.map((e) => [e.name, e.id]));
    assert.deepEqual(
      (await fx.list(ids.get('first'))).entries.map((e) => e.name),
      ['outside'],
    );
    for (const name of ['second', 'loop']) {
      assert.equal((await fx.list(ids.get(name))).total, 0);
      assert.equal(fx.getSnapshot().hasChildren(ids.get(name)), false);
    }
  },
);

test(
  'changing traversal settings requires a fresh walk before publishing checkpoint rows',
  { timeout: 10000 },
  async (t) => {
    const { root, base, open } = workspace(t);
    const snapshotPath = join(base, 'state');
    mkdirSync(join(root, 'ignored'));
    writeFileSync(join(root, 'ignored', 'inside'), 'previously indexed');
    const first = open({ snapshotPath });
    await first.populateFromRoots();
    await first.dispose();
    const second = open({ snapshotPath, excludeGlobs: ['ignored/'] });
    assert.equal(indexed(second.getSnapshot()).size, 0);
    const stale = nextEvent(second, 'warning', (e) => e.code === 'SNAPSHOT_STALE');
    await second.populateFromRoots();
    await stale;
    assert.equal(indexed(second.getSnapshot()).has('ignored/inside'), false);
  },
);
