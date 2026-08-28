// Phase 13.5 — integration tests for the git decoration companion.
//
// Coverage:
//   1. registerGitDecorations returns a disposable handle.
//   2. After initial status fetch, decorations land on the engine for
//      changed paths.
//   3. client.onChange triggers a recompute.
//   4. Batcher coalesces rapid onChange calls — 3 fires within one
//      debounce window → one onDidChange.
//   5. propagateToParent: false → only leaves decorated.
//   6. propagateToParent: true → ancestors decorated with muted letter.
//   7. dispose() unregisters and stops calling client.onChange.
//   8. refresh() forces immediate re-fetch.
//   9. (bonus) dispose() stops the batcher (late client.onChange has
//      no effect on listeners).
//  10. (bonus) createBatcher timing sanity — trailing-debounce fires
//      once per burst.

import { test } from 'node:test';
import { strict as assert } from 'node:assert';

const { registerGitDecorations, createBatcher } = await import(
  '../dist/git.js'
);

// ─── Fake engine / snapshot ───────────────────────────────────────────

// Minimal snapshot that answers `getById(id) → { id, parentId }` for
// the ancestor walk. Rows are described as `{ id, parentId, path }`.
function createFakeEngineForGit(rows) {
  const byId = new Map();
  const byPath = new Map(); // workspace-relative path → Entry
  for (const r of rows) {
    const entry = {
      id: r.id,
      parentId: r.parentId,
      name: r.path.split('/').pop() ?? '',
      kind: r.kind ?? 0,
      size: 0,
      mtimeMs: 0,
      ctimeMs: 0,
      isIgnored: false,
      isReadonly: false,
      isHidden: false,
    };
    byId.set(r.id, entry);
    byPath.set(r.path, entry);
  }
  const snapshot = {
    getById: (id) => byId.get(id) ?? null,
  };
  const registeredProviders = [];
  let registeredCount = 0;
  let disposedCount = 0;
  const fx = {
    getSnapshot: () => snapshot,
    getByUri: (uri) => {
      // URI path is `<rootPath>/<workspaceRelative>`; strip the root.
      const rel = uri.path.replace(/^\/ROOT\/?/, '');
      return byPath.get(rel) ?? null;
    },
    registerDecorationProvider: (provider) => {
      registeredProviders.push(provider);
      registeredCount += 1;
      return {
        dispose: () => {
          disposedCount += 1;
        },
      };
    },
    _stats: () => ({
      registeredCount,
      disposedCount,
      providers: registeredProviders,
    }),
  };
  return { fx, snapshot, byId };
}

// ─── Fake GitClient ───────────────────────────────────────────────────

function createFakeGitClient(initialEntries = []) {
  let status = toMap(initialEntries);
  const listeners = new Set();
  let getStatusCalls = 0;

  function toMap(entries) {
    const m = new Map();
    for (const e of entries) m.set(e.path, e);
    return m;
  }

  return {
    async getStatus(_root) {
      getStatusCalls += 1;
      // Return a fresh copy so mutations don't leak.
      return new Map(status);
    },
    onChange(cb) {
      listeners.add(cb);
      return () => {
        listeners.delete(cb);
      };
    },
    // Test controls
    setStatus(entries) {
      status = toMap(entries);
    },
    fire() {
      for (const l of [...listeners]) l();
    },
    get listenerCount() {
      return listeners.size;
    },
    get getStatusCalls() {
      return getStatusCalls;
    },
  };
}

// ─── Helpers ──────────────────────────────────────────────────────────

function nextTick() {
  return new Promise((resolve) => setImmediate(resolve));
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Wait until `predicate()` returns true or timeout. Works around the
// fact that `registerGitDecorations` kicks off the initial fetch
// asynchronously (fire-and-forget).
async function waitFor(predicate, { timeoutMs = 500, stepMs = 5 } = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await sleep(stepMs);
  }
  assert.fail(`waitFor timed out after ${timeoutMs}ms`);
}

// Build a small tree: root folder + nested file.
//   id=1 /            (root)
//   id=2 /src         (folder)
//   id=3 /src/a.ts    (file)
//   id=4 /src/b.ts    (file)
//   id=5 /README.md   (file)
function sampleRows() {
  return [
    { id: 1, parentId: null, path: '', kind: 1 },
    { id: 2, parentId: 1, path: 'src', kind: 1 },
    { id: 3, parentId: 2, path: 'src/a.ts', kind: 0 },
    { id: 4, parentId: 2, path: 'src/b.ts', kind: 0 },
    { id: 5, parentId: 1, path: 'README.md', kind: 0 },
  ];
}

// ─── Test 1: disposable handle ────────────────────────────────────────

test('registerGitDecorations returns a disposable handle', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
  });
  assert.equal(typeof handle.dispose, 'function');
  assert.equal(typeof handle.refresh, 'function');
  handle.dispose();
  // Second dispose is a no-op.
  handle.dispose();
});

// ─── Test 2: initial status → decorations on engine ───────────────────

test('after initial fetch, decorations appear for changed paths', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([
    { path: 'src/a.ts', status: 'M' },
    { path: 'README.md', status: '?' },
  ]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: false,
  });

  // Initial fetch is scheduled async; wait for it to land.
  await waitFor(() => client.getStatusCalls >= 1);
  // And one more tick for the async path-resolution loop.
  await nextTick();
  await nextTick();

  const { providers } = fx._stats();
  assert.equal(providers.length, 1, 'exactly one provider registered');
  const provider = providers[0];

  // `provide(entry)` returns the decoration for the changed ids.
  const decA = provider.provide({ id: 3 });
  assert.ok(decA, 'src/a.ts must be decorated');
  assert.equal(decA.letter, 'M', 'a bare SCM status is a letter, not a count badge');
  assert.equal(decA.badge, undefined, 'leaf must not also carry a badge');
  assert.ok(typeof decA.color === 'string', 'M status must have a color');

  const decReadme = provider.provide({ id: 5 });
  assert.ok(decReadme);
  assert.equal(decReadme.letter, '?');

  // Clean paths return null.
  assert.equal(provider.provide({ id: 4 }), null, 'src/b.ts is clean');
  assert.equal(provider.provide({ id: 2 }), null, 'folder not propagated');

  handle.dispose();
});

// ─── Test 3: client.onChange triggers recompute ───────────────────────

test('client.onChange triggers a recompute', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([
    { path: 'src/a.ts', status: 'M' },
  ]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: false,
    batchOptions: { debounceMs: 10, maxWaitMs: 100 },
  });

  await waitFor(() => client.getStatusCalls >= 1);
  const initialCalls = client.getStatusCalls;

  // Change status and fire onChange. A debounce window plus a tick
  // later the recompute must have run.
  client.setStatus([
    { path: 'src/a.ts', status: 'M' },
    { path: 'src/b.ts', status: 'A' },
  ]);
  client.fire();

  await waitFor(
    () => client.getStatusCalls > initialCalls,
    { timeoutMs: 200 },
  );
  await nextTick();

  const { providers } = fx._stats();
  const provider = providers[0];
  const decB = provider.provide({ id: 4 });
  assert.ok(decB, 'src/b.ts must now be decorated');
  assert.equal(decB.letter, 'A');

  handle.dispose();
});

// ─── Test 4: batcher coalesces rapid onChange calls ───────────────────

test('batcher coalesces 3 rapid onChange calls into 1 recompute', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([
    { path: 'src/a.ts', status: 'M' },
  ]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: false,
    batchOptions: { debounceMs: 50, maxWaitMs: 500 },
  });

  // Wait for initial fetch so it's out of the way. Pump the event
  // loop a few times so the full recompute (getStatus + async
  // getByUri loop + any listener notification) fully settles before
  // we subscribe — otherwise an in-flight initial notify could race
  // our coalescing assertion.
  await waitFor(() => client.getStatusCalls >= 1);
  for (let i = 0; i < 5; i += 1) await nextTick();
  await sleep(20);

  const baseline = client.getStatusCalls;

  // Count onDidChange firings so we can assert coalescing.
  let didChangeCount = 0;
  const { providers } = fx._stats();
  const provider = providers[0];
  const sub = provider.onDidChange(() => {
    didChangeCount += 1;
  });

  // Three fires within ~10ms — should collapse into one.
  client.fire();
  await sleep(5);
  client.fire();
  await sleep(5);
  client.fire();

  // Wait past the debounce window plus a tick for the async recompute.
  await sleep(80);
  await nextTick();
  await nextTick();

  assert.equal(
    client.getStatusCalls,
    baseline + 1,
    `3 fires in 10ms must yield exactly 1 getStatus call; got ${client.getStatusCalls - baseline}`,
  );
  assert.equal(
    didChangeCount,
    1,
    `3 fires in 10ms must yield exactly 1 onDidChange; got ${didChangeCount}`,
  );

  sub.dispose();
  handle.dispose();
});

// ─── Test 5: propagateToParent: false → only leaves ──────────────────

test('propagateToParent: false → only leaves decorated', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([
    { path: 'src/a.ts', status: 'M' },
  ]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: false,
  });

  await waitFor(() => client.getStatusCalls >= 1);
  await nextTick();
  await nextTick();

  const { providers } = fx._stats();
  const provider = providers[0];

  assert.ok(provider.provide({ id: 3 }), 'leaf must be decorated');
  assert.equal(
    provider.provide({ id: 2 }),
    null,
    'ancestor folder must NOT be decorated when propagateToParent is false',
  );
  assert.equal(
    provider.provide({ id: 1 }),
    null,
    'root must NOT be decorated when propagateToParent is false',
  );

  handle.dispose();
});

// ─── Test 6: propagateToParent: true → ancestors with muted letter ───

test('propagateToParent: true → ancestors decorated with muted (M*) letter', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([
    { path: 'src/a.ts', status: 'M' },
  ]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: true,
  });

  await waitFor(() => client.getStatusCalls >= 1);
  await nextTick();
  await nextTick();

  const { providers } = fx._stats();
  const provider = providers[0];

  const leaf = provider.provide({ id: 3 });
  assert.ok(leaf);
  assert.equal(leaf.letter, 'M', 'leaf keeps full letter');

  const folder = provider.provide({ id: 2 });
  assert.ok(folder, 'src folder must be decorated via propagation');
  assert.equal(folder.badge, 'M*', 'ancestor carries distinct muted letter (M*)');

  const root = provider.provide({ id: 1 });
  assert.ok(root, 'root must be decorated via propagation');
  assert.equal(root.badge, 'M*');

  // Leaf color should differ from ancestor color (muted).
  assert.notEqual(leaf.color, folder.color, 'ancestor must use muted color');

  handle.dispose();
});

// ─── Test 7: dispose() unregisters + stops onChange ──────────────────

test('dispose() unregisters provider and unsubscribes from client', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([
    { path: 'src/a.ts', status: 'M' },
  ]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: false,
    batchOptions: { debounceMs: 10, maxWaitMs: 100 },
  });

  await waitFor(() => client.getStatusCalls >= 1);
  assert.equal(client.listenerCount, 1, 'client must have one listener');

  handle.dispose();

  assert.equal(client.listenerCount, 0, 'dispose must unsubscribe');
  const { disposedCount } = fx._stats();
  assert.equal(disposedCount, 1, 'engine registration must be disposed');

  // Firing onChange after dispose must not trigger further getStatus.
  const callsBefore = client.getStatusCalls;
  client.fire();
  await sleep(50);
  assert.equal(
    client.getStatusCalls,
    callsBefore,
    'no getStatus after dispose',
  );
});

// ─── Test 8: refresh() forces immediate fetch ────────────────────────

test('refresh() forces immediate re-fetch and awaits completion', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: false,
    // Huge debounce so the natural path can't race us.
    batchOptions: { debounceMs: 10_000, maxWaitMs: 20_000 },
  });

  await waitFor(() => client.getStatusCalls >= 1);
  const initial = client.getStatusCalls;

  client.setStatus([
    { path: 'src/b.ts', status: 'A' },
  ]);

  // Explicit refresh — must not rely on any debounce window.
  await handle.refresh();

  assert.ok(
    client.getStatusCalls >= initial + 1,
    `refresh must trigger a fetch; calls: ${initial} → ${client.getStatusCalls}`,
  );

  const { providers } = fx._stats();
  const provider = providers[0];
  const dec = provider.provide({ id: 4 });
  assert.ok(dec, 'src/b.ts decoration must be present after refresh()');
  assert.equal(dec.letter, 'A');

  handle.dispose();
});

// ─── Test 9: post-dispose onChange does not notify listeners ─────────

test('post-dispose onChange fire does not call onDidChange listeners', async () => {
  const { fx } = createFakeEngineForGit(sampleRows());
  const client = createFakeGitClient([
    { path: 'src/a.ts', status: 'M' },
  ]);
  const handle = registerGitDecorations({
    fx,
    client,
    rootPath: '/ROOT',
    propagateToParent: false,
    batchOptions: { debounceMs: 10, maxWaitMs: 100 },
  });

  await waitFor(() => client.getStatusCalls >= 1);

  let didChange = 0;
  const { providers } = fx._stats();
  providers[0].onDidChange(() => {
    didChange += 1;
  });

  handle.dispose();
  client.fire(); // post-dispose
  await sleep(30);
  assert.equal(didChange, 0, 'no listeners after dispose');
});

// ─── Test 10: createBatcher timing sanity ────────────────────────────

test('createBatcher trailing-debounces a burst into one firing', async () => {
  let fires = 0;
  const received = [];
  const batcher = createBatcher(
    (ids) => {
      fires += 1;
      received.push(new Set(ids));
    },
    { debounceMs: 30, maxWaitMs: 300 },
  );

  batcher.enqueue(1);
  batcher.enqueue(2);
  batcher.enqueue(3);
  batcher.enqueue(2); // duplicate — set dedupes

  // Not yet fired — still inside debounce window.
  assert.equal(fires, 0);
  await sleep(50);

  assert.equal(fires, 1, 'burst collapses to one firing');
  assert.deepEqual([...received[0]].sort(), [1, 2, 3]);

  // New burst after quiescence fires again.
  batcher.enqueue(7);
  await sleep(50);
  assert.equal(fires, 2);

  // cancel() drops pending without firing.
  batcher.enqueue(9);
  batcher.cancel();
  await sleep(50);
  assert.equal(fires, 2, 'cancel must not fire the callback');

  batcher.dispose();
});
