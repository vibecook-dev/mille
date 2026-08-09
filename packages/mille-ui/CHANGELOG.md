# Changelog

## Unreleased

### Fixed

- **Remote lazy expansion viewport deadlock** — `FileTree` now advertises at
  least one physical screen of hydration capacity (plus its bounded overscan),
  even when the remote mirror initially knows only the root row. This lets the
  host send metadata for newly discovered children instead of leaving an
  expanded remote folder visually empty.
- **Live-region politeness (Phase 6.3)** — polite and assertive messages now
  use two separate pre-mounted regions with matching implicit roles
  (`status` / `alert`). Assistive tech latches a region's politeness when it
  is inserted, so flipping `aria-live` on one shared node was unreliable, and
  `role="status"` + `aria-live="assertive"` contradicted itself.
- **Storm counts and mutation failures (Phase 6.3)** — messages dropped by the
  announcer's throttle are counted and reported (`… (and 3 more)`) instead of
  vanishing. Batch mutations announce partial progress when they fail partway
  (`Deleted 2 of 4 items: EACCES…`) and single mutations announce failure;
  previously only whole-batch success was announced.
- **`CommandRegistry` additions are optional** — `getContext` and
  `dispatchWithContext` are declared optional so hosts with their own
  registry implementation keep compiling. `createCommandRegistry` always
  provides them and callers already feature-detect.
- **Multi-root active tab override** — bare `activePath` no longer marks every
  root’s matching file active; use `activeEntryId` or `activeRootPath` +
  `activePath` to disambiguate.
- **SCM path containment (P0)** — `createShellScmClient` / history clients
  validate every relative path with `assertPathUnderRoot` (rejects `..`,
  absolute, drive paths). Playground IPC no longer trusts renderer-supplied
  `rootPath` (active workspace only).
- **Symlink escape on compare (P0)** — `assertPathUnderRoot` is lexical, so a
  symlink inside the workspace pointing outside it passed containment and the
  working-tree read followed it. `compare` now resolves both the root and the
  target with `realpathSync` and re-checks containment before reading; new
  exported `assertRealPathUnderRoot` covers the on-disk case.
- **Git revision argument injection (P0)** — `getContents` interpolates
  `revision` into the positional `git show <rev>:<path>` argument, so a
  revision beginning with `-` reached git as an *option*
  (`--output=<file>` is an arbitrary-file-write primitive; it carries no
  `:`, so the old colon check missed it). New exported `assertSafeRevision`
  rejects option-like and non-revision characters before any spawn.
  `git log --max-count` is now a coerced positive integer.
- **Multi-root demonstrated end to end** — the playground workspace is now a
  root *list*: "Add folder to workspace…" appends a root, git decorations
  register one provider per root (distinct `providerId`, absolute-URI lookup
  keeps them disjoint), Changed Files runs `git status` per root and stamps
  each seed with its owning `rootPath`, and `resolveRootPath` maps an engine
  root entry back to its absolute path. IPC now trusts any *open* root rather
  than only the active one. Demo diagnostics / test-status seeds stay on the
  primary root.
- **Multi-root SCM revert (P0)** — `scm.revert` groups by owning root via
  `selectedScmTargets` / `groupScmTargetsByRoot` and passes `rootPath` per
  batch so `rootA/same.ts` and `rootB/same.ts` cannot collapse.
- **Concurrent command lifecycle (P1)** — `dispatchWithLifecycle` builds a
  per-dispatch context (no registry WeakMap stash). Concurrent awaits no
  longer cross-wire `signal` / `reportProgress`.
- **Enablement on normal dispatch (P1)** — `registry.dispatch` and
  `dispatchWithContext` honor `enablement`; context menus use the latter and
  surface failures via `host.notify`.
- **Open Files multi-root identity (P1)** — `normalizeEditorState` /
  `projectOpenFilesView` key by entryId / rootPath+path so identical relative
  paths across roots stay distinct.
- **Shell SCM AbortSignal (P1)** — in-flight git children are `kill`ed on abort.
- **Playground git-status IPC** — `get-git-status` uses the same trusted
  active-workspace root check as history/SCM handlers.

### Added

- **Live announcer (Phase 6.3)** — `@vibecook/mille-ui/a11y` exports
  `createLiveAnnouncer` for storm-safe `aria-live` feedback (lazy DOM mount,
  coalesce window + min interval + `announceMany`). Mutation defaults
  (`file.create` / `rename` / `delete` / `move` / `copy`) call `host.notify`
  on success so hosts can forward to the announcer.
- **Command contribution contract (Phase 5.4)** — `@vibecook/mille-ui/commands`
  adds `contributeCommands`, `dispatchWithLifecycle` (progress / cancel /
  failure notify / telemetry), `evaluateEnablement`, `partitionCommandsForMenu`
  (submenus), and `buildCommandContext` for workspace/editor/SCM/diagnostics
  surfaces. Commands gain `enablement`, `submenu`, `submenuLabel`, and `order`.
  Context menus render nested Radix submenus and grey disabled items.
- **File history + SCM actions (Phase 5.3)** — `@vibecook/mille-ui/history`
  exports `FileHistoryClient`, `ScmClient`, `runScmRevert` / `runScmCompare` /
  `runFileHistory` (confirm + progress + cancel), `scmHistoryCommands`, and
  map clients for tests. Shell-backed git via
  `createShellFileHistoryClient` / `createShellScmClient` on
  `@vibecook/mille-ui/git/node`. `FileTree` accepts `hostHooks` to inject SCM
  context into the command registry.
- **Explorer views (Phase 5.2)** — `@vibecook/mille-ui/views` exports pure
  projectors (`projectOpenFilesView`, `projectChangedFilesView`,
  `projectProblemsView`, `projectFailedTestsView`, `projectCustomScopeView`),
  `resolveExplorerView` (path/id → EntryId, multi-root via `seed.rootPath` /
  `seed.id`), and virtualized `ExplorerViewList` with stable `item.key`
  selection, `aria-activedescendant`, and badge accessible text. Playground
  demonstrates Open Files, live Changed Files (`git status` IPC), Problems,
  and Failed Tests. Bench: `pnpm --filter @vibecook/mille-ui bench:views`.
- **Browser-safe undo types** — port client imports undo normalizers from a
  Node-free module so the Electron/Vite renderer build no longer pulls the
  native loader via `client.ts`.
- **Test-status decorations (Phase 5.1)** — `@vibecook/mille-ui/test-status`
  exports `registerTestStatusDecorations`, `TestStatusClient`, and
  `createMapTestStatusClient`. Leaf glyphs: `✗` failed, `!` errored, `…`
  running, `○` skipped, `✓` passed (opt-in via `showPassed`). Folders
  aggregate failure counts with muted colors. Same hardening as diagnostics.
  Tokens: `--mille-decoration-test-{passed,failed,errored,running,skipped}`
  (+ muted). Distinct from `@vibecook/mille-ui/testing` (fake engine helpers).
- **Editor-state decorations (Phase 5.1)** — `@vibecook/mille-ui/editor-state`
  exports `registerEditorStateDecorations`, `EditorStateClient`, and
  `createMapEditorStateClient`. Dirty tabs show `●`, clean open tabs show
  `○` (toggle with `decorateOpen`); tooltips cover active / unsaved / open.
  Shares diagnostics hardening (generation token, port `resolvePath`, path
  validation, concurrency, value-diff, `onError`). Tokens:
  `--mille-decoration-dirty|open|active`.
- **Diagnostics decorations (Phase 5.1)** — `@vibecook/mille-ui/diagnostics`
  exports `registerDiagnosticsDecorations`, a host-supplied
  `DiagnosticsClient`, and `createMapDiagnosticsClient` for demos/tests.
  Leaf rows show problem-count badges colored by max severity
  (`error > warning > info > hint`); folders aggregate descendant counts with
  muted colors. Badge cap, color/tooltip/badge overrides, and
  `propagateToParent` match the git companion ergonomics. CSS tokens:
  `--mille-decoration-error|warning|info|hint` (+ muted variants). Hardening:
  generation-token stale-fetch discard, background `onError`, port
  `resolvePath` fallback, workspace-relative path validation, bounded
  concurrent path resolve, value-diff notifications, and accessible
  decoration labels (`aria-label` + sr-only text on `FileDecorations`).
- **Trash default and undo journal** — `delete` soft-trashes into a managed
  recycle directory outside the workspace (`$TMPDIR/mille-recycle/<hash>/`,
  undoable); `{ trash: false }` is permanent. Soft-delete undo validates
  filesystem identity of the recycle payload before restore. Public
  `canUndo` / `peekUndo` / `undo` reverse create, rename, move, and soft-delete.
- **Transfer progress and cancellation** — recursive copies accept
  `operationId` / `AbortSignal`, emit `OP_PROGRESS` / `OP_COMPLETE` /
  `OP_CANCELLED` warnings, and honor `cancelOperation` between recursive steps
  with partial-destination cleanup on cancel.
- **Collision policy expansion** — transfer options and DnD accept
  `overwrite`, `skip`, and `merge` in addition to `error`/`rename`. Case-only
  sibling names collide on case-insensitive volumes. `dragDrop.onCollision`
  prompts only when `probeDestination` reports a real collision, with optional
  apply-to-all. `onDropError` surfaces failed drops instead of swallowing them.
- **Transfer safety** — self-overwrite, path-traversal names, and
  workspace-escaping symlink parents are rejected; directory recursive copy
  does not follow directory symlinks.
- **Real external import via `copyFromPath`** — OS drag-in requires the engine
  `copyFromPath` API and imports file/directory contents instead of creating
  empty placeholder entries. Per-item failures are collected and reported;
  partial directory copies do not leave silent empty files.
- **Root-aware scoped-search handoff** — folder menus add Find in Folder,
  Include in Search, and Exclude from Search. `onSearchScope` receives a
  provider-neutral, bounded, atomic request containing exact root-aware
  targets, including multi-selection for include/exclude. The host remains
  responsible for translating literals to its content-search provider.
- **Authoritative refresh and controlled collapse** — files/folders expose
  Refresh from Disk, roots expose Refresh Workspace, and hosts can intercept
  both through `onRefresh`. Collapse All and the new Collapse Descendants
  command now update the tree's actual React expansion state; the imperative
  handle adds `collapseDescendants(id)`. A 100,000-wide/10,000-deep benchmark
  guards descendant-state computation.
- **Root-aware file-system host actions** — the default command registry and
  context menu now provide Copy Absolute Path, Copy Workspace-Relative Path,
  Reveal in File Manager, Open Containing Folder, and Open in Terminal.
  `FileTree` exposes narrow async callbacks with a canonical `FileActionTarget`;
  the Electron playground validates workspace containment in its main process
  before invoking clipboard, shell, or terminal capabilities.
- **Active-entry disposition policy** — `activeEntry` descriptors can tag
  generated or external targets; `activeEntryPolicy` controls opt-in reveal of
  hidden, ignored/excluded, and generated entries; and
  `onActiveEntryResolution` reports visible/hidden/ignored/generated/external/
  missing outcomes. Conservative defaults avoid pending reveals and external
  workspace lookups.
- **Typed file-open policy** — `openBehavior` optionally previews files on
  single click while retaining selection-only as the default.
  `onOpen(entry, event)` identifies preview/permanent mode and the mouse,
  keyboard, search, or command source so editor hosts can implement one preview
  slot and permanent-tab promotion consistently.
- **Active-editor following** — `activeEntry` marks the file active in the
  host editor independently of tree focus/selection, while
  `autoRevealActiveEntry` optionally expands and scrolls through the lazy
  indexed-path pipeline without stealing focus or snapping back on unrelated
  updates.
- **Versioned navigation persistence** — path-based expansion, selection,
  focus, filter mode, and pixel scroll anchors via
  `initialNavigationState`, `onNavigationStateChange`, and
  `FileTreeRef.captureNavigationState()` / `restoreNavigationState()`.
  State is migrated, validated, bounded, and restored through lazy indexed
  path resolution rather than unstable process-local entry IDs.
- **Minimal archival theme** — paper/ink Structure-panel look from the
  spaghetti-ui `FileTreeNode` design. CSS:
  `@vibecook/mille-ui/theme/minimal.css` (activate with
  `data-mille-theme="minimal"`). Icons: `minimalIconTheme` via
  `@vibecook/mille-ui/icons/minimal`. Matches: mono 10px / folder
  natural-case mono labels, `[+]`/`[-]` w-4 disclosure, inverted
  selection, dashed indent rails, `depth*12+8` padding, section
  `px-2 py-2` gutter.
- **`data-mille-kind`** on rows (`directory` | `file` | `symlink`) so
  host themes can style folder labels without re-implementing the row.
- **`--mille-indent-guide-style`** token (default `solid`; minimal uses
  `dashed`).
- **`--mille-row-padding-inline`** — base inset before `depth * indent`
  (minimal uses `8px`); indent guides offset to match.

## 0.2.1 — 2026-07-12

Soft-duotone icons + Project-view row polish. Additive; no public-API breaks.

### Added

- **`duotoneIconTheme`** — `@vibecook/mille-ui/icons/duotone` (and
  re-export from `@vibecook/mille-ui/icons`). Soft-duotone set: filled
  blue folders and dark file bodies with a language-color chip. Designed
  for dense IDE sidebars; playground default.
- **Library / symlink row affordances** — data attributes for library
  roots and directory-target symlinks so host chrome can style them
  (e.g. node_modules tint, symlink mark).

### Fixed

- Virtualized rows no longer double vertical spacing (`position:
  relative !important` removed so `translateY` virtualization works).
- Expand chevrons track `symlinkTargetIsDir` / unwalked dirs correctly
  for pnpm-style package links.
- VCS decoration badges paint before the filename (Project-view order).

## 0.2.0 — 2026-04-25

Demo-ready release. Track A complete (port-side decorations + real git
client + Material icon bundle) plus headless bundle trim and an
imperative tree handle. No public-API breaks; new exports are additive.

### Added

- **`createShellGitClient`** — `@vibecook/mille-ui/git/node`. Real
  `git status --porcelain=v2 -z` + `.git/HEAD` / `.git/index` watcher
  (100 ms debounced). The pure-spec `@vibecook/mille-ui/git` entrypoint
  remains browser-safe; Node-only shell client moved to a sibling
  subpath.
- **Material Icon Theme** — `loadMaterialIconTheme()` now returns the
  real upstream bundle (MIT, `material-extensions/vscode-material-icon-theme`).
  Built at publish time via `scripts/build-material.mjs`. See
  `NOTICES.md`.
- **Imperative `FileTreeRef`** — `forwardRef` on `FileTree` exposes
  `revealPath` / `revealId` / `scrollToRow` / `clearSelection` /
  `clearFilter` / `clearClipboard` / `focusFilter`. New
  `useFileTreeRef` hook for nested consumers (no prop-drilling).
- **Decoration providers across the port boundary** — pairs with the
  engine's new `decorations` / `decorationChanged` frame types.
  `registerGitDecorations` / `registerAgentRulesDecorations` accept a
  `FileExplorer` *or* `PortFileExplorer`.
- **`size-limit` enforced in CI** — headless bundle has a 13 KB
  fail-on-regression boundary (SPEC §12 target 12 KB).

### Changed

- **Headless bundle 21.69 KB → 12.46 KB gzip.** Logic hooks +
  ARIA primitives are now exported directly from
  `@vibecook/mille-ui/headless` without the styled-row chrome.
- Git decoration helper relocated to `@vibecook/mille-ui/git/node` so
  the browser-safe `@vibecook/mille-ui/git` entrypoint no longer pulls
  in `node:child_process` / `node:fs`.

### Fixed

- Decoration provider registration is robust against `undefined` from
  the NAPI edge (`getByUri` guard).
- Host-level `registerDecorationProvider` fans out to every connected
  port client, not just the one that registered.

### Deferred to v0.3 (carried)

- Headless bundle to land under the 12 KB §12 target (currently 12.46 KB).
- Full Logic-hook + View split for the remaining row primitives.
- Playwright perf guardrails + visual regression baselines.
- libgit2 (or `isomorphic-git`) GitClient for environments without a
  `git` binary on PATH.

## 0.1.0 — 2026-04-20

Initial release of `@vibecook/mille-ui` — React file-tree UI companion
to `@vibecook/mille`.

### Added

- `<FileTreeProvider>` + `<FileTree>` — virtualized tree rendering over `MirrorSnapshot`
- Default + headless entry points (`@vibecook/mille-ui` + `@vibecook/mille-ui/headless`)
- Command registry with `Mod+N`, `F2`, `Delete`, arrow nav, Shift-range select, typeahead, `Cmd+F`
- Inline rename + new-file / new-folder flow
- Context menu via `@radix-ui/react-context-menu`
- Cut / copy / paste with multi-select delete confirmation
- Filter (client-side) + ranked search (`fx.search` via `<FileTreeFilter>`)
- Decorations pipeline (`<FileDecorations>` + dual tree / decoration versions)
- VS Code File Icon Theme JSON compat — Seti / Material / vscode-icons drop in
- Drag-and-drop: tree↔tree, OS→tree, tree→chat (MIME `application/vnd.claude.attachment`)
- Git decoration companion (`@vibecook/mille-ui/git`) — host-supplied client
- Agent-rules companion (`@vibecook/mille-ui/agent-rules`) — `.cursor/rules`, `.kiro/steering/`, `CLAUDE.md`, etc.
- WAI-ARIA 1.2 Tree Pattern compliance

### Deferred to v0.2

- Decoration protocol frame for the port client (host-to-client provider forwarding)
- Real libgit2 integration (v0.1 ships a stub)
- Material Icon Theme bundle
- Playwright perf guardrail + visual regression baselines
- Full Logic-hook + View split per SPEC §4.9

### Contributors

- James Yong
