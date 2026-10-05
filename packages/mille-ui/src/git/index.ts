// Phase 13 — `@vibecook/mille-ui/git` barrel.
//
// Public surface of the git decoration companion. See MILLE_UI_PLAN.md
// Phase 13 and MILLE_UI_SPEC.md §7 / §14.1.

export type {
  GitClient,
  GitStatusEntry,
  GitStatusLetter,
} from './client.js';

export type {
  BatchOptions,
  Batcher,
} from './batch.js';
export { createBatcher } from './batch.js';

export type {
  EngineDecorationProvider,
  FileExplorerLike,
  GitDecorationsHandle,
  RegisterGitDecorationsOptions,
} from './provider.js';
export { registerGitDecorations } from './provider.js';

// Phase B4 — the shell-based `GitClient` + `.git` watcher use
// `node:child_process` + `node:fs` and cannot run in a browser /
// renderer context. They live under `@vibecook/mille-ui/git/node` so
// Vite / esbuild don't attempt to externalize Node built-ins into
// renderer bundles. Host-side consumers (main process, utility
// process) import them explicitly:
//
//     import { createShellGitClient } from '@vibecook/mille-ui/git/node';
//
// Types still flow through the renderer-safe barrel so a renderer
// that passes a `GitClient` across IPC can type-check without
// pulling any Node code into its bundle.
export type {
  ChildProcessLike,
  ShellGitClientOptions,
  SpawnLike,
} from './shell-client.js';
export type { WatchDotGitOptions } from './watch-dotgit.js';

// SPEC mille-git §17 — the `GitService` contract. Renderer-safe: ADE code
// imports these types from here and receives an instance from its host; the
// native engine and the factories live in `@vibecook/mille/git` (Node only).
export { GIT_PROTOCOL_VERSION, GitError, isGitError } from './service-types.js';
export type {
  CommitFile,
  CommitRequest,
  DiffHunk,
  DiffLine,
  DiffRequest,
  DiffResult,
  DiffSpec,
  GitEngineOptions,
  GitErrorCode,
  GitIdentity,
  GitRepoKind,
  GitService,
  GitTransport,
  GitTransportEvents,
  GitWarning,
  GitWorktree,
  LogCommit,
  LogPage,
  LogRequest,
  ReadObjectRequest,
  StatusEntry,
  StatusSnapshot,
  StatusXy,
  WorktreeId,
} from './service-types.js';
