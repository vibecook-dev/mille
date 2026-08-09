// useVirtualizerForSnapshot — thin wrapper around `@tanstack/react-virtual`
// that adapts FileTree's visible row count to the virtualizer.
//
// Fixed-height mode in v0.1; the `rowHeight` option can be a number (fixed)
// or a function. Measured mode (variable height) lands alongside the
// row-height callback in a later phase.
//
// Returns only the subset we actually use downstream (virtual items, total
// size, scroll helpers) to keep the Phase 3 surface small.

import type { RefObject } from 'react';
import { useVirtualizer, type VirtualItem } from '@tanstack/react-virtual';
import type {
  VirtualizerOffsetObserver,
  VirtualizerRectObserver,
} from '../components/types.js';

export type { VirtualizerOffsetObserver, VirtualizerRectObserver };

export interface UseVirtualizerForSnapshotOptions {
  /** Authoritative total visible row count. */
  readonly count: number;
  readonly rowHeight: number;
  readonly overscan: number;
  readonly scrollerRef: RefObject<HTMLElement | null>;
  /**
   * Optional rect observer override. Production callers leave this
   * `undefined` to use virtual-core's default (ResizeObserver-based
   * reader of `offsetWidth`/`offsetHeight`). Useful for tests under
   * happy-dom where the default path returns zero dimensions.
   */
  readonly observeElementRect?: VirtualizerRectObserver;
  /**
   * Optional offset observer override. Same rationale as
   * `observeElementRect`.
   */
  readonly observeElementOffset?: VirtualizerOffsetObserver;
}

export interface UseVirtualizerForSnapshotResult {
  readonly count: number;
  readonly virtualItems: readonly VirtualItem[];
  readonly totalSize: number;
  /** Physical size of the scroll viewport along the virtualized axis. */
  readonly viewportSize: number;
  readonly scrollOffset: number;
  scrollToIndex(index: number, opts?: { align?: 'start' | 'center' | 'end' | 'auto' }): void;
  measureElement(node: Element | null): void;
}

export function useVirtualizerForSnapshot(
  options: UseVirtualizerForSnapshotOptions,
): UseVirtualizerForSnapshotResult {
  const {
    count,
    rowHeight,
    overscan,
    scrollerRef,
    observeElementRect,
    observeElementOffset,
  } = options;

  const virtualizer = useVirtualizer<HTMLElement, Element>({
    count,
    getScrollElement: () => scrollerRef.current,
    estimateSize: () => rowHeight,
    overscan,
    // React rows retain entry-id keys. The virtualizer itself can use stable
    // logical indexes without forcing a row lookup before its window exists.
    getItemKey: (index: number): number => index,
    ...(observeElementRect ? { observeElementRect } : null),
    ...(observeElementOffset ? { observeElementOffset } : null),
  });

  return {
    count,
    virtualItems: virtualizer.getVirtualItems(),
    totalSize: virtualizer.getTotalSize(),
    viewportSize:
      virtualizer.scrollRect?.height ?? scrollerRef.current?.clientHeight ?? 0,
    scrollOffset: virtualizer.scrollOffset ?? 0,
    scrollToIndex: (index, opts) => {
      virtualizer.scrollToIndex(index, opts);
    },
    measureElement: (node) => {
      virtualizer.measureElement(node);
    },
  };
}
