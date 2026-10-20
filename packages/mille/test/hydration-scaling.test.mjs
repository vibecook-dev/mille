// Store hydration grows linearly through every native path (petition M8).
//
// mille 0.3.4 cloned the whole snapshot on every store publication and
// scanned the whole path index on every folder reconciliation, so filling a
// store cost O(entries) per call: 5,500 entries hydrated in 0.45 s, 22,000 in
// 15 s. The Rust suite pins the store itself (`tests/hydration_scaling.rs`);
// this pins the three ways an embedder fills it through the binding, on a
// real tree: `prefetch` folder by folder, one native depth walk, and
// `populateFromRoots`.
//
// The bound is loose on purpose: a 16× larger tree may take at most 32× the
// time. Linear is 16×, n·log n ~20×; the quadratic store measured 52–58× for
// the walks at 8× already. The prefetch path needs the 16× span: per-folder
// constants (a 1 ms sleep per listing, before M8) hide the quadratic term at
// small sizes — at 1k → 8k the old store's deep prefetch grew 19.96×, inside
// any bound linear code could pass. This file shares the runner with the
// rest of the suite, so sizes are interleaved and timed best-of-three — a
// stall on either side cannot decide the verdict. `pnpm bench:hydration`
// measures the real sizes.

import { test, before, after } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { createFileExplorerHost } from '../dist/index.js';

const SMALL = 1_000;
const LARGE = 16_000;
const MAX_GROWTH = 32;
const DEEP_LEVELS = 24;
const DIRECTORY_KIND = 1;

/** Exactly `total` entries below `root`: `wide` = folders of 500 files,
 * `deep` = 5-file folders nested 24 levels (one folder per six entries). */
function buildTree(root, shape, total) {
  let remaining = total;
  const files = (directory, count) => {
    for (let index = 0; index < count; index++) {
      writeFileSync(join(directory, `file-${index}.txt`), 'm8');
    }
  };
  for (let top = 0; remaining > 0; top++) {
    let directory = join(root, `${shape}-${top}`);
    const levels = shape === 'wide' ? 1 : DEEP_LEVELS;
    for (let level = 0; level < levels && remaining > 0; level++) {
      mkdirSync(directory);
      remaining--;
      const count = Math.min(shape === 'wide' ? 500 : 5, remaining);
      files(directory, count);
      remaining -= count;
      directory = join(directory, `level-${level + 1}`);
    }
  }
}

const HYDRATE = {
  async prefetch(fx, rootId) {
    const queue = [rootId];
    for (let head = 0; head < queue.length; head++) {
      await fx.prefetch(queue[head]);
      const snapshot = fx.getSnapshot();
      for (const childId of snapshot.childrenOf(queue[head])) {
        if (snapshot.getById(childId)?.kind === DIRECTORY_KIND) queue.push(childId);
      }
    }
  },
  async walk(fx, rootId) {
    await fx.prefetch(rootId, { depth: DEEP_LEVELS + 8 });
  },
  async roots(fx) {
    await fx.populateFromRoots();
  },
};

function countBelow(fx, rootId) {
  const snapshot = fx.getSnapshot();
  let count = 0;
  const stack = [rootId];
  while (stack.length > 0) {
    for (const childId of snapshot.childrenOf(stack.pop())) {
      count++;
      stack.push(childId);
    }
  }
  return count;
}

async function timeHydration(root, path, expected) {
  const host = await createFileExplorerHost({
    roots: [root],
    initialWalk: 'roots-only',
    followSymlinks: false,
    respectIgnore: true,
  });
  try {
    const fx = host.local;
    const rootId = fx.getSnapshot().roots()[0].id;
    await fx.startWatching();
    const started = performance.now();
    await HYDRATE[path](fx, rootId);
    const elapsed = performance.now() - started;
    assert.equal(countBelow(fx, rootId), expected, `${path} hydrates every entry`);
    return elapsed;
  } finally {
    await host.dispose();
  }
}

const trees = {};

before(() => {
  for (const shape of ['wide', 'deep']) {
    trees[shape] = {};
    for (const size of [SMALL, LARGE]) {
      const root = mkdtempSync(join(tmpdir(), `mille-hydration-${shape}-`));
      buildTree(root, shape, size);
      trees[shape][size] = root;
    }
  }
});

after(() => {
  for (const bySize of Object.values(trees)) {
    for (const root of Object.values(bySize)) removeTempDir(root, 'hydration scaling');
  }
});

for (const path of Object.keys(HYDRATE)) {
  for (const shape of ['wide', 'deep']) {
    test(`${path} hydration of a ${shape} tree grows linearly`, async (t) => {
      // Warm the binding and the code path before either timing.
      await timeHydration(trees[shape][SMALL], path, SMALL);
      let small = Infinity;
      let large = Infinity;
      for (let run = 0; run < 3; run++) {
        small = Math.min(small, await timeHydration(trees[shape][SMALL], path, SMALL));
        large = Math.min(large, await timeHydration(trees[shape][LARGE], path, LARGE));
      }
      const growth = large / small;
      t.diagnostic(
        `${SMALL} entries ${small.toFixed(1)} ms, ${LARGE} ${large.toFixed(1)} ms (${growth.toFixed(2)}×)`,
      );
      assert.ok(
        growth <= MAX_GROWTH,
        `${path}/${shape}: ${SMALL} entries took ${small.toFixed(1)} ms, ${LARGE} took ` +
          `${large.toFixed(1)} ms — ${growth.toFixed(1)}× for ${LARGE / SMALL}× the entries ` +
          `(bound ${MAX_GROWTH}×)`,
      );
    });
  }
}
