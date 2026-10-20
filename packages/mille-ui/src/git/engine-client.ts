// Adapters from the engine's `GitService` onto the clients the tree and the
// history views already consume (SPEC mille-git §10.4, §17.2, Appendix A).
//
// Renderer-safe: they speak only the `GitService` interface — no napi, no
// `node:` import — so the same code runs in a utility over the in-process
// transport and in a renderer over a MessagePort transport.
//
// Why each adapter looks the way it does:
//
// - **`GitClient` collapses XY on purpose.** `GitService.status` keeps both
//   porcelain columns (a file staged *and* modified again is `MM`); a tree
//   badge shows one letter. The collapse is the shell client's `xyToEntry`
//   exactly — index letter (staged) wins, else the worktree letter;
//   untracked is `?` and unmerged is `U`, both with `staged` omitted — so
//   swapping the shell client for the engine moves no badge. This is
//   intentionally lossy: a Changes room reads `GitService.status`, never this.
// - **Rows outside the explorer root are dropped.** The engine's status is
//   whole-repository and paths are checkout-relative, but the explorer root
//   may be a package deep inside the checkout. A row the tree cannot resolve
//   would be a decoration on nothing; each kept row's `path` is re-expressed
//   relative to the explorer root, which is what the decoration provider
//   joins back onto it.
// - **`getStatus` never throws** (§23): not-a-repo, a missing git binary, a
//   closed engine — all an empty map, as the shell client does.
// - **No include-set here.** Which paths a commit takes is Changes-room UI
//   state (§15.5); `ScmClient` has no `commit` (§32.4).
// - **History reads `readObject` / `readWorktreeFile`, never `diff`.**
//   `getContents` and `compare` want whole files; `revert` is the engine's
//   index → worktree `restore` (never `--source=HEAD`, AC-017), which is what
//   the shell client's `git restore --worktree` did.
// - **An abort rejects `ABORT_ERR`, as the shell clients do** (§12.5): a
//   plain `Error` with `code: 'ABORT_ERR'`, not the service's
//   `GitError('ECANCELED')`, so swapping the shell client for the engine
//   changes no caller's cancel check. `runScm*` maps it to
//   `ScmActionError('ECANCELED')` for both.
//
// Spelling: the adapter compares paths lexically (it cannot realpath in a
// renderer). Open the worktree and construct the adapter with the same
// spelling of the root — the explorer's — as `open`'s identity keeps the
// spelling it was given.

import { isSafeWorkspaceRelativePath } from '../diagnostics/provider.js';
import type {
  FileHistoryClient,
  FileHistoryQuery,
  FileHistoryRevision,
  ScmClient,
  ScmCompareRequest,
  ScmCompareResult,
} from '../history/types.js';
import type { GitClient, GitStatusEntry } from './client.js';
import { GitError, isGitError, type GitService, type StatusEntry, type StatusSnapshot } from './service-types.js';

// ─── Paths (lexical, POSIX, renderer-safe) ────────────────────────────

function toPosix(path: string): string {
  const slashed = path.replace(/\\/g, '/');
  return slashed.length > 1 ? slashed.replace(/\/+$/, '') : slashed;
}

/** Windows drive paths compare case-insensitively; POSIX paths do not. */
function sameSpelling(a: string, b: string): boolean {
  return /^[A-Za-z]:/.test(a) ? a.toLowerCase() === b.toLowerCase() : a === b;
}

/**
 * `abs` relative to `root` (both POSIX, absolute): `''` when equal, `null`
 * when `abs` is not under `root`.
 */
function relativeUnder(root: string, abs: string): string | null {
  if (sameSpelling(root, abs)) return '';
  const prefix = root.endsWith('/') ? root : `${root}/`;
  if (abs.length <= prefix.length) return null;
  return sameSpelling(abs.slice(0, prefix.length), prefix) ? abs.slice(prefix.length) : null;
}

function joinPosix(root: string, rel: string): string {
  if (rel.length === 0) return root;
  return root.endsWith('/') ? `${root}${rel}` : `${root}/${rel}`;
}

/**
 * The renderer-safe twin of `assertPathUnderRoot`: a non-empty relative POSIX
 * path with no `..`, `.`, empty segment, drive, backslash or NUL. Same
 * message as the shell client so callers matching on it keep working.
 */
function assertRelative(path: string): string {
  if (typeof path !== 'string' || path.length === 0) {
    throw new GitError('EINVAL', 'path must be a non-empty workspace-relative path');
  }
  if (!isSafeWorkspaceRelativePath(path)) {
    throw new GitError('EINVAL', `path escapes workspace root: ${path}`, path);
  }
  return path;
}

/**
 * What an aborted adapter call rejects with: a plain `Error` whose `code` is
 * `'ABORT_ERR'` — exactly what the shell clients throw (`shell-history.ts`
 * `runGit`), so a caller written against them recognises an engine abort
 * unchanged (§12.5). `GitService` itself keeps `GitError('ECANCELED')`;
 * `runScm*` turns either into `ScmActionError('ECANCELED')`.
 */
function abortError(cause?: unknown): Error {
  const err =
    cause === undefined
      ? new Error('git operation aborted')
      : new Error('git operation aborted', { cause });
  (err as Error & { code?: string }).code = 'ABORT_ERR';
  return err;
}

/**
 * Await an engine call made with `signal`. Once the caller's signal has
 * fired, whatever the call rejected with — the engine's `ECANCELED`, or a
 * failure that raced the abort — surfaces as `ABORT_ERR`, as it does from
 * the shell clients. An `ECANCELED` with no abort behind it (the worktree
 * closed or the engine was disposed under the call) is not the caller's
 * cancel and passes through as the engine said it.
 */
async function abortable<T>(signal: AbortSignal | undefined, call: () => Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (error) {
    if (signal?.aborted) throw abortError(error);
    throw error;
  }
}

// ─── GitClient ────────────────────────────────────────────────────────

/** §10.4: one letter per row, exactly as the shell client's `xyToEntry`. */
function collapse(entry: StatusEntry, path: string): GitStatusEntry | null {
  if (entry.conflicted) return { path, status: 'U' };
  if (entry.untracked) return { path, status: '?' };
  if (entry.xy.index !== '.') return { path, status: entry.xy.index, staged: true };
  if (entry.xy.worktree !== '.') return { path, status: entry.xy.worktree, staged: false };
  return null;
}

/**
 * Keys: absolute POSIX paths (checkout root + row path). Values: the row's
 * path relative to the explorer `root`. Rows outside `root` are omitted.
 */
function toStatusMap(snapshot: StatusSnapshot, root: string): Map<string, GitStatusEntry> {
  const map = new Map<string, GitStatusEntry>();
  const checkout = toPosix(snapshot.worktreePath);
  for (const entry of snapshot.entries) {
    const key = joinPosix(checkout, entry.path);
    const rel = relativeUnder(root, key);
    if (rel === null || rel.length === 0) continue;
    const collapsed = collapse(entry, rel);
    if (collapsed !== null) map.set(key, collapsed);
  }
  return map;
}

/**
 * A `GitClient` over `service`'s status of the worktree opened as
 * `rootPath` (§17.2): `service.open(rootPath)` first. `getStatus` never
 * throws; `onChange` fires on every published snapshot and keeps the latest
 * one cached, so the provider's re-fetch is a map build, not another walk.
 */
export function createGitServiceClient(service: GitService, rootPath: string): GitClient {
  const root = toPosix(rootPath);
  const listeners = new Set<() => void>();
  let unsubscribe: (() => void) | null = null;
  let latest: StatusSnapshot | null = null;

  function remember(snapshot: StatusSnapshot): void {
    if (latest === null || snapshot.generation >= latest.generation) latest = snapshot;
  }

  function subscribe(): void {
    if (unsubscribe !== null) return;
    try {
      unsubscribe = service.onStatus(rootPath, (snapshot) => {
        remember(snapshot);
        for (const listener of [...listeners]) {
          try {
            listener();
          } catch {
            /* swallow — one listener mustn't block others */
          }
        }
      });
    } catch {
      unsubscribe = null;
    }
  }

  function unsubscribeIfIdle(): void {
    if (listeners.size > 0 || unsubscribe === null) return;
    unsubscribe();
    unsubscribe = null;
    latest = null; // only trusted while a subscription keeps it current
  }

  return {
    async getStatus(): Promise<ReadonlyMap<string, GitStatusEntry>> {
      try {
        let snapshot = unsubscribe !== null ? latest : null;
        if (snapshot === null) {
          snapshot = await service.status(rootPath);
          if (unsubscribe !== null) remember(snapshot);
        }
        return toStatusMap(snapshot, root);
      } catch {
        return new Map();
      }
    },
    onChange(cb: () => void): () => void {
      const entry = (): void => cb();
      listeners.add(entry);
      subscribe();
      let active = true;
      return () => {
        if (!active) return;
        active = false;
        listeners.delete(entry);
        unsubscribeIfIdle();
      };
    },
  };
}

// ─── Root → checkout path translation ─────────────────────────────────

/**
 * Translate explorer-root-relative paths into the checkout-relative paths the
 * engine speaks. The explorer root may sit inside the checkout (a monorepo
 * package); its offset comes from the opened worktree's identity, fetched
 * once.
 */
function checkoutPaths(service: GitService, worktreePath: string) {
  let offset: Promise<{ checkout: string; prefix: string }> | null = null;

  function offsetOf(): Promise<{ checkout: string; prefix: string }> {
    if (offset === null) {
      offset = service.identity(worktreePath).then(
        (identity) => {
          const checkout = toPosix(identity.worktreePath);
          const prefix = relativeUnder(checkout, toPosix(worktreePath)) ?? '';
          return { checkout, prefix };
        },
        (error: unknown) => {
          offset = null; // retry next time (the worktree may open later)
          throw error;
        },
      );
    }
    return offset;
  }

  /** Validate `paths` (relative to `rootPath`, default the worktree) → checkout-relative. */
  return async function translate(
    paths: readonly string[],
    rootPath: string | undefined,
  ): Promise<string[]> {
    const safe = paths.map(assertRelative);
    const { checkout, prefix } = await offsetOf();
    let base = prefix;
    if (rootPath !== undefined && !sameSpelling(toPosix(rootPath), toPosix(worktreePath))) {
      const rel = relativeUnder(checkout, toPosix(rootPath));
      if (rel === null) {
        throw new GitError('EINVAL', `rootPath is outside the checkout: ${rootPath}`, rootPath);
      }
      base = rel;
    }
    return safe.map((p) => (base.length === 0 ? p : `${base}/${p}`));
  };
}

function safeLimit(limit: unknown, fallback: number): number {
  const n = typeof limit === 'number' ? limit : Number(limit);
  if (!Number.isFinite(n) || n <= 0) return fallback;
  return Math.min(Math.floor(n), 10_000);
}

/** Codes that mean "no history here", which the shell client reports as `[]` / `null`. */
const QUIET_CODES = new Set(['ENOTAREPO', 'ENOGIT', 'ENOENT', 'EACCES', 'EUNKNOWN', 'EUNSUPPORTED']);

function isQuiet(error: unknown): boolean {
  return isGitError(error) && QUIET_CODES.has(error.code);
}

// ─── FileHistoryClient ────────────────────────────────────────────────

/**
 * A `FileHistoryClient` over `service` for the worktree opened as
 * `worktreePath`: `getHistory` is a path-filtered `log` page (default 50),
 * `getContents` is `readObject`. Escaping paths and option-like revisions
 * throw; a missing repository or git binary is an empty history / `null`.
 */
export function createGitServiceHistoryClient(
  service: GitService,
  worktreePath: string,
): FileHistoryClient {
  const translate = checkoutPaths(service, worktreePath);
  return {
    async getHistory(query: FileHistoryQuery): Promise<readonly FileHistoryRevision[]> {
      const [path] = await translate([query.path], query.rootPath);
      try {
        const page = await service.log(worktreePath, {
          path: path as string,
          limit: safeLimit(query.limit, 50),
        });
        return page.commits.map((commit) => {
          const revision: {
            -readonly [K in keyof FileHistoryRevision]: FileHistoryRevision[K];
          } = { id: commit.id, shortId: commit.shortId, timestampMs: commit.timestampMs };
          if (commit.author !== undefined) revision.author = commit.author;
          if (commit.message !== undefined) revision.message = commit.message;
          if (commit.parents !== undefined) revision.parents = commit.parents;
          return revision;
        });
      } catch (error) {
        if (isQuiet(error)) return [];
        throw error;
      }
    },
    async getContents(query) {
      const [path] = await translate([query.path], query.rootPath);
      try {
        return await service.readObject(worktreePath, {
          path: path as string,
          revision: query.revision,
        });
      } catch (error) {
        if (isQuiet(error)) return null;
        throw error;
      }
    },
  };
}

// ─── ScmClient ────────────────────────────────────────────────────────

/**
 * An `ScmClient` over `service` for the worktree opened as `worktreePath`
 * (§17.2): `revert` → `restore` (index → worktree), `stage` / `unstage` by
 * name, `compare` from `readWorktreeFile` / `readObject` with the shell
 * client's labels (`'Working Tree'` / the revision). No `commit` (§32.4).
 */
export function createGitServiceScmClient(service: GitService, worktreePath: string): ScmClient {
  const translate = checkoutPaths(service, worktreePath);

  function signalOpts(signal: AbortSignal | undefined): { signal?: AbortSignal } {
    return signal === undefined ? {} : { signal };
  }

  function sideLabel(side: ScmCompareRequest['left']): string {
    return side.kind === 'working' ? 'Working Tree' : side.revision;
  }

  return {
    async revert(paths, opts) {
      if (paths.length === 0) return;
      const targets = await translate(paths, opts?.rootPath);
      const signal = opts?.signal;
      await abortable(signal, () => service.restore(worktreePath, targets, signalOpts(signal)));
    },
    async stage(paths, opts) {
      if (paths.length === 0) return;
      const targets = await translate(paths, opts?.rootPath);
      const signal = opts?.signal;
      await abortable(signal, () => service.stage(worktreePath, targets, signalOpts(signal)));
    },
    async unstage(paths, opts) {
      if (paths.length === 0) return;
      const targets = await translate(paths, opts?.rootPath);
      const signal = opts?.signal;
      await abortable(signal, () => service.unstage(worktreePath, targets, signalOpts(signal)));
    },
    async compare(request, opts): Promise<ScmCompareResult> {
      const [path] = await translate([request.path], request.rootPath);
      const target = path as string;
      const signal = opts?.signal;
      if (signal?.aborted) throw abortError();

      async function load(side: ScmCompareRequest['left']): Promise<string | Uint8Array | null> {
        if (signal?.aborted) throw abortError();
        try {
          return await abortable(signal, () =>
            side.kind === 'working'
              ? service.readWorktreeFile(worktreePath, target, signalOpts(signal))
              : service.readObject(worktreePath, {
                  path: target,
                  revision: side.revision,
                  ...signalOpts(signal),
                }),
          );
        } catch (error) {
          if (isQuiet(error)) return null; // a missing side is not an error
          throw error;
        }
      }

      return {
        path: request.path,
        leftLabel: sideLabel(request.left),
        rightLabel: sideLabel(request.right),
        left: await load(request.left),
        right: await load(request.right),
      };
    },
  };
}
