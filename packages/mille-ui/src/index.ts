// Top-level entry for `@vibecook/mille-ui`.
//
// This entry ships the full styled component surface (mille-tree CSS
// class hooks + layout-critical inline styles + CSS-variable-gated
// colors). For consumers who want to restyle everything themselves,
// the sibling `@vibecook/mille-ui/headless` entry re-exports the same
// structural components and hooks alongside a `milleClassNames` catalog
// — see `./headless/index.ts`.

export const VERSION = '0.3.4'; // x-release-please-version

// Phase 1 — provider, context, snapshot hooks.
export { FileTreeProvider } from './provider.js';
export type { FileTreeProviderProps } from './provider.js';
export { FileTreeContext } from './context.js';
export type { FileTreeContextValue, FileTreeFx } from './context.js';
export {
  useFileTreeContext,
  useFileTreeSnapshot,
  useVirtualizerForSnapshot,
  useExpandedSet,
  useSetExpandedBridge,
  useFileTreeSelection,
  useFileTreeKeyboard,
  useTypeahead,
  useControlledState,
  useRenameState,
  useContextMenuState,
  useFileDecorations,
  mergeDecorations,
  EMPTY_DECORATION,
  useClipboardState,
  useFilterState,
  useSearchResults,
  useFileTreeDragDrop,
  useAutoExpandDwell,
  useFileTreeRef,
  MIME_URI_LIST,
  MIME_MILLE_ENTRIES,
  MIME_CLAUDE_ATTACHMENT,
  MIME_TEXT_PLAIN,
} from './hooks/index.js';
export type {
  UseVirtualizerForSnapshotOptions,
  UseVirtualizerForSnapshotResult,
  ExpandedSetHandle,
  SetExpandedFx,
  FileTreeSelectionHandle,
  UseFileTreeSelectionOptions,
  UseFileTreeKeyboardOptions,
  UseFileTreeKeyboardResult,
  KeyboardExpansionActions,
  KeyboardRowLookup,
  KeyboardClipboardActions,
  ViewportKeyboardActions,
  TypeaheadHandle,
  TypeaheadRowSource,
  TypeaheadVisibleRow,
  UseTypeaheadOptions,
  UseControlledStateOptions,
  UseRenameStateOptions,
  RenameStateHandle,
  RenameStateFx,
  RenameStateSnapshot,
  RenameStateControlled,
  RenameCommitResult,
  RenameFileSystemError,
  ContextMenuStateHandle,
  ClipboardStateHandle,
  FilterStateHandle,
  UseFilterStateControlled,
  UseFilterStateOptions,
  SearchableEngine,
  SearchStatus,
  UseSearchResultsOptions,
  UseSearchResultsResult,
  DragDropEffect,
  DropPosition,
  DragDropValidateContext,
  DragDropValidationResult,
  DragDropState,
  DragDropHandlers,
  UseFileTreeDragDropResult,
  AutoExpandDwellHandle,
  UseAutoExpandDwellOptions,
} from './hooks/index.js';
export type { CommandRegistryHandle } from './types.js';
export type { FileRefreshTarget } from './commands/types.js';
export { fileActionTargetForId } from './file-actions.js';
export type { FileActionSnapshot, FileActionTarget } from './file-actions.js';
export { fileSearchRequestForIds } from './file-search.js';
export type {
  FileSearchRequest,
  FileSearchRequestKind,
} from './file-search.js';
export type { ExpansionSnapshot } from './tree-expansion.js';
export { expandedDescendantIds } from './tree-expansion.js';
export {
  classifyActiveEntry,
  normalizeActiveEntryTarget,
  shouldAutoRevealActiveEntry,
} from './active-entry-policy.js';
export type {
  ActiveEntryAutoReveal,
  ActiveEntryClassificationInput,
  ActiveEntryDisposition,
  ActiveEntryInput,
  ActiveEntryOrigin,
  ActiveEntryPolicy,
  ActiveEntryResolution,
  ActiveEntryTarget,
} from './active-entry-policy.js';
export {
  PERMANENT_KEYBOARD_OPEN,
  PERMANENT_SEARCH_OPEN,
  commandOpenEvent,
} from './open-policy.js';
export type {
  FileOpenBehavior,
  FileOpenEvent,
  FileOpenMode,
  FileOpenSource,
} from './open-policy.js';
export {
  FILE_TREE_NAVIGATION_STATE_VERSION,
  FILE_TREE_NAVIGATION_LIMITS,
  parseFileTreeNavigationState,
  serializeFileTreeNavigationState,
  fileTreePathForId,
  captureFileTreeNavigationState,
} from './navigation-state.js';
export type {
  FileTreeNavigationState,
  FileTreeScrollAnchor,
  FileTreeSearchMode,
  CaptureFileTreeNavigationStateOptions,
} from './navigation-state.js';

// Phase 3 — virtualized read-only tree.
// Phase 6 adds FileContextMenu + ContextMenuItem.
// Phase 10 adds FileDecorations.
export {
  FileTree,
  FileTreeRow,
  DisclosureChevron,
  IndentGuides,
  LoadingBadge,
  FileRenameInput,
  FileContextMenu,
  ContextMenuItem,
  FileDecorations,
  decorationAccessibleLabel,
  FileTreeFilter,
  SearchResultList,
  FileTreeDragIndicator,
} from './components/index.js';
export type {
  AriaRowProps,
  CollisionPolicy,
  CollisionPromptRequest,
  CollisionPromptResult,
  DragDropOptions,
  DisclosureChevronProps,
  FileTreeEngine,
  FileTreeProps,
  FileTreeRef,
  FileTreeNavigationRestoreResult,
  FileTreeRowProps,
  FileTreeSnapshotLike,
  RootLabelContext,
  IndentGuidesProps,
  LoadingBadgeProps,
  MergedDecoration,
  VirtualizerOffsetObserver,
  VirtualizerRectObserver,
  FileRenameInputProps,
  FileContextMenuProps,
  ContextMenuItemProps,
  FileDecorationsProps,
  FileTreeFilterHandle,
  FileTreeFilterMode,
  FileTreeFilterProps,
  SearchResultListProps,
  FileTreeDragIndicatorProps,
} from './components/index.js';
