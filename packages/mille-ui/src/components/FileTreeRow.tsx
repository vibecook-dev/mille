// FileTreeRow — the default row renderer.
//
// Phase B8 (v0.2): this component is now a **thin view** on top of the
// `useFileTreeRow` logic hook. The hook owns ARIA + event wiring +
// rename / DnD / decoration state; the view spreads the returned prop
// bundles onto its DOM nodes and sub-components.
//
// Structure per MILLE_UI_SPEC.md §4.3:
//
//   <div role="treeitem" aria-level aria-selected aria-expanded>
//     <IndentGuides depth={depth} />
//     <DisclosureChevron expanded hasChildren />
//     <FileIcon entry={entry} expanded={expanded} />
//     <span className="mille-row-name">{displayName}</span>
//     {pending && <LoadingBadge />}
//     <FileDecorations decorations={decorations} />
//   </div>
//
// Wrapped in `React.memo` keyed on identifying props; re-renders only
// when ids / versions / flags change.

import { memo, type CSSProperties, type ReactElement, type ReactNode } from 'react';
import type { VisibleRow } from '@vibecook/mille';
import * as ContextMenu from '@radix-ui/react-context-menu';
import { FileIcon, defaultIconTheme } from '../icons/index.js';
import { useFileTreeRow } from '../hooks/useFileTreeRow.js';
import type { FileTreeRowProps } from './types.js';
import { DisclosureChevron } from './DisclosureChevron.js';
import { IndentGuides } from './IndentGuides.js';
import { LoadingBadge } from './LoadingBadge.js';
import { FileRenameInput } from './FileRenameInput.js';
import { FileDecorations } from './FileDecorations.js';

function FileTreeRowImpl(props: FileTreeRowProps): ReactElement {
  const {
    rowProps,
    chevronProps,
    iconProps,
    nameProps,
    renameProps,
    decorationProps,
    pending,
    useContextMenuWrapper,
    depth,
  } = useFileTreeRow(props);

  const iconTheme = props.iconTheme ?? defaultIconTheme;

  // Layout matches JetBrains Project view:
  //   chevron · icon · VCS badge · name · loading
  // (decorations sit before the name, not trailing right).
  const rowNode: ReactNode = (
    <div {...rowProps}>
      <IndentGuides depth={depth} />
      <DisclosureChevron {...chevronProps} />
      <FileIcon {...iconProps} theme={iconTheme} />
      <FileDecorations {...decorationProps} />
      {renameProps !== null ? (
        <FileRenameInput {...renameProps} />
      ) : (
        <span {...nameProps} />
      )}
      {pending ? <LoadingBadge /> : null}
      {props.loadError !== undefined ? (
        <button
          type="button"
          className="mille-load-error-badge"
          aria-label={`Retry loading ${props.displayName ?? props.row.name}`}
          title={`${props.loadError.code}: ${props.loadError.message}`}
          onClick={(event) => {
            event.stopPropagation();
            props.onRetryLoad?.();
          }}
        >
          ↻
        </button>
      ) : null}
    </div>
  );

  // Phase 6: each row gets its own Radix <ContextMenu.Root>. Radix's
  // anchor-at-cursor behavior keys off the Root that captured the real
  // contextmenu event, so a per-row Root matches VS Code / Finder
  // semantics (menu always anchors where the click happened, even when
  // the user switches between rows quickly).
  if (!useContextMenuWrapper) {
    return <>{rowNode}</>;
  }

  const contextMenuContent = props.contextMenuContent;
  return (
    <ContextMenu.Root>
      <ContextMenu.Trigger asChild>{rowNode}</ContextMenu.Trigger>
      {contextMenuContent !== undefined && contextMenuContent !== null
        ? contextMenuContent
        : null}
    </ContextMenu.Root>
  );
}

/**
 * Row memoization.
 *
 * Phase 10 refines this: the comparator checks **data identity** of the
 * props that drive what the row actually renders (row entry, depth,
 * flags, decorations, aria, rename state). Callback props (`onClick`,
 * `onRenameCommit`, etc.) are intentionally NOT compared — the parent
 * tree recreates them as inline closures on every render, but their
 * *behavior* is keyed on stable, externally-owned state (`row`,
 * `selection`, `renameState`). Comparing their identity would defeat
 * the memo on every tree re-render and turn a decoration-only snapshot
 * bump into a 500-row re-render storm (SPEC §12 target: < 4 ms).
 *
 * Snapshot adapters may materialize fresh `VisibleRow` objects on each
 * tree version. Comparing their render-relevant fields keeps an unrelated
 * filesystem change from repainting the entire viewport. Position and
 * ARIA metadata are compared separately so inserts/reorders still move and
 * re-announce every affected row correctly.
 *
 * The `decorations` identity check is the load-bearing Phase 10 piece:
 * `mergeDecorations` returns a stable reference when the underlying
 * `readonly Decoration[]` is unchanged, so decoration-only snapshot
 * bumps only disturb rows whose decorations actually changed.
 */
function arraysEqual(
  a: readonly string[] | undefined,
  b: readonly string[] | undefined,
): boolean {
  if (a === b) return true;
  if (a === undefined || b === undefined || a.length !== b.length) return false;
  for (let index = 0; index < a.length; index += 1) {
    if (a[index] !== b[index]) return false;
  }
  return true;
}

function visibleRowsEqual(a: VisibleRow, b: VisibleRow): boolean {
  return (
    a === b ||
    (a.id === b.id &&
      a.parentId === b.parentId &&
      a.name === b.name &&
      a.kind === b.kind &&
      a.size === b.size &&
      a.mtimeMs === b.mtimeMs &&
      a.ctimeMs === b.ctimeMs &&
      a.symlinkTargetIsDir === b.symlinkTargetIsDir &&
      arraysEqual(a.pathSegments, b.pathSegments) &&
      a.isIgnored === b.isIgnored &&
      a.isReadonly === b.isReadonly &&
      a.isHidden === b.isHidden &&
      a.depth === b.depth &&
      a.hasChildren === b.hasChildren &&
      a.isExpanded === b.isExpanded &&
      a.pending === b.pending)
  );
}

function stylesEqual(a: CSSProperties, b: CSSProperties): boolean {
  if (a === b) return true;
  const aKeys = Object.keys(a) as Array<keyof CSSProperties>;
  const bKeys = Object.keys(b) as Array<keyof CSSProperties>;
  if (aKeys.length !== bKeys.length) return false;
  for (const key of aKeys) {
    if (a[key] !== b[key]) return false;
  }
  return true;
}

/** @internal Exported for comparator regression tests. */
export function areFileTreeRowPropsEqual(
  prev: FileTreeRowProps,
  next: FileTreeRowProps,
): boolean {
  return (
    visibleRowsEqual(prev.row, next.row) &&
    prev.displayName === next.displayName &&
    prev.depth === next.depth &&
    prev.selected === next.selected &&
    prev.focused === next.focused &&
    prev.active === next.active &&
    prev.expanded === next.expanded &&
    prev.hasChildren === next.hasChildren &&
    prev.pending === next.pending &&
    prev.loadError?.code === next.loadError?.code &&
    prev.loadError?.message === next.loadError?.message &&
    prev.onRetryLoad === next.onRetryLoad &&
    prev.iconTheme === next.iconTheme &&
    prev.className === next.className &&
    stylesEqual(prev.style, next.style) &&
    prev.ariaProps['aria-level'] === next.ariaProps['aria-level'] &&
    prev.ariaProps['aria-setsize'] === next.ariaProps['aria-setsize'] &&
    prev.ariaProps['aria-posinset'] === next.ariaProps['aria-posinset'] &&
    prev.ariaProps['aria-selected'] === next.ariaProps['aria-selected'] &&
    prev.ariaProps['aria-expanded'] === next.ariaProps['aria-expanded'] &&
    prev.isStickyRoot === next.isStickyRoot &&
    prev.entering === next.entering &&
    prev.repositioning === next.repositioning &&
    prev.cut === next.cut &&
    prev.hidden === next.hidden &&
    prev.decorations === next.decorations &&
    prev.renameTargetId === next.renameTargetId &&
    prev.renameError === next.renameError &&
    prev.renameErrorRevision === next.renameErrorRevision &&
    prev.disableContextMenu === next.disableContextMenu &&
    prev.contextMenuContent === next.contextMenuContent &&
    // Phase 11 — DnD visual state. Without these, `data-mille-dragging`
    // and `data-mille-drop-target-*` would never reflect changes because
    // the Phase-10 memo would skip re-renders during a drag.
    prev.dragging === next.dragging &&
    prev.dropTargetPosition === next.dropTargetPosition &&
    prev.disableDragDrop === next.disableDragDrop
  );
}

export const FileTreeRow = memo(FileTreeRowImpl, areFileTreeRowPropsEqual);
FileTreeRow.displayName = 'FileTreeRow';
