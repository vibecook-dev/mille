// Adapters from `GitService` onto `GitClient` / `FileHistoryClient` /
// `ScmClient` (SPEC mille-git PR 7, §10.4, §17.2).
//
// Most of this runs against a scripted fake `GitService` — the adapters are
// renderer-safe and need no engine. The porcelain golden shared with the Rust
// parser (crates/mille-git/tests/fixtures/porcelain) pins the shell client and
// the engine adapter to the same badges (T-XY). The last block re-runs the
// shell suite's real-repository behaviours (AC-007) against the native engine
// and skips without a git-enabled `.node`.

import { strict as assert } from 'node:assert';
import { readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  createGitServiceClient,
  createGitServiceHistoryClient,
  createGitServiceScmClient,
  GitError,
  isGitError,
} from '../dist/git.js';
import { createShellGitClient } from '../dist/git-node.js';
import { runScmRevert, ScmActionError } from '../dist/history.js';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import {
  git,
  makeRepo,
  needsGit,
  tempDir,
} from '../../mille/test/fixtures/git-fixture.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const GOLDEN = join(__dirname, '..', '..', '..', 'crates', 'mille-git', 'tests', 'fixtures', 'porcelain');
const goldenZ = readFileSync(join(GOLDEN, 'v2-mixed.z'));
const goldenJson = JSON.parse(readFileSync(join(GOLDEN, 'v2-mixed.expected.json'), 'utf8'));

const ROOT = '/ws/repo';

function identityOf(worktreePath) {
  return {
    kind: 'worktree',
    worktreePath,
    gitdir: `${worktreePath}/.git`,
    commonDir: `${worktreePath}/.git`,
    bare: false,
    head: 'a'.repeat(40),
    branch: 'main',
    detached: false,
  };
}

function snapshotOf(entries, generation = 1, worktreePath = ROOT) {
  return {
    worktreePath,
    identity: identityOf(worktreePath),
    entries,
    tookMs: 1,
    pathLimited: false,
    generation,
    statusBackend: 'gix',
  };
}

/** A scripted `GitService`: `overrides` replace methods; calls are recorded. */
function fakeService(overrides = {}, { checkout = ROOT } = {}) {
  const calls = [];
  const statusListeners = new Map();
  const record =
    (name, fallback) =>
    (...args) => {
      calls.push([name, ...args]);
      const impl = overrides[name] ?? fallback;
      return impl(...args);
    };
  const service = {
    calls,
    statusListenerCount: () => [...statusListeners.values()].reduce((n, s) => n + s.size, 0),
    push(worktree, snapshot) {
      for (const cb of [...(statusListeners.get(worktree) ?? [])]) cb(snapshot);
    },
    open: record('open', async () => identityOf(checkout)),
    close: record('close', async () => {}),
    identity: record('identity', async () => identityOf(checkout)),
    status: record('status', async () => snapshotOf([])),
    onStatus(worktree, cb) {
      calls.push(['onStatus', worktree]);
      if (overrides.onStatus) return overrides.onStatus(worktree, cb);
      const set = statusListeners.get(worktree) ?? new Set();
      statusListeners.set(worktree, set);
      set.add(cb);
      return () => set.delete(cb);
    },
    diff: record('diff', async () => assert.fail('adapters never diff')),
    log: record('log', async () => ({ commits: [], tookMs: 0, exhausted: true })),
    commitFiles: record('commitFiles', async () => []),
    readObject: record('readObject', async () => null),
    readWorktreeFile: record('readWorktreeFile', async () => null),
    stage: record('stage', async () => {}),
    unstage: record('unstage', async () => {}),
    restore: record('restore', async () => {}),
    commit: record('commit', async () => assert.fail('ScmClient has no commit')),
    worktrees: record('worktrees', async () => []),
    onWorktrees: () => () => {},
    advisePerformanceConfig: record('advisePerformanceConfig', async () => ({})),
    dispose: record('dispose', async () => {}),
  };
  return service;
}

// ─── GitClient ────────────────────────────────────────────────────────

test('T-XY: the GitClient collapses XY exactly as xyToEntry, keyed by absolute path', async () => {
  const service = fakeService({ status: async () => snapshotOf(goldenJson.entries) });
  const map = await createGitServiceClient(service, ROOT).getStatus(ROOT);

  assert.deepEqual(map.get(`${ROOT}/src/both.ts`), { path: 'src/both.ts', status: 'M', staged: true });
  assert.deepEqual(map.get(`${ROOT}/src/a.ts`), { path: 'src/a.ts', status: 'M', staged: false });
  assert.deepEqual(map.get(`${ROOT}/gone.ts`), { path: 'gone.ts', status: 'D', staged: false });
  assert.deepEqual(map.get(`${ROOT}/moved.ts`), { path: 'moved.ts', status: 'R', staged: true });
  assert.deepEqual(map.get(`${ROOT}/intent.ts`), { path: 'intent.ts', status: 'A', staged: false });
  // Unmerged and untracked carry no `staged` at all.
  assert.deepEqual(map.get(`${ROOT}/conflict file.ts`), { path: 'conflict file.ts', status: 'U' });
  assert.deepEqual(map.get(`${ROOT}/deleted-by-us.ts`), { path: 'deleted-by-us.ts', status: 'U' });
  assert.deepEqual(map.get(`${ROOT}/with space.ts`), { path: 'with space.ts', status: '?' });
  assert.equal(map.size, goldenJson.entries.length);
});

test(
  'T-PORC parity: the shell client and the engine adapter badge the golden identically',
  { skip: process.platform === 'win32' && 'shell keys use the platform separator' },
  async () => {
    const warnings = [];
    const shell = createShellGitClient({
      rootPath: ROOT,
      disableWatcher: true,
      warn: (m) => warnings.push(m),
      spawn: () => {
        const handlers = { close: [], error: [] };
        const stdout = {
          on: (event, cb) => event === 'data' && queueMicrotask(() => cb(goldenZ)),
        };
        const child = {
          stdout,
          stderr: { on: () => {} },
          on: (event, cb) => handlers[event]?.push(cb),
        };
        setTimeout(() => handlers.close.forEach((cb) => cb(0)), 5);
        return child;
      },
    });
    const fromShell = await shell.getStatus(ROOT);
    const fromEngine = await createGitServiceClient(
      fakeService({ status: async () => snapshotOf(goldenJson.entries) }),
      ROOT,
    ).getStatus(ROOT);

    // `u` records with a space in the path are the case the shell parser
    // used to cut to the last token (`conflict file.ts` → `file.ts`).
    assert.ok(fromShell.has(`${ROOT}/conflict file.ts`), 'shell lost the unmerged path with a space');
    assert.equal(fromShell.has(`${ROOT}/file.ts`), false);
    assert.deepEqual(
      [...fromShell.entries()].sort(([a], [b]) => a.localeCompare(b)),
      [...fromEngine.entries()].sort(([a], [b]) => a.localeCompare(b)),
    );
    assert.equal(warnings.length, goldenJson.warnings);
  },
);

test('an explorer root inside the checkout keeps only its rows, relative to itself', async () => {
  const service = fakeService({ status: async () => snapshotOf(goldenJson.entries) });
  const map = await createGitServiceClient(service, `${ROOT}/src`).getStatus(`${ROOT}/src`);
  assert.deepEqual(
    [...map.values()].map((e) => e.path).sort(),
    ['a.ts', 'both.ts', 'staged.ts'],
  );
  assert.deepEqual([...map.keys()].sort(), [
    `${ROOT}/src/a.ts`,
    `${ROOT}/src/both.ts`,
    `${ROOT}/src/staged.ts`,
  ]);
  // A sibling that merely shares the prefix is not inside the root.
  const sibling = fakeService({
    status: async () => snapshotOf([{ ...goldenJson.entries[0], path: 'srcfoo/x.ts' }]),
  });
  assert.equal((await createGitServiceClient(sibling, `${ROOT}/src`).getStatus('')).size, 0);
});

test('getStatus never throws (§23); each call is a fresh map', async () => {
  const failing = fakeService({
    status: async () => {
      throw new GitError('EUNKNOWN', 'engine gone');
    },
    onStatus: () => {
      throw new GitError('EINVAL', 'disposed');
    },
  });
  const client = createGitServiceClient(failing, ROOT);
  const off = client.onChange(() => {});
  assert.deepEqual([...(await client.getStatus(ROOT))], []);
  off();

  const ok = fakeService({ status: async () => snapshotOf(goldenJson.entries) });
  const okClient = createGitServiceClient(ok, ROOT);
  const a = await okClient.getStatus(ROOT);
  const b = await okClient.getStatus(ROOT);
  assert.notEqual(a, b);
  assert.deepEqual([...a], [...b]);
});

test('onChange subscribes lazily, serves the pushed snapshot, and lets go', async () => {
  const service = fakeService({ status: async () => snapshotOf([]) });
  const client = createGitServiceClient(service, ROOT);
  assert.equal(service.statusListenerCount(), 0, 'no subscription before onChange');

  let fired = 0;
  const off1 = client.onChange(() => (fired += 1));
  const off2 = client.onChange(() => (fired += 1));
  assert.equal(service.statusListenerCount(), 1, 'one engine subscription for many listeners');

  service.push(ROOT, snapshotOf([goldenJson.entries[0]], 7));
  assert.equal(fired, 2);
  const statusCalls = service.calls.filter(([name]) => name === 'status').length;
  const map = await client.getStatus(ROOT);
  assert.ok(map.has(`${ROOT}/src/a.ts`), 'the pushed snapshot is what getStatus returns');
  assert.equal(
    service.calls.filter(([name]) => name === 'status').length,
    statusCalls,
    'a pushed snapshot is served from the cache, not re-fetched',
  );
  // An older generation never replaces a newer one.
  service.push(ROOT, snapshotOf([], 6));
  assert.ok((await client.getStatus(ROOT)).has(`${ROOT}/src/a.ts`));

  off1();
  off1();
  assert.equal(service.statusListenerCount(), 1);
  off2();
  assert.equal(service.statusListenerCount(), 0, 'the last listener ends the subscription');
});

// ─── FileHistoryClient ────────────────────────────────────────────────

test('history: getHistory is a path-filtered log page; getContents is readObject', async () => {
  const service = fakeService({
    log: async () => ({
      commits: [
        { id: 'b'.repeat(40), shortId: 'bbbbbbb', author: 'A', message: 'second', timestampMs: 2000, parents: ['a'.repeat(40)] },
        { id: 'a'.repeat(40), shortId: 'aaaaaaa', timestampMs: 1000, parents: [] },
      ],
      tookMs: 1,
      exhausted: true,
    }),
    readObject: async () => 'v1\n',
  });
  const history = createGitServiceHistoryClient(service, ROOT);
  const revisions = await history.getHistory({ path: 'src/a.ts' });
  assert.deepEqual(revisions, [
    { id: 'b'.repeat(40), shortId: 'bbbbbbb', author: 'A', message: 'second', timestampMs: 2000, parents: ['a'.repeat(40)] },
    { id: 'a'.repeat(40), shortId: 'aaaaaaa', timestampMs: 1000, parents: [] },
  ]);
  assert.equal(await history.getContents({ path: 'src/a.ts', revision: 'HEAD~1' }), 'v1\n');
  assert.deepEqual(
    service.calls.filter(([n]) => n === 'log' || n === 'readObject'),
    [
      ['log', ROOT, { path: 'src/a.ts', limit: 50 }],
      ['readObject', ROOT, { path: 'src/a.ts', revision: 'HEAD~1' }],
    ],
  );
  await history.getHistory({ path: 'x.ts', limit: 3 });
  assert.deepEqual(service.calls.findLast(([n]) => n === 'log'), ['log', ROOT, { path: 'x.ts', limit: 3 }]);
});

test('history: a root inside the checkout is translated to checkout-relative paths', async () => {
  const service = fakeService({}, { checkout: ROOT });
  const history = createGitServiceHistoryClient(service, `${ROOT}/packages/app`);
  await history.getHistory({ path: 'src/x.ts' });
  await history.getContents({ path: 'src/x.ts', revision: 'HEAD' });
  assert.deepEqual(
    service.calls.filter(([n]) => n === 'log' || n === 'readObject').map((c) => c[2].path),
    ['packages/app/src/x.ts', 'packages/app/src/x.ts'],
  );
  assert.equal(service.calls.filter(([n]) => n === 'identity').length, 1, 'the offset is fetched once');
});

test('history: escapes throw before the engine is asked; quiet failures are empty', async () => {
  const service = fakeService({
    log: async () => {
      throw new GitError('ENOTAREPO', 'not a git repository');
    },
    readObject: async () => {
      throw new GitError('ENOGIT', 'no git');
    },
  });
  const history = createGitServiceHistoryClient(service, ROOT);
  for (const path of ['../secret.txt', '/etc/passwd', 'a/../../b', 'C:\\x', 'a\\b', '']) {
    await assert.rejects(history.getHistory({ path }), /escapes|non-empty/);
  }
  assert.equal(service.calls.some(([n]) => n === 'log'), false, 'an escape reached the engine');
  assert.deepEqual(await history.getHistory({ path: 'a.ts' }), []);
  assert.equal(await history.getContents({ path: 'a.ts', revision: 'HEAD' }), null);

  const strict = fakeService({
    readObject: async () => {
      throw new GitError('EINVAL', 'invalid revision (looks like a git option): -o');
    },
  });
  await assert.rejects(
    createGitServiceHistoryClient(strict, ROOT).getContents({ path: 'a.ts', revision: '-o' }),
    (e) => isGitError(e) && e.code === 'EINVAL',
  );
});

// ─── ScmClient ────────────────────────────────────────────────────────

test('scm: revert is restore, stage/unstage by name, never commit', async () => {
  const service = fakeService();
  const scm = createGitServiceScmClient(service, ROOT);
  assert.equal('commit' in scm, false, '§32.4: ScmClient has no commit');
  const ac = new AbortController();
  await scm.revert(['a.ts', 'src/b.ts'], { signal: ac.signal });
  await scm.stage(['a.ts']);
  await scm.unstage(['a.ts'], { signal: ac.signal });
  await scm.revert([]);
  const mutations = service.calls.filter(([n]) => ['restore', 'stage', 'unstage'].includes(n));
  assert.deepEqual(mutations, [
    ['restore', ROOT, ['a.ts', 'src/b.ts'], { signal: ac.signal }],
    ['stage', ROOT, ['a.ts'], {}],
    ['unstage', ROOT, ['a.ts'], { signal: ac.signal }],
  ]);
  await assert.rejects(scm.revert(['../x']), /escapes/);
  assert.equal(service.calls.filter(([n]) => n === 'restore').length, 1);
});

test('scm: compare reads the working tree and a revision, with the shell labels', async () => {
  const service = fakeService({
    readWorktreeFile: async () => 'dirty\n',
    readObject: async (_w, req) => (req.revision === 'HEAD' ? 'v2\n' : null),
  });
  const scm = createGitServiceScmClient(service, ROOT);
  const result = await scm.compare({
    path: 'tracked.ts',
    left: { kind: 'revision', revision: 'HEAD' },
    right: { kind: 'working' },
  });
  assert.deepEqual(result, {
    path: 'tracked.ts',
    leftLabel: 'HEAD',
    rightLabel: 'Working Tree',
    left: 'v2\n',
    right: 'dirty\n',
  });
  await assert.rejects(
    scm.compare({ path: '../secret.txt', left: { kind: 'working' }, right: { kind: 'working' } }),
    /escapes/,
  );
  const ac = new AbortController();
  ac.abort();
  await assert.rejects(
    scm.compare(
      { path: 'tracked.ts', left: { kind: 'working' }, right: { kind: 'working' } },
      { signal: ac.signal },
    ),
    (e) => isGitError(e) && e.code === 'ECANCELED' && /abort/i.test(e.message),
  );
});

test('scm: runScmRevert drives the adapter like any ScmClient', async () => {
  const service = fakeService();
  const client = createGitServiceScmClient(service, ROOT);
  await runScmRevert(['a.ts'], { client, hooks: { confirm: async () => true } });
  assert.deepEqual(service.calls.find(([n]) => n === 'restore'), ['restore', ROOT, ['a.ts'], {}]);
  const ac = new AbortController();
  ac.abort();
  await assert.rejects(
    runScmRevert(['a.ts'], { client, signal: ac.signal, hooks: { confirm: async () => true } }),
    (err) => err instanceof ScmActionError && err.code === 'ECANCELED',
  );
});

// ─── The native engine behind the adapters (AC-007) ───────────────────

async function engineService(roots) {
  const { createGitService, createInProcessGitTransport } = await import('@vibecook/mille/git');
  return createGitService(await createInProcessGitTransport({ allowedRoots: roots }));
}

/** The history suite's fixture: two commits, a dirty worktree, a sibling secret. */
function historyFixture() {
  const dir = makeRepo({ 'tracked.ts': 'v1\n' }, 'mille-git-history-');
  writeFileSync(join(dir, 'tracked.ts'), 'v2\n');
  git(dir, 'commit', '-q', '-am', 'second');
  writeFileSync(join(dir, 'tracked.ts'), 'dirty\n');
  const outside = join(dirname(dir), `${dir.split('/').pop()}-secret.txt`);
  writeFileSync(outside, 'SECRET\n');
  return { dir, outside };
}

test('AC-007: engine history — getHistory order and getContents at a revision', needsGit(), async () => {
  const { dir, outside } = historyFixture();
  const service = await engineService([dir]);
  try {
    await service.open(dir);
    const history = createGitServiceHistoryClient(service, dir);
    const revisions = await history.getHistory({ path: 'tracked.ts', limit: 10 });
    assert.equal(revisions.length, 2);
    assert.equal(revisions[0].message, 'second');
    assert.equal(revisions[1].message, 'initial');
    assert.equal(await history.getContents({ path: 'tracked.ts', revision: revisions[1].id }), 'v1\n');
    assert.equal(await history.getContents({ path: 'tracked.ts', revision: revisions[0].id }), 'v2\n');
    await assert.rejects(history.getHistory({ path: `../${outside.split('/').pop()}` }), /escapes/);
    await assert.rejects(
      history.getContents({ path: 'tracked.ts', revision: '--output=/tmp/pwned' }),
      (e) => isGitError(e) && e.code === 'EINVAL',
    );
  } finally {
    await service.dispose();
    removeTempDir(dir);
    rmSync(outside, { force: true });
  }
});

test('AC-007 / AC-017: engine scm — compare, and revert restores index bytes', needsGit(), async () => {
  const { dir, outside } = historyFixture();
  const service = await engineService([dir]);
  try {
    await service.open(dir);
    const scm = createGitServiceScmClient(service, dir);
    const cmp = await scm.compare({
      path: 'tracked.ts',
      left: { kind: 'revision', revision: 'HEAD' },
      right: { kind: 'working' },
    });
    assert.deepEqual([cmp.leftLabel, cmp.rightLabel, cmp.left, cmp.right], [
      'HEAD',
      'Working Tree',
      'v2\n',
      'dirty\n',
    ]);
    const missing = await scm.compare({
      path: 'nope.ts',
      left: { kind: 'revision', revision: 'HEAD' },
      right: { kind: 'working' },
    });
    assert.deepEqual([missing.left, missing.right], [null, null]);

    // Staged + further worktree edits: revert lands the *index* bytes.
    writeFileSync(join(dir, 'tracked.ts'), 'staged\n');
    await scm.stage(['tracked.ts']);
    writeFileSync(join(dir, 'tracked.ts'), 'worktree only\n');
    await scm.revert(['tracked.ts']);
    assert.equal(readFileSync(join(dir, 'tracked.ts'), 'utf8'), 'staged\n');
    await scm.unstage(['tracked.ts']);
    assert.match(git(dir, 'status', '--porcelain'), /^ M tracked\.ts/m);

    await assert.rejects(
      scm.compare({
        path: `../${outside.split('/').pop()}`,
        left: { kind: 'working' },
        right: { kind: 'working' },
      }),
      /escapes/,
    );
    assert.equal(readFileSync(outside, 'utf8'), 'SECRET\n');
  } finally {
    await service.dispose();
    removeTempDir(dir);
    rmSync(outside, { force: true });
  }
});

test(
  'engine scm: compare refuses to read through a symlink that escapes the root',
  process.platform === 'win32' ? { skip: 'symlinks need privileges on Windows' } : needsGit(),
  async () => {
    const { dir, outside } = historyFixture();
    symlinkSync(outside, join(dir, 'link.txt'));
    const service = await engineService([dir]);
    try {
      await service.open(dir);
      const scm = createGitServiceScmClient(service, dir);
      await assert.rejects(
        scm.compare({ path: 'link.txt', left: { kind: 'working' }, right: { kind: 'working' } }),
        /escapes/,
      );
    } finally {
      await service.dispose();
      removeTempDir(dir);
      rmSync(outside, { force: true });
    }
  },
);

test('engine GitClient: status rows for a real checkout, and a non-repo is empty', needsGit(), async () => {
  const dir = makeRepo({ 'a.ts': 'a\n', 'pkg/b.ts': 'b\n' }, 'mille-git-client-');
  const plain = tempDir('mille-git-plain-');
  const service = await engineService([dir, plain]);
  try {
    writeFileSync(join(dir, 'pkg', 'b.ts'), 'b2\n');
    writeFileSync(join(dir, 'new.ts'), 'n\n');
    await service.open(join(dir, 'pkg'));
    const map = await createGitServiceClient(service, join(dir, 'pkg')).getStatus('');
    assert.deepEqual([...map.entries()], [
      [`${dir}/pkg/b.ts`, { path: 'b.ts', status: 'M', staged: false }],
    ]);
    await service.open(plain);
    assert.equal((await createGitServiceClient(service, plain).getStatus(plain)).size, 0);
  } finally {
    await service.dispose();
    removeTempDir(dir);
    removeTempDir(plain);
  }
});
