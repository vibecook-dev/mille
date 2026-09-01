import { test } from 'node:test';
import { strict as assert } from 'node:assert';

import { readTreeProjection } from '../dist/hooks/treeProjection.js';

function makeSnapshot(treeVersion, counters, projectionVersion = treeVersion) {
  const rows = [{ id: 1, name: 'root' }];
  return {
    treeVersion,
    projectionVersion,
    decorationVersion: 0,
    roots: () => rows,
    visibleRowCount: () => {
      counters.count += 1;
      return { known: rows.length, pendingExpansions: new Set() };
    },
    visibleRows: ({ offset, limit }) => {
      counters.rows += 1;
      counters.maxLimit = Math.max(counters.maxLimit, limit);
      return rows.slice(offset, offset + limit);
    },
    getById: () => null,
    directChildCount: () => null,
    hasChildren: () => false,
    getDecorations: () => [],
  };
}

test('decoration-only snapshots reuse the windowed structural projection', () => {
  const counters = { count: 0, rows: 0, maxLimit: 0 };
  const expanded = new Set([1]);
  const first = readTreeProjection(makeSnapshot(7, counters), expanded, null);
  const decorationOnly = readTreeProjection(
    makeSnapshot(7, counters),
    expanded,
    first,
  );

  assert.equal(decorationOnly, first);
  assert.deepEqual(counters, { count: 1, rows: 0, maxLimit: 0 });
});

test('equal versions from a different engine source never reuse a projection', () => {
  const counters = { count: 0, rows: 0, maxLimit: 0 };
  const expanded = new Set([1]);
  const sourceA = {};
  const sourceB = {};
  const first = readTreeProjection(makeSnapshot(7, counters), expanded, null, sourceA);
  const second = readTreeProjection(makeSnapshot(7, counters), expanded, first, sourceB);

  assert.notEqual(second, first);
  assert.equal(second.source, sourceB);
  assert.deepEqual(counters, { count: 2, rows: 0, maxLimit: 0 });
});

test('tree-version and expansion changes refresh counts without reading rows', () => {
  const counters = { count: 0, rows: 0, maxLimit: 0 };
  const expanded = new Set([1]);
  const first = readTreeProjection(makeSnapshot(7, counters), expanded, null);
  const nextTree = readTreeProjection(makeSnapshot(8, counters), expanded, first);
  const nextExpansion = readTreeProjection(
    makeSnapshot(8, counters),
    new Set([1, 2]),
    nextTree,
  );

  assert.notEqual(nextTree, first);
  assert.notEqual(nextExpansion, nextTree);
  assert.deepEqual(counters, { count: 3, rows: 0, maxLimit: 0 });
});

test('viewport hydration rematerializes without advancing treeVersion', () => {
  const counters = { count: 0, rows: 0, maxLimit: 0 };
  const expanded = new Set();
  const first = readTreeProjection(makeSnapshot(7, counters, 10), expanded, null);
  const hydrated = readTreeProjection(makeSnapshot(7, counters, 11), expanded, first);

  assert.notEqual(hydrated, first);
  assert.deepEqual(counters, { count: 2, rows: 0, maxLimit: 0 });
});

test('row windows are bounded and the complete projection stays lazy', () => {
  const counters = { count: 0, rows: 0, maxLimit: 0 };
  const projection = readTreeProjection(makeSnapshot(7, counters), new Set(), null);

  assert.deepEqual(projection.readRows(0, 1).map((row) => row.id), [1]);
  assert.equal(counters.rows, 1);
  assert.equal(counters.maxLimit, 1);
  assert.deepEqual(projection.readAllRows().map((row) => row.id), [1]);
  assert.equal(counters.rows, 2, 'full projection materializes only on explicit demand');
});

test('identity windows use the snapshot id-only contract without row payloads', () => {
  const counters = { count: 0, rows: 0, maxLimit: 0, ids: 0 };
  const snapshot = {
    ...makeSnapshot(7, counters),
    visibleRowCount: () => ({ known: 500_000, pendingExpansions: new Set() }),
    visibleRowIds: ({ offset, limit }) => {
      counters.ids += limit;
      return Array.from({ length: limit }, (_, index) => offset + index + 1);
    },
  };
  const projection = readTreeProjection(snapshot, new Set(), null);

  assert.deepEqual(projection.readIds(400_000, 3), [400_001, 400_002, 400_003]);
  assert.equal(counters.ids, 3);
  assert.equal(counters.rows, 0);

  const fallbackCounters = { count: 0, rows: 0, maxLimit: 0 };
  const fallback = readTreeProjection(makeSnapshot(8, fallbackCounters), new Set(), null);
  assert.deepEqual(fallback.readIds(0, 1), [1]);
  assert.equal(fallbackCounters.rows, 1);
});

test('visible index lookup probes bounded chunks around a prior index', () => {
  const rows = Array.from({ length: 20_000 }, (_, index) => ({
    id: index + 1,
    name: `row-${index + 1}`,
  }));
  const requests = [];
  const snapshot = {
    ...makeSnapshot(8, { count: 0, rows: 0, maxLimit: 0 }),
    roots: () => [rows[0]],
    visibleRowCount: () => ({ known: rows.length, pendingExpansions: new Set() }),
    visibleRows: ({ offset, limit }) => {
      requests.push({ offset, limit });
      return rows.slice(offset, offset + limit);
    },
  };
  const projection = readTreeProjection(snapshot, new Set(), null);

  assert.equal(projection.findRowIndex(11_001, 10_000), 11_000);
  assert.ok(requests.length > 1, 'lookup should expand beyond its first probe');
  assert.ok(Math.max(...requests.map((request) => request.limit)) <= 256);
  assert.ok(requests.reduce((total, request) => total + request.limit, 0) <= 4_096);

  requests.length = 0;
  assert.equal(projection.findExactRowIndex(15_001, 10_000), 15_000);
  assert.ok(requests.length > 1, 'legacy exact lookup should retain the windowed fallback');
  assert.ok(Math.max(...requests.map((request) => request.limit)) <= 256);
  assert.ok(requests.reduce((total, request) => total + request.limit, 0) <= rows.length);
});

test('exact visible index lookup uses the snapshot position contract without rows', () => {
  const counters = { count: 0, rows: 0, maxLimit: 0, indexes: 0 };
  const snapshot = {
    ...makeSnapshot(9, counters),
    visibleRowCount: () => ({ known: 500_000, pendingExpansions: new Set() }),
    visibleRowIndex: (id) => {
      counters.indexes += 1;
      return id === 400_001 ? 400_000 : null;
    },
  };
  const projection = readTreeProjection(snapshot, new Set(), null);

  assert.equal(projection.findExactRowIndex(400_001, 0), 400_000);
  assert.equal(counters.indexes, 1);
  assert.equal(counters.rows, 0);
});
