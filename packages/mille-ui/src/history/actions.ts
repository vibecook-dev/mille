// Phase 5.3 — run SCM actions with confirm + progress + cancellation.

import type {
  ScmActionHooks,
  ScmClient,
  ScmCompareRequest,
  ScmCompareResult,
  ScmProgressEvent,
} from './types.js';
import { ScmActionError } from './types.js';

export interface RunScmActionOptions {
  readonly client: ScmClient;
  readonly hooks?: ScmActionHooks;
  /** Stable id for progress correlation. Default: auto. */
  readonly operationId?: string;
  readonly signal?: AbortSignal;
}

let nextOpId = 1;

function makeOpId(): string {
  const id = `scm-${nextOpId}`;
  nextOpId += 1;
  return id;
}

function emit(
  hooks: ScmActionHooks | undefined,
  event: ScmProgressEvent,
): void {
  try {
    hooks?.onProgress?.(event);
  } catch {
    /* ignore progress handler failures */
  }
}

function throwIfAborted(signal: AbortSignal | undefined, action: string): void {
  if (signal?.aborted) {
    throw new ScmActionError('ECANCELED', action, `${action} cancelled`);
  }
}

/**
 * SPEC mille-git §12.5: a client call that ended in an abort — the shell and
 * engine adapters' `ABORT_ERR`, or any failure once `signal` has fired — is
 * a cancel, reported as `ScmActionError('ECANCELED')` and the `cancelled`
 * phase rather than a failure. Anything else passes through.
 */
function asCancel(error: unknown, signal: AbortSignal | undefined, action: string): unknown {
  if (error instanceof ScmActionError) return error;
  const code =
    typeof error === 'object' && error !== null ? (error as { code?: unknown }).code : undefined;
  if (signal?.aborted || code === 'ABORT_ERR') {
    return new ScmActionError('ECANCELED', action, `${action} cancelled`);
  }
  return error;
}

/**
 * Discard working-tree changes for `paths` after optional confirmation.
 */
export async function runScmRevert(
  paths: readonly string[],
  options: RunScmActionOptions & { rootPath?: string; confirmMessage?: string },
): Promise<void> {
  const action = 'scm.revert';
  if (paths.length === 0) {
    throw new ScmActionError('EINVAL', action, 'no paths to revert');
  }
  if (typeof options.client.revert !== 'function') {
    throw new ScmActionError(
      'EUNSUPPORTED',
      action,
      'SCM client does not support revert',
    );
  }

  const operationId = options.operationId ?? makeOpId();
  const hooks = options.hooks;
  const message =
    options.confirmMessage ??
    (paths.length === 1
      ? `Discard changes to ${paths[0]}?`
      : `Discard changes to ${paths.length} files?`);

  emit(hooks, {
    operationId,
    action,
    phase: 'preparing',
    message,
    paths,
    fraction: 0,
  });

  throwIfAborted(options.signal, action);

  const ok = hooks?.confirm ? await hooks.confirm(message) : true;
  if (!ok) {
    emit(hooks, {
      operationId,
      action,
      phase: 'cancelled',
      message: 'User declined',
      paths,
    });
    throw new ScmActionError('ECANCELED', action, 'revert declined by user');
  }

  emit(hooks, {
    operationId,
    action,
    phase: 'running',
    message: 'Reverting…',
    paths,
    fraction: 0.2,
  });

  try {
    throwIfAborted(options.signal, action);
    const revertOpts: { rootPath?: string; signal?: AbortSignal } = {};
    if (options.rootPath !== undefined) revertOpts.rootPath = options.rootPath;
    if (options.signal !== undefined) revertOpts.signal = options.signal;
    await options.client.revert(paths, revertOpts);
    emit(hooks, {
      operationId,
      action,
      phase: 'completed',
      message: 'Reverted',
      paths,
      fraction: 1,
    });
  } catch (thrown) {
    const error = asCancel(thrown, options.signal, action);
    if (error instanceof ScmActionError) {
      emit(hooks, {
        operationId,
        action,
        phase: error.code === 'ECANCELED' ? 'cancelled' : 'failed',
        message: error.message,
        paths,
      });
      throw error;
    }
    emit(hooks, {
      operationId,
      action,
      phase: 'failed',
      message: error instanceof Error ? error.message : String(error),
      paths,
    });
    try {
      hooks?.onError?.(error, { action, paths });
    } catch {
      /* ignore */
    }
    throw error;
  }
}

/**
 * Compare two revisions/sides with progress reporting.
 */
export async function runScmCompare(
  request: ScmCompareRequest,
  options: RunScmActionOptions,
): Promise<ScmCompareResult | void> {
  const action = 'scm.compare';
  if (typeof options.client.compare !== 'function') {
    throw new ScmActionError(
      'EUNSUPPORTED',
      action,
      'SCM client does not support compare',
    );
  }

  const operationId = options.operationId ?? makeOpId();
  const hooks = options.hooks;
  const paths = [request.path];

  emit(hooks, {
    operationId,
    action,
    phase: 'preparing',
    message: 'Preparing compare…',
    paths,
    fraction: 0,
  });
  throwIfAborted(options.signal, action);

  emit(hooks, {
    operationId,
    action,
    phase: 'running',
    message: 'Comparing…',
    paths,
    fraction: 0.3,
  });

  try {
    throwIfAborted(options.signal, action);
    const compareOpts: { signal?: AbortSignal } = {};
    if (options.signal !== undefined) compareOpts.signal = options.signal;
    const result = await options.client.compare(request, compareOpts);
    emit(hooks, {
      operationId,
      action,
      phase: 'completed',
      message: 'Compare ready',
      paths,
      fraction: 1,
    });
    return result;
  } catch (thrown) {
    const error = asCancel(thrown, options.signal, action);
    if (error instanceof ScmActionError) {
      emit(hooks, {
        operationId,
        action,
        phase: error.code === 'ECANCELED' ? 'cancelled' : 'failed',
        message: error.message,
        paths,
      });
      throw error;
    }
    emit(hooks, {
      operationId,
      action,
      phase: 'failed',
      message: error instanceof Error ? error.message : String(error),
      paths,
    });
    try {
      hooks?.onError?.(error, { action, paths });
    } catch {
      /* ignore */
    }
    throw error;
  }
}

/**
 * Load a file timeline via the history client with progress events.
 */
export async function runFileHistory(
  client: {
    getHistory(
      query: import('./types.js').FileHistoryQuery,
    ): Promise<readonly import('./types.js').FileHistoryRevision[]>;
  },
  query: import('./types.js').FileHistoryQuery,
  options: {
    hooks?: ScmActionHooks;
    operationId?: string;
    signal?: AbortSignal;
  } = {},
): Promise<readonly import('./types.js').FileHistoryRevision[]> {
  const action = 'scm.history';
  const operationId = options.operationId ?? makeOpId();
  const hooks = options.hooks;
  const paths = [query.path];

  emit(hooks, {
    operationId,
    action,
    phase: 'preparing',
    message: 'Loading history…',
    paths,
    fraction: 0,
  });
  throwIfAborted(options.signal, action);

  emit(hooks, {
    operationId,
    action,
    phase: 'running',
    message: 'Reading revisions…',
    paths,
    fraction: 0.4,
  });

  try {
    throwIfAborted(options.signal, action);
    const revisions = await client.getHistory(query);
    emit(hooks, {
      operationId,
      action,
      phase: 'completed',
      message: `${revisions.length} revision(s)`,
      paths,
      fraction: 1,
    });
    return revisions;
  } catch (error) {
    emit(hooks, {
      operationId,
      action,
      phase: 'failed',
      message: error instanceof Error ? error.message : String(error),
      paths,
    });
    try {
      hooks?.onError?.(error, { action, paths });
    } catch {
      /* ignore */
    }
    throw error;
  }
}
