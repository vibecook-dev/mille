// Phase-3 scoped type surface for the components layer.
//
// These types ARE NOT re-exported from `src/types.ts` — that file is
// owned by Phase 1. Everything component-shaped lives here and is
// re-exported from `src/index.ts` alongside `FileTree` / `FileTreeRow`.
//
// Where later phases (4+ selection, 7 cut/copy, 10 decorations, 11 DnD)
// need to add props, those props declare here as opt-in. For Phase 3
// they're typed but unused.

import type {
  CSSProperties,
  ComponentType,
  DragEvent as ReactDragEvent,
  FocusEvent as ReactFocusEvent,
  KeyboardEvent as ReactKeyboardEvent,
  MouseEvent as ReactMouseEvent,
  ReactNode,
} from 'react';
import type {
  Decoration,
  DirectoryLoadState,
  Entry,
  EntryId,
  MirrorSnapshot,
  VisibleRow,
  VisibleRowCount,
  VisibleRowsOptions,
} from '@vibecook/mille';
import type { IconTheme } from '../icons/types.js';
import type { CommandRegistryHandle } from '../types.js';
import type {
  FileTreeNavigationState,
  FileTreeSearchMode,
} from '../navigation-state.js';
import type { FileOpenBehavior, FileOpenEvent } from '../open-policy.js';
import type {
  ActiveEntryInput,
  ActiveEntryPolicy,
  ActiveEntryResolution,
} from '../active-entry-policy.js';
import type { FileActionTarget } from '../file-actions.js';
import type { FileSearchRequest } from '../file-search.js';
import type { FileRefreshTarget, HostHooks } from '../commands/types.js';

// Observer signatures mirror virtual-core's. Declared here rather
// than in the hook to avoid a component → hook → component cycle.
// Kept minimally typed (no Virtualizer generic) so consumers don't
// have to name @tanstack/virtual-core's type internals.
export type VirtualizerRectObserver = (
  instance: unknown,
  cb: (rect: { width: number; height: number }) => void,
) => (() => void) | undefined | void;

export type VirtualizerOffsetObserver = (
  instance: unknown,
  cb: (offset: number, isScrolling: boolean) => void,
) => (() => void) | undefined | void;

/**
 * Minimum engine surface the tree actually uses. The real
 * `FileExplorer` satisfies this structurally; so does the test fake.
 * Kept separate from `FileTreeFx` (context.tsx) so components can take
 * an engine directly without going through context shape changes.
 */
export interface FileTreeEngine {
  getSnapshot(): FileTreeSnapshotLike;
  /** Optional indexed workspace-relative path resolver. */
  resolvePath?(path: string): Promise<EntryId | null> | EntryId | null;
  /** Optional payload-free full-order fallback for typeahead. */
  findVisiblePrefix?(
    prefix: string,
    fromId: EntryId | null,
    skipCurrent: boolean,
    expanded: ReadonlySet<EntryId>,
  ): Promise<EntryId | null> | EntryId | null;
  on(event: 'change', listener: (n?: unknown) => void): { dispose(): void };
  setExpanded(diff: {
    readonly add?: readonly EntryId[];
    readonly remove?: readonly EntryId[];
  }): void;
  /** Optional host hint describing the currently mounted virtual window. */
  setViewport?(options: {
    readonly offset: number;
    readonly limit: number;
    readonly overscan?: number;
  }): void;
  /** Optional authoritative disk reconciliation used by refresh commands. */
  resync?(
    id: EntryId,
    options?: { readonly recursive?: boolean },
  ): Promise<number>;
  /** Optional complete-workspace disk reconciliation. */
  resyncWorkspace?(): Promise<number>;
}

/**
 * A snapshot-like object. Real `MirrorSnapshot` from the engine
 * conforms; `createFakeSnapshot` in testing also conforms. Typed
 * structurally so the virtualizer hook and FileTree don't demand the
 * engine package's exact class.
 */
export interface FileTreeSnapshotLike {
  readonly treeVersion: number;
  /** Optional cache-hydration dimension; falls back to treeVersion. */
  readonly projectionVersion?: number;
  readonly decorationVersion: number;
  /** Effective visibility settings exposed by production snapshots. */
  readonly showHiddenFiles?: boolean;
  readonly showIgnoredFiles?: boolean;
  roots(): readonly Entry[];
  visibleRows(options: VisibleRowsOptions): readonly VisibleRow[];
  /** Optional ID-only projection for selection and identity consumers. */
  visibleRowIds?(options: VisibleRowsOptions): readonly EntryId[];
  visibleRowCount(
    expanded: ReadonlySet<EntryId>,
    includeIgnored?: boolean,
  ): VisibleRowCount;
  /** Optional exact-position fast path supplied by production snapshots. */
  visibleRowIndex?(
    id: EntryId,
    expanded: ReadonlySet<EntryId>,
    includeIgnored?: boolean,
  ): number | null;
  getById(id: EntryId): Entry | null;
  directChildCount(id: EntryId): number | null;
  hasChildren(id: EntryId): boolean;
  /** Optional progressive hydration state supplied by port snapshots. */
  directoryLoadState?(id: EntryId): DirectoryLoadState;
  getDecorations(id: EntryId): readonly Decoration[];
}

// `MirrorSnapshot` is structurally compatible; keep the reference so
// future type changes land on both.
export type AnyTreeSnapshot = FileTreeSnapshotLike | MirrorSnapshot;

/**
 * Merged decoration overlay for a single row. Phase 10 fold of the
 * engine-published `readonly Decoration[]` into the UI-facing flat
 * record. Identity-stable across renders: `mergeDecorations` caches on
 * the source array reference so unchanged rows keep the same merged
 * object and the row memo skips re-render.
 *
 * - `badge` — 1–2 char overlay on the row (e.g. `'M'`, `'!'`).
 * - `letter` — single-character SCM-style indicator (`'M'`/`'A'`/`'D'`).
 * - `color` — applied to the letter/badge; does NOT tint the row name.
 * - `tooltip` — concatenated across providers (newline-joined).
 * - `fontWeight` — 0 ⇒ the row is "ignored" (e.g. .gitignored), dim it.
 * - `strikethrough` — strike the row name (e.g. deleted files).
 * - `propagateToParent` — engine-side hint; ancestor folders should pick
 *   up a muted version of this decoration.
 */
export interface MergedDecoration {
  readonly badge?: string;
  readonly color?: string;
  readonly tooltip?: string;
  readonly fontWeight?: number;
  readonly strikethrough?: boolean;
  readonly letter?: string;
  readonly propagateToParent?: boolean;
}

export interface AriaRowProps {
  readonly 'aria-level': number;
  readonly 'aria-setsize'?: number;
  readonly 'aria-posinset'?: number;
  readonly 'aria-selected'?: boolean;
  readonly 'aria-expanded'?: boolean;
}

/** Context supplied when deriving a presentation-only workspace root label. */
export interface RootLabelContext {
  /** Position in the snapshot's current ordered root list. */
  readonly index: number;
  /** Zero-based position among roots with the same filesystem basename. */
  readonly duplicateIndex: number;
  /** Number of current roots with the same filesystem basename. */
  readonly duplicateCount: number;
}

/**
 * Props passed to the row renderer. Phase 4 will add more focus /
 * selection / keyboard handlers; Phase 10 will populate decorations.
 */
export interface FileTreeRowProps {
  readonly row: VisibleRow;
  /** Presentation-only name override; commands continue using `row.name`. */
  readonly displayName?: string;
  readonly depth: number;
  readonly selected: boolean;
  readonly focused: boolean;
  /** This entry is active in the host editor, independently of tree focus. */
  readonly active?: boolean;
  readonly expanded: boolean;
  readonly hasChildren: boolean;
  readonly pending: boolean;
  readonly loadError?: { readonly code: string; readonly message: string };
  readonly onRetryLoad?: () => void;
  readonly decorations: MergedDecoration;
  readonly iconTheme?: IconTheme;
  readonly className?: string;
  readonly style: CSSProperties;
  readonly ariaProps: AriaRowProps;
  readonly isStickyRoot?: boolean;
  /** Internal file-change enter animation marker. */
  readonly entering?: boolean;
  /** Internal marker for a mounted row whose virtual offset changed. */
  readonly repositioning?: boolean;
  /**
   * Phase 7 — whether this row's id is in the clipboard `cutIds` set.
   * Rendered as `data-mille-cut="true"`; the default CSS dims the row
   * to ~50% opacity until paste or Esc clears the clipboard.
   */
  readonly cut?: boolean;
  /**
   * Phase 8 — the filter state (`searchMode === 'filter'` + non-empty
   * query) has hidden this row. Default renderer applies
   * `display: none` to the row element so the tree layout collapses.
   * ARIA consumers should still consider this row as non-existent
   * (it's not in the roving tabindex either).
   */
  readonly hidden?: boolean;

  onExpand(): void;
  onCollapse(): void;
  onChevronClick(e: ReactMouseEvent<HTMLElement>): void;
  onClick(e: ReactMouseEvent<HTMLElement>): void;
  onDoubleClick(e: ReactMouseEvent<HTMLElement>): void;
  onContextMenu(e: ReactMouseEvent<HTMLElement>): void;
  onKeyDown?(e: ReactKeyboardEvent<HTMLElement>): void;
  /** Phase 4: attached on the row element so the container learns about
   * focus changes (roving tabindex, focus restoration). */
  onFocus?(e: ReactFocusEvent<HTMLElement>): void;

  // ─── Phase 5 — inline rename ─────────────────────────────────────
  /**
   * If equal to `row.id`, render a `<FileRenameInput>` in place of the
   * row's name span.
   */
  readonly renameTargetId?: EntryId | null;
  /** Commit a new name for the currently-renaming row. */
  onRenameCommit?(newName: string): void;
  /** Cancel the inline rename; hide the input, keep the row. */
  onRenameCancel?(): void;
  /**
   * Local validator passed through to the rename input. Returns a
   * human-readable message on failure, `null` on success.
   */
  validateRename?(newName: string): string | null;
  /**
   * Engine-supplied error to surface as a tooltip (e.g. from a
   * `FileSystemError` on a rejected `fx.rename`). Cleared by the input
   * on next keystroke.
   */
  readonly renameError?: string | null;
  /** Increments for repeated rename failures with identical messages. */
  readonly renameErrorRevision?: number;

  // ─── Phase 6 — context menu ──────────────────────────────────────
  /**
   * If `true`, the row does NOT wrap itself in a Radix context-menu
   * Root + Trigger. Right-click events fall through to the parent,
   * matching the pre-Phase-6 behavior. Opt-out is row-level so hosts
   * can selectively disable (e.g. on a system-managed root).
   */
  readonly disableContextMenu?: boolean;
  /**
   * Content rendered inside the Radix `<ContextMenu.Root>` once the
   * row's right-click fires. Typically `<FileContextMenu ... />`.
   * When `undefined`, the row still wraps in a Root+Trigger (so
   * keyboard Shift+F10 can dispatch), but no menu content appears.
   */
  readonly contextMenuContent?: ReactNode;
  /**
   * Called with the row's DOM node when it mounts; called with `null`
   * when it unmounts. Used by the tree so the keyboard handler can
   * synthesize a `contextmenu` event on the focused row.
   */
  readonly registerRowElement?: (id: EntryId, el: HTMLElement | null) => void;

  // ─── Phase 11 — drag-and-drop ────────────────────────────────────
  /**
   * Opt-out: when `true`, the row is not rendered as `draggable` and
   * does not wire any DnD handlers. Defaults to `false` — every row
   * becomes a draggable source and a potential drop target unless the
   * tree-level `disableDragDrop` prop is also set.
   */
  readonly disableDragDrop?: boolean;
  /** `true` while this row's id is in the tree's `draggingIds` set. */
  readonly dragging?: boolean;
  /**
   * Drop-target state for this row — `null` when the row is not the
   * current drop target. Drives the `data-mille-drop-target-*` attrs.
   */
  readonly dropTargetPosition?: DropPosition | null;

  onDragStart?(e: ReactDragEvent<HTMLElement>): void;
  onDragEnter?(e: ReactDragEvent<HTMLElement>): void;
  onDragOver?(e: ReactDragEvent<HTMLElement>): void;
  onDragLeave?(e: ReactDragEvent<HTMLElement>): void;
  onDrop?(e: ReactDragEvent<HTMLElement>): void;
  onDragEnd?(e: ReactDragEvent<HTMLElement>): void;
}

/**
 * v0.2 B6 — Imperative tree handle exposed via `React.forwardRef`.
 *
 * `<FileTree>` is uncontrolled by default — selection, filter, clipboard,
 * and expansion all live in-tree. Hosts that need to drive the tree
 * programmatically (command palettes, "Reset" buttons, keyboard
 * shortcuts that span beyond the tree) attach a ref and call these
 * methods. Controlled-prop callers keep working unchanged.
 *
 * See MILLE_UI_SPEC.md §3.5.
 */
export interface FileTreeRef {
  /**
   * Expand ancestors + scroll to + focus the entry at the given
   * workspace-relative POSIX path. No-op if the path doesn't
   * resolve. Returns `true` on success.
   *
   * Resolution strategy:
   *   1. If the engine exposes `resolvePath`, use its authoritative path index.
   *   2. Otherwise try the legacy `getByUri` contract.
   *   3. Otherwise walk the snapshot's children level-by-level,
   *      matching each path segment against `entry.name`.
   *   4. If none resolves, returns `false`.
   */
  revealPath(path: string): Promise<boolean>;

  /** Same as `revealPath` but by `EntryId` (skips the path→id lookup). */
  revealId(id: EntryId): boolean;

  /**
   * Scroll the virtualizer so the row at `index` is at the top. When the
   * requested index is outside the current visible-row list, the call
   * is a no-op.
   */
  scrollToRow(index: number): void;

  /** Clear multi-select state; leaves focus alone. */
  clearSelection(): void;

  /** Reset filter text to the empty string. */
  clearFilter(): void;

  /** Drop cut/copy markers. */
  clearClipboard(): void;

  /** Collapse every currently expanded folder, including workspace roots. */
  collapseAll(): void;

  /** Collapse expanded descendants while preserving the target folder itself. */
  collapseDescendants(id: EntryId): void;

  /**
   * Expand multiple known folders in one state transition. Useful for hosts
   * restoring persisted expansion state and deterministic benchmark harnesses.
   */
  expand(ids: readonly EntryId[]): void;

  /** Capture bounded, versioned, path-based state suitable for persistence. */
  captureNavigationState(): FileTreeNavigationState;

  /**
   * Restore path-based state through the engine's lazy resolver. Missing paths
   * are skipped and reported instead of invalidating the entire restore.
   */
  restoreNavigationState(
    state: FileTreeNavigationState | string,
  ): Promise<FileTreeNavigationRestoreResult>;

  /**
   * Focus the filter input. Returns `true` if a filter input was found
   * (the caller-supplied `filterInputRef` or the tree's embedded
   * filter when `showFilter` is on), `false` otherwise.
   */
  focusFilter(): boolean;

  /**
   * Convenience — clears selection + filter + clipboard + returns
   * keyboard focus to the tree container.
   */
  reset(): void;
}

export interface FileTreeNavigationRestoreResult {
  readonly expanded: number;
  readonly selected: number;
  readonly focused: boolean;
  readonly scrollAnchored: boolean;
  readonly missingPaths: readonly string[];
}

/**
 * Effect applied to a successful drop. `'move'` is the default;
 * `'copy'` fires when the user holds Alt/Option.
 */
export type DragDropEffect = 'move' | 'copy';

/** Where the pointer landed inside a row. */
export type DropPosition = 'self' | 'above' | 'below';

/** Validation context handed to the consumer's `onDropValidate`. */
export interface DragDropValidateContext {
  readonly sourceIds: readonly EntryId[];
  readonly targetParentId: EntryId | null;
  readonly effect: DragDropEffect;
}

/** Return shape for the consumer's `onDropValidate` override. */
export interface DragDropValidationResult {
  readonly ok: boolean;
  readonly warning?: string;
  readonly requireConfirm?: boolean;
}

/** Collision policy accepted by transfer mutations and DnD. */
export type CollisionPolicy = 'error' | 'rename' | 'overwrite' | 'skip' | 'merge';

/** Host prompt payload when a drop target already exists. */
export interface CollisionPromptRequest {
  readonly sourcePath?: string;
  readonly sourceId?: number;
  readonly targetParentId: number;
  readonly desiredName: string;
  readonly remaining: number;
  /** Only present when a real collision was probed. */
  readonly status: 'exists' | 'case_conflict';
  readonly existingName?: string;
}

export type CollisionPromptResult =
  | CollisionPolicy
  | { readonly policy: CollisionPolicy; readonly applyToAll?: boolean };

/**
 * Options for the drag-drop subsystem. Phase 11 full surface:
 *   - `internal` / `externalOut` / `externalIn` gate each drag flavor.
 *   - `crossRoot` disables cross-workspace-root drops by default.
 *   - `onDropValidate` is a pre-drop veto hook (runs inside dragover).
 *   - `onConfirm` drives gitignore / warning confirmations.
 *   - `onCollision` prompts overwrite/rename/skip/merge with apply-to-all.
 *
 * `external` is preserved for backwards-compat with the Phase-3
 * placeholder: when present it overrides `externalIn`.
 */
export interface DragDropOptions {
  readonly internal?: boolean;
  readonly externalOut?: boolean;
  readonly externalIn?: DragDropEffect | false;
  /** @deprecated use `externalIn`. Retained for Phase-3 compatibility. */
  readonly external?: DragDropEffect | false;
  readonly crossRoot?: boolean;
  /**
   * Destination collision behavior for internal move/copy and external import.
   * Default: `error`. Hosts that prompt can override per item via `onCollision`.
   */
  readonly collision?: CollisionPolicy;
  readonly onDropValidate?: (
    ctx: DragDropValidateContext,
  ) => DragDropValidationResult;
  readonly onConfirm?: (message: string) => boolean | Promise<boolean>;
  /**
   * Per-item collision prompt. Return a policy, or `{ policy, applyToAll }`
   * to reuse it for the remaining batch items.
   */
  readonly onCollision?: (
    request: CollisionPromptRequest,
  ) => CollisionPromptResult | Promise<CollisionPromptResult>;
  /** Surfaces drop/import failures that would otherwise only reject the promise. */
  readonly onDropError?: (error: unknown) => void;
  readonly autoExpandDelayMs?: number;
}

/**
 * `<FileTree>` public props. Every prop beyond the v0.1 Phase-3 subset
 * is marked DEFERRED in a trailing comment so later phases know where
 * to wire them.
 */
export interface FileTreeProps {
  // ─── Phase-3 props — load-bearing ────────────────────────────────
  readonly fx?: FileTreeEngine;
  readonly rowHeight?: number;
  readonly overscan?: number;
  readonly iconTheme?: IconTheme;
  readonly emptyState?: ReactNode;
  readonly loadingState?: ReactNode;
  readonly ariaLabel: string;
  readonly className?: string;
  readonly style?: CSSProperties;
  readonly rowRenderer?: ComponentType<FileTreeRowProps>;
  readonly stickyRoots?: boolean;
  /**
   * Derive a presentation-only label for each workspace root. The callback
   * is re-evaluated against the current root order. Entry names, filesystem
   * paths, command targets, and persisted navigation paths are unchanged.
   *
   * By default, duplicate basenames render as `workspace (1)`,
   * `workspace (2)`, while unique roots retain their basename.
   */
  readonly rootLabel?: (root: Entry, context: RootLabelContext) => string;
  /** One-time path-based state restored lazily after the first root arrives. */
  readonly initialNavigationState?: FileTreeNavigationState | string | null;
  /** Debounced observer for hosts that persist state to workspace storage. */
  readonly onNavigationStateChange?: (state: FileTreeNavigationState) => void;
  /** Persistence observer debounce. Default 150 ms. */
  readonly navigationStateDebounceMs?: number;

  // ─── Phase 3.4 — active editor following ────────────────────────
  /**
   * Entry currently active in the host editor. Pass a known `EntryId` for
   * direct updates or a workspace-relative/root-qualified path when the
   * target may need lazy path-chain hydration.
   */
  readonly activeEntry?: ActiveEntryInput | null;
  /**
   * Expand and scroll to `activeEntry` whenever that target changes.
   * Defaults to `false`. The reveal does not change tree focus or selection,
   * and unrelated renders do not repeat it after the user navigates away.
   */
  readonly autoRevealActiveEntry?: boolean;
  /**
   * Auto-reveal exceptions. Hidden, ignored/excluded, and generated targets
   * are suppressed by default; external and missing targets never reveal.
   */
  readonly activeEntryPolicy?: ActiveEntryPolicy;
  /** Reports the disposition and auto-reveal decision for each new target. */
  readonly onActiveEntryResolution?: (result: ActiveEntryResolution) => void;

  /**
   * Mouse open policy. Defaults to selection-only single click and permanent
   * double-click. Keyboard, search, and command opens are permanent.
   */
  readonly openBehavior?: FileOpenBehavior;

  // ─── DEFERRED (Phase 4+): selection, focus, keyboard ─────────────
  readonly focusedId?: EntryId | null;
  readonly onFocusedIdChange?: (id: EntryId | null) => void;
  readonly selectedIds?: ReadonlySet<EntryId>;
  readonly onSelectionChange?: (ids: ReadonlySet<EntryId>) => void;
  readonly multiSelect?: boolean;

  // ─── DEFERRED (Phase 5): rename ──────────────────────────────────
  readonly renameTargetId?: EntryId | null;
  readonly onRenameTargetIdChange?: (id: EntryId | null) => void;

  // ─── Phase 7: in-app clipboard (cut/copy/paste) ──────────────────
  /**
   * Observer for clipboard state changes. Fires on every cut/copy/clear
   * with the current sets; convenient for hosts that mirror the
   * clipboard into their own state (e.g. for a toolbar indicator).
   * Omitted `onClipboardChange` means the tree owns the state opaquely.
   */
  readonly onClipboardChange?: (clipboard: {
    readonly cutIds: ReadonlySet<EntryId>;
    readonly copyIds: ReadonlySet<EntryId>;
  }) => void;

  // ─── Phase 6: context menu ───────────────────────────────────────
  /**
   * If provided, replaces the built-in `<FileContextMenu>` content.
   * Supply a `<ContextMenu.Content>`-compatible subtree (or another
   * React node that hosts one). Useful when the host wants total
   * control over the menu layout while still leveraging the tree's
   * right-click / Shift+F10 plumbing.
   */
  readonly contextMenuSlot?: ReactNode;
  /**
   * If `true`, the tree does NOT install Radix context-menu wrappers on
   * any row. Right-click on a row bubbles up to the document so hosts
   * can show their own menu (or none). Keyboard Shift+F10 is a no-op in
   * this mode.
   */
  readonly disableContextMenu?: boolean;
  /**
   * Extra items appended inside the built-in `<FileContextMenu>` after
   * the built-in groups. Ignored when `contextMenuSlot` is set.
   */
  readonly contextMenuExtraItems?: ReactNode;
  /**
   * Phase 5.3 — merged into `CommandContext.host` for context-menu /
   * command dispatch. Use for SCM/history hooks (`scm`, `history`,
   * `onCompareResult`, …) without forking the tree.
   */
  readonly hostHooks?: HostHooks & Record<string, unknown>;
  /**
   * Shown when no command matches the current `when` context. Ignored
   * when `contextMenuSlot` is set.
   */
  readonly contextMenuEmptyPlaceholder?: ReactNode;

  // ─── DEFERRED (Phase 2/9): command registry override ─────────────
  readonly commands?: CommandRegistryHandle;

  // ─── Phase 8: filter / search ────────────────────────────────────
  readonly filter?: string;
  readonly onFilterChange?: (next: string) => void;
  /**
   * Phase 8 — tri-state filter behavior:
   *   - `'off'`     — filter input is ignored; tree renders everything.
   *   - `'filter'`  — client-side substring match on `row.name`
   *                   (non-matches hidden via `display:none`).
   *   - `'search'`  — replaces the tree with a virtualized ranked list
   *                   backed by `fx.search(filter)`. Enter opens the
   *                   selected hit via `onOpen`.
   * Default `'off'` for backward compatibility.
   */
  readonly searchMode?: FileTreeSearchMode;
  readonly onSearchModeChange?: (next: FileTreeSearchMode) => void;
  /**
   * Phase 8 — forwarded to the `Mod+F` handler. When set, pressing
   * `Mod+F` while focus is in the tree focuses the filter input; when
   * unset, `Mod+F` still dispatches through the command registry as
   * `tree.focusFilter` (the host owns the side effect).
   */
  readonly filterInputRef?: { readonly current: { focus(): void } | null };
  /**
   * Phase 8 — embed a `<FileTreeFilter>` inside the tree's own layout
   * (above the virtual scroller). Convenient for single-tree apps that
   * don't want to plumb a separate input. When rendered, `Mod+F`
   * focuses this embedded input even without `filterInputRef`.
   */
  readonly showFilter?: boolean;
  /**
   * Phase 8 — debounce override for search (ms). Tests only. Ignored
   * in production builds.
   */
  readonly __testSearchDebounceMs?: number;

  // ─── Phase 11 — drag-drop ────────────────────────────────────────
  readonly dragDrop?: DragDropOptions;
  /**
   * Master switch: when `true`, no row in the tree is draggable and no
   * row acts as a drop target. Equivalent to passing
   * `disableDragDrop: true` on every row. Defaults to `false`.
   */
  readonly disableDragDrop?: boolean;

  // ─── Open / reveal callbacks ─────────────────────────────────────
  /**
   * Called when the tree requests that the host open a file. The event
   * distinguishes preview from permanent opens and identifies the source.
   */
  readonly onOpen?: (entry: Entry, event: FileOpenEvent) => void;
  readonly onRevealInEditor?: (entry: Entry) => void;
  readonly onCopyPath?: (
    target: FileActionTarget,
    kind: 'absolute' | 'relative',
  ) => void | Promise<void>;
  readonly onRevealInFileManager?: (target: FileActionTarget) => void | Promise<void>;
  readonly onOpenContainingFolder?: (target: FileActionTarget) => void | Promise<void>;
  readonly onOpenTerminal?: (target: FileActionTarget) => void | Promise<void>;
  readonly onRefresh?: (target: FileRefreshTarget) => void | Promise<void>;
  readonly onSearchScope?: (request: FileSearchRequest) => void | Promise<void>;

  // ─── Phase 10: decoration pipeline ───────────────────────────────
  /**
   * Provider id merge priority (left → right; later wins on field
   * conflict). Reserved for future per-provider merge override — the
   * engine already merges decorations per SPEC §9.4, so supplying this
   * today is a no-op. Declared now so hosts can forward-declare; the
   * UI-side fallback lands when `registerDecorationProvider` lands a
   * per-provider ungrouped surface on the snapshot.
   */
  readonly decorationsOrder?: readonly string[];

  // ─── Testing hooks (not part of the stable API surface) ──────────
  /**
   * Testing-only: override `@tanstack/react-virtual`'s default
   * `observeElementRect`. happy-dom doesn't drive the default path
   * (ResizeObserver + `offsetWidth`/`offsetHeight`); tests supply a
   * synthetic observer. Do not use outside tests.
   */
  readonly __testObserveElementRect?: VirtualizerRectObserver;
  /** Testing-only counterpart for scroll offset. See above. */
  readonly __testObserveElementOffset?: VirtualizerOffsetObserver;
  /** Testing-only structural projection materialization observer. */
  readonly __testOnProjectionMaterialized?: (rowCount: number) => void;
}
