import type { EntryId, VisibleRow, VisibleRowCount } from '@vibecook/mille';
import type { FileTreeSnapshotLike } from '../components/types.js';

/** Structural projection cached independently from decoration snapshots. */
export interface TreeProjection {
  /** Engine/snapshot-source identity. Version counters are source-local. */
  readonly source: object | null;
  readonly treeVersion: number;
  readonly projectionVersion: number;
  readonly expanded: ReadonlySet<EntryId>;
  readonly visibleCount: VisibleRowCount;
  /** Read a bounded slice for the mounted virtual window. */
  readRows(offset: number, limit: number): readonly VisibleRow[];
  /** Read ordered ids without materializing complete row payloads. */
  readIds(offset: number, limit: number): readonly EntryId[];
  /** Lazily materialize full order for discrete keyboard/imperative commands. */
  readAllRows(): readonly VisibleRow[];
  /** Find an id near a prior logical index using bounded row-window probes. */
  findRowIndex(id: EntryId, hintIndex: number, maxProbeRows?: number): number;
  /** Find an exact visible index through the snapshot fast path when available. */
  findExactRowIndex(id: EntryId, hintIndex: number): number;
}

/**
 * Build a lazy structural projection when tree structure or expansion identity
 * changes. Decoration-only snapshots reuse the previous projection. Ordinary
 * rendering reads only a virtual window; complete ordering is paid only by
 * discrete commands that explicitly request it.
 */
export function readTreeProjection(
  snapshot: FileTreeSnapshotLike,
  expanded: ReadonlySet<EntryId>,
  previous: TreeProjection | null,
  source: object | null = null,
): TreeProjection {
  const projectionVersion = snapshot.projectionVersion ?? snapshot.treeVersion;
  if (
    previous !== null &&
    previous.source === source &&
    previous.treeVersion === snapshot.treeVersion &&
    previous.projectionVersion === projectionVersion &&
    previous.expanded === expanded
  ) {
    return previous;
  }

  const visibleCount = snapshot.visibleRowCount(expanded);
  let cachedWindow:
    | { readonly offset: number; readonly limit: number; readonly rows: readonly VisibleRow[] }
    | null = null;
  let cachedAllRows: readonly VisibleRow[] | null = visibleCount.known === 0 ? [] : null;
  const readRows = (offset: number, limit: number): readonly VisibleRow[] => {
    const safeOffset = Math.max(0, Math.min(visibleCount.known, Math.trunc(offset)));
    const safeLimit = Math.max(
      0,
      Math.min(visibleCount.known - safeOffset, Math.trunc(limit)),
    );
    if (safeLimit === 0) return [];
    if (cachedAllRows !== null) return cachedAllRows.slice(safeOffset, safeOffset + safeLimit);
    if (cachedWindow?.offset === safeOffset && cachedWindow.limit === safeLimit) {
      return cachedWindow.rows;
    }
    const rows = snapshot.visibleRows({ expanded, offset: safeOffset, limit: safeLimit });
    cachedWindow = { offset: safeOffset, limit: safeLimit, rows };
    return rows;
  };
  const readIds = (offset: number, limit: number): readonly EntryId[] => {
    const safeOffset = Math.max(0, Math.min(visibleCount.known, Math.trunc(offset)));
    const safeLimit = Math.max(
      0,
      Math.min(visibleCount.known - safeOffset, Math.trunc(limit)),
    );
    if (safeLimit === 0) return [];
    if (cachedAllRows !== null) {
      return cachedAllRows.slice(safeOffset, safeOffset + safeLimit).map((row) => row.id);
    }
    if (snapshot.visibleRowIds) {
      return snapshot.visibleRowIds({
        expanded,
        offset: safeOffset,
        limit: safeLimit,
      });
    }
    return readRows(safeOffset, safeLimit).map((row) => row.id);
  };
  const readAllRows = (): readonly VisibleRow[] => {
    if (cachedAllRows === null) {
      cachedAllRows = snapshot.visibleRows({
        expanded,
        offset: 0,
        limit: visibleCount.known,
      });
    }
    return cachedAllRows;
  };
  const findRowIndex = (
    id: EntryId,
    hintIndex: number,
    maxProbeRows = 16_384,
  ): number => {
    if (visibleCount.known === 0 || maxProbeRows <= 0) return -1;
    if (cachedAllRows !== null) return cachedAllRows.findIndex((row) => row.id === id);
    const chunkSize = 256;
    const hint = Math.max(0, Math.min(visibleCount.known - 1, Math.trunc(hintIndex)));
    const baseStart = Math.max(
      0,
      Math.min(visibleCount.known - chunkSize, hint - Math.floor(chunkSize / 2)),
    );
    const visited = new Set<number>();
    let probedRows = 0;
    const probe = (rawStart: number): number => {
      const start = Math.max(0, Math.min(visibleCount.known - 1, rawStart));
      if (visited.has(start) || probedRows >= maxProbeRows) return -1;
      visited.add(start);
      const limit = Math.min(chunkSize, visibleCount.known - start, maxProbeRows - probedRows);
      const rows = readRows(start, limit);
      probedRows += limit;
      const localIndex = rows.findIndex((row) => row.id === id);
      return localIndex === -1 ? -1 : start + localIndex;
    };

    let found = probe(baseStart);
    if (found !== -1) return found;
    for (let distance = chunkSize; probedRows < maxProbeRows; distance += chunkSize) {
      found = probe(baseStart + distance);
      if (found !== -1) return found;
      found = probe(baseStart - distance);
      if (found !== -1) return found;
      if (baseStart + distance >= visibleCount.known && baseStart - distance <= 0) break;
    }
    return -1;
  };
  const findExactRowIndex = (id: EntryId, hintIndex: number): number => {
    if (cachedAllRows !== null) return cachedAllRows.findIndex((row) => row.id === id);
    if (snapshot.visibleRowIndex) {
      return snapshot.visibleRowIndex(id, expanded) ?? -1;
    }
    return findRowIndex(id, hintIndex, visibleCount.known);
  };
  return {
    source,
    treeVersion: snapshot.treeVersion,
    projectionVersion,
    expanded,
    visibleCount,
    readRows,
    readIds,
    readAllRows,
    findRowIndex,
    findExactRowIndex,
  };
}
