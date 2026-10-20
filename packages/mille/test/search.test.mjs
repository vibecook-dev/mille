// Phase 10 commit 10.2 — FileExplorer.search smoke tests.
//
// Exercises the typed wrapper end-to-end against a walker-populated
// tree. The fuzzy engine (nucleo) has its own unit tests in fx-core;
// these tests verify the marshaling — that query/options arrive on
// the native side and hits come back with entry, score, and
// matchedIndices populated in camelCase.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { FileExplorer } from '../dist/index.js';

function tempRoot() {
  return mkdtempSync(join(tmpdir(), 'mille-search-'));
}

async function seed(dir) {
  writeFileSync(join(dir, 'main.rs'), 'fn main() {}');
  writeFileSync(join(dir, 'lib.rs'), '');
  writeFileSync(join(dir, 'README.md'), '');
  mkdirSync(join(dir, 'sub'));
  writeFileSync(join(dir, 'sub/module.rs'), '');
  writeFileSync(join(dir, 'sub/other.txt'), '');
  const fx = new FileExplorer({ roots: [dir] });
  await fx.populateFromRoots();
  return fx;
}

test('search: empty query returns empty array', async () => {
  const dir = tempRoot();
  try {
    const fx = await seed(dir);
    const hits = fx.search('');
    assert.ok(Array.isArray(hits));
    assert.equal(hits.length, 0);
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('search: finds files matching a substring query', async () => {
  const dir = tempRoot();
  try {
    const fx = await seed(dir);
    const hits = fx.search('main');
    assert.ok(hits.length >= 1, `expected >=1 hit for 'main', got ${hits.length}`);
    const names = hits.map((h) => h.entry.name);
    assert.ok(names.includes('main.rs'), `expected main.rs in hits: ${JSON.stringify(names)}`);
    // Hit shape sanity.
    const top = hits[0];
    assert.equal(typeof top.score, 'number');
    assert.ok(top.score > 0, `expected score > 0, got ${top.score}`);
    assert.ok(Array.isArray(top.matchedIndices));
    assert.ok(top.matchedIndices.length > 0, 'expected at least one matched index');
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('search: matchedIndices point at the right characters', async () => {
  const dir = tempRoot();
  try {
    const fx = await seed(dir);
    const hits = fx.search('main');
    const mainHit = hits.find((h) => h.entry.name === 'main.rs');
    assert.ok(mainHit, 'main.rs should match');
    // 'main' is chars 0..=3 of 'main.rs'.
    assert.deepEqual([...mainHit.matchedIndices], [0, 1, 2, 3]);
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('search: limit option truncates results', async () => {
  const dir = tempRoot();
  try {
    // Seed 10 files that all fuzzy-match 'file'.
    const files = Array.from({ length: 10 }, (_, i) => `file_${i}.rs`);
    for (const f of files) writeFileSync(join(dir, f), '');
    const fx = new FileExplorer({ roots: [dir] });
    await fx.populateFromRoots();

    const unbounded = fx.search('file');
    assert.ok(unbounded.length >= 10, `expected >=10 hits, got ${unbounded.length}`);

    const limited = fx.search('file', { limit: 1 });
    assert.equal(limited.length, 1);

    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('search: fuzzy (skip-char) queries still match', async () => {
  const dir = tempRoot();
  try {
    const fx = await seed(dir);
    // 'mdl' should fuzzy-match 'module.rs' (m..d(?)..l).
    const hits = fx.search('mdl');
    const names = hits.map((h) => h.entry.name);
    assert.ok(
      names.includes('module.rs'),
      `expected module.rs in fuzzy-hit names: ${JSON.stringify(names)}`,
    );
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('search: non-matching query returns empty', async () => {
  const dir = tempRoot();
  try {
    const fx = await seed(dir);
    const hits = fx.search('zzzqqq');
    assert.equal(hits.length, 0);
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('search: caseSensitive option respects case', async () => {
  // Note: macOS default filesystem (APFS) is case-insensitive, so we
  // can't seed both 'Main.rs' and 'main.rs' as distinct files. Use
  // distinct basenames that still differ in case on the significant
  // letters — 'Zoom.rs' vs 'zebra.rs' — to exercise both paths.
  const dir = tempRoot();
  try {
    writeFileSync(join(dir, 'Zoom.rs'), '');
    writeFileSync(join(dir, 'zebra.rs'), '');
    const fx = new FileExplorer({ roots: [dir] });
    await fx.populateFromRoots();

    // Under Respect mode, lowercase 'z' should match lowercase 'zebra'
    // but NOT uppercase 'Zoom'.
    const sensitive = fx.search('zebra', { caseSensitive: true });
    const sensitiveNames = sensitive.map((h) => h.entry.name);
    assert.ok(sensitiveNames.includes('zebra.rs'), `expected zebra.rs in case-sensitive hits`);

    const sensitiveMiss = fx.search('Zebra', { caseSensitive: true });
    const sensitiveMissNames = sensitiveMiss.map((h) => h.entry.name);
    assert.ok(
      !sensitiveMissNames.includes('zebra.rs'),
      `zebra.rs should not match 'Zebra' under caseSensitive, got ${JSON.stringify(sensitiveMissNames)}`,
    );

    // Smart-case (default): lowercase needle matches either case.
    const smart = fx.search('zoom');
    const smartNames = smart.map((h) => h.entry.name);
    assert.ok(smartNames.includes('Zoom.rs'), `expected Zoom.rs under smart-case`);

    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});

test('search: kinds filters by entry kind across the native boundary', async () => {
  const dir = tempRoot();
  try {
    const fx = await seed(dir);
    const dirs = fx.search('sub', { kinds: [1] });
    assert.ok(dirs.length >= 1);
    assert.ok(
      dirs.every((h) => h.entry.kind === 1),
      `only directories: ${JSON.stringify(dirs.map((h) => h.entry.name))}`,
    );
    assert.ok(dirs.some((h) => h.entry.name === 'sub'));

    const files = fx.search('rs', { kinds: [0] });
    assert.ok(files.length >= 3);
    assert.ok(files.every((h) => h.entry.kind === 0));

    // An empty list admits nothing; an unknown kind matches nothing.
    assert.equal(fx.search('rs', { kinds: [] }).length, 0);
    assert.equal(fx.search('rs', { kinds: [99] }).length, 0);
    await fx.dispose();
  } finally {
    removeTempDir(dir);
  }
});
