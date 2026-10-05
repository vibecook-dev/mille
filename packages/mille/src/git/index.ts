// `@vibecook/mille/git` — the Node-only half of the git engine (SPEC
// mille-git §8.3, §17.1).
//
// Node / Electron UtilityProcess only: this entry loads the native binary.
// Renderer code imports the `GitService` *types* from `@vibecook/mille-ui/git`
// and receives an instance from its host; it never imports this module.
//
// This barrel MUST NOT import or re-export `@vibecook/mille-ui`: `mille-ui`
// peer-depends on `mille`, so either would close the loop and an `export *`
// would pull `registerGitDecorations` into the native package. And the root
// `@vibecook/mille` barrel MUST NOT re-export this one (§18.3).

export { GitEngine, isNativeGitAvailable } from './engine.js';
export type { NativeGitEngine } from './engine.js';
export { GitError, isGitError } from './error.js';
export { createGitService } from './service.js';
export { bindExplorer, createInProcessGitTransport } from './transport.js';
export { GIT_PROTOCOL_VERSION } from './types.js';
export type {
  GitCallMethod,
  GitCallTable,
  GitService,
  GitTransport,
  GitTransportEvents,
} from './types.js';
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
  GitStatusLetter,
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
} from './types.js';
