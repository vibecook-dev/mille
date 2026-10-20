// Store hydration scaling (petition M8).
//
// mille 0.3.4 hydrated super-linearly: an embedder measured 5,500 entries in
// 0.45 s, 11,000 in 2.8 s, 22,000 in 15 s and a 43,310-entry home folder in
// 44 s at 494 MB — through all three ways a store fills, alike. Each one is
// timed here against the same synthetic trees:
//
//   prefetch  `prefetch(id)` for every folder, breadth first — the lazy path a
//             tree view or a folder picker takes, one depth-1 listing per call
//   walk      `prefetch(root, { depth })` — one bounded native depth walk
//   roots     `populateFromRoots()` — the eager whole-root walk
//
// Two shapes, because they stress different terms: `wide` is a few large
// folders (500 files each, so sibling ordering and per-batch publication
// dominate), `deep` is many small folders nested 24 levels down (so per-call
// and per-ancestor costs dominate — prefetch makes one call per folder).
// Every top-level folder carries its own `.gitignore`, the way a home folder
// full of projects does, so the number of ignore files grows with the tree.
//
// Every (shape, size, path) runs in a fresh child process so its peak RSS is
// its own. The host is configured the way VibeField's files worker configures
// it: `initialWalk: 'roots-only'`, `followSymlinks: false`,
// `respectIgnore: true`, and the fixture carries a root `.gitignore` too.
//
// Gates (exit non-zero): every run must hydrate exactly the fixture's entry
// count; each (shape, path) series' fitted scaling exponent — the slope of
// log(time) over log(entries) — must stay under
// MILLE_HYDRATION_MAX_EXPONENT (default 1.35: linear is 1, n·log n over this
// range ~1.1, the 0.3.4 store measured ~2); and peak RSS growth per hydrated
// entry must stay under MILLE_HYDRATION_MAX_BYTES_PER_ENTRY at the largest
// size. Absolute times are reporters: hardware decides them, the exponent does
// not depend on it.
//
//   pnpm bench:hydration
//   pnpm bench:hydration -- --report hydration-report.json
//   MILLE_HYDRATION_SIZES=5500,11000 MILLE_HYDRATION_PATHS=walk pnpm bench:hydration

import { strict as assert } from 'node:assert';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';

import { removeTempDir } from '../../../scripts/test-temp.mjs';

const PATHS = ['prefetch', 'walk', 'roots'];
const SHAPES = ['wide', 'deep'];
const WIDE_FANOUT = 500;
const DEEP_LEVELS = 24;
const DEEP_FILES = 5;
const FILE_BYTES = 'mille-hydration\n';
const DIRECTORY_KIND = 1;

function list(name, fallback, allowed) {
  const raw = process.env[name];
  const values = raw ? raw.split(',').map((value) => value.trim()) : fallback;
  if (allowed) {
    for (const value of values) assert.ok(allowed.includes(value), `${name}: unknown ${value}`);
  }
  return values;
}

/**
 * Write a tree holding exactly `total` entries below `root` (the root itself
 * is not counted). Returns `total` so the caller can assert against it.
 */
function buildFixture(root, shape, total) {
  let remaining = total;
  writeFileSync(join(root, '.gitignore'), '*.log\nbuild/\n');
  remaining--;
  // A top-level folder is a project with its own ignore rules (matching
  // nothing the fixture writes). Returns how many entries it wrote.
  const project = (directory, budget) => {
    if (budget === 0) return 0;
    writeFileSync(join(directory, '.gitignore'), '*.tmp\n');
    return 1;
  };
  const files = (directory, count) => {
    for (let index = 0; index < count; index++) {
      writeFileSync(join(directory, `file-${String(index).padStart(3, '0')}.txt`), FILE_BYTES);
    }
  };
  if (shape === 'wide') {
    for (let folder = 0; remaining > 0; folder++) {
      const directory = join(root, `folder-${String(folder).padStart(4, '0')}`);
      mkdirSync(directory);
      remaining--;
      remaining -= project(directory, remaining);
      const count = Math.min(WIDE_FANOUT, remaining);
      files(directory, count);
      remaining -= count;
    }
  } else {
    for (let chain = 0; remaining > 0; chain++) {
      let directory = join(root, `chain-${String(chain).padStart(4, '0')}`);
      for (let level = 0; level < DEEP_LEVELS && remaining > 0; level++) {
        mkdirSync(directory);
        remaining--;
        if (level === 0) remaining -= project(directory, remaining);
        const count = Math.min(DEEP_FILES, remaining);
        files(directory, count);
        remaining -= count;
        directory = join(directory, `level-${String(level + 1).padStart(2, '0')}`);
      }
    }
  }
  return total;
}

/** Count every entry reachable from `rootId` (the root excluded). */
function countHydrated(fx, rootId) {
  const snapshot = fx.getSnapshot();
  let count = 0;
  const stack = [rootId];
  while (stack.length > 0) {
    const id = stack.pop();
    for (const child of snapshot.childrenOf(id)) {
      count++;
      stack.push(child);
    }
  }
  return count;
}

async function child(path, root, expected) {
  const { createFileExplorerHost } = await import('../dist/index.js');
  const host = await createFileExplorerHost({
    roots: [root],
    initialWalk: 'roots-only',
    followSymlinks: false,
    respectIgnore: true,
  });
  const fx = host.local;
  try {
    const rootId = fx.getSnapshot().roots()[0].id;
    // Settle module loading and the root row before the baseline is taken.
    await fx.startWatching();
    const baselineKb = process.resourceUsage().maxRSS;
    const started = performance.now();
    if (path === 'prefetch') {
      const queue = [rootId];
      for (let head = 0; head < queue.length; head++) {
        const id = queue[head];
        await fx.prefetch(id);
        const snapshot = fx.getSnapshot();
        for (const childId of snapshot.childrenOf(id)) {
          if (snapshot.getById(childId)?.kind === DIRECTORY_KIND) queue.push(childId);
        }
      }
    } else if (path === 'walk') {
      await fx.prefetch(rootId, { depth: DEEP_LEVELS + 8 });
    } else {
      await fx.populateFromRoots();
    }
    const ms = performance.now() - started;
    const peakKb = process.resourceUsage().maxRSS;
    const hydrated = countHydrated(fx, rootId);
    process.stdout.write(`${JSON.stringify({ ms, hydrated, expected, baselineKb, peakKb })}\n`);
  } finally {
    await host.dispose();
  }
}

function fitExponent(points) {
  const xs = points.map((point) => Math.log(point.entries));
  const ys = points.map((point) => Math.log(point.ms));
  const mean = (values) => values.reduce((sum, value) => sum + value, 0) / values.length;
  const mx = mean(xs);
  const my = mean(ys);
  let numerator = 0;
  let denominator = 0;
  for (let index = 0; index < xs.length; index++) {
    numerator += (xs[index] - mx) * (ys[index] - my);
    denominator += (xs[index] - mx) ** 2;
  }
  return numerator / denominator;
}

async function main() {
  const sizes = list('MILLE_HYDRATION_SIZES', ['5500', '11000', '22000', '44000']).map(Number);
  const shapes = list('MILLE_HYDRATION_SHAPES', SHAPES, SHAPES);
  const paths = list('MILLE_HYDRATION_PATHS', PATHS, PATHS);
  const runs = Math.max(1, Number(process.env.MILLE_HYDRATION_RUNS ?? 1));
  const maxExponent = Number(process.env.MILLE_HYDRATION_MAX_EXPONENT ?? 1.35);
  const maxBytesPerEntry = Number(process.env.MILLE_HYDRATION_MAX_BYTES_PER_ENTRY ?? 4096);
  const reportIndex = process.argv.indexOf('--report');
  const reportPath = reportIndex === -1 ? null : process.argv[reportIndex + 1];
  const self = fileURLToPath(import.meta.url);

  const rows = [];
  for (const shape of shapes) {
    for (const size of sizes) {
      const root = mkdtempSync(join(tmpdir(), `mille-hydration-${shape}-`));
      try {
        const expected = buildFixture(root, shape, size);
        for (const path of paths) {
          const samples = [];
          for (let run = 0; run < runs; run++) {
            const output = execFileSync(
              process.execPath,
              [self, '--child', path, root, String(expected)],
              { encoding: 'utf8', maxBuffer: 1 << 20 },
            );
            samples.push(JSON.parse(output.trim().split('\n').at(-1)));
          }
          samples.sort((a, b) => a.ms - b.ms);
          const median = samples[Math.floor(samples.length / 2)];
          const row = {
            shape,
            path,
            entries: expected,
            hydrated: median.hydrated,
            ms: Number(median.ms.toFixed(1)),
            peakMb: Number((median.peakKb / 1024).toFixed(1)),
            growthMb: Number(((median.peakKb - median.baselineKb) / 1024).toFixed(1)),
            bytesPerEntry: Math.round(((median.peakKb - median.baselineKb) * 1024) / expected),
            load: Number(
              execFileSync('sysctl', ['-n', 'vm.loadavg'], { encoding: 'utf8' })
                .trim()
                .split(/\s+/)[1] ?? 'NaN',
            ),
          };
          rows.push(row);
          console.log(
            `${shape.padEnd(5)} ${path.padEnd(8)} ${String(expected).padStart(6)} entries` +
              `  ${String(row.ms).padStart(9)} ms  peak ${String(row.peakMb).padStart(6)} MB` +
              `  +${String(row.growthMb).padStart(6)} MB  ${String(row.bytesPerEntry).padStart(6)} B/entry` +
              `  (load ${row.load})`,
          );
        }
      } finally {
        removeTempDir(root, `hydration ${shape} ${size}`);
      }
    }
  }

  const failures = [];
  const series = [];
  for (const shape of shapes) {
    for (const path of paths) {
      const points = rows.filter((row) => row.shape === shape && row.path === path);
      for (const point of points) {
        if (point.hydrated !== point.entries) {
          failures.push(`${shape}/${path} at ${point.entries}: hydrated ${point.hydrated} entries`);
        }
      }
      const exponent = points.length >= 2 ? fitExponent(points) : null;
      const largest = points.at(-1);
      series.push({ shape, path, exponent, bytesPerEntry: largest?.bytesPerEntry ?? null });
      if (exponent !== null && exponent > maxExponent) {
        failures.push(`${shape}/${path}: scaling exponent ${exponent.toFixed(2)} > ${maxExponent}`);
      }
      if (largest && largest.bytesPerEntry > maxBytesPerEntry) {
        failures.push(
          `${shape}/${path}: ${largest.bytesPerEntry} B/entry at ${largest.entries} > ${maxBytesPerEntry}`,
        );
      }
    }
  }
  for (const { shape, path, exponent } of series) {
    if (exponent !== null) {
      console.log(`${shape.padEnd(5)} ${path.padEnd(8)} exponent ${exponent.toFixed(2)}`);
    }
  }
  if (reportPath) {
    writeFileSync(
      reportPath,
      `${JSON.stringify({ maxExponent, maxBytesPerEntry, runs, rows, series }, null, 2)}\n`,
    );
  }
  if (failures.length > 0) {
    for (const failure of failures) console.error(`FAIL ${failure}`);
    process.exit(1);
  }
}

if (process.argv[2] === '--child') {
  await child(process.argv[3], process.argv[4], Number(process.argv[5]));
} else {
  await main();
}
