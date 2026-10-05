// `GitEngine.bindExplorer` — the explorer's watcher feeds git status through
// native EventBus subscribers (SPEC mille-git §11.2, PR 6b).
//
// Every explorer here is the raw napi `FileExplorer`, never the typed
// wrapper, and none of them gets an `on*` listener: the bus carries zero JS
// threadsafe functions, so whatever reaches git got there natively
// (T-NATIVE-SUB). Each engine sets `idleRescanMs` far beyond the test, so an
// idle full rescan cannot be what makes a row appear — only a hint can.

import { strict as assert } from 'node:assert';
import { mkdirSync, rmSync, symlinkSync, writeFileSync, renameSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { native } from '../dist/index.js';
import { GitEngine, isGitError } from '../dist/git/index.js';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import {
  git,
  initRepo,
  makeRepo,
  needsGit,
  tempDir,
  waitFor,
  write,
  xyOf,
} from './fixtures/git-fixture.mjs';

const NEVER = 600_000;

function engineFor(allowedRoots) {
  return new GitEngine({ allowedRoots, debounceMs: 20, maxWaitMs: 200, idleRescanMs: NEVER });
}

/** A raw native explorer with its watcher running and no JS listeners. */
async function watchedExplorer(root, { walk = true } = {}) {
  const fx = new native.FileExplorer({ roots: [root], watchDebounceMs: 30 });
  if (walk) {
    await fx.populateFromRoots();
  } else {
    fx.seedWorkspaceRoots();
  }
  await fx.startWatching();
  return fx;
}

function childNames(fx, id) {
  const snapshot = fx.getSnapshot();
  return snapshot.childrenOf(id).map((child) => snapshot.getById(child)?.name);
}

/** The latest snapshot `engine` publishes for `worktree`, kept current. */
function track(engine, worktree) {
  const state = { latest: null, all: [] };
  state.off = engine.onStatus(worktree, (snapshot) => {
    state.latest = snapshot;
    state.all.push(snapshot);
  });
  return state;
}

test(
  "T-NATIVE-SUB: a worktree-only save reaches status through 'event', no JS listener",
  needsGit(),
  async (t) => {
    const repo = makeRepo({ 'a.ts': 'a\n', 'src/b.ts': 'b\n' });
    const engine = engineFor([repo]);
    const fx = await watchedExplorer(repo);
    try {
      await engine.open(repo);
      await engine.status(repo);
      engine.bindExplorer(fx);
      const seen = track(engine, repo);

      const started = Date.now();
      writeFileSync(join(repo, 'src', 'b.ts'), 'b changed\n');
      const snapshot = await waitFor(
        () => (xyOf(seen.latest ?? { entries: [] }, 'src/b.ts') === '.M' ? seen.latest : null),
        { what: 'src/b.ts .M via the native event subscriber' },
      );
      const latency = Date.now() - started;
      t.diagnostic(`save → snapshot ${latency} ms (explorer debounce 30 + git debounce 20)`);
      assert.equal(snapshot.pathLimited, true, 'a hint, not a full rescan, carried it');
      assert.ok(latency < 3000, `AC-003 budget blown: ${latency} ms`);
      seen.off();
    } finally {
      engine.dispose();
      await fx.dispose();
      removeTempDir(repo);
    }
  },
);

test(
  'T-NATIVE-SUB: a ChangeTree with no watcher reaches status through the supplement',
  needsGit(),
  async () => {
    const repo = makeRepo({ 'deep/x.ts': 'x\n', 'top.ts': 't\n' });
    const engine = engineFor([repo]);
    // Roots seeded only, and never `startWatching`/`resync` (both start the
    // watcher): no 'event' can be emitted. `resolvePath` hydrates the chain
    // to a file and announces it on 'change:tree' — the one signal left.
    const fx = new native.FileExplorer({ roots: [repo] });
    try {
      fx.seedWorkspaceRoots();
      await engine.open(repo);
      await engine.status(repo);
      engine.bindExplorer(fx);
      const seen = track(engine, repo);

      writeFileSync(join(repo, 'deep', 'x.ts'), 'x changed\n');
      const id = await fx.resolvePath(join(repo, 'deep', 'x.ts'));
      assert.equal(typeof id, 'number', 'resolvePath hydrated the chain');
      await waitFor(() => xyOf(seen.latest ?? { entries: [] }, 'deep/x.ts') === '.M', {
        what: 'deep/x.ts .M via the ChangeTree subscriber',
      });
      seen.off();
    } finally {
      engine.dispose();
      await fx.dispose();
      removeTempDir(repo);
    }
  },
);

test('T-DELTA / AC-014: binding git leaves takePendingChanges to the host', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'a\n' });
  const engine = engineFor([repo]);
  const fx = await watchedExplorer(repo);
  try {
    await engine.open(repo);
    await engine.status(repo);
    engine.bindExplorer(fx);
    const seen = track(engine, repo);
    const id = await fx.resolvePath(join(repo, 'a.ts'));
    assert.equal(typeof id, 'number');
    fx.takePendingChanges(); // the walk's own changes

    writeFileSync(join(repo, 'a.ts'), 'a, now longer\n');
    // Git heard the save, so the watcher batch that carried it is done…
    await waitFor(() => xyOf(seen.latest ?? { entries: [] }, 'a.ts') === '.M', {
      what: 'git status to see the save',
    });
    // …and the host's drain still holds it: git did not take it.
    const changes = fx.takePendingChanges();
    assert.ok(
      changes.changedIds.includes(id),
      `host ChangeSet lost a.ts (${id}): ${JSON.stringify(changes.changedIds)}`,
    );
    seen.off();
  } finally {
    engine.dispose();
    await fx.dispose();
    removeTempDir(repo);
  }
});

test(
  'T-HINT-LAZY: a save under a folder the explorer never walked still refreshes status',
  needsGit(),
  async () => {
    const repo = makeRepo({ 'deep/nested/dir/file.ts': 'one\n', 'top.ts': 't\n' });
    const engine = engineFor([repo]);
    const fx = await watchedExplorer(repo, { walk: false });
    try {
      await engine.open(repo);
      await engine.status(repo);
      engine.bindExplorer(fx);
      const seen = track(engine, repo);
      const snapshot = fx.getSnapshot();
      const rootId = fx.getSnapshot().roots()[0].id;
      assert.equal(snapshot.childrenOf(rootId).length, 0, 'nothing below the root was walked');

      writeFileSync(join(repo, 'deep', 'nested', 'dir', 'file.ts'), 'two\n');
      await waitFor(
        () => xyOf(seen.latest ?? { entries: [] }, 'deep/nested/dir/file.ts') === '.M',
        { what: 'the unwalked nested save in status' },
      );
      // What happens to the store afterwards is the explorer's business: its
      // reconcile may list the nearest known directory and, as events land,
      // the ancestors below it. The property is the precondition above — at
      // the moment of the save no entry for the file or its folders existed —
      // and that git saw the save anyway.
      seen.off();
    } finally {
      engine.dispose();
      await fx.dispose();
      removeTempDir(repo);
    }
  },
);

test(
  'routing: one explorer over two checkouts hints only the owner, through a symlinked spelling',
  needsGit(),
  async () => {
    const workspace = tempDir('mille-git-ws-');
    initRepo(join(workspace, 'alpha'));
    initRepo(join(workspace, 'beta'));
    for (const name of ['alpha', 'beta']) {
      write(join(workspace, name), 'x.ts', 'x\n');
      git(join(workspace, name), 'add', '-A');
      git(join(workspace, name), 'commit', '-q', '-m', 'init');
    }
    // The explorer and the engine both use a symlinked spelling of the
    // workspace — the `/var` vs `/private/var` shape. Watcher paths arrive in
    // that spelling; routing must still find each checkout by realpath.
    let root = workspace;
    const holder = tempDir('mille-git-link-');
    if (process.platform !== 'win32') {
      root = join(holder, 'ws-link');
      symlinkSync(workspace, root);
    }
    const engine = engineFor([root]);
    const fx = await watchedExplorer(root);
    try {
      const alpha = join(root, 'alpha');
      const beta = join(root, 'beta');
      assert.equal((await engine.open(alpha)).worktreePath, alpha);
      await engine.open(beta);
      await engine.status(alpha);
      const betaBefore = await engine.status(beta);
      engine.bindExplorer(fx);
      const seenAlpha = track(engine, alpha);
      const seenBeta = track(engine, beta);

      writeFileSync(join(workspace, 'alpha', 'x.ts'), 'x changed\n');
      await waitFor(() => xyOf(seenAlpha.latest ?? { entries: [] }, 'x.ts') === '.M', {
        what: 'alpha/x.ts .M',
      });
      await new Promise((r) => setTimeout(r, 300));
      assert.equal(seenBeta.all.length, 0, 'beta got a status run for alpha’s save');
      assert.equal((await engine.status(beta)).generation, betaBefore.generation);
      seenAlpha.off();
      seenBeta.off();
    } finally {
      engine.dispose();
      await fx.dispose();
      removeTempDir(holder);
      removeTempDir(workspace);
    }
  },
);

test(
  'a rename hints both names, so the old row goes with the new one',
  needsGit(),
  async () => {
    const repo = makeRepo({ 'src/old.ts': 'body\n' });
    const engine = engineFor([repo]);
    const fx = await watchedExplorer(repo);
    try {
      await engine.open(repo);
      await engine.status(repo);
      engine.bindExplorer(fx);
      const seen = track(engine, repo);

      renameSync(join(repo, 'src', 'old.ts'), join(repo, 'src', 'new.ts'));
      await waitFor(
        () => {
          const s = seen.latest ?? { entries: [] };
          return xyOf(s, 'src/old.ts') === '.D' && xyOf(s, 'src/new.ts') === '.?';
        },
        { what: 'src/old.ts .D and src/new.ts .? after an on-disk rename' },
      );
      seen.off();
    } finally {
      engine.dispose();
      await fx.dispose();
      removeTempDir(repo);
    }
  },
);

test('unbinding stops the hints; the engine itself keeps working', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'a\n', 'b.ts': 'b\n' });
  const engine = engineFor([repo]);
  const fx = await watchedExplorer(repo);
  try {
    await engine.open(repo);
    await engine.status(repo);
    const unbind = engine.bindExplorer(fx);
    unbind();
    unbind(); // idempotent
    const seen = track(engine, repo);

    writeFileSync(join(repo, 'a.ts'), 'a changed\n');
    await new Promise((r) => setTimeout(r, 600));
    assert.equal(seen.all.length, 0, 'an unbound explorer still hinted git');

    engine.hintPaths(repo, ['a.ts']);
    await waitFor(() => xyOf(seen.latest ?? { entries: [] }, 'a.ts') === '.M', {
      what: 'a.ts via an explicit hint',
    });
    seen.off();
  } finally {
    engine.dispose();
    await fx.dispose();
    removeTempDir(repo);
  }
});

test('lifetimes: either side may go first', needsGit(), async () => {
  const repo = makeRepo({ 'a.ts': 'a\n' });
  // Engine disposed while the explorer keeps watching.
  {
    const engine = engineFor([repo]);
    const fx = await watchedExplorer(repo);
    await engine.open(repo);
    engine.bindExplorer(fx);
    engine.dispose();
    writeFileSync(join(repo, 'a.ts'), 'after engine dispose\n');
    mkdirSync(join(repo, 'later'), { recursive: true });
    const rootId = fx.getSnapshot().roots()[0].id;
    await waitFor(() => childNames(fx, rootId).includes('later'), {
      what: 'the explorer to keep reconciling after the engine went away',
    });
    await fx.dispose();
  }
  // Explorer disposed while the engine keeps its binding.
  {
    const engine = engineFor([repo]);
    const fx = await watchedExplorer(repo);
    try {
      await engine.open(repo);
      engine.bindExplorer(fx);
      await fx.dispose();
      writeFileSync(join(repo, 'a.ts'), 'after explorer dispose\n');
      const snapshot = await engine.refresh(repo, null);
      assert.equal(xyOf(snapshot, 'a.ts'), '.M');
    } finally {
      engine.dispose();
    }
  }
  rmSync(join(repo, 'later'), { recursive: true, force: true });
  removeTempDir(repo);
});

test('bindExplorer refuses something that is not an explorer', needsGit(), () => {
  const repo = makeRepo();
  const engine = engineFor([repo]);
  try {
    assert.throws(() => engine.bindExplorer({}), (e) => isGitError(e) && e.code === 'EINVAL');
    assert.throws(() => engine.bindExplorer(null), (e) => isGitError(e) && e.code === 'EINVAL');
  } finally {
    engine.dispose();
    removeTempDir(repo);
  }
});
