// The git engine's TypeScript contract — SPEC mille-git Appendix A.
//
// This file IS the published contract ADE code builds against. It is
// renderer-safe: types, one constant, and a plain `Error` subclass, with no
// napi and no `node:` import, so a renderer bundle can depend on it. The
// engine itself lives in `@vibecook/mille/git` (Node / UtilityProcess only),
// which keeps a structurally identical private copy rather than importing this
// one (`mille-ui` peer-depends on `mille`; the reverse would close the loop).
// `test-types/git-service-compat.ts` fails `typecheck` when the two drift.
//
// A renderer never constructs a `GitService` from the native engine. Its host
// hands it one built over a `GitTransport` — in-process in a utility, over a
// MessagePort in a renderer — and the adapters in `engine-client.ts` turn that
// into the `GitClient` / `FileHistoryClient` / `ScmClient` the tree and the
// history views already consume.

import type { GitStatusLetter } from './client.js';

/**
 * The git RPC's own version, independent of the explorer protocol's
 * `PROTOCOL_VERSION` (§19.2, §24): a git change must not move the explorer's
 * clock, and the two channels are separate ports.
 */
export const GIT_PROTOCOL_VERSION = 1;

/** An absolute worktree path, as passed to `open`. */
export type WorktreeId = string;
export type GitRepoKind = 'none' | 'worktree' | 'bare';

export interface GitIdentity {
  readonly kind: GitRepoKind;
  /**
   * The checkout root, which may be an ancestor of the path `open` was given
   * (a package inside a monorepo). Spelled as the caller spelled it, not
   * realpath'd, so decoration keys match the explorer's URIs.
   */
  readonly worktreePath: string;
  /** The resolved gitdir — for a linked worktree, past the `gitdir:` file. */
  readonly gitdir: string | null;
  readonly commonDir: string | null;
  readonly bare: boolean;
  /** Full hex id of `HEAD`; `null` on an unborn branch or a non-repo. */
  readonly head: string | null;
  readonly branch: string | null;
  readonly detached: boolean;
}

/** Both porcelain columns, preserved. `.` is an unchanged slot. */
export interface StatusXy {
  readonly index: GitStatusLetter | '.';
  readonly worktree: GitStatusLetter | '.';
}

export interface StatusEntry {
  /** Checkout-relative POSIX path; for a rename, the new name. */
  readonly path: string;
  readonly origPath?: string;
  readonly xy: StatusXy;
  /** The index slot is set. False for untracked and conflicted rows. */
  readonly staged: boolean;
  /** `?`: `xy` is `.?`. */
  readonly untracked: boolean;
  /** Unmerged: `xy` is git's conflict pair (`UU`, `AA`, `DU`, …). */
  readonly conflicted: boolean;
}

/** One published status generation: whole repository, XY preserved. */
export interface StatusSnapshot {
  readonly worktreePath: string;
  readonly identity: GitIdentity;
  /** Sorted, capped at `statusRowCap`. */
  readonly entries: readonly StatusEntry[];
  /** Rows the cap left out. */
  readonly truncated?: number;
  readonly tookMs: number;
  /** This generation merged a path-limited run (§10.6). */
  readonly pathLimited: boolean;
  /** Monotonic per checkout. */
  readonly generation: number;
  readonly statusBackend: 'gix' | 'cli';
}

export type DiffSpec = 'worktree' | 'index' | 'commit';

export interface DiffRequest {
  /** Checkout-relative POSIX path. */
  readonly path: string;
  /** `worktree`: index → worktree. `index`: HEAD → index. `commit`: parent → `commit`. */
  readonly spec: DiffSpec;
  readonly commit?: string;
  readonly maxBytes?: number;
  readonly maxLines?: number;
  readonly signal?: AbortSignal;
}

export interface DiffLine {
  readonly kind: 'ctx' | 'add' | 'del';
  readonly text: string;
  /** Cut at 1 024 characters (§13.2 step 5). Absent when not. */
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
  /** Over a cap: no hunks. Decided from sizes before any load when it can be. */
  readonly tooLarge: boolean;
  readonly binary?: boolean;
  readonly bytes?: number;
  readonly lines?: number;
  readonly hunks?: readonly DiffHunk[];
  /** Always present, including `tooLarge` and `binary` results. */
  readonly tookMs: number;
}

export interface ReadObjectRequest {
  readonly path: string;
  /** A revision (`HEAD`, a sha, `HEAD~2`, a branch). Not the working tree. */
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
  /** The include-set (checkout-relative). Only these are committed. */
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

/** One `'warning'` transport event (§22.2). Never carries file contents. */
export interface GitWarning {
  readonly code: 'ENOGIT' | 'PARSE' | 'WATCH' | 'DEGRADED_CLI';
  readonly message: string;
  readonly worktree?: string;
}

export interface GitEngineOptions {
  /** Caller-path + mutation containment. `.git` walk MAY leave these. */
  readonly allowedRoots: readonly string[];
  readonly gitPath?: string;
  readonly statusRowCap?: number;
  readonly debounceMs?: number;
  readonly maxWaitMs?: number;
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

/**
 * A failed git call. Errors are rebuilt wherever they land (a MessagePort
 * cannot carry a class), so test with `isGitError`, not `instanceof`.
 */
export class GitError extends Error {
  readonly code: GitErrorCode;
  readonly path?: string;

  constructor(code: GitErrorCode, message: string, path?: string) {
    super(message);
    this.name = 'GitError';
    this.code = code;
    if (path !== undefined && path.length > 0) this.path = path;
  }
}

/** Structural check: `name === 'GitError'` with a string `code`. */
export function isGitError(e: unknown): e is GitError {
  return (
    e instanceof Error &&
    e.name === 'GitError' &&
    typeof (e as { code?: unknown }).code === 'string'
  );
}

/** Transport events and their bodies. */
export interface GitTransportEvents {
  /** A snapshot of a worktree the transport opened (keyed by `open`'s path). */
  readonly status: { readonly worktree: WorktreeId; readonly snapshot: StatusSnapshot };
  readonly worktrees: {
    readonly worktree: WorktreeId;
    readonly worktrees: readonly GitWorktree[];
  };
  readonly warning: GitWarning;
}

/**
 * The seam under `GitService` (§19.3): in-process (direct napi) in a utility
 * process, framed over a second MessagePort for a renderer. `call` takes every
 * `GitService` verb plus `hintPaths` (`{ worktree, paths: string[] | null }`,
 * worktree-relative POSIX); `args` are plain cloneable objects and the abort
 * signal travels beside them, never inside.
 */
export interface GitTransport {
  call(method: string, args: unknown, opts?: { signal?: AbortSignal }): Promise<unknown>;
  on(event: 'status' | 'worktrees' | 'warning', cb: (body: unknown) => void): () => void;
  close(): void;
}

/** The ADE-facing git surface (§17.1). */
export interface GitService {
  open(worktreePath: string): Promise<GitIdentity>;
  close(worktreePath: string): Promise<void>;
  identity(worktreePath: string): Promise<GitIdentity>;

  /** Never throws for not-a-repo or a missing git: an empty snapshot instead. */
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
  /** Discard worktree changes, restoring **index** bytes (never HEAD's). */
  restore(
    worktreePath: string,
    paths: readonly string[],
    opts?: { signal?: AbortSignal },
  ): Promise<void>;
  /** Commit the include-set; resolves to the new commit's 40-hex id. */
  commit(worktreePath: string, req: CommitRequest): Promise<string>;

  worktrees(worktreePath: string): Promise<readonly GitWorktree[]>;
  onWorktrees(worktreePath: string, cb: (w: readonly GitWorktree[]) => void): () => void;

  advisePerformanceConfig(worktreePath: string): Promise<Readonly<Record<string, string>>>;
  dispose(): Promise<void>;
}
