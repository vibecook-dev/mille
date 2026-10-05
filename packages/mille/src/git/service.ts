// `createGitService(transport)` — the `GitService` façade over any
// `GitTransport` (SPEC mille-git §17.1, §19.3).
//
// Deliberately transport-blind and native-free: it imports no napi, no
// `node:` module and nothing from `./engine.js`, so the same façade serves an
// in-process transport in a utility today and a MessagePort transport in a
// renderer tomorrow (PR 8), and an ADE room can be built and tested against a
// fake transport with no binary at all (AC-012). Every verb is one
// `transport.call(method, args, { signal })` from the `GitCallTable`; events
// arrive keyed by the worktree spelling `open` was given and fan out here.
//
// One argument only (§17.1): `allowedRoots` and every other engine option
// belong to the transport (`createInProcessGitTransport(options)`), because a
// renderer-side service must not be able to widen what the engine will touch.

import type {
  GitCallMethod,
  GitCallTable,
  GitService,
  GitTransport,
  GitTransportEvents,
  GitWorktree,
  StatusSnapshot,
} from './types.js';

type Listener<T> = (value: T) => void;

class Fanout<T> {
  readonly #byWorktree = new Map<string, Set<Listener<T>>>();

  add(worktree: string, cb: Listener<T>): () => void {
    let set = this.#byWorktree.get(worktree);
    if (set === undefined) {
      set = new Set();
      this.#byWorktree.set(worktree, set);
    }
    // Wrap so the same callback registered twice is two subscriptions.
    const entry: Listener<T> = (value) => cb(value);
    set.add(entry);
    return () => {
      const current = this.#byWorktree.get(worktree);
      if (current === undefined) return;
      current.delete(entry);
      if (current.size === 0) this.#byWorktree.delete(worktree);
    };
  }

  emit(worktree: string, value: T): void {
    const set = this.#byWorktree.get(worktree);
    if (set === undefined) return;
    for (const cb of [...set]) {
      try {
        cb(value);
      } catch {
        // Swallowed, as the explorer's listeners are: one bad subscriber must
        // not block the others, nor crash the host process.
      }
    }
  }

  clear(): void {
    this.#byWorktree.clear();
  }
}

function signalOpts(signal: AbortSignal | undefined): { signal?: AbortSignal } | undefined {
  return signal === undefined ? undefined : { signal };
}

/** Build the `GitService` façade over `transport`. It owns the transport. */
export function createGitService(transport: GitTransport): GitService {
  const statusListeners = new Fanout<StatusSnapshot>();
  const worktreeListeners = new Fanout<readonly GitWorktree[]>();
  let disposed = false;

  const offStatus = transport.on('status', (body) => {
    const { worktree, snapshot } = body as GitTransportEvents['status'];
    statusListeners.emit(worktree, snapshot);
  });
  const offWorktrees = transport.on('worktrees', (body) => {
    const { worktree, worktrees } = body as GitTransportEvents['worktrees'];
    worktreeListeners.emit(worktree, worktrees);
  });

  function call<M extends GitCallMethod>(
    method: M,
    args: GitCallTable[M]['args'],
    signal?: AbortSignal,
  ): Promise<GitCallTable[M]['result']> {
    return transport.call(method, args, signalOpts(signal)) as Promise<
      GitCallTable[M]['result']
    >;
  }

  return {
    open: (worktree) => call('open', { worktree }),
    close: async (worktree) => {
      await call('close', { worktree });
    },
    identity: (worktree) => call('identity', { worktree }),
    status: (worktree) => call('status', { worktree }),
    onStatus: (worktree, cb) => statusListeners.add(worktree, cb),

    diff: (worktree, { signal, ...request }) => call('diff', { worktree, request }, signal),
    log: (worktree, { signal, ...request } = {}) => call('log', { worktree, request }, signal),
    commitFiles: (worktree, sha) => call('commitFiles', { worktree, sha }),
    readObject: (worktree, { signal, ...request }) =>
      call('readObject', { worktree, request }, signal),
    readWorktreeFile: (worktree, path, opts) =>
      call('readWorktreeFile', { worktree, path }, opts?.signal),

    stage: async (worktree, paths, opts) => {
      await call('stage', { worktree, paths: [...paths] }, opts?.signal);
    },
    unstage: async (worktree, paths, opts) => {
      await call('unstage', { worktree, paths: [...paths] }, opts?.signal);
    },
    restore: async (worktree, paths, opts) => {
      await call('restore', { worktree, paths: [...paths] }, opts?.signal);
    },
    commit: (worktree, { signal, ...request }) =>
      call('commit', { worktree, request: { ...request, paths: [...request.paths] } }, signal),

    worktrees: (worktree) => call('worktrees', { worktree }),
    onWorktrees: (worktree, cb) => worktreeListeners.add(worktree, cb),

    advisePerformanceConfig: (worktree) => call('advisePerformanceConfig', { worktree }),

    async dispose() {
      if (disposed) return;
      disposed = true;
      offStatus();
      offWorktrees();
      statusListeners.clear();
      worktreeListeners.clear();
      transport.close();
    },
  } satisfies GitService;
}
