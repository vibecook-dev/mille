// GitError — the typed failure of every git engine call (SPEC mille-git §18.2).
//
// The native binding encodes a `mille_git::GitError` as a napi error whose
// message is `GIT|<code>|<path>|<message>`. It is a separate prefix from the
// explorer's `FX|` on purpose: the two code unions differ (`ENOTAREPO`,
// `EHOOK`, `ELOCKED` … have no FileSystemError meaning), and a host that
// catches both must be able to tell which engine spoke.
//
// `isGitError` is structural rather than `instanceof`: the renderer-safe
// contract in `@vibecook/mille-ui/git` declares its own `GitError`, and an
// error that crossed a MessagePort is rebuilt on the far side, so identity of
// the class is never something a caller can rely on. `name` + `code` are.

import type { GitErrorCode } from './types.js';

const GIT_PREFIX = 'GIT|';

const CODES: ReadonlySet<string> = new Set<GitErrorCode>([
  'EINVAL',
  'EACCES',
  'ENOENT',
  'ENOTAREPO',
  'ENOGIT',
  'ECANCELED',
  'EUNSUPPORTED',
  'EHOOK',
  'ETIMEOUT',
  'ELOCKED',
  'EUNKNOWN',
]);

export class GitError extends Error {
  readonly code: GitErrorCode;
  readonly path?: string;

  constructor(code: GitErrorCode, message: string, path?: string) {
    super(message);
    this.name = 'GitError';
    this.code = code;
    if (path !== undefined && path.length > 0) {
      this.path = path;
    }
  }
}

/** Narrow a thrown value to a git engine error, whichever package built it. */
export function isGitError(e: unknown): e is GitError {
  return (
    e instanceof Error &&
    e.name === 'GitError' &&
    typeof (e as { code?: unknown }).code === 'string'
  );
}

/**
 * Parse `GIT|<code>|<path>|<message>`. `null` for anything else. The message
 * may contain `|`; only the first two separators are structural.
 *
 * @internal — exported for tests.
 */
export function tryParseGitReason(reason: unknown): GitError | null {
  if (typeof reason !== 'string' || !reason.startsWith(GIT_PREFIX)) return null;
  const rest = reason.slice(GIT_PREFIX.length);
  const first = rest.indexOf('|');
  if (first < 0) return null;
  const afterCode = rest.slice(first + 1);
  const second = afterCode.indexOf('|');
  if (second < 0) return null;
  const rawCode = rest.slice(0, first);
  const code = (CODES.has(rawCode) ? rawCode : 'EUNKNOWN') as GitErrorCode;
  const path = afterCode.slice(0, second);
  return new GitError(code, afterCode.slice(second + 1), path.length > 0 ? path : undefined);
}

/** Map a napi rejection to a `GitError`, keeping the original stack. */
export function toGitError(e: unknown): unknown {
  const parsed = tryParseGitReason((e as { message?: unknown } | null)?.message);
  if (parsed === null) return e;
  const stack = (e as { stack?: unknown } | null)?.stack;
  if (typeof stack === 'string') parsed.stack = stack;
  return parsed;
}

/** The error an aborted call rejects with (`ECANCELED`, §23). */
export function abortError(): GitError {
  return new GitError('ECANCELED', 'git operation aborted');
}

export async function wrapGit<T>(promise: Promise<T>): Promise<T> {
  try {
    return await promise;
  } catch (e: unknown) {
    throw toGitError(e);
  }
}

export function wrapGitSync<T>(fn: () => T): T {
  try {
    return fn();
  } catch (e: unknown) {
    throw toGitError(e);
  }
}
