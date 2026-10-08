// The napi `GitEngine` behind the `git` feature (SPEC mille-git PR 6a).
//
// Runs against whichever `.node` the loader picks. With the default build the
// git half skips, and what remains is the feature-off contract: no engine, no
// throw (T-FEAT-OFF). With `build:napi:git*` it is the feature-on smoke and the
// binding's error, cancellation and lifecycle seams (T-FEAT-ON, T-ABORT).

import { strict as assert } from 'node:assert';
import { chmodSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import * as root from '../dist/index.js';
import { buildIdentity } from '../dist/index.js';
import {
  GitEngine,
  GitError,
  isGitError,
  isNativeGitAvailable,
} from '../dist/git/index.js';
import { tryParseGitReason } from '../dist/git/error.js';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import {
  git,
  gitStdin,
  makeRepo,
  needsGit,
  tempDir,
  waitFor,
  write,
  xyOf,
} from './fixtures/git-fixture.mjs';

test('isNativeGitAvailable agrees with the features the loaded binary reports', () => {
  const features = buildIdentity().nativeFeatures;
  assert.ok(Array.isArray(features));
  assert.equal(isNativeGitAvailable(), features.includes('git'));
  if (process.env.MILLE_EXPECT_GIT === '1') {
    assert.equal(isNativeGitAvailable(), true, 'MILLE_EXPECT_GIT=1 needs a build:napi:git* binary');
  }
});

test('the root barrel does not export the git engine (§18.3)', () => {
  for (const name of ['GitEngine', 'isNativeGitAvailable', 'createGitService', 'GitError']) {
    assert.equal(name in root, false, `@vibecook/mille must not export ${name}`);
  }
});

test('GIT| reasons parse into GitError; anything else is not one', () => {
  const err = tryParseGitReason('GIT|EINVAL|../x|path escapes: a|b');
  assert.ok(err instanceof GitError);
  assert.equal(err.code, 'EINVAL');
  assert.equal(err.path, '../x');
  assert.equal(err.message, 'path escapes: a|b');
  assert.equal(isGitError(err), true);

  const noPath = tryParseGitReason('GIT|EHOOK||commit-msg rejected');
  assert.equal(noPath.code, 'EHOOK');
  assert.equal(noPath.path, undefined);

  assert.equal(tryParseGitReason('GIT|EBOGUS||x').code, 'EUNKNOWN');
  assert.equal(tryParseGitReason('FX|EINVAL||x'), null);
  assert.equal(tryParseGitReason('GIT|EINVAL'), null);
  assert.equal(tryParseGitReason(undefined), null);
  assert.equal(isGitError(new Error('plain')), false);
});

test('without the git feature the wrapper refuses to construct, and says why', () => {
  if (isNativeGitAvailable()) return;
  assert.throws(
    () => new GitEngine({ allowedRoots: ['/'] }),
    (err) => isGitError(err) && err.code === 'EUNSUPPORTED' && /build:napi:git/.test(err.message),
  );
});

test('open, status, hints and the status listener on a real repository', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'a\n', 'src/b.ts': 'b\n' });
  const engine = new GitEngine({ allowedRoots: [repo], debounceMs: 20 });
  try {
    const identity = await engine.open(repo);
    assert.equal(identity.kind, 'worktree');
    assert.equal(identity.worktreePath, repo);
    assert.match(identity.branch ?? '', /^(main|master)$/);
    assert.match(identity.head ?? '', /^[0-9a-f]{40}$/);
    assert.equal(identity.bare, false);
    assert.ok(identity.gitdir?.endsWith('.git'));
    assert.deepEqual(engine.identity(repo), identity);

    const clean = await engine.status(repo);
    assert.deepEqual(clean.entries, []);
    assert.equal(typeof clean.tookMs, 'number');
    assert.equal(typeof clean.generation, 'number');
    assert.equal(clean.statusBackend, 'gix');

    const seen = [];
    const off = engine.onStatus(repo, (snapshot) => seen.push(snapshot));
    // A worktree-only save moves no gitdir file, so only the hint can see it.
    write(repo, 'src/b.ts', 'b2\n');
    write(repo, 'new.ts', 'n\n');
    engine.hintPaths(repo, ['src/b.ts', 'new.ts']);
    const hinted = await waitFor(
      () => seen.find((s) => xyOf(s, 'src/b.ts') === '.M' && xyOf(s, 'new.ts') === '.?'),
      { what: 'a hinted snapshot with src/b.ts .M and new.ts .?' },
    );
    assert.equal(hinted.pathLimited, true);
    assert.equal(typeof hinted.generation, 'number');
    off();
    off(); // idempotent

    // `null` hints a full status.
    write(repo, 'a.ts', 'a2\n');
    const full = await engine.refresh(repo, null);
    assert.equal(xyOf(full, 'a.ts'), '.M');
    assert.equal(full.pathLimited, false);
  } finally {
    engine.dispose();
    removeTempDir(repo);
  }
});

test('diff, log, readObject, readWorktreeFile, commitFiles and worktrees', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'one\ntwo\n' });
  const engine = new GitEngine({ allowedRoots: [repo] });
  try {
    await engine.open(repo);
    const first = git(repo, 'rev-parse', 'HEAD').trim();
    write(repo, 'a.ts', 'one\nTWO\n');
    git(repo, 'commit', '-q', '-am', 'second\n\nbody');
    write(repo, 'a.ts', 'one\nTWO\nthree\n');

    const diff = await engine.diff(repo, { path: 'a.ts', spec: 'worktree' });
    assert.equal(diff.tooLarge, false);
    assert.equal(typeof diff.tookMs, 'number');
    assert.ok(diff.hunks.length >= 1);
    assert.ok(diff.hunks[0].lines.some((l) => l.kind === 'add' && l.text === 'three'));

    const head = git(repo, 'rev-parse', 'HEAD').trim();
    const commitDiff = await engine.diff(repo, { path: 'a.ts', spec: 'commit', commit: head });
    assert.ok(commitDiff.hunks[0].lines.some((l) => l.kind === 'del' && l.text === 'two'));

    const page = await engine.log(repo, { limit: 10 });
    assert.deepEqual(
      page.commits.map((c) => c.id),
      [head, first],
    );
    assert.equal(page.commits[0].message, 'second');
    assert.equal(typeof page.commits[0].timestampMs, 'number');
    assert.equal(page.exhausted, true);
    const pathPage = await engine.log(repo, { path: 'a.ts' });
    assert.equal(pathPage.commits.length, 2);

    assert.equal(await engine.readObject(repo, { path: 'a.ts', revision: first }), 'one\ntwo\n');
    assert.equal(await engine.readObject(repo, { path: 'missing.ts', revision: 'HEAD' }), null);
    assert.equal(await engine.readWorktreeFile(repo, 'a.ts'), 'one\nTWO\nthree\n');

    writeFileSync(join(repo, 'blob.bin'), Buffer.from([0xff, 0xfe, 0x00, 0x01]));
    const bytes = await engine.readWorktreeFile(repo, 'blob.bin');
    assert.ok(bytes instanceof Uint8Array && !(typeof bytes === 'string'));
    assert.deepEqual([...bytes], [0xff, 0xfe, 0x00, 0x01]);

    const files = await engine.commitFiles(repo, head);
    assert.deepEqual(files, { files: [{ path: 'a.ts', status: 'M' }], truncated: false });

    const worktrees = await engine.worktrees(repo);
    assert.equal(worktrees.length, 1);
    assert.equal(worktrees[0].isCurrent, true);

    const advice = await engine.advisePerformanceConfig(repo);
    assert.equal(typeof advice, 'object');
  } finally {
    engine.dispose();
    removeTempDir(repo);
  }
});

test('commitFiles says when its 10 000-file cap cut the list (§14.3)', needsGit(), async () => {
  const repo = makeRepo({ 'seed.ts': 'seed\n' });
  const engine = new GitEngine({ allowedRoots: [repo] });
  try {
    // Plumbing, not ten thousand files on disk: one blob named `n` times in a
    // flat tree, committed with no parent, so every entry is an addition.
    const blob = gitStdin(repo, ['hash-object', '-w', '--stdin'], 'x\n').trim();
    const rootCommitOf = (n) => {
      let listing = '';
      for (let i = 0; i < n; i += 1) listing += `100644 blob ${blob}\tf${String(i).padStart(5, '0')}.txt\n`;
      const tree = gitStdin(repo, ['mktree'], listing).trim();
      return gitStdin(repo, ['commit-tree', tree, '-m', `${n} files`], '').trim();
    };
    const atCap = rootCommitOf(10_000);
    const overCap = rootCommitOf(10_001);
    await engine.open(repo);

    const whole = await engine.commitFiles(repo, atCap);
    assert.equal(whole.truncated, false, 'exactly the cap is the whole list');
    assert.equal(whole.files.length, 10_000);

    const cut = await engine.commitFiles(repo, overCap);
    assert.equal(cut.truncated, true);
    assert.equal(cut.files.length, 10_000);
    assert.equal(cut.files[0].path, 'f00000.txt');
    assert.equal(cut.files.at(-1).path, 'f09999.txt', 'the first 10 000 by path');
  } finally {
    engine.dispose();
    removeTempDir(repo);
  }
});

test('stage, unstage, restore and commit the include-set', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'a\n', 'b.ts': 'b\n', 'c.ts': 'c\n' });
  const engine = new GitEngine({ allowedRoots: [repo] });
  try {
    await engine.open(repo);
    write(repo, 'a.ts', 'a2\n');
    write(repo, 'b.ts', 'b2\n');
    write(repo, 'c.ts', 'c2\n');

    await engine.stage(repo, ['a.ts']);
    assert.equal(xyOf(await engine.refresh(repo, null), 'a.ts'), 'M.');
    await engine.unstage(repo, ['a.ts']);
    assert.equal(xyOf(await engine.refresh(repo, null), 'a.ts'), '.M');

    // restore writes index bytes, not HEAD's (AC-017).
    write(repo, 'c.ts', 'c-staged\n');
    await engine.stage(repo, ['c.ts']);
    write(repo, 'c.ts', 'c-worktree\n');
    await engine.restore(repo, ['c.ts']);
    assert.equal(readFileSync(join(repo, 'c.ts'), 'utf8'), 'c-staged\n');

    const sha = await engine.commit(repo, {
      summary: 'include two',
      body: 'line one\nline two',
      paths: ['a.ts', 'b.ts'],
    });
    assert.match(sha, /^[0-9a-f]{40}$/);
    assert.equal(git(repo, 'log', '-1', '--format=%B').trim(), 'include two\n\nline one\nline two');
    const after = await engine.refresh(repo, null);
    assert.equal(xyOf(after, 'a.ts'), null);
    assert.equal(xyOf(after, 'b.ts'), null);
    // c.ts was staged but excluded: the commit unstaged it, the change stays.
    assert.notEqual(xyOf(after, 'c.ts'), null);
  } finally {
    engine.dispose();
    removeTempDir(repo);
  }
});

test('engine errors arrive as GitError with the GIT| code', needsGit(), async () => {
  const repo = makeRepo();
  const plain = tempDir('mille-git-none-');
  const engine = new GitEngine({ allowedRoots: [repo, plain] });
  try {
    await assert.rejects(engine.open('relative/path'), (e) => isGitError(e) && e.code === 'EINVAL');
    const outside = tempDir('mille-git-outside-');
    try {
      await assert.rejects(
        engine.open(outside),
        (e) => e instanceof GitError && e.code === 'EINVAL' && e.path === outside,
      );
    } finally {
      removeTempDir(outside);
    }

    // Not a repository: open and status never throw; mutations are ENOTAREPO.
    const none = await engine.open(plain);
    assert.equal(none.kind, 'none');
    const empty = await engine.status(plain);
    assert.deepEqual(empty.entries, []);
    assert.equal(empty.identity.kind, 'none');
    await assert.rejects(engine.stage(plain, ['x']), (e) => isGitError(e) && e.code === 'ENOTAREPO');

    // A path escaping the checkout.
    await engine.open(repo);
    await assert.rejects(
      engine.diff(repo, { path: '../escape.ts', spec: 'worktree' }),
      (e) => isGitError(e) && e.code === 'EINVAL',
    );
    await assert.rejects(
      engine.diff(repo, { path: 'a.ts', spec: 'sideways' }),
      (e) => isGitError(e) && e.code === 'EINVAL' && /unknown diff spec/.test(e.message),
    );
    // Not open.
    assert.throws(() => engine.identity(tempDir('mille-git-closed-')), (e) => isGitError(e));

    // An empty allow-list fails closed.
    const closed = new GitEngine({ allowedRoots: [] });
    try {
      await assert.rejects(closed.open(repo), (e) => isGitError(e) && e.code === 'EINVAL');
    } finally {
      closed.dispose();
    }
  } finally {
    engine.dispose();
    removeTempDir(repo);
    removeTempDir(plain);
  }
});

test('a missing git binary: reads work, writes are ENOGIT, warned once', needsGit(), async () => {
  const repo = makeRepo();
  const engine = new GitEngine({ allowedRoots: [repo], gitPath: join(repo, 'no-such-git') });
  const warnings = [];
  const off = engine.onWarning((w) => warnings.push(w));
  try {
    assert.equal(engine.gitPath, null);
    await engine.open(repo);
    write(repo, 'a.ts', 'changed\n');
    assert.equal(xyOf(await engine.refresh(repo, null), 'a.ts'), '.M');
    await assert.rejects(engine.stage(repo, ['a.ts']), (e) => isGitError(e) && e.code === 'ENOGIT');
    await assert.rejects(engine.stage(repo, ['a.ts']), (e) => isGitError(e) && e.code === 'ENOGIT');
    await waitFor(() => warnings.length > 0, { what: 'an ENOGIT warning' });
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(warnings.filter((w) => w.code === 'ENOGIT').length, 1);
    assert.equal(typeof warnings[0].message, 'string');
  } finally {
    off();
    engine.dispose();
    removeTempDir(repo);
  }
});

test('AbortSignal: pre-aborted calls reject ECANCELED without starting', needsGit(), async () => {
  const repo = makeRepo();
  const engine = new GitEngine({ allowedRoots: [repo] });
  try {
    await engine.open(repo);
    const ac = new AbortController();
    ac.abort();
    for (const call of [
      () => engine.diff(repo, { path: 'a.ts', spec: 'worktree', signal: ac.signal }),
      () => engine.log(repo, { signal: ac.signal }),
      () => engine.readObject(repo, { path: 'a.ts', revision: 'HEAD', signal: ac.signal }),
      () => engine.stage(repo, ['a.ts'], { signal: ac.signal }),
      () => engine.commit(repo, { summary: 's', paths: ['a.ts'], signal: ac.signal }),
    ]) {
      await assert.rejects(call(), (e) => isGitError(e) && e.code === 'ECANCELED');
    }
    // The native registry rejects a second call reusing an in-flight id.
    const first = engine.native.log(repo, {}, 'same-id');
    assert.throws(() => engine.native.log(repo, {}, 'same-id'), /GIT\|EINVAL\|.*duplicate operationId/);
    await first;
    assert.equal(engine.native.cancelOperation('same-id'), false, 'released after completion');
  } finally {
    engine.dispose();
    removeTempDir(repo);
  }
});

test(
  'AbortSignal mid-commit kills a sleeping commit-msg hook (T-ABORT)',
  process.platform === 'win32' ? { skip: 'POSIX hook script' } : needsGit(),
  async () => {
    const repo = makeRepo();
    const engine = new GitEngine({ allowedRoots: [repo] });
    try {
      await engine.open(repo);
      const hook = join(repo, '.git', 'hooks', 'commit-msg');
      mkdirSync(join(repo, '.git', 'hooks'), { recursive: true });
      writeFileSync(hook, '#!/bin/sh\nsleep 30\n');
      chmodSync(hook, 0o755);
      write(repo, 'a.ts', 'changed\n');
      const ac = new AbortController();
      const started = Date.now();
      const pending = engine.commit(repo, { summary: 'slow', paths: ['a.ts'], signal: ac.signal });
      setTimeout(() => ac.abort(), 300);
      await assert.rejects(pending, (e) => isGitError(e) && e.code === 'ECANCELED');
      assert.ok(Date.now() - started < 10_000, 'abort must not wait out the hook');
    } finally {
      engine.dispose();
      removeTempDir(repo);
    }
  },
);

test('dispose cancels, unsubscribes, and fails every later call', needsGit(), async () => {
  const repo = makeRepo();
  const engine = new GitEngine({ allowedRoots: [repo] });
  await engine.open(repo);
  const off = engine.onStatus(repo, () => {});
  engine.dispose();
  engine.dispose();
  off();
  try {
    await assert.rejects(engine.status(repo), (e) => isGitError(e) && /disposed/.test(e.message));
    assert.throws(() => engine.hintPaths(repo, null), (e) => isGitError(e) && e.code === 'EINVAL');
    assert.throws(() => engine.onStatus(repo, () => {}), (e) => isGitError(e));
  } finally {
    removeTempDir(repo);
  }
});
