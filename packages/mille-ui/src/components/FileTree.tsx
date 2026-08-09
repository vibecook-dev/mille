// FileTree — Phase-3/4 virtualized tree with full keyboard + selection.
//
// Composition:
//   - If the caller passes `fx`, we self-compose `<FileTreeProvider>`
//     so `<FileTree fx={...} />` works standalone.
//   - Else we read the engine + snapshot from `useFileTreeContext`.
//
// Phase 4 adds on top of Phase 3:
//   - Full WAI-ARIA tree keyboard model via `useFileTreeKeyboard`.
//   - Focus + selection separation via `useFileTreeSelection`.
//   - Roving tabindex (exactly one row tabbable).
//   - Typeahead.
//   - Controlled focus/selection opt-in props.
//   - Focus restoration: re-entering the tree via Tab focuses the last
//     row the user interacted with, or the first row if none.

import {
  forwardRef,
  useCallback,
  useDeferredValue,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type FocusEvent as ReactFocusEvent,
  type ForwardedRef,
  type MouseEvent as ReactMouseEvent,
  type ReactElement,
  type ReactNode,
} from 'react';
import type { Entry, EntryId, VisibleRow } from '@vibecook/mille';
import { FileTreeProvider } from '../provider.js';
import { useFileTreeContext } from '../hooks/useFileTreeContext.js';
import { useExpandedSet } from '../hooks/useExpandedSet.js';
import { useSetExpandedBridge } from '../hooks/useSetExpandedBridge.js';
import { useVirtualizerForSnapshot } from '../hooks/useVirtualizerForSnapshot.js';
import {
  captureWindowedViewportAnchor,
  resolveWindowedViewportAnchor,
} from '../hooks/viewportAnchor.js';
import { useFileTreeSelection } from '../hooks/useFileTreeSelection.js';
import {
  planLayoutAnimation,
  type LayoutAnimationPlan,
  type RenderedRowPosition,
} from '../hooks/layoutAnimation.js';
import { usePrefersReducedMotion } from '../hooks/usePrefersReducedMotion.js';
import {
  readTreeProjection,
  type TreeProjection,
} from '../hooks/treeProjection.js';
import {
  useFileTreeKeyboard,
  type KeyboardExpansionActions,
  type KeyboardRowLookup,
  type ViewportKeyboardActions,
} from '../hooks/useFileTreeKeyboard.js';
import {
  useRenameState,
  type RenameStateFx,
  type RenameStateSnapshot,
} from '../hooks/useRenameState.js';
import { useContextMenuState } from '../hooks/useContextMenuState.js';
import { useClipboardState } from '../hooks/useClipboardState.js';
import { useFilterState } from '../hooks/useFilterState.js';
import { useControlledState } from '../hooks/useControlledState.js';
import { useFileTreeDragDrop } from '../hooks/useFileTreeDragDrop.js';
import { FileTreeRow } from './FileTreeRow.js';
import { FileContextMenu } from './FileContextMenu.js';
import { FileTreeFilter, type FileTreeFilterHandle } from './FileTreeFilter.js';
import { SearchResultList } from './SearchResultList.js';
import type { SearchableEngine } from '../hooks/useSearchResults.js';
import { mergeDecorations } from '../hooks/useFileDecorations.js';
import type { Command, CommandContext, CommandRegistry } from '../commands/types.js';
import { defaultCommands } from '../commands/defaults.js';
import {
  captureFileTreeNavigationState,
  parseFileTreeNavigationState,
  type FileTreeNavigationState,
  type FileTreeSearchMode,
} from '../navigation-state.js';
import {
  classifyActiveEntry,
  normalizeActiveEntryTarget,
  shouldAutoRevealActiveEntry,
  type ActiveEntryAutoReveal,
} from '../active-entry-policy.js';
import { expandedDescendantIds } from '../tree-expansion.js';
import type {
  AriaRowProps,
  FileTreeEngine,
  FileTreeNavigationRestoreResult,
  FileTreeProps,
  FileTreeRef,
  FileTreeRowProps,
  FileTreeSnapshotLike,
} from './types.js';

// Local structural type for the "full registry" narrowing. We can't
// import the full CommandRegistry at the context level because the
// provider deliberately exposes only `dispatch`; hosts that pass a
// full registry satisfy this shape structurally.
interface CommandRegistryHandleLike {
  all(): readonly Command[];
}

/**
 * Public entry. If `fx` is passed, composes its own
 * `<FileTreeProvider>` so single-tree apps can drop in without setting
 * up a provider. Callers that wrap `<FileTree>` in their own
 * `<FileTreeProvider>` (e.g. to pass a command registry) omit `fx`.
 *
 * v0.2 B6 — wrapped in `React.forwardRef<FileTreeRef, ...>` so hosts can
 * drive the tree imperatively via `revealPath` / `reset` / etc. Callers
 * that don't need the handle just omit `ref` — existing code keeps
 * working unchanged.
 */
function FileTreeBase(
  props: FileTreeProps,
  ref: ForwardedRef<FileTreeRef>,
): ReactElement {
  const { fx, ...rest } = props;
  if (fx) {
    return (
      <FileTreeProvider fx={fx as unknown as Parameters<typeof FileTreeProvider>[0]['fx']}>
        <FileTreeInner {...rest} forwardedRef={ref} />
      </FileTreeProvider>
    );
  }
  return <FileTreeInner {...rest} forwardedRef={ref} />;
}

export const FileTree = forwardRef<FileTreeRef, FileTreeProps>(FileTreeBase);
FileTree.displayName = 'FileTree';

// ─── Inner tree — assumes provider is present ──────────────────────

type FileTreeInnerProps = Omit<FileTreeProps, 'fx'> & {
  readonly forwardedRef?: ForwardedRef<FileTreeRef>;
};

function FileTreeInner(props: FileTreeInnerProps): ReactElement {
  const {
    rowHeight = 22,
    overscan = 20,
    iconTheme,
    emptyState,
    loadingState,
    ariaLabel,
    className,
    style,
    rowRenderer,
    stickyRoots = true,
    rootLabel,
    initialNavigationState,
    onNavigationStateChange,
    navigationStateDebounceMs = 150,
    activeEntry,
    autoRevealActiveEntry = false,
    activeEntryPolicy,
    onActiveEntryResolution,
    openBehavior,
    multiSelect = true,
    focusedId: controlledFocusedId,
    onFocusedIdChange,
    selectedIds: controlledSelectedIds,
    onSelectionChange,
    renameTargetId: controlledRenameTargetId,
    onRenameTargetIdChange,
    onOpen,
    onCopyPath,
    onRevealInFileManager,
    onOpenContainingFolder,
    onOpenTerminal,
    onRefresh,
    onSearchScope,
    onClipboardChange,
    contextMenuSlot,
    contextMenuExtraItems,
    contextMenuEmptyPlaceholder,
    hostHooks,
    disableContextMenu = false,
    filter: controlledFilter,
    onFilterChange,
    searchMode: controlledSearchMode,
    onSearchModeChange,
    filterInputRef,
    showFilter = false,
    dragDrop,
    disableDragDrop = false,
    forwardedRef,
    __testSearchDebounceMs,
    __testObserveElementRect,
    __testObserveElementOffset,
    __testOnProjectionMaterialized,
  } = props;

  const ctx = useFileTreeContext();
  // Cast through unknown: FileTreeFx from context is a narrower shape
  // but structurally compatible with the subset we need here.
  const fx = ctx.fx as unknown as FileTreeEngine;
  const latestSnapshot = ctx.snapshot as unknown as FileTreeSnapshotLike;
  // External-store notifications are urgent by React design. Defer the
  // expensive virtual tree projection so bursts of watcher deltas can be
  // coalesced in the background while the last committed tree stays put.
  // This avoids painting transient intermediate filesystem states without
  // delaying the store itself or any non-React consumers.
  const snapshot = useDeferredValue(latestSnapshot);
  const rootDisplayNames = useMemo(() => {
    const roots = snapshot.roots();
    const duplicateCounts = new Map<string, number>();
    for (const root of roots) {
      duplicateCounts.set(root.name, (duplicateCounts.get(root.name) ?? 0) + 1);
    }
    const duplicateIndexes = new Map<string, number>();
    const labels = new Map<EntryId, string>();
    roots.forEach((root, index) => {
      const duplicateCount = duplicateCounts.get(root.name) ?? 1;
      const duplicateIndex = duplicateIndexes.get(root.name) ?? 0;
      duplicateIndexes.set(root.name, duplicateIndex + 1);
      labels.set(
        root.id,
        rootLabel?.(root, { index, duplicateIndex, duplicateCount }) ??
          (duplicateCount > 1 ? `${root.name} (${duplicateIndex + 1})` : root.name),
      );
    });
    return labels;
  }, [rootLabel, snapshot]);
  const commandsHandle = ctx.commands;
  const initialNavigationStateRef = useRef<FileTreeNavigationState | null>(
    parseFileTreeNavigationState(initialNavigationState),
  );

  // Rename + create state. Drives the inline FileRenameInput on the
  // matching row. Controlled mode when caller passes `renameTargetId`.
  const renameState = useRenameState({
    fx: fx as unknown as RenameStateFx,
    snapshot: snapshot as unknown as RenameStateSnapshot,
    ...(controlledRenameTargetId !== undefined && onRenameTargetIdChange
      ? {
          controlled: {
            value: controlledRenameTargetId,
            onChange: onRenameTargetIdChange,
          },
        }
      : {}),
  });

  // Phase 8 — filter / search state. Always instantiated, but only
  // *applied* when `searchMode !== 'off'`. The filter input's `onChange`
  // flows through here; the tree then decides whether to substring-match
  // visible rows (`'filter'`) or swap to `<SearchResultList>` (`'search'`).
  const filterState = useFilterState(
    controlledFilter !== undefined && onFilterChange
      ? { controlled: { value: controlledFilter, onChange: onFilterChange } }
      : controlledFilter !== undefined
        ? { controlled: { value: controlledFilter, onChange: () => {} } }
        : { defaultValue: initialNavigationStateRef.current?.filter ?? '' },
  );
  const [searchMode, setSearchMode] = useControlledState<FileTreeSearchMode>({
    value: controlledSearchMode,
    defaultValue: initialNavigationStateRef.current?.searchMode ?? 'off',
    ...(onSearchModeChange ? { onChange: onSearchModeChange } : {}),
  });

  // Embedded filter input ref (used when `showFilter` is true). Separate
  // from `filterInputRef` so we can fall back to focusing the embedded
  // one even when the caller didn't supply a ref.
  const embeddedFilterInputRef = useRef<FileTreeFilterHandle | null>(null);

  const scrollerRef = useRef<HTMLDivElement | null>(null);
  const initialRootIdsRef = useRef<readonly EntryId[]>(
    snapshot.roots().map((root) => root.id),
  );
  const autoExpandedRootIdsRef = useRef<Set<EntryId>>(
    new Set(initialRootIdsRef.current),
  );
  const {
    expanded,
    toggle,
    add: addExpanded,
    remove: removeExpanded,
    setMany: setManyExpanded,
    clear: clearExpanded,
  } = useExpandedSet(initialRootIdsRef.current);

  // Workspace roots are open by default. Roots commonly arrive after
  // the provider's initial empty snapshot, so expand each one the first
  // time it appears. Remembering which roots were initialized prevents
  // later snapshot updates from reopening a root the user collapsed.
  useEffect(() => {
    const unseenRootIds: EntryId[] = [];
    for (const root of snapshot.roots()) {
      if (autoExpandedRootIdsRef.current.has(root.id)) continue;
      autoExpandedRootIdsRef.current.add(root.id);
      unseenRootIds.push(root.id);
    }
    if (unseenRootIds.length > 0) {
      setManyExpanded({ add: unseenRootIds });
    }
  }, [snapshot, setManyExpanded]);

  // Selection (selection + focus state) — respects controlled props.
  const selection = useFileTreeSelection({
    ...(controlledSelectedIds !== undefined
      ? { selectedIds: controlledSelectedIds }
      : {}),
    ...(onSelectionChange ? { onSelectionChange } : {}),
    ...(controlledFocusedId !== undefined
      ? { focusedId: controlledFocusedId }
      : {}),
    ...(onFocusedIdChange ? { onFocusedIdChange } : {}),
  });

  // Bridge: every change to `expanded` is forwarded to the engine
  // (wrapped in `startTransition`). Engine's subsequent delta flows
  // back through the provider's `useSyncExternalStore`.
  useSetExpandedBridge(fx, expanded);

  const projectionCacheRef = useRef<TreeProjection | null>(null);
  const projection = readTreeProjection(
    snapshot,
    expanded,
    projectionCacheRef.current,
  );
  useLayoutEffect(() => {
    projectionCacheRef.current = projection;
  }, [projection]);

  const visibleCount = projection.visibleCount;
  const count = visibleCount.known;
  const pendingSet = visibleCount.pendingExpansions;

  const rootsCount = snapshot.roots().length;

  // The virtualizer needs only the authoritative count. Once it publishes its
  // mounted indexes, read exactly that structural window from the snapshot.
  const { virtualItems, totalSize, viewportSize, scrollOffset, scrollToIndex } =
    useVirtualizerForSnapshot({
      count,
      rowHeight,
      overscan,
      scrollerRef,
      ...(__testObserveElementRect
        ? { observeElementRect: __testObserveElementRect }
        : null),
      ...(__testObserveElementOffset
        ? { observeElementOffset: __testObserveElementOffset }
        : null),
    });
  const viewportScrollOffset = scrollerRef.current?.scrollTop ?? scrollOffset;
  const mountedViewportOffset = virtualItems[0]?.index ?? 0;
  const mountedViewportLimit =
    virtualItems.length === 0
      ? 0
      : (virtualItems[virtualItems.length - 1]?.index ?? mountedViewportOffset) -
        mountedViewportOffset +
        1;
  // A partially hydrated remote snapshot can contain fewer known rows than
  // the screen can display. Publishing only those known rows deadlocks lazy
  // expansion: the host learns the child ids, but withholds their metadata
  // because they fall outside the advertised one-row window. Keep rendering
  // the authoritative mounted range while advertising enough capacity to
  // hydrate one physical viewport (plus the UI's bounded overscan).
  const hydrationViewportLimit = Math.max(
    mountedViewportLimit,
    Math.ceil(viewportSize / rowHeight) + overscan * 2,
  );
  const visibleRows = projection.readRows(
    mountedViewportOffset,
    mountedViewportLimit,
  );
  const readAllVisibleRows = useCallback(
    (): readonly VisibleRow[] => projection.readAllRows(),
    [projection],
  );
  const rowAtIndex = useCallback(
    (index: number): VisibleRow | undefined =>
      visibleRows[index - mountedViewportOffset],
    [visibleRows, mountedViewportOffset],
  );
  useEffect(() => {
    __testOnProjectionMaterialized?.(visibleRows.length);
  }, [visibleRows, __testOnProjectionMaterialized]);

  // Reconcile the mounted interaction neighborhood only. Offscreen selections
  // stay intact (their mirror records may be evicted); a deleted mounted focus
  // repairs to the nearest row occupying its old local position.
  const interactionProjectionRef = useRef({
    treeVersion: snapshot.treeVersion,
    offset: mountedViewportOffset,
    rows: visibleRows,
  });
  useLayoutEffect(() => {
    const previous = interactionProjectionRef.current;
    interactionProjectionRef.current = {
      treeVersion: snapshot.treeVersion,
      offset: mountedViewportOffset,
      rows: visibleRows,
    };
    if (previous.treeVersion === snapshot.treeVersion) return;
    const previousIds = new Set(previous.rows.map((row) => row.id));
    const currentIds = new Set(visibleRows.map((row) => row.id));
    const survives = (id: EntryId): boolean =>
      currentIds.has(id) || !previousIds.has(id) || snapshot.getById(id) !== null;
    let selectionChanged = false;
    const survivingSelection = new Set<EntryId>();
    for (const id of selection.selectedIds) {
      if (survives(id)) survivingSelection.add(id);
      else selectionChanged = true;
    }
    const previousFocusedId = selection.focusedId;
    let focusedId = previousFocusedId;
    if (focusedId !== null && !survives(focusedId)) {
      const previousIndex = previous.rows.findIndex((row) => row.id === focusedId);
      const fallback = visibleRows[Math.min(Math.max(previousIndex, 0), visibleRows.length - 1)];
      focusedId = fallback?.id ?? null;
      if (
        previousFocusedId !== null &&
        selection.selectedIds.has(previousFocusedId) &&
        survivingSelection.size === 0 &&
        focusedId !== null
      ) {
        survivingSelection.add(focusedId);
        selectionChanged = true;
      }
    }
    const anchorId =
      selection.anchorId === null || survives(selection.anchorId)
        ? selection.anchorId
        : focusedId;
    if (selectionChanged) selection.setSelection(survivingSelection);
    if (focusedId !== selection.focusedId) selection.setFocused(focusedId);
    if (anchorId !== selection.anchorId) selection.setAnchor(anchorId);
  }, [
    snapshot,
    snapshot.treeVersion,
    mountedViewportOffset,
    visibleRows,
    selection,
  ]);

  useEffect(() => {
    fx.setViewport?.({
      offset: mountedViewportOffset,
      limit: hydrationViewportLimit,
      // The published capacity already includes UI overscan.
      overscan: 0,
    });
  }, [
    fx,
    snapshot.treeVersion,
    mountedViewportOffset,
    hydrationViewportLimit,
  ]);

  const previousProjectionRef = useRef({
    treeVersion: snapshot.treeVersion,
    projection,
    rowHeight,
    scrollOffset: viewportScrollOffset,
  });
  const viewportAnchorResolution = useMemo(() => {
    const previous = previousProjectionRef.current;
    if (
      previous.treeVersion === snapshot.treeVersion ||
      previous.rowHeight !== rowHeight ||
      viewportScrollOffset <= 0
    ) {
      return null;
    }
    const previousWindow = {
      rowCount: previous.projection.visibleCount.known,
      readRows: previous.projection.readRows,
      findRowIndex: previous.projection.findRowIndex,
    };
    const nextWindow = {
      rowCount: projection.visibleCount.known,
      readRows: projection.readRows,
      findRowIndex: projection.findRowIndex,
    };
    const anchor = captureWindowedViewportAnchor(
      previousWindow,
      previous.scrollOffset,
      rowHeight,
    );
    return anchor
      ? resolveWindowedViewportAnchor(anchor, previousWindow, nextWindow, rowHeight)
      : null;
  }, [snapshot.treeVersion, projection, rowHeight, viewportScrollOffset]);
  const viewportAnchorAdjustment =
    viewportAnchorResolution &&
    Math.abs(viewportAnchorResolution.scrollOffsetPx - viewportScrollOffset) > 0.5
      ? viewportAnchorResolution
      : null;
  const anchoredTreeVersionRef = useRef<number | null>(null);

  useLayoutEffect(() => {
    // Publish the new projection before scrolling. Browser scroll observers may
    // notify synchronously; a nested render must not try to re-anchor the same
    // treeVersion against the previous projection a second time.
    previousProjectionRef.current = {
      treeVersion: snapshot.treeVersion,
      projection,
      rowHeight,
      scrollOffset: viewportAnchorAdjustment?.scrollOffsetPx ?? viewportScrollOffset,
    };
    if (viewportAnchorAdjustment) {
      anchoredTreeVersionRef.current = snapshot.treeVersion;
      const scroller = scrollerRef.current;
      if (scroller) {
        scroller.scrollTo({ top: viewportAnchorAdjustment.scrollOffsetPx });
      }
    }
  }, [
    snapshot.treeVersion,
    projection,
    rowHeight,
    viewportScrollOffset,
    viewportAnchorAdjustment,
  ]);

  const prefersReducedMotion = usePrefersReducedMotion();
  const previousTreeVersionRef = useRef(snapshot.treeVersion);
  const previousRenderedPositionsRef = useRef<ReadonlyMap<EntryId, number>>(new Map());
  const [layoutAnimation, setLayoutAnimation] = useState<LayoutAnimationPlan>(() => ({
    active: false,
    enteringIds: new Set<EntryId>(),
    repositioningIds: new Set<EntryId>(),
    suppressedBy: 'initial',
  }));
  const treeVersionChanged = previousTreeVersionRef.current !== snapshot.treeVersion;
  const renderedPositions = useMemo<readonly RenderedRowPosition[]>(() => {
    const positions: RenderedRowPosition[] = [];
    for (const item of virtualItems) {
      const row = rowAtIndex(item.index);
      if (row) positions.push({ id: row.id, offsetPx: item.start });
    }
    return positions;
  }, [virtualItems, rowAtIndex]);
  const nextLayoutAnimation = useMemo<LayoutAnimationPlan>(() => {
    if (!treeVersionChanged) return layoutAnimation;
    return planLayoutAnimation(previousRenderedPositionsRef.current, renderedPositions, {
      viewportAnchored:
        viewportAnchorAdjustment !== null ||
        anchoredTreeVersionRef.current === snapshot.treeVersion,
      prefersReducedMotion,
      animationInFlight: layoutAnimation.active,
    });
  }, [
    treeVersionChanged,
    layoutAnimation,
    renderedPositions,
    viewportAnchorAdjustment,
    snapshot.treeVersion,
    prefersReducedMotion,
  ]);
  const displayedLayoutAnimation = prefersReducedMotion
    ? {
        active: false,
        enteringIds: new Set<EntryId>(),
        repositioningIds: new Set<EntryId>(),
        suppressedBy: 'reduced-motion' as const,
      }
    : nextLayoutAnimation;
  const animationSuppression =
    displayedLayoutAnimation.suppressedBy === 'anchored' ||
    displayedLayoutAnimation.suppressedBy === 'reduced-motion' ||
    displayedLayoutAnimation.suppressedBy === 'in-flight' ||
    displayedLayoutAnimation.suppressedBy === 'budget'
      ? displayedLayoutAnimation.suppressedBy
      : undefined;

  useLayoutEffect(() => {
    previousRenderedPositionsRef.current = new Map(
      renderedPositions.map((row) => [row.id, row.offsetPx]),
    );
  }, [renderedPositions]);

  useEffect(() => {
    if (previousTreeVersionRef.current === snapshot.treeVersion) return;
    previousTreeVersionRef.current = snapshot.treeVersion;
    setLayoutAnimation(nextLayoutAnimation);
    if (!nextLayoutAnimation.active) return;
    const timeout = setTimeout(() => {
      setLayoutAnimation({
        active: false,
        enteringIds: new Set<EntryId>(),
        repositioningIds: new Set<EntryId>(),
        suppressedBy: 'no-visible-change',
      });
    }, 170);
    return () => clearTimeout(timeout);
    // `nextLayoutAnimation` belongs to this exact treeVersion render. A state
    // render must not restart the transaction for the same version.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [snapshot.treeVersion]);

  // ─── Keyboard helpers ──────────────────────────────────────────────

  const expansionActions = useMemo<KeyboardExpansionActions>(
    () => ({
      isExpanded: (id) => expanded.has(id),
      expand: (id) => addExpanded(id),
      collapse: (id) => removeExpanded(id),
      toggle: (id) => toggle(id),
    }),
    [expanded, addExpanded, removeExpanded, toggle],
  );

  const findExactVisibleRowIndex = useCallback(
    (id: EntryId, hintIndex = mountedViewportOffset): number =>
      projection.findExactRowIndex(id, hintIndex),
    [mountedViewportOffset, projection],
  );

  const findVisibleRowIndex = useCallback(
    (id: EntryId, hintIndex = mountedViewportOffset): number => {
      const boundedIndex = projection.findRowIndex(id, hintIndex);
      return boundedIndex !== -1
        ? boundedIndex
        : findExactVisibleRowIndex(id, hintIndex);
    },
    [mountedViewportOffset, projection, findExactVisibleRowIndex],
  );

  const indexedTypeaheadMatchRef = useRef<(id: EntryId) => void>(() => {});
  const rowsLookup = useMemo<KeyboardRowLookup>(
    () => ({
      get visibleRows() {
        return readAllVisibleRows();
      },
      rowCount: count,
      readRows: projection.readRows,
      readRowIds: projection.readIds,
      findRowIndex: findVisibleRowIndex,
      findExactRowIndex: findExactVisibleRowIndex,
      ...(typeof fx.findVisiblePrefix === 'function'
        ? {
            findTypeaheadMatch: async (
              prefix: string,
              fromId: EntryId | null,
              skipCurrent: boolean,
            ) =>
              (await fx.findVisiblePrefix!(prefix, fromId, skipCurrent, expanded)) ?? null,
            onTypeaheadMatch: (id: EntryId) => indexedTypeaheadMatchRef.current(id),
          }
        : {}),
      getRowById: (id) => {
        const mounted = visibleRows.find((row) => row.id === id);
        if (mounted) return mounted;
        const index = findVisibleRowIndex(id);
        return index === -1 ? null : projection.readRows(index, 1)[0] ?? null;
      },
    }),
    [
      readAllVisibleRows,
      count,
      findVisibleRowIndex,
      findExactVisibleRowIndex,
      projection,
      fx,
      expanded,
      mountedViewportOffset,
      visibleRows,
    ],
  );

  const viewportActions = useMemo<ViewportKeyboardActions>(
    () => ({
      pageUp: (fromId) => {
        const el = scrollerRef.current;
        const viewportPx = el ? el.clientHeight || 0 : 0;
        const rowsPerPage = viewportPx > 0
          ? Math.max(1, Math.floor(viewportPx / rowHeight))
          : 10;
        if (count === 0) return null;
        const fromIdx = fromId !== null
          ? findVisibleRowIndex(fromId)
          : -1;
        const base = fromIdx >= 0 ? fromIdx : 0;
        const targetIdx = Math.max(0, base - rowsPerPage);
        const target = projection.readRows(targetIdx, 1)[0];
        return target ? target.id : null;
      },
      pageDown: (fromId) => {
        const el = scrollerRef.current;
        const viewportPx = el ? el.clientHeight || 0 : 0;
        const rowsPerPage = viewportPx > 0
          ? Math.max(1, Math.floor(viewportPx / rowHeight))
          : 10;
        if (count === 0) return null;
        const fromIdx = fromId !== null
          ? findVisibleRowIndex(fromId)
          : -1;
        const base = fromIdx >= 0 ? fromIdx : 0;
        const targetIdx = Math.min(count - 1, base + rowsPerPage);
        const target = projection.readRows(targetIdx, 1)[0];
        return target ? target.id : null;
      },
    }),
    [count, findVisibleRowIndex, projection, rowHeight],
  );

  // ─── Phase 5: create-new-entry flow ─────────────────────────────────
  //
  // When Mod+N / Mod+Shift+N fires, we synthesize a provisional name,
  // ask the engine to create the entry, then open the rename input on
  // the returned id so the user types the real name. If the engine
  // rejects our provisional name (sanitization), we try `__new_1`,
  // `__new_2` etc. before giving up.
  const onCreate = useCallback(
    async (kind: 'file' | 'directory') => {
      // Resolve parent id: prefer focused folder, else focused's parent,
      // else first root.
      const focusedId = selection.focusedId;
      let parentId: EntryId | null = null;
      if (focusedId !== null) {
        const focused = snapshot.getById(focusedId);
        if (focused !== null) {
          parentId = focused.kind === 1 ? focused.id : focused.parentId;
        }
      }
      if (parentId === null) {
        const firstRoot = snapshot.roots()[0];
        if (firstRoot) parentId = firstRoot.id;
      }
      if (parentId === null) return;

      // Duck-typed create: FileTreeEngine doesn't declare create on its
      // minimal surface — the real engine does. Test fakes also do.
      const fxWithCreate = fx as unknown as {
        create?(parentId: EntryId, name: string, kind: number): Promise<{ id: EntryId }>;
      };
      if (typeof fxWithCreate.create !== 'function') return;

      const numericKind = kind === 'directory' ? 1 : 0;
      const provisional = `__mille_new_${kind}__`;
      const fallbackBase = '__new_';
      let attempt = 0;
      let lastErr: unknown = null;
      // Try provisional name, then `__new_1`, `__new_2`, ...
      while (attempt <= 20) {
        const name = attempt === 0 ? provisional : `${fallbackBase}${attempt}`;
        try {
          const entry = await fxWithCreate.create(parentId, name, numericKind);
          if (entry && typeof entry.id === 'number') {
            renameState.startRename(entry.id);
          }
          return;
        } catch (e) {
          lastErr = e;
          attempt += 1;
        }
      }
      // Exhausted attempts; surface via console only — the host's
      // `file.create` command registry can install its own error surface.
      if (lastErr && typeof process !== 'undefined' && process.env?.['NODE_ENV'] !== 'production') {
        // eslint-disable-next-line no-console
        console.warn('mille-ui: file.create: provisional-name attempts exhausted', lastErr);
      }
    },
    [fx, snapshot, selection, renameState],
  );

  // ─── Phase 6: context menu state ────────────────────────────────
  //
  // Row DOM ref registry (for keyboard-triggered menu open) + the
  // "open on row" dispatcher called by `useFileTreeKeyboard`.
  const contextMenuState = useContextMenuState();
  const onOpenContextMenu = useCallback(
    (id: EntryId | null) => {
      if (disableContextMenu) return;
      if (id === null) return;
      contextMenuState.openAtRow(id);
    },
    [contextMenuState, disableContextMenu],
  );

  // ─── Phase 7: in-app clipboard (cut/copy/paste) ─────────────────
  //
  // State local to the tree. Host observers read via `onClipboardChange`.
  // The registry's `file.paste` command is registered on mount if not
  // already present — this lets consumers who wire their own (older)
  // registry still get paste semantics from Mod+V. Hosts that register
  // their own `file.paste` win via the normal shadow/restore stack.
  const clipboard = useClipboardState();

  // Forward changes to any caller-supplied observer.
  useEffect(() => {
    if (!onClipboardChange) return;
    onClipboardChange({
      cutIds: clipboard.cutIds,
      copyIds: clipboard.copyIds,
    });
  }, [onClipboardChange, clipboard.cutIds, clipboard.copyIds]);

  // Register `file.paste` into a full command registry if absent. We
  // do NOT try to install into the minimal `dispatch`-only handle:
  // hosts that pass only a handle are opting into a different wiring
  // and can register `file.paste` themselves.
  useEffect(() => {
    const handle = commandsHandle as unknown as {
      get?: (id: string) => Command | undefined;
      register?: (command: Command) => { dispose(): void };
    } | null;
    if (!handle || typeof handle.register !== 'function' || typeof handle.get !== 'function') {
      return undefined;
    }
    if (handle.get('file.paste') !== undefined) return undefined;
    const paste = defaultCommands.find((c) => c.id === 'file.paste');
    if (!paste) return undefined;
    const disp = handle.register(paste);
    return () => {
      disp.dispose();
    };
  }, [commandsHandle]);

  // ─── Phase 11 — drag-and-drop coordinator ───────────────────────
  //
  // Always instantiated so row-level DnD handlers are stable identities
  // across renders (the hook's `handlers` object is memoized). Row
  // wiring gates on `disableDragDrop` to decide whether to actually
  // attach the listeners; the hook's state is harmlessly idle when no
  // drags occur.
  //
  // The `DragDropOptions` type in `components/types.ts` overlaps with
  // the hook's own option surface but declares a deprecated `external`
  // alias. Normalize: if `external` is present and `externalIn` is not,
  // forward `external` as `externalIn`.
  const dndOptions = useMemo<Parameters<typeof useFileTreeDragDrop>[0]>(() => {
    if (!dragDrop) return {};
    return {
      ...(dragDrop.internal !== undefined ? { internal: dragDrop.internal } : null),
      ...(dragDrop.externalOut !== undefined ? { externalOut: dragDrop.externalOut } : null),
      ...(dragDrop.externalIn !== undefined
        ? { externalIn: dragDrop.externalIn }
        : dragDrop.external !== undefined
          ? { externalIn: dragDrop.external }
          : null),
      ...(dragDrop.crossRoot !== undefined ? { crossRoot: dragDrop.crossRoot } : null),
      ...(dragDrop.collision !== undefined ? { collision: dragDrop.collision } : null),
      ...(dragDrop.onDropValidate !== undefined ? { onDropValidate: dragDrop.onDropValidate } : null),
      ...(dragDrop.onConfirm !== undefined ? { onConfirm: dragDrop.onConfirm } : null),
      ...(dragDrop.onCollision !== undefined ? { onCollision: dragDrop.onCollision } : null),
      ...(dragDrop.onDropError !== undefined ? { onDropError: dragDrop.onDropError } : null),
      ...(dragDrop.autoExpandDelayMs !== undefined ? { autoExpandDelayMs: dragDrop.autoExpandDelayMs } : null),
    };
  }, [dragDrop]);

  const dnd = useFileTreeDragDrop(dndOptions, { selectedIds: selection.selectedIds });

  const { onKeyDown } = useFileTreeKeyboard({
    selection,
    expansion: expansionActions,
    rows: rowsLookup,
    viewport: viewportActions,
    commands: commandsHandle,
    multiSelect,
    ...(onOpen
      ? {
          onOpenFallback: (
            row: VisibleRow,
            event: Parameters<NonNullable<typeof onOpen>>[1],
          ) => onOpen(row, event),
        }
      : {}),
    onStartRename: (id) => {
      renameState.startRename(id);
    },
    onCreate,
    onOpenContextMenu,
    onEscape: () => {
      // Phase 7: Esc clears the clipboard in addition to selection. The
      // rename-input owns Esc when a rename is active — this callback
      // only fires when the tree's root `onKeyDown` handles the
      // keystroke (i.e. user NOT in rename).
      clipboard.clear();
      // Phase 8 — Esc also clears the filter so the tree returns to
      // an unfiltered view. Harmless when no filter was active.
      filterState.clear();
    },
    onFocusFilter: () => {
      // Phase 8 — prefer the caller-supplied ref; fall back to the
      // embedded filter input when `showFilter` is true.
      const ext = filterInputRef?.current;
      if (ext && typeof ext.focus === 'function') {
        ext.focus();
        return;
      }
      const emb = embeddedFilterInputRef.current;
      if (emb && typeof emb.focus === 'function') {
        emb.focus();
      }
      // When neither is available, `tree.focusFilter` has already been
      // dispatched through the registry (Phase 2 contract) — the host
      // owns the side effect in that case.
    },
    clipboard: {
      cutIds: clipboard.cutIds,
      copyIds: clipboard.copyIds,
      markCut: clipboard.markCut,
      markCopy: clipboard.markCopy,
      clear: clipboard.clear,
    },
  });

  // ─── Phase 6: menu context + rendered content ───────────────────
  //
  // The `commandsHandle` published through the Provider is the minimal
  // `CommandRegistryHandle` (dispatch-only); a host wiring a real
  // `CommandRegistry` (from `createCommandRegistry`) passes one that is
  // structurally compatible with the full interface. We narrow by
  // sniffing for `all()`; when absent, we skip the menu rendering
  // altogether so no empty menus appear.
  const fullRegistry = commandsHandle as unknown as
    | (CommandRegistry & CommandRegistryHandleLike)
    | null;
  const hasFullRegistry =
    fullRegistry !== null &&
    typeof (fullRegistry as unknown as { all?: unknown }).all === 'function';

  const menuSnapshotRef = useRef(snapshot);
  useEffect(() => {
    menuSnapshotRef.current = snapshot;
  }, [snapshot]);
  const menuCommandContext = useMemo<CommandContext | null>(() => {
    if (!hasFullRegistry) return null;
    const focusedId = selection.focusedId;
    const selectedIds = selection.selectedIds;
    // Snapshot-only watcher updates should not recreate the same menu
    // subtree once per visible row. Getters keep the command data live at
    // menu-open/dispatch time while the context object remains stable until
    // actual UI command state (focus, selection, clipboard, rename) changes.
    return Object.defineProperties({
      // `fx` is structurally richer than the Engine surface we use;
      // for menu population we only need the snapshot + state, but
      // hosts' custom commands may rely on the full engine.
      fx: fx as unknown as CommandContext['fx'],
      focusedId,
      selectedIds,
      isMultiSelect: multiSelect && selectedIds.size > 1,
      isRenaming: renameState.renameTargetId !== null,
      host: {
        ...(hostHooks ?? null),
        ...(onOpen ? { onOpen } : null),
        ...(onCopyPath ? { copyPath: onCopyPath } : null),
        ...(onRevealInFileManager
          ? { revealInFileManager: onRevealInFileManager }
          : null),
        ...(onOpenContainingFolder
          ? { openContainingFolder: onOpenContainingFolder }
          : null),
        ...(onOpenTerminal
          ? { openTerminalForEntry: onOpenTerminal }
          : null),
        ...(onRefresh ? { refresh: onRefresh } : null),
        ...(onSearchScope ? { searchScope: onSearchScope } : null),
      },
      expansion: {
        expandedIds: expanded,
        setExpanded: setManyExpanded,
      },
      cutIds: clipboard.cutIds,
      copyIds: clipboard.copyIds,
    }, {
      snapshot: {
        enumerable: true,
        get: () => menuSnapshotRef.current as unknown as CommandContext['snapshot'],
      },
      focusedEntry: {
        enumerable: true,
        get: (): Entry | null =>
          focusedId !== null ? menuSnapshotRef.current.getById(focusedId) : null,
      },
      selectedEntries: {
        enumerable: true,
        get: (): Entry[] => {
          const entries: Entry[] = [];
          for (const id of selectedIds) {
            const entry = menuSnapshotRef.current.getById(id);
            if (entry !== null) entries.push(entry);
          }
          return entries;
        },
      },
    }) as CommandContext;
  }, [
    hasFullRegistry,
    fx,
    selection.focusedId,
    selection.selectedIds,
    multiSelect,
    renameState.renameTargetId,
    clipboard.cutIds,
    clipboard.copyIds,
    onOpen,
    onCopyPath,
    onRevealInFileManager,
    onOpenContainingFolder,
    onOpenTerminal,
    onRefresh,
    onSearchScope,
    hostHooks,
    expanded,
    setManyExpanded,
  ]);

  const contextMenuContent = useMemo<ReactNode>(() => {
    if (disableContextMenu) return null;
    if (contextMenuSlot !== undefined) return contextMenuSlot;
    if (!hasFullRegistry || menuCommandContext === null || fullRegistry === null) {
      return null;
    }
    return (
      <FileContextMenu
        registry={fullRegistry as CommandRegistry}
        context={menuCommandContext}
        {...(contextMenuExtraItems !== undefined
          ? { extraItems: contextMenuExtraItems }
          : null)}
        {...(contextMenuEmptyPlaceholder !== undefined
          ? { emptyPlaceholder: contextMenuEmptyPlaceholder }
          : null)}
      />
    );
  }, [
    disableContextMenu,
    contextMenuSlot,
    hasFullRegistry,
    fullRegistry,
    menuCommandContext,
    contextMenuExtraItems,
    contextMenuEmptyPlaceholder,
  ]);

  // Chevron click — toggles local expansion and dispatches through the
  // command registry when one is attached.
  const onChevronClick = useCallback(
    (id: EntryId, wasExpanded: boolean) => {
      toggle(id);
      if (commandsHandle) {
        const commandId = wasExpanded ? 'tree.collapse' : 'tree.expand';
        try {
          const result = commandsHandle.dispatch(commandId, { id });
          if (result && typeof (result as Promise<void>).catch === 'function') {
            (result as Promise<void>).catch(() => {
              /* swallow — local toggle already applied */
            });
          }
        } catch {
          /* swallow — local toggle already applied */
        }
      }
    },
    [commandsHandle, toggle],
  );

  // ─── Focus restoration ─────────────────────────────────────────────
  //
  // Remember the last focused id across blurs so re-entering the tree
  // via Tab restores the previous focused row. Separate from selection
  // focus — focus can advance while selection stays put.
  const lastFocusedRef = useRef<EntryId | null>(null);
  useEffect(() => {
    if (selection.focusedId !== null) {
      lastFocusedRef.current = selection.focusedId;
    }
  }, [selection.focusedId]);

  // If focus enters the container but no row is focused (e.g. user
  // Tab'd in), pick the remembered row (or the first row as a fallback).
  const onContainerFocus = useCallback(
    (e: ReactFocusEvent<HTMLElement>) => {
      // Only intercept focus events where the target is the container
      // itself, not a descendant row (rows fire onFocus separately).
      if (e.target !== e.currentTarget) return;
      if (selection.focusedId !== null) return;
      const restoreId = lastFocusedRef.current;
      const first = projection.readRows(0, 1)[0];
      if (restoreId !== null && findVisibleRowIndex(restoreId) !== -1) {
        selection.setFocused(restoreId);
      } else if (first) {
        selection.setFocused(first.id);
      }
    },
    [selection, projection, findVisibleRowIndex],
  );

  const onRowFocus = useCallback(
    (id: EntryId) => {
      if (selection.focusedId !== id) {
        selection.setFocused(id);
      }
    },
    [selection],
  );

  // ─── v0.2 B6 — Imperative handle ───────────────────────────────────
  //
  // Exposes a `FileTreeRef` surface for hosts that want to drive the
  // tree programmatically (command palette, "Reset" button, etc.). All
  // state mutators delegate to the existing hooks (`selection.clear`,
  // `filterState.clear`, `clipboard.clear`, `addExpanded`, …); the
  // `revealPath` prefers `fx.resolvePath`, then falls back to the legacy
  // URI and snapshot child-walk strategies.

  // Computing an ancestor chain from a target id to its root walks
  // `entry.parentId`. Returned leaf-first; callers reverse for
  // top-down expansion. Short-circuits when any ancestor is missing.
  const ancestorsOf = useCallback(
    (id: EntryId, source: FileTreeSnapshotLike = snapshot): readonly EntryId[] => {
      const chain: EntryId[] = [];
      let cursor: EntryId | null = id;
      let guard = 0;
      while (cursor !== null && guard < 10_000) {
        const entry = source.getById(cursor);
        if (entry === null) break;
        if (entry.parentId === null) break;
        chain.push(entry.parentId);
        cursor = entry.parentId;
        guard += 1;
      }
      return chain;
    },
    [snapshot],
  );

  const pendingRevealRef = useRef<{
    readonly id: EntryId;
    readonly focus: boolean;
  } | null>(null);
  const scrollToRevealedIndex = useCallback(
    (index: number) => {
      const scroller = scrollerRef.current;
      if (scroller) {
        scroller.scrollTo({ top: index * rowHeight });
      } else {
        scrollToIndex(index, { align: 'start' });
      }
    },
    [rowHeight, scrollToIndex],
  );

  const revealIdWithFocus = useCallback(
    (id: EntryId, focus: boolean): boolean => {
      // Verify the id actually exists in the current mirror. If not we
      // can't reveal it — the caller is probably racing a walker; they
      // should retry once the next delta lands.
      if (snapshot.getById(id) === null) return false;

      const ancestors = ancestorsOf(id);
      const needsExpansion = ancestors.some((ancestorId) => !expanded.has(ancestorId));
      pendingRevealRef.current = { id, focus };
      if (focus) selection.setFocused(id);
      if (needsExpansion) {
        setManyExpanded({ add: ancestors });
        return true;
      }

      const idx = findExactVisibleRowIndex(id);
      if (idx >= 0) {
        scrollToRevealedIndex(idx);
        pendingRevealRef.current = null;
      }
      return true;
    },
    [
      snapshot,
      ancestorsOf,
      expanded,
      setManyExpanded,
      selection,
      findExactVisibleRowIndex,
      scrollToRevealedIndex,
    ],
  );

  const revealId = useCallback(
    (id: EntryId): boolean => revealIdWithFocus(id, true),
    [revealIdWithFocus],
  );

  const revealResolvedIdWithFocus = useCallback(
    (id: EntryId, focus: boolean): boolean => {
      if (snapshot.getById(id) !== null) return revealIdWithFocus(id, focus);
      const latest = fx.getSnapshot();
      pendingRevealRef.current = { id, focus };
      if (latest.getById(id) !== null) {
        setManyExpanded({ add: ancestorsOf(id, latest) });
      }
      return true;
    },
    [snapshot, revealIdWithFocus, fx, setManyExpanded, ancestorsOf],
  );

  const revealResolvedId = useCallback(
    (id: EntryId): boolean => revealResolvedIdWithFocus(id, true),
    [revealResolvedIdWithFocus],
  );

  useLayoutEffect(() => {
    indexedTypeaheadMatchRef.current = (id) => {
      selection.selectOne(id);
      revealResolvedId(id);
    };
  }, [selection, revealResolvedId]);

  useLayoutEffect(() => {
    const pending = pendingRevealRef.current;
    if (pending === null) return;
    const { id, focus } = pending;
    if (snapshot.getById(id) === null) {
      return;
    }
    const ancestors = ancestorsOf(id);
    if (ancestors.some((ancestorId) => !expanded.has(ancestorId))) {
      setManyExpanded({ add: ancestors });
      return;
    }
    const index = findExactVisibleRowIndex(id);
    if (index === -1) return;
    scrollToRevealedIndex(index);
    if (focus) selection.setFocused(id);
    pendingRevealRef.current = null;
  }, [
    snapshot,
    ancestorsOf,
    expanded,
    setManyExpanded,
    findExactVisibleRowIndex,
    scrollToRevealedIndex,
    selection,
  ]);

  // Prefer the engine's indexed workspace-relative resolver. Legacy engines
  // may expose getByUri or only snapshot children; those remain compatibility
  // fallbacks. Every strategy returns `null` when resolution fails.
  const resolvePath = useCallback(
    async (path: string): Promise<EntryId | null> => {
      const trimmed = path.replace(/^\/+/, '').replace(/\/+$/, '');
      if (trimmed.length === 0) {
        const firstRoot = snapshot.roots()[0];
        return firstRoot ? firstRoot.id : null;
      }
      const segments = trimmed.split('/');

      // Strategy 1 — authoritative engine/host path index.
      if (typeof fx.resolvePath === 'function') {
        try {
          const id = await fx.resolvePath(trimmed);
          if (typeof id === 'number') return id;
        } catch {
          // Compatibility fallbacks below may still resolve a known path.
        }
      }

      // Strategy 2 — engine URI lookup when available.
      const fxWithUri = fx as unknown as {
        getByUri?: (uri: { scheme: string; path: string }) =>
          | Promise<{ id: EntryId } | null>
          | { id: EntryId }
          | null;
      };
      if (typeof fxWithUri.getByUri === 'function') {
        try {
          const firstRoot = snapshot.roots()[0];
          const candidates: Array<{ scheme: string; path: string }> = [];
          if (firstRoot) {
            candidates.push({ scheme: 'mille', path: `/${firstRoot.name}/${trimmed}` });
          }
          candidates.push({ scheme: 'file', path: trimmed });
          for (const uri of candidates) {
            const entry = await fxWithUri.getByUri(uri);
            if (entry && typeof entry.id === 'number') {
              return entry.id;
            }
          }
        } catch {
          // Swallow — fall through to the child-walk fallback.
        }
      }

      // Strategy 3 — depth-first walk over each root's children.
      // Matches the first segment against any root by name; subsequent
      // segments against direct children. Uses the snapshot's
      // already-loaded entries only (no engine RPC).
      const roots = snapshot.roots();
      const snapAny = snapshot as unknown as {
        childrenOf?: (id: EntryId) => readonly (Entry | EntryId)[];
      };
      const childrenOf = (parentId: EntryId): readonly Entry[] => {
        if (typeof snapAny.childrenOf === 'function') {
          const out: Entry[] = [];
          for (const child of snapAny.childrenOf(parentId)) {
            const entry = typeof child === 'number' ? snapshot.getById(child) : child;
            if (entry !== null) out.push(entry);
          }
          return out;
        }
        // Fallback: scan the lazily materialized visible order for entries
        // whose parentId matches. Unexpanded subtrees won't be represented. Callers
        // needing deeper resolution should expand first.
        const out: Entry[] = [];
        for (const row of readAllVisibleRows()) {
          if (row.parentId === parentId) out.push(row as unknown as Entry);
        }
        return out;
      };

      // Descend one segment at a time.
      let cursor: Entry | null = null;
      const first = segments[0];
      if (first === undefined) return null;
      for (const root of roots) {
        if (root.name === first) {
          cursor = root;
          break;
        }
      }
      // Also allow the first segment to be a direct child of the first
      // root — matches the common case where `path` is relative to the
      // single workspace root (e.g. 'src/foo.ts').
      if (cursor === null && roots.length > 0) {
        const firstRoot = roots[0];
        if (firstRoot) {
          for (const child of childrenOf(firstRoot.id)) {
            if (child.name === first) {
              cursor = child;
              break;
            }
          }
          // If the path segment matched, start the remaining descent
          // below from the second segment; otherwise we'll return null
          // after the next loop because cursor is still null.
          if (cursor !== null) {
            for (let i = 1; i < segments.length; i += 1) {
              const seg = segments[i];
              const kids = childrenOf(cursor.id);
              let matched: Entry | null = null;
              for (const k of kids) {
                if (k.name === seg) {
                  matched = k;
                  break;
                }
              }
              if (matched === null) return null;
              cursor = matched;
            }
            return cursor.id;
          }
        }
      }
      if (cursor === null) return null;

      // Descend the remaining segments from the matched first-segment
      // entry (the "first segment is a root name" branch).
      for (let i = 1; i < segments.length; i += 1) {
        const seg = segments[i];
        const kids = childrenOf(cursor.id);
        let matched: Entry | null = null;
        for (const k of kids) {
          if (k.name === seg) {
            matched = k;
            break;
          }
        }
        if (matched === null) return null;
        cursor = matched;
      }
      return cursor.id;
    },
    [fx, snapshot, readAllVisibleRows],
  );

  const revealPath = useCallback(
    async (path: string): Promise<boolean> => {
      const id = await resolvePath(path);
      if (id === null) return false;
      return revealResolvedId(id);
    },
    [resolvePath, revealResolvedId],
  );

  const normalizedActiveEntry =
    activeEntry === null || activeEntry === undefined
      ? null
      : normalizeActiveEntryTarget(activeEntry);
  const activeEntryTarget = normalizedActiveEntry?.target;
  const activeEntryOrigin = normalizedActiveEntry?.origin ?? 'workspace';
  const revealHiddenActiveEntry = activeEntryPolicy?.revealHidden === true;
  const revealIgnoredActiveEntry = activeEntryPolicy?.revealIgnored === true;
  const revealGeneratedActiveEntry = activeEntryPolicy?.revealGenerated === true;
  const [resolvedActivePathId, setResolvedActivePathId] =
    useState<EntryId | null>(null);
  const resolvedActiveEntryId =
    activeEntryOrigin !== 'external' && typeof activeEntryTarget === 'number'
      ? activeEntryTarget
      : resolvedActivePathId;
  const activeResolutionRevisionRef = useRef(0);
  const resolveActivePathRef = useRef(resolvePath);
  const revealActiveIdRef = useRef(revealResolvedIdWithFocus);
  const reportActiveResolutionRef = useRef(onActiveEntryResolution);
  useLayoutEffect(() => {
    resolveActivePathRef.current = resolvePath;
    revealActiveIdRef.current = revealResolvedIdWithFocus;
    reportActiveResolutionRef.current = onActiveEntryResolution;
  }, [resolvePath, revealResolvedIdWithFocus, onActiveEntryResolution]);
  useEffect(() => {
    const revision = activeResolutionRevisionRef.current + 1;
    activeResolutionRevisionRef.current = revision;
    const invalidate = (): void => {
      if (activeResolutionRevisionRef.current === revision) {
        activeResolutionRevisionRef.current += 1;
      }
    };

    if (activeEntryTarget === undefined) {
      setResolvedActivePathId(null);
      return invalidate;
    }

    const finish = (id: EntryId | null): void => {
      if (activeResolutionRevisionRef.current !== revision) return;
      const latest = fx.getSnapshot() as FileTreeSnapshotLike;
      const entry = id === null ? null : latest.getById(id);
      const disposition = classifyActiveEntry({
        origin: activeEntryOrigin,
        entry,
        showHiddenFiles: latest.showHiddenFiles,
        showIgnoredFiles: latest.showIgnoredFiles,
      });
      let autoReveal: ActiveEntryAutoReveal = 'not-requested';
      if (autoRevealActiveEntry) {
        const allowed = shouldAutoRevealActiveEntry(disposition, {
          revealHidden: revealHiddenActiveEntry,
          revealIgnored: revealIgnoredActiveEntry,
          revealGenerated: revealGeneratedActiveEntry,
        });
        if (!allowed || id === null) {
          autoReveal = 'suppressed';
        } else {
          autoReveal = revealActiveIdRef.current(id, false)
            ? 'attempted'
            : 'failed';
        }
      }
      reportActiveResolutionRef.current?.({
        target: activeEntryTarget,
        origin: activeEntryOrigin,
        entryId: id,
        disposition,
        autoReveal,
      });
    };

    setResolvedActivePathId(null);
    if (activeEntryOrigin === 'external') {
      finish(null);
      return invalidate;
    }
    if (typeof activeEntryTarget === 'number') {
      finish(activeEntryTarget);
      return invalidate;
    }

    void resolveActivePathRef.current(activeEntryTarget).then(
      (id) => {
        if (activeResolutionRevisionRef.current !== revision) return;
        setResolvedActivePathId(id);
        finish(id);
      },
      () => {
        if (activeResolutionRevisionRef.current === revision) {
          setResolvedActivePathId(null);
          finish(null);
        }
      },
    );
    return invalidate;
  }, [
    activeEntryTarget,
    activeEntryOrigin,
    autoRevealActiveEntry,
    revealHiddenActiveEntry,
    revealIgnoredActiveEntry,
    revealGeneratedActiveEntry,
    fx,
  ]);

  const pendingNavigationScrollRef = useRef<{
    readonly id: EntryId;
    readonly offsetPx: number;
  } | null>(null);

  const captureNavigationState = useCallback((): FileTreeNavigationState => {
    const scrollTop = scrollerRef.current?.scrollTop ?? viewportScrollOffset;
    const anchorIndex =
      count === 0 ? -1 : Math.max(0, Math.min(count - 1, Math.floor(scrollTop / rowHeight)));
    const anchorRow = anchorIndex === -1 ? undefined : projection.readRows(anchorIndex, 1)[0];
    return captureFileTreeNavigationState({
      snapshot,
      expandedIds: expanded,
      selectedIds: selection.selectedIds,
      focusedId: selection.focusedId,
      filter: filterState.filter,
      searchMode,
      scrollAnchor: anchorRow
        ? {
            id: anchorRow.id,
            offsetPx: Math.max(0, scrollTop - anchorIndex * rowHeight),
          }
        : null,
    });
  }, [
    viewportScrollOffset,
    count,
    rowHeight,
    projection,
    snapshot,
    expanded,
    selection.selectedIds,
    selection.focusedId,
    filterState.filter,
    searchMode,
  ]);

  const restoreNavigationState = useCallback(
    async (
      input: FileTreeNavigationState | string,
    ): Promise<FileTreeNavigationRestoreResult> => {
      const state = parseFileTreeNavigationState(input);
      if (state === null) throw new TypeError('Invalid file-tree navigation state');

      const allPaths = new Set<string>([
        ...state.expandedPaths,
        ...state.selectedPaths,
        ...(state.focusedPath ? [state.focusedPath] : []),
        ...(state.scrollAnchor ? [state.scrollAnchor.path] : []),
      ]);
      const resolved = new Map<string, EntryId>();
      const paths = [...allPaths];
      // Keep lazy hydration bounded: large persisted workspaces should not
      // launch thousands of filesystem walks in one microtask.
      for (let offset = 0; offset < paths.length; offset += 32) {
        const batch = paths.slice(offset, offset + 32);
        const ids = await Promise.all(
          batch.map(async (path) => {
            try {
              return await resolvePath(path);
            } catch {
              return null;
            }
          }),
        );
        for (let index = 0; index < batch.length; index += 1) {
          const path = batch[index]!;
          const id = ids[index];
          if (id !== null && id !== undefined) resolved.set(path, id);
        }
      }

      const expandedIds = state.expandedPaths.flatMap((path) => {
        const id = resolved.get(path);
        return id === undefined ? [] : [id];
      });
      clearExpanded();
      setManyExpanded({ add: expandedIds });

      const selectedIds = new Set<EntryId>();
      for (const path of state.selectedPaths) {
        const id = resolved.get(path);
        if (id !== undefined) selectedIds.add(id);
      }
      selection.setSelection(selectedIds);

      const focusedId =
        state.focusedPath === null ? undefined : resolved.get(state.focusedPath);
      selection.setFocused(focusedId ?? null);
      selection.setAnchor(focusedId ?? null);
      filterState.setFilter(state.filter);
      setSearchMode(state.searchMode);

      const scrollId =
        state.scrollAnchor === null ? undefined : resolved.get(state.scrollAnchor.path);
      if (scrollId !== undefined && state.scrollAnchor !== null) {
        pendingNavigationScrollRef.current = {
          id: scrollId,
          offsetPx: state.scrollAnchor.offsetPx,
        };
        setManyExpanded({ add: ancestorsOf(scrollId, fx.getSnapshot()) });
      }

      const missingPaths = paths.filter((path) => !resolved.has(path));
      return {
        expanded: expandedIds.length,
        selected: selectedIds.size,
        focused: focusedId !== undefined,
        scrollAnchored: scrollId !== undefined,
        missingPaths,
      };
    },
    [
      resolvePath,
      clearExpanded,
      setManyExpanded,
      selection,
      filterState,
      setSearchMode,
      ancestorsOf,
      fx,
    ],
  );

  useLayoutEffect(() => {
    const pending = pendingNavigationScrollRef.current;
    if (pending === null) return;
    const index = projection.findExactRowIndex(pending.id, mountedViewportOffset);
    if (index === -1) return;
    const top = index * rowHeight + pending.offsetPx;
    const scroller = scrollerRef.current;
    if (scroller) scroller.scrollTo({ top });
    else scrollToIndex(index, { align: 'start' });
    pendingNavigationScrollRef.current = null;
  }, [projection, mountedViewportOffset, rowHeight, scrollToIndex]);

  const didRestoreInitialNavigationRef = useRef(false);
  useEffect(() => {
    if (didRestoreInitialNavigationRef.current || snapshot.roots().length === 0) return;
    didRestoreInitialNavigationRef.current = true;
    const state = initialNavigationStateRef.current;
    if (state !== null) void restoreNavigationState(state);
  }, [snapshot, restoreNavigationState]);

  const didPublishNavigationRef = useRef(false);
  const lastPublishedNavigationRef = useRef<string | null>(null);
  useEffect(() => {
    if (!onNavigationStateChange) return;
    if (!didPublishNavigationRef.current) {
      didPublishNavigationRef.current = true;
      lastPublishedNavigationRef.current = JSON.stringify(captureNavigationState());
      return;
    }
    const timeout = setTimeout(() => {
      const state = captureNavigationState();
      const serialized = JSON.stringify(state);
      if (serialized === lastPublishedNavigationRef.current) return;
      lastPublishedNavigationRef.current = serialized;
      onNavigationStateChange(state);
    }, Math.max(0, navigationStateDebounceMs));
    return () => clearTimeout(timeout);
  }, [
    onNavigationStateChange,
    navigationStateDebounceMs,
    captureNavigationState,
  ]);

  const focusFilter = useCallback((): boolean => {
    const ext = filterInputRef?.current;
    if (ext && typeof ext.focus === 'function') {
      ext.focus();
      return true;
    }
    const emb = embeddedFilterInputRef.current;
    if (emb && typeof emb.focus === 'function') {
      emb.focus();
      return true;
    }
    return false;
  }, [filterInputRef]);

  useImperativeHandle(
    forwardedRef,
    (): FileTreeRef => ({
      revealPath,
      revealId,
      scrollToRow: (index: number) => {
        scrollToIndex(index, { align: 'start' });
      },
      clearSelection: () => {
        selection.clear();
      },
      clearFilter: () => {
        filterState.clear();
      },
      clearClipboard: () => {
        clipboard.clear();
      },
      collapseAll: () => {
        clearExpanded();
      },
      collapseDescendants: (id: EntryId) => {
        const remove = expandedDescendantIds(snapshot, expanded, id);
        if (remove.length > 0) setManyExpanded({ remove });
      },
      expand: (ids: readonly EntryId[]) => {
        setManyExpanded({ add: ids });
      },
      captureNavigationState,
      restoreNavigationState,
      focusFilter,
      reset: () => {
        selection.clear();
        filterState.clear();
        clipboard.clear();
        // Return keyboard focus to the tree. The tree container itself
        // isn't focusable (rows own the roving tabindex); when a first
        // row exists, focus it so arrow keys work immediately. Without
        // any rows we call `.focus()` on the container anyway — if the
        // caller stapled a `tabIndex` this still lands meaningfully.
        const first = projection.readRows(0, 1)[0];
        if (first !== undefined) {
          selection.setFocused(first.id);
          const scrollEl = scrollerRef.current;
          const rowEl = scrollEl
            ? scrollEl.querySelector<HTMLElement>(
                `[data-mille-row-id="${first.id}"]`,
              )
            : null;
          if (rowEl && typeof rowEl.focus === 'function') {
            rowEl.focus();
            return;
          }
        }
        const el = scrollerRef.current;
        if (el && typeof el.focus === 'function') el.focus();
      },
    }),
    [
      revealPath,
      revealId,
      scrollToIndex,
      selection,
      filterState,
      clipboard,
      clearExpanded,
      expanded,
      setManyExpanded,
      snapshot,
      captureNavigationState,
      restoreNavigationState,
      focusFilter,
      projection,
    ],
  );

  // ─── Phase 8 — filter / search derivations ──────────────────────────
  //
  // `filterActive` means: user has typed something AND the mode makes
  // the filter meaningful. `'off'` leaves the filter inert; `'filter'`
  // triggers substring hiding; `'search'` swaps the tree for the
  // ranked result list.
  const filterText = filterState.filter;
  const filterActive = searchMode !== 'off' && filterText.length > 0;
  const inSearchMode = searchMode === 'search' && filterActive;

  // Pre-compute the lowercased needle once per render to avoid doing
  // it inside a hot per-row map.
  const filterNeedle = useMemo<string>(
    () => (searchMode === 'filter' && filterActive ? filterText.toLowerCase() : ''),
    [searchMode, filterActive, filterText],
  );

  // ─── Render decisions ──────────────────────────────────────────────
  const showEmpty = count === 0 && pendingSet.size === 0 && rootsCount > 0;
  const showLoading =
    rootsCount === 0 && count === 0 && snapshot.treeVersion === 0;

  if (showLoading && loadingState !== undefined) {
    return (
      <div
        role="tree"
        aria-label={ariaLabel}
        aria-multiselectable={multiSelect}
        aria-busy="true"
        className={className ? `mille-tree ${className}` : 'mille-tree'}
        data-mille-tree-state="loading"
        style={{ height: '100%', overflow: 'auto', ...style }}
      >
        {loadingState}
      </div>
    );
  }

  if (showEmpty && emptyState !== undefined) {
    return (
      <div
        role="tree"
        aria-label={ariaLabel}
        aria-multiselectable={multiSelect}
        className={className ? `mille-tree ${className}` : 'mille-tree'}
        data-mille-tree-state="empty"
        style={{ height: '100%', overflow: 'auto', ...style }}
      >
        {emptyState}
      </div>
    );
  }

  const Row = rowRenderer ?? FileTreeRow;

  const scrollerStyle: CSSProperties = {
    height: '100%',
    overflow: 'auto',
    position: 'relative',
    ...(showFilter ? { height: 'calc(100% - 32px)' } : null),
    ...style,
  };

  const innerStyle: CSSProperties = {
    height: `${totalSize}px`,
    position: 'relative',
    width: '100%',
  };

  // ─── Phase 8 — render the embedded filter + (tree | search list) ───
  const embeddedFilterNode = showFilter ? (
    <FileTreeFilter
      ref={embeddedFilterInputRef}
      mode={searchMode === 'off' ? 'filter' : searchMode}
      {...(controlledFilter !== undefined ? { value: filterText } : {})}
      onChange={(next) => {
        filterState.setFilter(next);
        if (onFilterChange) onFilterChange(next);
      }}
      onModeChange={(next) => {
        setSearchMode(next);
      }}
    />
  ) : null;

  // Search mode: swap the tree DOM for the virtualized result list.
  if (inSearchMode) {
    const searchContent = (
      <SearchResultList
        fx={fx as unknown as SearchableEngine}
        query={filterText}
        {...(onOpen
          ? {
              onOpen: (entryId, event) => {
                const entry = snapshot.getById(entryId);
                if (entry) {
                  // `onOpen` receives a `VisibleRow`-compatible object.
                  onOpen(
                    entry as unknown as Parameters<typeof onOpen>[0],
                    event,
                  );
                }
              },
            }
          : null)}
        {...(__testSearchDebounceMs !== undefined
          ? { debounceMs: __testSearchDebounceMs }
          : null)}
        {...(__testObserveElementRect
          ? { __testObserveElementRect }
          : null)}
        {...(__testObserveElementOffset
          ? { __testObserveElementOffset }
          : null)}
      />
    );
    const outerStyle: CSSProperties = {
      height: '100%',
      display: 'flex',
      flexDirection: 'column',
      minHeight: 0,
      ...style,
    };
    return (
      <div
        className={className ? `mille-tree ${className}` : 'mille-tree'}
        data-mille-tree-state="search"
        style={outerStyle}
        onKeyDown={onKeyDown}
      >
        {embeddedFilterNode}
        <div style={{ flex: '1 1 auto', minHeight: 0 }}>{searchContent}</div>
      </div>
    );
  }

  const treeScrollerNode = (
    <div
      ref={scrollerRef}
      role="tree"
      aria-label={ariaLabel}
      aria-multiselectable={multiSelect}
      className={
        showFilter
          ? 'mille-tree'
          : className
            ? `mille-tree ${className}`
            : 'mille-tree'
      }
      data-mille-tree-state="ready"
      data-mille-layout-animating={displayedLayoutAnimation.active ? 'true' : undefined}
      data-mille-animation-suppressed={animationSuppression}
      data-mille-filter-active={filterActive && searchMode === 'filter' ? 'true' : undefined}
      style={scrollerStyle}
      onKeyDown={onKeyDown}
      onFocus={onContainerFocus}
    >
      <div style={innerStyle}>
        {virtualItems.map((vItem) => {
          const row = rowAtIndex(vItem.index);
          if (!row) return null;

          const rowExpanded = expanded.has(row.id);
          const pending = pendingSet.has(row.id) || row.pending === true;
          const depth = row.depth;
          const isStickyRoot = stickyRoots && depth === 0;
          const isSelected = selection.selectedIds.has(row.id);
          const isFocused = selection.focusedId === row.id;
          const isActive = resolvedActiveEntryId === row.id;

          const aria: AriaRowProps = {
            'aria-level': depth + 1,
            'aria-setsize': count,
            'aria-posinset': vItem.index + 1,
            'aria-selected': isSelected,
            ...(row.hasChildren ? { 'aria-expanded': rowExpanded } : null),
          };

          const rowStyle: CSSProperties = {
            position: 'absolute',
            top: 0,
            left: 0,
            right: 0,
            height: `${rowHeight}px`,
            transform: `translateY(${vItem.start}px)`,
          };

          // Phase 10: compute merged decoration per row. `getDecorations`
          // returns an identity-stable `readonly Decoration[]` per
          // snapshot (engine SPEC §9.4), and `mergeDecorations` folds
          // that into an identity-stable `MergedDecoration` via a
          // WeakMap cache — so decoration-only snapshot bumps only
          // disturb rows whose raw array reference actually changed.
          const rowDecorations = mergeDecorations(
            snapshot.getDecorations(row.id),
          );

          const isCut = clipboard.cutIds.has(row.id);
          // Phase 8 — `'filter'` mode: hide rows whose name doesn't
          // contain the case-insensitive needle. `display:none` chosen
          // for v0.1 simplicity (virtualizer count stays stable); see
          // MILLE_UI_PLAN.md §Phase 8 for the trade-off discussion.
          const isHidden =
            filterNeedle.length > 0
              ? !row.name.toLowerCase().includes(filterNeedle)
              : false;

          const rowProps: FileTreeRowProps = {
            row,
            ...(depth === 0
              ? { displayName: rootDisplayNames.get(row.id) ?? row.name }
              : null),
            depth,
            selected: isSelected,
            focused: isFocused,
            active: isActive,
            expanded: rowExpanded,
            hasChildren: row.hasChildren,
            pending,
            decorations: rowDecorations,
            ...(iconTheme ? { iconTheme } : null),
            style: rowStyle,
            ariaProps: aria,
            isStickyRoot,
            entering: displayedLayoutAnimation.enteringIds.has(row.id),
            repositioning: displayedLayoutAnimation.repositioningIds.has(row.id),
            cut: isCut,
            hidden: isHidden,
            renameTargetId: renameState.renameTargetId,
            onRenameCommit: (newName: string) => {
              const result = renameState.commit(newName);
              if (result && typeof result.catch === 'function') {
                result.catch(() => {
                  /* error is tracked in `lastError`; no need to re-throw */
                });
              }
            },
            onRenameCancel: () => renameState.cancel(),
            validateRename: (newName: string) => renameState.validate(newName),
            renameError:
              renameState.renameTargetId === row.id && renameState.lastError
                ? renameState.lastError.message
                : null,
            renameErrorRevision:
              renameState.renameTargetId === row.id
                ? renameState.errorRevision
                : 0,
            onExpand: () => {
              if (!rowExpanded) toggle(row.id);
            },
            onCollapse: () => {
              if (rowExpanded) toggle(row.id);
            },
            onChevronClick: (e: ReactMouseEvent<HTMLElement>) => {
              e.stopPropagation();
              onChevronClick(row.id, rowExpanded);
            },
            onClick: (e: ReactMouseEvent<HTMLElement>) => {
              const withMod =
                multiSelect && (e.shiftKey || e.metaKey || e.ctrlKey);
              if (multiSelect && e.shiftKey) {
                const anchor = selection.anchorId ?? selection.focusedId ?? row.id;
                const anchorIndex = projection.findRowIndex(
                  anchor,
                  mountedViewportOffset,
                );
                const targetIndex = projection.findRowIndex(
                  row.id,
                  mountedViewportOffset,
                );
                if (anchorIndex !== -1 && targetIndex !== -1) {
                  const start = Math.min(anchorIndex, targetIndex);
                  const length = Math.abs(targetIndex - anchorIndex) + 1;
                  selection.selectRange(
                    anchor,
                    row.id,
                    projection.readRows(start, length).map((candidate) => candidate.id),
                  );
                } else {
                  selection.selectRange(
                    anchor,
                    row.id,
                    readAllVisibleRows().map((candidate) => candidate.id),
                  );
                }
              } else if (multiSelect && (e.metaKey || e.ctrlKey)) {
                selection.toggle(row.id);
              } else {
                selection.selectOne(row.id);
              }
              // Single-click expands/collapses folders (entire row, not
              // just the chevron). Matches Finder / the archival design.
              // `detail === 1` skips the second click of a double-click
              // so a folder is not toggled twice and left closed.
              if (
                row.hasChildren &&
                !withMod &&
                e.detail === 1
              ) {
                onChevronClick(row.id, rowExpanded);
              } else if (
                !row.hasChildren &&
                !withMod &&
                e.detail === 1 &&
                openBehavior?.singleClick === 'preview' &&
                onOpen
              ) {
                onOpen(row, {
                  mode: 'preview',
                  source: 'singleClick',
                });
              }
            },
            onDoubleClick: () => {
              // Folders already toggle on single-click; double-click only
              // opens leaf entries (files).
              if (!row.hasChildren && onOpen) {
                onOpen(row, {
                  mode: openBehavior?.doubleClick ?? 'permanent',
                  source: 'doubleClick',
                });
              }
            },
            onContextMenu: () => {
              // Phase 6: per SPEC §6.2, replace selection with target
              // if target is not already in selection. Then let Radix's
              // per-row <ContextMenu.Root> open the menu at cursor.
              if (!selection.selectedIds.has(row.id)) {
                selection.selectOne(row.id);
              } else {
                selection.setFocused(row.id);
              }
            },
            onFocus: () => onRowFocus(row.id),
            disableContextMenu,
            ...(contextMenuContent !== null && contextMenuContent !== undefined
              ? { contextMenuContent }
              : null),
            registerRowElement: contextMenuState.registerRowElement,
            // Phase 11 — drag-and-drop wiring. Each row receives closures
            // that capture its own `row.id`. Row-level gating happens
            // inside `FileTreeRow` via `disableDragDrop`. When the tree's
            // `disableDragDrop` prop is `true`, we skip attaching the
            // handlers entirely so the row's `draggable` attr never
            // renders and the DnD listeners never install.
            disableDragDrop,
            dragging: dnd.state.draggingIds.has(row.id),
            dropTargetPosition:
              dnd.state.dropTargetId === row.id ? dnd.state.dropPosition : null,
            ...(disableDragDrop
              ? null
              : {
                  onDragStart: (e) => dnd.handlers.onDragStart(row.id, e),
                  onDragEnter: (e) => dnd.handlers.onDragEnter(row.id, e),
                  onDragOver: (e) => dnd.handlers.onDragOver(row.id, e),
                  onDragLeave: (e) => dnd.handlers.onDragLeave(row.id, e),
                  onDrop: (e) => {
                    const result = dnd.handlers.onDrop(row.id, e);
                    // Keep the promise rejection observable for hosts that
                    // await row handlers; also forward via onDropError when set.
                    if (result && typeof result.catch === 'function') {
                      result.catch((error: unknown) => {
                        if (typeof dragDrop?.onDropError === 'function') {
                          try {
                            dragDrop.onDropError(error);
                          } catch {
                            /* host handler must not break React */
                          }
                        }
                      });
                    }
                  },
                  onDragEnd: (e) => dnd.handlers.onDragEnd(e),
                }),
          };

          return <Row key={row.id} {...rowProps} />;
        })}
      </div>
    </div>
  );

  if (showFilter) {
    const outerStyle: CSSProperties = {
      height: '100%',
      display: 'flex',
      flexDirection: 'column',
      minHeight: 0,
      ...style,
    };
    return (
      <div
        className={className ? `mille-tree-container ${className}` : 'mille-tree-container'}
        data-mille-tree-container=""
        style={outerStyle}
      >
        {embeddedFilterNode}
        <div style={{ flex: '1 1 auto', minHeight: 0, overflow: 'hidden' }}>
          {treeScrollerNode}
        </div>
      </div>
    );
  }

  return treeScrollerNode;
}

// Re-export helpers are handled via components/index.js.
export type { FileTreeProps, FileTreeRowProps };
export type FileTreeEmptyStateProps = { readonly children?: ReactNode };
