import { strict as assert } from 'node:assert';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { MessageChannel } from 'node:worker_threads';

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { connectFileExplorer, createFileExplorerHost } from '../dist/index.js';

const count = Number(process.env.MILLE_PROGRESSIVE_DIRECTORY_COUNT ?? 10_000);
const batchSize = Number(process.env.MILLE_PROGRESSIVE_DIRECTORY_BATCH ?? 256);
const firstPageBudgetMs = Number(
  process.env.MILLE_PROGRESSIVE_DIRECTORY_FIRST_PAGE_BUDGET_MS ?? 1_000,
);
const completeBudgetMs = Number(
  process.env.MILLE_PROGRESSIVE_DIRECTORY_COMPLETE_BUDGET_MS ?? 10_000,
);
const effectiveBatchSize = Math.max(16, Math.min(4096, Math.trunc(batchSize)));

function adaptivePublicationUpperBound(entryCount, initialSize) {
  let remaining = entryCount + 1; // the walked directory entry rides page one
  const ceiling = Math.min(4096, initialSize * 16);
  let target = initialSize;
  let publications = 0;
  while (remaining > 0) {
    remaining -= target;
    publications++;
    target = Math.min(ceiling, target * 2);
  }
  // Initial expansion, EOF marker, compact/final refresh, and watcher timing
  // can add a handful of protocol frames beyond native publications.
  return publications + 8;
}

const maxDeltaFrames = Number(
  process.env.MILLE_PROGRESSIVE_DIRECTORY_MAX_DELTA_FRAMES ??
    adaptivePublicationUpperBound(count, effectiveBatchSize),
);

const root = mkdtempSync(join(tmpdir(), 'mille-progressive-bench-'));
let host;
let client;
try {
  for (let index = 0; index < count; index++) {
    writeFileSync(join(root, `entry-${String(index).padStart(7, '0')}.txt`), '');
  }

  host = await createFileExplorerHost({
    roots: [root],
    initialWalk: 'roots-only',
    compactFolders: false,
    directoryBatchSize: batchSize,
  });
  const { port1, port2 } = new MessageChannel();
  host.attachPort(port1);
  client = await connectFileExplorer(port2, { prefetchRows: 256 });
  const rootId = client.getSnapshot().roots()[0].id;
  let firstPageMs = null;
  let completeMs = null;
  let deltaFrames = 0;
  let maxPartialRows = 0;
  port2.on('message', (frame) => {
    if (frame?.type === 'delta') deltaFrames++;
  });

  const started = performance.now();
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(new Error('progressive directory benchmark timed out')),
      completeBudgetMs * 2,
    );
    const sub = client.on('change', () => {
      const snapshot = client.getSnapshot();
      const rows = snapshot.visibleRows({
        expanded: new Set([rootId]),
        offset: 0,
        limit: 256,
      });
      if (snapshot.directChildCount(rootId) === null && rows.length > 1) {
        maxPartialRows = Math.max(maxPartialRows, rows.length - 1);
        firstPageMs ??= performance.now() - started;
      }
      if (snapshot.directChildCount(rootId) === count) {
        completeMs = performance.now() - started;
        clearTimeout(timeout);
        sub.dispose();
        resolve();
      }
    });
    client.setExpanded({ add: [rootId] });
  });

  const result = {
    count,
    batchSize,
    firstPageMs: Number(firstPageMs.toFixed(2)),
    completeMs: Number(completeMs.toFixed(2)),
    deltaFrames,
    maxPartialRows,
    firstPageBudgetMs,
    completeBudgetMs,
    maxDeltaFrames,
  };
  console.log(JSON.stringify(result, null, 2));
  assert.ok(firstPageMs !== null, 'a partial page must be observable before completion');
  assert.ok(firstPageMs < completeMs, 'first usable page must precede completion');
  assert.ok(firstPageMs <= firstPageBudgetMs, `first page ${firstPageMs}ms exceeded budget`);
  assert.ok(completeMs <= completeBudgetMs, `completion ${completeMs}ms exceeded budget`);
  assert.ok(
    deltaFrames <= maxDeltaFrames,
    `${deltaFrames} delta frames exceeded adaptive publication budget ${maxDeltaFrames}`,
  );
} finally {
  await client?.dispose();
  await host?.dispose();
  removeTempDir(root);
}
