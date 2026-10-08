// The typed wrapper over the napi `GitEngine` class (SPEC mille-git §18).
//
// Node / Electron UtilityProcess only. The class exists only in a `.node`
// built with the `git` cargo feature (`build:napi:git*`); published platform
// packages leave it out until its size is measured (§32.1), and so does the
// default dev build. `isNativeGitAvailable()` says which one this process
// loaded, and never throws (§17.5).
//
// What this layer adds over the raw class, and nothing more:
//   - results arrive as JSON text (see `crates/mille-binding/src/git.rs` for
//     why) and are parsed here;
//   - `GIT|…` rejections become `GitError`;
//   - an `AbortSignal` becomes the native `operationId` + `cancelOperation`
//     pair, registered before the call can start;
//   - file contents become `string` when they are valid UTF-8, otherwise
//     `Uint8Array` — the same split `FileHistoryClient.getContents` makes.
//
// The root `@vibecook/mille` barrel never exports this (§18.3).

import { tryLoadNative } from '../native-loader.js';
import { abortError, GitError, toGitError, wrapGit, wrapGitSync } from './error.js';
import type {
  CommitFileList,
  CommitRequest,
  DiffRequest,
  DiffResult,
  GitEngineOptions,
  GitIdentity,
  GitWarning,
  GitWorktree,
  LogPage,
  LogRequest,
  ReadObjectRequest,
  StatusSnapshot,
} from './types.js';

type JsonListener = (json: string) => void;

/** The raw napi class, as `crates/mille-binding/src/git.rs` defines it. */
export interface NativeGitEngine {
  gitPath(): string | null;
  open(worktree: string): Promise<string>;
  close(worktree: string): Promise<void>;
  identity(worktree: string): string;
  status(worktree: string): Promise<string>;
  refresh(worktree: string, paths: string[] | null): Promise<string>;
  hintPaths(worktree: string, paths: string[] | null): void;
  setStatusBackend(worktree: string, backend: 'gix' | 'cli'): void;
  diff(worktree: string, request: NativeDiffRequest, operationId?: string): Promise<string>;
  log(worktree: string, request: NativeLogRequest, operationId?: string): Promise<string>;
  commitFiles(worktree: string, sha: string, operationId?: string): Promise<string>;
  readObject(
    worktree: string,
    request: { path: string; revision: string },
    operationId?: string,
  ): Promise<Buffer | null | undefined>;
  readWorktreeFile(
    worktree: string,
    path: string,
    operationId?: string,
  ): Promise<Buffer | null | undefined>;
  stage(worktree: string, paths: string[], operationId?: string): Promise<void>;
  unstage(worktree: string, paths: string[], operationId?: string): Promise<void>;
  restore(worktree: string, paths: string[], operationId?: string): Promise<void>;
  commit(
    worktree: string,
    request: { summary: string; body?: string | undefined; paths: string[] },
    operationId?: string,
  ): Promise<string>;
  worktrees(worktree: string): Promise<string>;
  advisePerformanceConfig(worktree: string): Promise<string>;
  cancelOperation(operationId: string): boolean;
  onStatus(worktree: string, listener: JsonListener): number;
  onWorktrees(worktree: string, listener: JsonListener): number;
  onWarning(listener: JsonListener): number;
  off(subscriptionId: number): boolean;
  dispose(): void;
  bindExplorer(explorer: unknown): number;
}

// `undefined` is napi's `None`, so optional fields may be passed through as-is.
interface NativeDiffRequest {
  path: string;
  spec: string;
  commit?: string | undefined;
  maxBytes?: number | undefined;
  maxLines?: number | undefined;
}

interface NativeLogRequest {
  path?: string | undefined;
  limit?: number | undefined;
  skip?: number | undefined;
  revision?: string | undefined;
}

type NativeGitEngineCtor = new (options: Record<string, unknown>) => NativeGitEngine;

function nativeGitEngineCtor(): NativeGitEngineCtor | null {
  const loaded = tryLoadNative();
  const ctor = (loaded?.module as { GitEngine?: unknown } | undefined)?.GitEngine;
  return typeof ctor === 'function' ? (ctor as NativeGitEngineCtor) : null;
}

/**
 * Whether this process loaded a `.node` that carries the git engine. `false`
 * for the default and published binaries (built without the `git` feature),
 * when no binary loads at all, and when one fails to load. Never throws.
 */
export function isNativeGitAvailable(): boolean {
  try {
    return nativeGitEngineCtor() !== null;
  } catch {
    return false;
  }
}

const UTF8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** Valid UTF-8 → `string`; anything else → the bytes; missing → `null`. */
function decodeContents(bytes: Buffer | null | undefined): string | Uint8Array | null {
  if (bytes === null || bytes === undefined) return null;
  try {
    return UTF8.decode(bytes);
  } catch {
    return new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }
}

/**
 * The napi `FileExplorer` behind `fx`: the typed wrapper from
 * `@vibecook/mille` keeps it in `nativeFx`; a raw native explorer is itself.
 */
function nativeExplorerOf(fx: unknown): unknown {
  const wrapped = (fx as { nativeFx?: unknown } | null)?.nativeFx;
  const candidate = wrapped ?? fx;
  if (
    candidate === null ||
    typeof candidate !== 'object' ||
    typeof (candidate as { takePendingChanges?: unknown }).takePendingChanges !== 'function'
  ) {
    throw new GitError(
      'EINVAL',
      'bindExplorer needs a FileExplorer from @vibecook/mille in this process',
    );
  }
  return candidate;
}

function defined<T extends Record<string, unknown>>(value: T): T {
  for (const key of Object.keys(value)) {
    if (value[key] === undefined) delete value[key];
  }
  return value;
}

function listen<T>(cb: (value: T) => void): JsonListener {
  return (json: string) => {
    let value: T;
    try {
      value = JSON.parse(json) as T;
    } catch {
      return;
    }
    try {
      cb(value);
    } catch {
      // Swallowed, as the explorer's listeners are (`client-port.ts`): a
      // listener bug must not crash the host process from a native callback.
    }
  };
}

/**
 * The native git engine. One per process is the intended shape (§19.1): it
 * de-dupes sessions across every root it opens.
 */
export class GitEngine {
  readonly #native: NativeGitEngine;
  readonly #subscriptions = new Set<number>();
  #nextOperation = 0;
  #disposed = false;

  constructor(options: GitEngineOptions) {
    const Ctor = nativeGitEngineCtor();
    if (Ctor === null) {
      throw new GitError(
        'EUNSUPPORTED',
        'this @vibecook/mille native binary was built without the git engine ' +
          '(build it with `build:napi:git`); check isNativeGitAvailable() first',
      );
    }
    const native: Record<string, unknown> = defined({
      allowedRoots: [...options.allowedRoots],
      gitPath: options.gitPath,
      statusRowCap: options.statusRowCap,
      debounceMs: options.debounceMs,
      maxWaitMs: options.maxWaitMs,
      idleRescanMs: options.idleRescanMs,
      autoRenderMaxBytes: options.autoRenderMaxBytes,
      autoRenderMaxLines: options.autoRenderMaxLines,
      hardRefuseBytes: options.hardRefuseBytes,
      logPageDefault: options.logPageDefault,
      commitTimeoutMs: options.commitTimeoutMs,
      writePerformanceConfig: options.writePerformanceConfig,
    });
    this.#native = wrapGitSync(() => new Ctor(native));
  }

  /** @internal — the raw napi instance, for `bindExplorer`. */
  get native(): NativeGitEngine {
    return this.#native;
  }

  /** The git binary writes use, or `null` when none was found (`ENOGIT`). */
  get gitPath(): string | null {
    return wrapGitSync(() => this.#native.gitPath());
  }

  async open(worktreePath: string): Promise<GitIdentity> {
    return JSON.parse(await this.#call(() => this.#native.open(worktreePath))) as GitIdentity;
  }

  async close(worktreePath: string): Promise<void> {
    await this.#call(() => this.#native.close(worktreePath));
  }

  identity(worktreePath: string): GitIdentity {
    return JSON.parse(wrapGitSync(() => this.#native.identity(worktreePath))) as GitIdentity;
  }

  async status(worktreePath: string): Promise<StatusSnapshot> {
    return JSON.parse(await this.#call(() => this.#native.status(worktreePath))) as StatusSnapshot;
  }

  /** Run a status now: full for `null`, else limited to these paths (§10.6). */
  async refresh(worktreePath: string, paths: readonly string[] | null): Promise<StatusSnapshot> {
    const list = paths === null ? null : [...paths];
    const json = await this.#call(() => this.#native.refresh(worktreePath, list));
    return JSON.parse(json) as StatusSnapshot;
  }

  /**
   * Path hints: **worktree-relative POSIX** paths, `null` for a full status.
   * Arms the session's debounce and returns at once.
   */
  hintPaths(worktreePath: string, paths: readonly string[] | null): void {
    wrapGitSync(() =>
      this.#native.hintPaths(worktreePath, paths === null ? null : [...paths]),
    );
  }

  setStatusBackend(worktreePath: string, backend: 'gix' | 'cli'): void {
    wrapGitSync(() => this.#native.setStatusBackend(worktreePath, backend));
  }

  async diff(worktreePath: string, request: DiffRequest): Promise<DiffResult> {
    const json = await this.#cancellable(request.signal, (op) =>
      this.#native.diff(
        worktreePath,
        {
          path: request.path,
          spec: request.spec,
          commit: request.commit,
          maxBytes: request.maxBytes,
          maxLines: request.maxLines,
        },
        op,
      ),
    );
    return JSON.parse(json) as DiffResult;
  }

  async log(worktreePath: string, request: LogRequest = {}): Promise<LogPage> {
    const json = await this.#cancellable(request.signal, (op) =>
      this.#native.log(
        worktreePath,
        {
          path: request.path,
          limit: request.limit,
          skip: request.skip,
          revision: request.revision,
        },
        op,
      ),
    );
    return JSON.parse(json) as LogPage;
  }

  /** The files `sha` changed (§14.3); `truncated` when the 10 000 cap cut the list. */
  async commitFiles(
    worktreePath: string,
    sha: string,
    opts: { signal?: AbortSignal } = {},
  ): Promise<CommitFileList> {
    const json = await this.#cancellable(opts.signal, (op) =>
      this.#native.commitFiles(worktreePath, sha, op),
    );
    return JSON.parse(json) as CommitFileList;
  }

  async readObject(
    worktreePath: string,
    request: ReadObjectRequest,
  ): Promise<string | Uint8Array | null> {
    const bytes = await this.#cancellable(request.signal, (op) =>
      this.#native.readObject(
        worktreePath,
        { path: request.path, revision: request.revision },
        op,
      ),
    );
    return decodeContents(bytes);
  }

  async readWorktreeFile(
    worktreePath: string,
    path: string,
    opts: { signal?: AbortSignal } = {},
  ): Promise<string | Uint8Array | null> {
    const bytes = await this.#cancellable(opts.signal, (op) =>
      this.#native.readWorktreeFile(worktreePath, path, op),
    );
    return decodeContents(bytes);
  }

  async stage(
    worktreePath: string,
    paths: readonly string[],
    opts: { signal?: AbortSignal } = {},
  ): Promise<void> {
    await this.#cancellable(opts.signal, (op) =>
      this.#native.stage(worktreePath, [...paths], op),
    );
  }

  async unstage(
    worktreePath: string,
    paths: readonly string[],
    opts: { signal?: AbortSignal } = {},
  ): Promise<void> {
    await this.#cancellable(opts.signal, (op) =>
      this.#native.unstage(worktreePath, [...paths], op),
    );
  }

  /** Discard worktree changes, restoring **index** bytes (AC-017). */
  async restore(
    worktreePath: string,
    paths: readonly string[],
    opts: { signal?: AbortSignal } = {},
  ): Promise<void> {
    await this.#cancellable(opts.signal, (op) =>
      this.#native.restore(worktreePath, [...paths], op),
    );
  }

  /** Commit the include-set; resolves to the new commit's 40-hex id. */
  async commit(worktreePath: string, request: CommitRequest): Promise<string> {
    return this.#cancellable(request.signal, (op) =>
      this.#native.commit(
        worktreePath,
        { summary: request.summary, body: request.body, paths: [...request.paths] },
        op,
      ),
    );
  }

  async worktrees(worktreePath: string): Promise<GitWorktree[]> {
    return JSON.parse(await this.#call(() => this.#native.worktrees(worktreePath))) as GitWorktree[];
  }

  async advisePerformanceConfig(worktreePath: string): Promise<Record<string, string>> {
    const json = await this.#call(() => this.#native.advisePerformanceConfig(worktreePath));
    return JSON.parse(json) as Record<string, string>;
  }

  /** Published snapshots of an open worktree. Returns the unsubscriber. */
  onStatus(worktreePath: string, cb: (snapshot: StatusSnapshot) => void): () => void {
    return this.#subscribe(() => this.#native.onStatus(worktreePath, listen(cb)));
  }

  /** Worktree-list changes of an open worktree's repository. */
  onWorktrees(worktreePath: string, cb: (worktrees: GitWorktree[]) => void): () => void {
    return this.#subscribe(() => this.#native.onWorktrees(worktreePath, listen(cb)));
  }

  /**
   * Hear a same-process explorer's watcher: every OS path it reports is
   * routed to the open checkout that contains it and hinted as a
   * worktree-relative path (§11.2). Without this (or `hintPaths` forwarding)
   * only the gitdir watch refreshes status, so a worktree-only save goes
   * unseen. Takes nothing from the explorer's `takePendingChanges`.
   *
   * `fx` is a `FileExplorer` from `@vibecook/mille` (e.g. `host.local`) or the
   * raw native one. Returns the unbinder; `dispose()` also unbinds.
   */
  bindExplorer(fx: unknown): () => void {
    const nativeFx = nativeExplorerOf(fx);
    return this.#subscribe(() => this.#native.bindExplorer(nativeFx));
  }

  /** The engine's `'warning'` channel: degraded states, never thrown (§22.2). */
  onWarning(cb: (warning: GitWarning) => void): () => void {
    return this.#subscribe(() => this.#native.onWarning(listen(cb)));
  }

  /**
   * Cancel in-flight calls, drop every subscription and release the engine.
   * Idempotent; every later call rejects `EINVAL`.
   */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#subscriptions.clear();
    this.#native.dispose();
  }

  /** A native call whose synchronous throw must reject, as a `GitError`. */
  #call<T>(run: () => Promise<T>): Promise<T> {
    return this.#cancellable(undefined, run);
  }

  #subscribe(register: () => number): () => void {
    const id = wrapGitSync(register);
    this.#subscriptions.add(id);
    return () => {
      if (!this.#subscriptions.delete(id)) return;
      this.#native.off(id);
    };
  }

  /**
   * Run a cancellable native call. The `operationId` is registered by the
   * native side synchronously, inside `run`, before this returns — so an
   * abort that fires at any later instant finds the token.
   */
  #cancellable<T>(
    signal: AbortSignal | undefined,
    run: (operationId: string | undefined) => Promise<T>,
  ): Promise<T> {
    if (signal === undefined) {
      try {
        return wrapGit(run(undefined));
      } catch (e: unknown) {
        return Promise.reject(toGitError(e));
      }
    }
    if (signal.aborted) return Promise.reject(abortError());
    this.#nextOperation += 1;
    const operationId = `git-op-${this.#nextOperation}`;
    const onAbort = (): void => {
      this.#native.cancelOperation(operationId);
    };
    let pending: Promise<T>;
    try {
      pending = run(operationId);
    } catch (e: unknown) {
      return Promise.reject(toGitError(e));
    }
    signal.addEventListener('abort', onAbort, { once: true });
    return wrapGit(pending).finally(() => signal.removeEventListener('abort', onAbort));
  }
}
