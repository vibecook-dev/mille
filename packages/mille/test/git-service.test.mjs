// `createGitService(transport)` and the in-process `GitTransport`
// (SPEC mille-git PR 6c).
//
// The first half is AC-012: the `GitService` surface is usable against a fake
// transport — no binary, no git — which is how an ADE room is built and tested
// before any engine exists. The second half runs the real in-process transport
// against a git-enabled `.node` and skips without one.

import { strict as assert } from 'node:assert';
import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { createGitService } from '../dist/git/service.js';
import {
  bindExplorer,
  createInProcessGitTransport,
  GIT_PROTOCOL_VERSION,
  GitError,
  isGitError,
} from '../dist/git/index.js';
import { PROTOCOL_VERSION } from '../dist/protocol.js';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { makeRepo, needsGit, tempDir, waitFor, write, xyOf } from './fixtures/git-fixture.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));

/** A scripted transport: records calls, answers from `responses`, emits on demand. */
function fakeTransport(responses = {}) {
  const calls = [];
  const listeners = { status: new Set(), worktrees: new Set(), warning: new Set() };
  let closed = 0;
  return {
    calls,
    get closed() {
      return closed;
    },
    listenerCount(event) {
      return listeners[event].size;
    },
    emit(event, body) {
      for (const cb of [...listeners[event]]) cb(body);
    },
    call(method, args, opts) {
      calls.push({ method, args, opts });
      const answer = responses[method];
      return typeof answer === 'function' ? answer(args, opts) : Promise.resolve(answer);
    },
    on(event, cb) {
      listeners[event].add(cb);
      return () => listeners[event].delete(cb);
    },
    close() {
      closed += 1;
    },
  };
}

const W = '/ws/repo';
const identity = {
  kind: 'worktree',
  worktreePath: W,
  gitdir: `${W}/.git`,
  commonDir: `${W}/.git`,
  bare: false,
  head: 'a'.repeat(40),
  branch: 'main',
  detached: false,
};
const snapshot = (generation, entries = []) => ({
  worktreePath: W,
  identity,
  entries,
  tookMs: 1,
  pathLimited: false,
  generation,
  statusBackend: 'gix',
});

test('AC-012: the service module loads no native code', () => {
  const source = readFileSync(join(__dirname, '..', 'dist', 'git', 'service.js'), 'utf8');
  const imports = [...source.matchAll(/^\s*import\s[^'"]*['"]([^'"]+)['"]/gm)].map((m) => m[1]);
  assert.deepEqual(
    imports.filter((spec) => !spec.startsWith('./types')),
    [],
    `service.js must be transport-only; it imports: ${imports.join(', ')}`,
  );
});

test('§8.3 / §18.3: mille/git never imports mille-ui, and the root barrel never imports git', () => {
  const dist = join(__dirname, '..', 'dist');
  for (const file of readdirSync(join(dist, 'git')).filter((f) => f.endsWith('.js'))) {
    const source = readFileSync(join(dist, 'git', file), 'utf8');
    assert.doesNotMatch(source, /['"]@vibecook\/mille-ui/, `dist/git/${file} reaches into mille-ui`);
  }
  const pkg = JSON.parse(readFileSync(join(__dirname, '..', 'package.json'), 'utf8'));
  for (const field of ['dependencies', 'peerDependencies', 'optionalDependencies']) {
    assert.equal(pkg[field]?.['@vibecook/mille-ui'], undefined, `mille ${field} names mille-ui`);
  }
  const root = readFileSync(join(dist, 'index.js'), 'utf8');
  assert.doesNotMatch(root, /['"]\.\/git\//, 'the root barrel re-exports ./git');
});

test('the git protocol version is its own clock', () => {
  assert.equal(GIT_PROTOCOL_VERSION, 1);
  assert.equal(typeof PROTOCOL_VERSION, 'number');
});

test('AC-012: every GitService verb is one transport call with plain args', async () => {
  const transport = fakeTransport({
    open: identity,
    identity,
    status: snapshot(3),
    close: undefined,
    diff: { path: 'a.ts', tooLarge: false, tookMs: 2, hunks: [] },
    log: { commits: [], tookMs: 1, exhausted: true },
    commitFiles: { files: [{ path: 'a.ts', status: 'M' }], truncated: false },
    readObject: 'contents',
    readWorktreeFile: 'worktree',
    stage: undefined,
    unstage: undefined,
    restore: undefined,
    commit: 'b'.repeat(40),
    worktrees: [{ path: W, branch: 'main', head: null, bare: false, locked: false, prunable: false, isCurrent: true }],
    advisePerformanceConfig: { 'core.untrackedCache': 'true' },
  });
  const service = createGitService(transport);
  const ac = new AbortController();
  const signal = ac.signal;

  assert.deepEqual(await service.open(W), identity);
  assert.deepEqual(await service.identity(W), identity);
  assert.equal((await service.status(W)).generation, 3);
  assert.equal((await service.diff(W, { path: 'a.ts', spec: 'worktree', maxLines: 9, signal })).tookMs, 2);
  assert.equal((await service.log(W)).exhausted, true);
  await service.log(W, { path: 'a.ts', limit: 5, signal });
  assert.deepEqual(await service.commitFiles(W, 'c0ffee'), {
    files: [{ path: 'a.ts', status: 'M' }],
    truncated: false,
  });
  assert.equal(await service.readObject(W, { path: 'a.ts', revision: 'HEAD', signal }), 'contents');
  assert.equal(await service.readWorktreeFile(W, 'a.ts', { signal }), 'worktree');
  await service.stage(W, ['a.ts'], { signal });
  await service.unstage(W, ['a.ts']);
  await service.restore(W, ['b.ts'], { signal });
  assert.equal(
    await service.commit(W, { summary: 's', body: 'b', paths: ['a.ts'], signal }),
    'b'.repeat(40),
  );
  assert.equal((await service.worktrees(W)).length, 1);
  assert.deepEqual(await service.advisePerformanceConfig(W), { 'core.untrackedCache': 'true' });
  await service.close(W);

  const shape = transport.calls.map(({ method, args, opts }) => [method, args, opts?.signal === signal]);
  assert.deepEqual(shape, [
    ['open', { worktree: W }, false],
    ['identity', { worktree: W }, false],
    ['status', { worktree: W }, false],
    ['diff', { worktree: W, request: { path: 'a.ts', spec: 'worktree', maxLines: 9 } }, true],
    ['log', { worktree: W, request: {} }, false],
    ['log', { worktree: W, request: { path: 'a.ts', limit: 5 } }, true],
    ['commitFiles', { worktree: W, sha: 'c0ffee' }, false],
    ['readObject', { worktree: W, request: { path: 'a.ts', revision: 'HEAD' } }, true],
    ['readWorktreeFile', { worktree: W, path: 'a.ts' }, true],
    ['stage', { worktree: W, paths: ['a.ts'] }, true],
    ['unstage', { worktree: W, paths: ['a.ts'] }, false],
    ['restore', { worktree: W, paths: ['b.ts'] }, true],
    ['commit', { worktree: W, request: { summary: 's', body: 'b', paths: ['a.ts'] } }, true],
    ['worktrees', { worktree: W }, false],
    ['advisePerformanceConfig', { worktree: W }, false],
    ['close', { worktree: W }, false],
  ]);
  // Args must survive structured cloning — a MessagePort transport frames them.
  for (const { args } of transport.calls) assert.deepEqual(structuredClone(args), args);
  // Calls without a signal pass no opts at all.
  assert.equal(transport.calls[0].opts, undefined);
});

test('AC-012: events fan out by the worktree they were opened as', () => {
  const transport = fakeTransport();
  const service = createGitService(transport);
  const seenA = [];
  const seenB = [];
  const offA = service.onStatus('/a', (s) => seenA.push(s.generation));
  const offA2 = service.onStatus('/a', (s) => seenA.push(-s.generation));
  service.onStatus('/b', (s) => seenB.push(s.generation));
  const lists = [];
  const offList = service.onWorktrees('/a', (w) => lists.push(w.length));

  transport.emit('status', { worktree: '/a', snapshot: snapshot(1) });
  transport.emit('status', { worktree: '/b', snapshot: snapshot(2) });
  transport.emit('status', { worktree: '/c', snapshot: snapshot(9) });
  transport.emit('worktrees', { worktree: '/a', worktrees: [{}, {}] });
  transport.emit('worktrees', { worktree: '/b', worktrees: [{}] });
  assert.deepEqual(seenA, [1, -1]);
  assert.deepEqual(seenB, [2]);
  assert.deepEqual(lists, [2]);

  offA();
  offA(); // idempotent
  transport.emit('status', { worktree: '/a', snapshot: snapshot(3) });
  assert.deepEqual(seenA, [1, -1, -3]);
  offA2();
  offList();
  transport.emit('status', { worktree: '/a', snapshot: snapshot(4) });
  transport.emit('worktrees', { worktree: '/a', worktrees: [] });
  assert.deepEqual(seenA, [1, -1, -3]);
  assert.deepEqual(lists, [2]);
});

test('AC-012: a throwing listener does not starve the others', () => {
  const transport = fakeTransport();
  const service = createGitService(transport);
  const seen = [];
  service.onStatus(W, () => {
    throw new Error('listener bug');
  });
  service.onStatus(W, (s) => seen.push(s.generation));
  transport.emit('status', { worktree: W, snapshot: snapshot(5) });
  assert.deepEqual(seen, [5]);
});

test('AC-012: errors pass through untouched; dispose closes the transport once', async () => {
  const failure = new GitError('EHOOK', 'commit-msg said no');
  const transport = fakeTransport({ commit: () => Promise.reject(failure) });
  const service = createGitService(transport);
  await assert.rejects(service.commit(W, { summary: 's', paths: ['a'] }), (e) => e === failure);

  service.onStatus(W, () => assert.fail('a disposed service delivered a snapshot'));
  assert.equal(transport.listenerCount('status'), 1);
  await service.dispose();
  await service.dispose();
  assert.equal(transport.closed, 1);
  assert.equal(transport.listenerCount('status'), 0);
  assert.equal(transport.listenerCount('worktrees'), 0);
  transport.emit('status', { worktree: W, snapshot: snapshot(6) });
});

test('bindExplorer refuses a transport that is not in-process', () => {
  assert.throws(
    () => bindExplorer(fakeTransport(), {}),
    (e) => isGitError(e) && e.code === 'EUNSUPPORTED',
  );
});

// ─── The in-process transport (needs a git-enabled .node) ───────────────

test('in-process transport: open, status events, hintPaths and close', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'a\n' });
  const transport = await createInProcessGitTransport({
    allowedRoots: [repo],
    debounceMs: 20,
    idleRescanMs: 600_000,
  });
  const service = createGitService(transport);
  try {
    const opened = await service.open(repo);
    assert.equal(opened.kind, 'worktree');
    await service.status(repo);
    let latest = null;
    const off = service.onStatus(repo, (s) => {
      latest = s;
    });

    write(repo, 'a.ts', 'changed\n');
    await transport.call('hintPaths', { worktree: repo, paths: ['a.ts'] });
    await waitFor(() => latest !== null && xyOf(latest, 'a.ts') === '.M', {
      what: 'the hinted snapshot through the transport',
    });

    // `null` is a full status; a non-array is EINVAL, as is an unknown verb.
    await transport.call('hintPaths', { worktree: repo, paths: null });
    await assert.rejects(
      transport.call('hintPaths', { worktree: repo, paths: 'a.ts' }),
      (e) => isGitError(e) && e.code === 'EINVAL',
    );
    await assert.rejects(
      transport.call('push', { worktree: repo }),
      (e) => isGitError(e) && e.code === 'EINVAL' && /unknown git method/.test(e.message),
    );
    await assert.rejects(transport.call('status', {}), (e) => isGitError(e) && e.code === 'EINVAL');

    // Opened twice: one close keeps the events flowing, the second ends them.
    await service.open(repo);
    await service.close(repo);
    latest = null;
    write(repo, 'a.ts', 'again\n');
    await transport.call('hintPaths', { worktree: repo, paths: ['a.ts'] });
    await waitFor(() => latest !== null, { what: 'events after one of two closes' });
    off();
    await service.close(repo);
    await assert.rejects(service.status(repo), (e) => isGitError(e) && e.code === 'EINVAL');
  } finally {
    await service.dispose();
    removeTempDir(repo);
  }
});

test('in-process transport: AbortSignal reaches the engine; closed transport refuses', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'a\n' });
  const transport = await createInProcessGitTransport({ allowedRoots: [repo] });
  const service = createGitService(transport);
  try {
    await service.open(repo);
    const ac = new AbortController();
    ac.abort();
    await assert.rejects(
      service.readObject(repo, { path: 'a.ts', revision: 'HEAD', signal: ac.signal }),
      (e) => isGitError(e) && e.code === 'ECANCELED',
    );
    assert.equal(await service.readObject(repo, { path: 'a.ts', revision: 'HEAD' }), 'a\n');
    // Outside allowedRoots is EINVAL from the engine itself.
    const outside = tempDir('mille-git-out-');
    try {
      await assert.rejects(service.open(outside), (e) => isGitError(e) && e.code === 'EINVAL');
    } finally {
      removeTempDir(outside);
    }
  } finally {
    await service.dispose();
  }
  await assert.rejects(
    transport.call('status', { worktree: repo }),
    (e) => isGitError(e) && /closed/.test(e.message),
  );
  removeTempDir(repo);
});

test('bindExplorer(transport, fx) wires an in-process transport to an explorer', needsGit(), async () => {
  const { native } = await import('../dist/index.js');
  const repo = makeRepo({ 'a.ts': 'a\n' });
  const transport = await createInProcessGitTransport({
    allowedRoots: [repo],
    debounceMs: 20,
    idleRescanMs: 600_000,
  });
  const service = createGitService(transport);
  const fx = new native.FileExplorer({ roots: [repo], watchDebounceMs: 30 });
  try {
    await fx.populateFromRoots();
    await fx.startWatching();
    await service.open(repo);
    await service.status(repo);
    const unbind = bindExplorer(transport, fx);
    let latest = null;
    service.onStatus(repo, (s) => {
      latest = s;
    });
    write(repo, 'a.ts', 'a, saved in the editor\n');
    await waitFor(() => latest !== null && xyOf(latest, 'a.ts') === '.M', {
      what: 'the bound explorer’s save in status',
    });
    unbind();
  } finally {
    await service.dispose();
    await fx.dispose();
    removeTempDir(repo);
  }
});
