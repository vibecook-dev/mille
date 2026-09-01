// useSetExpandedBridge — wires a local ExpandedSet to the engine's
// `fx.setExpanded({ add, remove })`.
//
// On every change to `expanded`, the bridge diffs against the
// previously-seen set and forwards the delta to the engine. The call is
// wrapped in `startTransition` so expansion doesn't block the
// virtualizer's commit (React 19 concurrent rendering).
//
// Usage:
//   const { expanded, toggle } = useExpandedSet();
//   useSetExpandedBridge(fx, expanded);

import { useEffect, useRef, useTransition } from 'react';
import type { EntryId } from '@vibecook/mille';

/** Minimum engine surface the bridge actually calls. */
export interface SetExpandedFx {
  setExpanded(diff: {
    readonly add?: readonly EntryId[];
    readonly remove?: readonly EntryId[];
  }): void;
}

export function useSetExpandedBridge(
  fx: SetExpandedFx,
  expanded: ReadonlySet<EntryId>,
): { readonly isPending: boolean } {
  const [isPending, startTransitionImpl] = useTransition();
  const prevRef = useRef<ReadonlySet<EntryId>>(expanded);
  const prevFxRef = useRef<SetExpandedFx | null>(null);
  // Seed the ref on first mount so the initial expansion set is pushed
  // to the engine exactly once. Subsequent diffs run against prevRef.
  const initialisedRef = useRef(false);

  useEffect(() => {
    // `setExpanded` is engine-local. Replacing the engine must replay the
    // complete current set even when both engines use identical numeric ids.
    const engineChanged = prevFxRef.current !== fx;
    const prev =
      initialisedRef.current && !engineChanged ? prevRef.current : new Set<EntryId>();
    initialisedRef.current = true;
    prevFxRef.current = fx;

    const add: EntryId[] = [];
    const remove: EntryId[] = [];
    for (const id of expanded) {
      if (!prev.has(id)) add.push(id);
    }
    for (const id of prev) {
      if (!expanded.has(id)) remove.push(id);
    }
    prevRef.current = expanded;

    if (add.length === 0 && remove.length === 0) return;

    startTransitionImpl(() => {
      const diff: { add?: readonly EntryId[]; remove?: readonly EntryId[] } = {};
      if (add.length > 0) diff.add = add;
      if (remove.length > 0) diff.remove = remove;
      fx.setExpanded(diff);
    });
  }, [fx, expanded, startTransitionImpl]);

  return { isPending };
}
