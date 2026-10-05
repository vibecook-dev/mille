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

/** The git RPC's version, independent of the explorer's `PROTOCOL_VERSION`. */
export const GIT_PROTOCOL_VERSION = 1;

/** Transport events and their bodies. */
export interface GitTransportEvents {
  /** A snapshot of a worktree the transport opened, keyed by `open`'s path. */
  readonly status: { readonly worktree: WorktreeId; readonly snapshot: StatusSnapshot };
  readonly worktrees: {
    readonly worktree: WorktreeId;
    readonly worktrees: readonly GitWorktree[];
  };
  readonly warning: GitWarning;
}

/** §19.3. Structurally identical to `@vibecook/mille-ui/git`'s. */
export interface GitTransport {
  call(method: string, args: unknown, opts?: { signal?: AbortSignal }): Promise<unknown>;
  on(event: 'status' | 'worktrees' | 'warning', cb: (body: unknown) => void): () => void;
  close(): void;
}

/** §17.1. Structurally identical to `@vibecook/mille-ui/git`'s. */
export interface GitService {
  open(worktreePath: string): Promise<GitIdentity>;
  close(worktreePath: string): Promise<void>;
  identity(worktreePath: string): Promise<GitIdentity>;
  status(worktreePath: string): Promise<StatusSnapshot>;
  onStatus(worktreePath: string, cb: (s: StatusSnapshot) => void): () => void;
  diff(worktreePath: string, req: DiffRequest): Promise<DiffResult>;
  log(worktreePath: string, req?: LogRequest): Promise<LogPage>;
  commitFiles(worktreePath: string, sha: string): Promise<readonly CommitFile[]>;
  readObject(worktreePath: string, req: ReadObjectRequest): Promise<string | Uint8Array | null>;
  readWorktreeFile(
    worktreePath: string,
    path: string,
    opts?: { signal?: AbortSignal },
  ): Promise<string | Uint8Array | null>;
  stage(
    worktreePath: string,
    paths: readonly string[],
    opts?: { signal?: AbortSignal },
  ): Promise<void>;
  unstage(
    worktreePath: string,
    paths: readonly string[],
    opts?: { signal?: AbortSignal },
  ): Promise<void>;
  restore(
    worktreePath: string,
    paths: readonly string[],
    opts?: { signal?: AbortSignal },
  ): Promise<void>;
  commit(worktreePath: string, req: CommitRequest): Promise<string>;
  worktrees(worktreePath: string): Promise<readonly GitWorktree[]>;
  onWorktrees(worktreePath: string, cb: (w: readonly GitWorktree[]) => void): () => void;
  advisePerformanceConfig(worktreePath: string): Promise<Readonly<Record<string, string>>>;
  dispose(): Promise<void>;
}

/**
 * The RPC table `createGitService` speaks over `GitTransport.call` — what a
 * MessagePort transport frames (PR 8) and what the in-process transport
 * dispatches. Args are plain cloneable objects; an abort signal travels in
 * `call`'s `opts`, never in `args`. `hintPaths` is the one verb with no
 * `GitService` method: the explorer host's routed hints (§18.1).
 */
export interface GitCallTable {
  open: { args: { worktree: string }; result: GitIdentity };
  close: { args: { worktree: string }; result: void };
  identity: { args: { worktree: string }; result: GitIdentity };
  status: { args: { worktree: string }; result: StatusSnapshot };
  diff: { args: { worktree: string; request: Omit<DiffRequest, 'signal'> }; result: DiffResult };
  log: { args: { worktree: string; request: Omit<LogRequest, 'signal'> }; result: LogPage };
  commitFiles: { args: { worktree: string; sha: string }; result: CommitFile[] };
  readObject: {
    args: { worktree: string; request: Omit<ReadObjectRequest, 'signal'> };
    result: string | Uint8Array | null;
  };
  readWorktreeFile: { args: { worktree: string; path: string }; result: string | Uint8Array | null };
  stage: { args: { worktree: string; paths: string[] }; result: void };
  unstage: { args: { worktree: string; paths: string[] }; result: void };
  restore: { args: { worktree: string; paths: string[] }; result: void };
  commit: { args: { worktree: string; request: Omit<CommitRequest, 'signal'> }; result: string };
  worktrees: { args: { worktree: string }; result: GitWorktree[] };
  advisePerformanceConfig: { args: { worktree: string }; result: Record<string, string> };
  hintPaths: { args: { worktree: string; paths: string[] | null }; result: void };
}

export type GitCallMethod = keyof GitCallTable;
