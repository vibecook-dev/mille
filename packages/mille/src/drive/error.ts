// DriveError — the typed failure of every drive call.
//
// The native binding encodes a `mille_drive::DriveError` as a napi error whose
// message is `DRIVE|<code>|<path>|<message>`: a prefix of its own, as git's
// `GIT|` is, because the code unions differ and a host that catches several
// engines must be able to tell which one spoke.
//
// `isDriveError` is structural rather than `instanceof`, as `isGitError` is:
// an error that crossed a process or port is rebuilt on the far side.

import type { DriveErrorCode } from './types.js';

const DRIVE_PREFIX = 'DRIVE|';

const CODES: ReadonlySet<string> = new Set<DriveErrorCode>([
  'EIO',
  'ENOENT',
  'EEXIST',
  'EMISMATCH',
  'ETOOBIG',
  'ENOSPC',
  'EINVAL',
  'EBUSY',
  'EFORMAT',
  'EPROTO',
  'ETRANSPORT',
  'EUNKNOWNITEM',
  'EUNKNOWNVERSION',
  'EUNSUPPORTED',
  'EUNKNOWN',
]);

export class DriveError extends Error {
  readonly code: DriveErrorCode;
  readonly path?: string;

  constructor(code: DriveErrorCode, message: string, path?: string) {
    super(message);
    this.name = 'DriveError';
    this.code = code;
    if (path !== undefined && path.length > 0) {
      this.path = path;
    }
  }
}

/** Narrow a thrown value to a drive error, whichever package built it. */
export function isDriveError(e: unknown): e is DriveError {
  return (
    e instanceof Error &&
    e.name === 'DriveError' &&
    typeof (e as { code?: unknown }).code === 'string'
  );
}

/**
 * Parse `DRIVE|<code>|<path>|<message>`. `null` for anything else. The message
 * may contain `|`; only the first two separators are structural.
 *
 * @internal — exported for tests.
 */
export function tryParseDriveReason(reason: unknown): DriveError | null {
  if (typeof reason !== 'string' || !reason.startsWith(DRIVE_PREFIX)) return null;
  const rest = reason.slice(DRIVE_PREFIX.length);
  const first = rest.indexOf('|');
  if (first < 0) return null;
  const afterCode = rest.slice(first + 1);
  const second = afterCode.indexOf('|');
  if (second < 0) return null;
  const rawCode = rest.slice(0, first);
  const code = (CODES.has(rawCode) ? rawCode : 'EUNKNOWN') as DriveErrorCode;
  const path = afterCode.slice(0, second);
  return new DriveError(code, afterCode.slice(second + 1), path.length > 0 ? path : undefined);
}

/** Map a napi rejection to a `DriveError`, keeping the original stack. */
export function toDriveError(e: unknown): unknown {
  const parsed = tryParseDriveReason((e as { message?: unknown } | null)?.message);
  if (parsed === null) return e;
  const stack = (e as { stack?: unknown } | null)?.stack;
  if (typeof stack === 'string') parsed.stack = stack;
  return parsed;
}

export async function wrapDriveAsync<T>(promise: Promise<T>): Promise<T> {
  try {
    return await promise;
  } catch (e: unknown) {
    throw toDriveError(e);
  }
}

export function wrapDrive<T>(fn: () => T): T {
  try {
    return fn();
  } catch (e: unknown) {
    throw toDriveError(e);
  }
}
