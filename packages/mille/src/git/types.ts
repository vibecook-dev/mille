// The git engine's data shapes, as `@vibecook/mille/git` implements them.
//
// This is the implementation's local copy of SPEC mille-git Appendix A, and it
// is deliberately NOT the published contract ADE code imports. That contract
// lives in renderer-safe `@vibecook/mille-ui/git` (`service-types.ts`), and
// this package must not import it: `mille-ui` peer-depends on `mille`, so an
// import here would be `mille → mille-ui → mille`, and a re-export would drag
// `registerGitDecorations` into the native package (§8.3). The two copies are
// held together structurally — the factories return plain objects of these
// shapes — and by a type-level assertion in `mille-ui`'s typecheck.
//
// Field spellings are exactly what the native engine serializes (mille-git's
// `Serialize` derives); changing one here without the Rust side changes
// nothing but the lie this file tells.

/** Porcelain letters (`mille-ui/git`'s `GitStatusLetter`). */
export type GitStatusLetter = 'M' | 'A' | 'D' | 'U' | 'R' | 'C' | '?' | '!';

export type WorktreeId = string;
export type GitRepoKind = 'none' | 'worktree' | 'bare';

export interface GitIdentity {
  readonly kind: GitRepoKind;
  readonly worktreePath: string;
  readonly gitdir: string | null;
  readonly commonDir: string | null;
  readonly bare: boolean;
  readonly head: string | null;
  readonly branch: string | null;
  readonly detached: boolean;
}

export interface StatusXy {
  readonly index: GitStatusLetter | '.';
  readonly worktree: GitStatusLetter | '.';
}

export interface StatusEntry {
  readonly path: string;
  readonly origPath?: string;
  readonly xy: StatusXy;
  readonly staged: boolean;
  readonly untracked: boolean;
  readonly conflicted: boolean;
}

export interface StatusSnapshot {
  readonly worktreePath: string;
  readonly identity: GitIdentity;
  readonly entries: readonly StatusEntry[];
  readonly truncated?: number;
  readonly tookMs: number;
  readonly pathLimited: boolean;
  readonly generation: number;
  readonly statusBackend: 'gix' | 'cli';
}

export type DiffSpec = 'worktree' | 'index' | 'commit';

export interface DiffRequest {
  readonly path: string;
  readonly spec: DiffSpec;
  readonly commit?: string;
  readonly maxBytes?: number;
  readonly maxLines?: number;
  readonly signal?: AbortSignal;
}

export interface DiffLine {
  readonly kind: 'ctx' | 'add' | 'del';
  readonly text: string;
  /** The line was cut at 1 024 characters (§13.2 step 5). Absent when not. */
  readonly truncated?: boolean;
}

export interface DiffHunk {
  readonly oldStart: number;
  readonly oldLines: number;
  readonly newStart: number;
  readonly newLines: number;
  readonly lines: readonly DiffLine[];
}

export interface DiffResult {
  readonly path: string;
  readonly tooLarge: boolean;
  readonly binary?: boolean;
  readonly bytes?: number;
  readonly lines?: number;
  readonly hunks?: readonly DiffHunk[];
  readonly tookMs: number;
}

export interface ReadObjectRequest {
  readonly path: string;
  readonly revision: string;
  readonly signal?: AbortSignal;
}

export interface LogRequest {
  readonly path?: string;
  readonly limit?: number;
  readonly skip?: number;
  readonly revision?: string;
  readonly signal?: AbortSignal;
}

export interface LogCommit {
  readonly id: string;
  readonly shortId: string;
  readonly author?: string;
  readonly message?: string;
  readonly timestampMs: number;
  readonly parents?: readonly string[];
}

export interface LogPage {
  readonly commits: readonly LogCommit[];
  readonly tookMs: number;
  readonly exhausted: boolean;
}

export interface CommitFile {
  readonly path: string;
  readonly origPath?: string;
  readonly status: GitStatusLetter;
}

export interface CommitRequest {
  readonly summary: string;
  readonly body?: string;
  readonly paths: readonly string[];
  readonly signal?: AbortSignal;
}

export interface GitWorktree {
  readonly path: string;
  readonly branch: string | null;
  readonly head: string | null;
  readonly bare: boolean;
  readonly locked: boolean;
  readonly prunable: boolean;
  readonly isCurrent: boolean;
}

/** One `'warning'` event (§22.2). Never carries file contents. */
export interface GitWarning {
  readonly code: 'ENOGIT' | 'PARSE' | 'WATCH' | 'DEGRADED_CLI';
  readonly message: string;
  /** The session's checkout root, when the warning is about one. */
  readonly worktree?: string;
}

export interface GitEngineOptions {
  /** Caller-path + mutation containment. `.git` walk MAY leave these. */
  readonly allowedRoots: readonly string[];
  readonly gitPath?: string;
  readonly statusRowCap?: number;
  readonly debounceMs?: number;
  readonly maxWaitMs?: number;
  /** Idle time after a path-limited run before a full rescan. Default 5 000. */
  readonly idleRescanMs?: number;
  readonly autoRenderMaxBytes?: number;
  readonly autoRenderMaxLines?: number;
  readonly hardRefuseBytes?: number;
  readonly logPageDefault?: number;
  readonly commitTimeoutMs?: number;
  readonly writePerformanceConfig?: boolean;
}

export type GitErrorCode =
  | 'EINVAL'
  | 'EACCES'
  | 'ENOENT'
  | 'ENOTAREPO'
  | 'ENOGIT'
  | 'ECANCELED'
  | 'EUNSUPPORTED'
  | 'EHOOK'
  | 'ETIMEOUT'
  | 'ELOCKED'
  | 'EUNKNOWN';
