// FileSystemError — typed error surface for the @vibecook/mille
// client. Mirrors api.d.ts and is shaped after VS Code's FileSystemError
// so consumers writing generic filesystem code can branch on `.code`.
//
// The native binding (fx-binding/src/error.rs) encodes every FxError as
// a napi::Error whose `reason` carries the structured payload
// `"FX|<code>|<path>|<message>"`. The TS client unpacks that reason back
// into a FileSystemError and rethrows with the stack trace preserved so
// callers see useful traces even though the error traveled through napi.

/** All error codes the Rust side can emit. See fx-core::ErrorCode. */
export type ErrorCode =
  | 'EACCES'
  | 'ENOENT'
  | 'EEXIST'
  | 'EISDIR'
  | 'ENOTDIR'
  | 'ELOOP'
  | 'ENOSPC'
  | 'EROFS'
  | 'EBUSY'
  | 'EINVAL'
  | 'ECANCELED'
  | 'EFBIG'
  | 'EUNSUPPORTED'
  | 'EUNKNOWN'
  /** A native read made no progress within `readTimeoutMs`, or one into the same folder is still stuck. */
  | 'ETIMEDOUT'
  /** A cloud placeholder whose contents are not local, and `materializeDataless` is off. */
  | 'EDATALESS';

export class FileSystemError extends Error {
  readonly code: ErrorCode;
  readonly path?: string;

  constructor(code: ErrorCode, message: string, path?: string) {
    super(message);
    this.name = 'FileSystemError';
    this.code = code;
    // Leave `.path` undefined when the Rust side emitted an empty path
    // (non-Io errors). Callers narrow with `if (err.path)` per api.d.ts.
    if (path !== undefined && path.length > 0) {
      this.path = path;
    }
  }
}

/** Narrow an `unknown` thrown value to FileSystemError. */
export function isFileSystemError(e: unknown): e is FileSystemError {
  return e instanceof FileSystemError;
}

const FX_PREFIX = 'FX|';

/**
 * Parse the "FX|<code>|<path>|<message>" wire format from fx-binding's
 * error.rs into a FileSystemError. Returns null for anything not in our
 * format so callers can rethrow the original error untouched.
 *
 * The message segment may itself contain `|` characters; we keep only
 * the first two separators as structural and treat everything after the
 * second `|` as the message (splitn-3 semantics in Rust parlance).
 *
 * @internal — exported for the test suite; not part of the public API.
 */
export function tryParseFxReason(reason: unknown): FileSystemError | null {
  if (typeof reason !== 'string' || !reason.startsWith(FX_PREFIX)) {
    return null;
  }
  const withoutPrefix = reason.slice(FX_PREFIX.length);
  const firstPipe = withoutPrefix.indexOf('|');
  if (firstPipe < 0) return null;
  const code = withoutPrefix.slice(0, firstPipe) as ErrorCode;
  const afterCode = withoutPrefix.slice(firstPipe + 1);
  const secondPipe = afterCode.indexOf('|');
  if (secondPipe < 0) return null;
  const path = afterCode.slice(0, secondPipe);
  const message = afterCode.slice(secondPipe + 1);
  return new FileSystemError(code, message, path.length > 0 ? path : undefined);
}

/**
 * Wrap a native async call so rejected FX-reason errors surface as
 * typed FileSystemError. Non-FX errors pass through untouched. All
 * typed client methods that touch the native binding route through
 * this helper.
 */
export async function wrap<T>(promise: Promise<T>): Promise<T> {
  try {
    return await promise;
  } catch (e: unknown) {
    throw rethrowFx(e);
  }
}

/** Synchronous counterpart to `wrap` for non-async native calls. */
export function wrapSync<T>(fn: () => T): T {
  try {
    return fn();
  } catch (e: unknown) {
    throw rethrowFx(e);
  }
}

function rethrowFx(e: unknown): unknown {
  // napi::Error surfaces in JS as an Error with .message = reason.
  const maybeMessage = (e as { message?: unknown } | null)?.message;
  const fxErr = tryParseFxReason(maybeMessage);
  if (fxErr) {
    // Prefer the original stack so the trace points at the call site
    // rather than the wrap() internals.
    const originalStack = (e as { stack?: unknown } | null)?.stack;
    if (typeof originalStack === 'string') {
      fxErr.stack = originalStack;
    }
    return fxErr;
  }
  return e;
}
