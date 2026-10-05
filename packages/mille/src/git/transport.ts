// The in-process `GitTransport` and the `bindExplorer` host helper
// (SPEC mille-git §17.1, §19.1, §19.3).
//
// In-process means the napi engine is in this process — playground's utility,
// a Node CLI, a test. `call` dispatches the `GitCallTable` straight onto
// `GitEngine`; `hintPaths` goes to the engine's `hint_paths`. A cross-process
// transport (the second MessagePort, PR 8) frames the same table, so the
// `GitService` façade above it cannot tell which one it has.
//
// Events are per opened spelling: a successful `open(w)` makes the transport
// forward `w`'s snapshots and worktree lists as `{ worktree: w, … }` until the
// matching `close(w)` — refcounted, like the engine's own sessions — so a
// `GitService.onStatus(w, …)` hears exactly the worktree it opened.

import type { FileExplorer } from '../client.js';
import { GitEngine } from './engine.js';
import { GitError } from './error.js';
import type {
  CommitRequest,
  DiffRequest,
  GitEngineOptions,
  GitTransport,
  GitTransportEvents,
  LogRequest,
  ReadObjectRequest,
} from './types.js';

type EventName = keyof GitTransportEvents;
type Listener = (body: unknown) => void;

/** The engine behind each in-process transport, for `bindExplorer`. */
const engines = new WeakMap<GitTransport, GitEngine>();

function invalid(message: string): GitError {
  return new GitError('EINVAL', message);
}

function stringArg(value: unknown, name: string): string {
  if (typeof value !== 'string' || value.length === 0) {
    throw invalid(`git transport: \`${name}\` must be a non-empty string`);
  }
  return value;
}

function pathsArg(value: unknown): string[] {
  if (!Array.isArray(value) || !value.every((p) => typeof p === 'string')) {
    throw invalid('git transport: `paths` must be an array of strings');
  }
  return value as string[];
}

function objectArg<T>(value: unknown, name: string): T {
  if (value === null || typeof value !== 'object') {
    throw invalid(`git transport: \`${name}\` must be an object`);
  }
  return value as T;
}

function withSignal<T extends object>(request: T, signal: AbortSignal | undefined): T {
  return signal === undefined ? request : { ...request, signal };
}

class InProcessGitTransport implements GitTransport {
  readonly #engine: GitEngine;
  readonly #listeners: Record<EventName, Set<Listener>> = {
    status: new Set(),
    worktrees: new Set(),
    warning: new Set(),
  };
  readonly #opened = new Map<string, { count: number; off: () => void }>();
  readonly #offWarning: () => void;
  #closed = false;

  constructor(engine: GitEngine) {
    this.#engine = engine;
    this.#offWarning = engine.onWarning((warning) => this.#emit('warning', warning));
  }

  call(method: string, args: unknown, opts?: { signal?: AbortSignal }): Promise<unknown> {
    if (this.#closed) return Promise.reject(invalid('the git transport is closed'));
    try {
      return this.#dispatch(method, (args ?? {}) as Record<string, unknown>, opts?.signal);
    } catch (e: unknown) {
      return Promise.reject(e);
    }
  }

  on(event: EventName, cb: Listener): () => void {
    const set = this.#listeners[event];
    if (set === undefined) throw invalid(`unknown git transport event: ${String(event)}`);
    const entry: Listener = (body) => cb(body);
    set.add(entry);
    return () => {
      set.delete(entry);
    };
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const { off } of this.#opened.values()) off();
    this.#opened.clear();
    this.#offWarning();
    for (const set of Object.values(this.#listeners)) set.clear();
    this.#engine.dispose();
  }

  get engine(): GitEngine {
    return this.#engine;
  }

  async #dispatch(
    method: string,
    args: Record<string, unknown>,
    signal: AbortSignal | undefined,
  ): Promise<unknown> {
    const engine = this.#engine;
    const worktree = stringArg(args.worktree, 'worktree');
    switch (method) {
      case 'open': {
        const identity = await engine.open(worktree);
        this.#retain(worktree);
        return identity;
      }
      case 'close':
        this.#release(worktree);
        await engine.close(worktree);
        return undefined;
      case 'identity':
        return engine.identity(worktree);
      case 'status':
        return engine.status(worktree);
      case 'diff':
        return engine.diff(
          worktree,
          withSignal(objectArg<DiffRequest>(args.request, 'request'), signal),
        );
      case 'log':
        return engine.log(
          worktree,
          withSignal(objectArg<LogRequest>(args.request ?? {}, 'request'), signal),
        );
      case 'commitFiles':
        return engine.commitFiles(worktree, stringArg(args.sha, 'sha'), withSignal({}, signal));
      case 'readObject':
        return engine.readObject(
          worktree,
          withSignal(objectArg<ReadObjectRequest>(args.request, 'request'), signal),
        );
      case 'readWorktreeFile':
        return engine.readWorktreeFile(
          worktree,
          stringArg(args.path, 'path'),
          withSignal({}, signal),
        );
      case 'stage':
        return engine.stage(worktree, pathsArg(args.paths), withSignal({}, signal));
      case 'unstage':
        return engine.unstage(worktree, pathsArg(args.paths), withSignal({}, signal));
      case 'restore':
        return engine.restore(worktree, pathsArg(args.paths), withSignal({}, signal));
      case 'commit':
        return engine.commit(
          worktree,
          withSignal(objectArg<CommitRequest>(args.request, 'request'), signal),
        );
      case 'worktrees':
        return engine.worktrees(worktree);
      case 'advisePerformanceConfig':
        return engine.advisePerformanceConfig(worktree);
      case 'hintPaths':
        // Worktree-relative POSIX, already routed; `null` = full status.
        engine.hintPaths(worktree, args.paths === null ? null : pathsArg(args.paths));
        return undefined;
      default:
        throw invalid(`unknown git method: ${method}`);
    }
  }

  #retain(worktree: string): void {
    const opened = this.#opened.get(worktree);
    if (opened !== undefined) {
      opened.count += 1;
      return;
    }
    const offStatus = this.#engine.onStatus(worktree, (snapshot) =>
      this.#emit('status', { worktree, snapshot }),
    );
    const offWorktrees = this.#engine.onWorktrees(worktree, (worktrees) =>
      this.#emit('worktrees', { worktree, worktrees }),
    );
    this.#opened.set(worktree, {
      count: 1,
      off: () => {
        offStatus();
        offWorktrees();
      },
    });
  }

  #release(worktree: string): void {
    const opened = this.#opened.get(worktree);
    if (opened === undefined) return;
    opened.count -= 1;
    if (opened.count > 0) return;
    opened.off();
    this.#opened.delete(worktree);
  }

  #emit<E extends EventName>(event: E, body: GitTransportEvents[E]): void {
    for (const cb of [...this.#listeners[event]]) {
      try {
        cb(body);
      } catch {
        // Swallowed: one bad subscriber must not block the others.
      }
    }
  }
}

/**
 * A `GitTransport` over a native engine in this process. `options` are the
 * engine's: `allowedRoots` (required — empty fails every `open` closed),
 * `gitPath`, caps, debounce. Rejects `EUNSUPPORTED` when the loaded binary
 * has no git engine; check `isNativeGitAvailable()` first and fall back to
 * the shell client (§17.5).
 */
export async function createInProcessGitTransport(
  options: GitEngineOptions,
): Promise<GitTransport> {
  const transport = new InProcessGitTransport(new GitEngine(options));
  engines.set(transport, transport.engine);
  return transport;
}

/**
 * Same-process host helper (§19.1, §19.3): let `fx`'s watcher feed git status.
 * Not a `GitTransport` method. Every OS path the explorer's watcher reports
 * is routed — natively, on the watcher thread — to the open checkout that
 * contains it, as a worktree-relative hint; nothing is taken from the host's
 * `takePendingChanges` drain. Returns the unbinder; closing the transport
 * also unbinds.
 *
 * `transport` must be in-process. Across a process split the helper's job
 * stays in the explorer's process and forwards routed relatives over
 * `call('hintPaths', …)`; that pairing arrives with the MessagePort transport
 * (PR 8), and until then a cross-process transport is `EUNSUPPORTED` here.
 */
export function bindExplorer(transport: GitTransport, fx: FileExplorer): () => void {
  const engine = engines.get(transport);
  if (engine === undefined) {
    throw new GitError(
      'EUNSUPPORTED',
      'bindExplorer needs a transport from createInProcessGitTransport in this process; ' +
        'a cross-process transport gets explorer hints through hintPaths',
    );
  }
  return engine.bindExplorer(fx);
}
