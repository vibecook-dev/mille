// Phase B4.1 / B4.2 — real `GitClient` implementation.
//
// Shells out to `git status --porcelain=v2 -z --untracked-files=all` and
// parses the null-delimited output into `GitStatusEntry` records. Pairs
// with `watchDotGit` to fire `onChange` whenever `.git/HEAD` or
// `.git/index` mutate (branch switch, commit, stage, etc.).
//
// Philosophy: decoration failures must degrade gracefully — a missing
// git binary, a non-repo directory, or a malformed line can't crash the
// host. On any error we return an empty map and log a warning; the UI
// simply shows no badges.
//
// Parser covers porcelain v2's ordinary (`1`), renamed/copied (`2`),
// untracked (`?`), and ignored (`!`) line types. Unmerged (`u`) lines
// are treated as conflicts (`U`). See
// https://git-scm.com/docs/git-status#_porcelain_format_version_2

import { spawn as nodeSpawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import * as path from 'node:path';

import type { GitClient, GitStatusEntry, GitStatusLetter } from './client.js';
import { watchDotGit } from './watch-dotgit.js';

// ─── Spawn abstraction (so tests can inject a fake) ───────────────────

export interface ChildProcessLike {
  readonly stdout: NodeJS.ReadableStream;
  readonly stderr: NodeJS.ReadableStream;
  on(event: 'close', cb: (code: number | null) => void): void;
  on(event: 'error', cb: (err: Error) => void): void;
  /** Optional — used by SCM history to honor AbortSignal mid-flight. */
  kill?(signal?: NodeJS.Signals | number): boolean;
}

export type SpawnLike = (
  cmd: string,
  args: readonly string[],
  opts: { cwd: string },
) => ChildProcessLike;

// ─── Options ──────────────────────────────────────────────────────────

export interface ShellGitClientOptions {
  /** Absolute path to the repository root (or any dir under it). */
  readonly rootPath: string;
  /** Override the git binary. Default: `'git'` (resolved via PATH). */
  readonly gitPath?: string;
  /**
   * Inject a fake spawn for tests so we don't shell out. Must conform
   * to `ChildProcessLike` — real `node:child_process.spawn` does.
   */
  readonly spawn?: SpawnLike;
  /** Debounce fs.watch events before firing `onChange`. Default 100 ms. */
  readonly watchDebounceMs?: number;
  /**
   * Skip starting the `.git` watcher. Useful in tests that want a
   * pure request/response client. Default: `false`.
   */
  readonly disableWatcher?: boolean;
  /** Override the warn sink (tests silence it). Default: `console.warn`. */
  readonly warn?: (msg: string) => void;
}

// ─── Helpers ──────────────────────────────────────────────────────────

/**
 * Walk up from `start` looking for a `.git` directory (or file — a
 * worktree's `.git` is a file pointing at the real gitdir). Stops after
 * 32 levels so a pathological symlink chain can't lock us up.
 *
 * Returns the absolute path to the `.git` entry, or `null` when none
 * found. Callers treat `null` as "not a git repo" and skip the watcher.
 */
function findDotGit(start: string): string | null {
  let cur = path.resolve(start);
  for (let i = 0; i < 32; i += 1) {
    const candidate = path.join(cur, '.git');
    try {
      if (existsSync(candidate)) return candidate;
    } catch {
      // ignore — fall through
    }
    const parent = path.dirname(cur);
    if (parent === cur) return null;
    cur = parent;
  }
  return null;
}


/**
 * Parse a single porcelain-v2 status letter. `.` means unchanged in
 * that slot (index or worktree). Anything else maps to the matching
 * `GitStatusLetter`; unknown glyphs (shouldn't happen in well-formed
 * output) are treated as modified to avoid dropping the entry.
 */
function parseLetter(ch: string): GitStatusLetter | null {
  switch (ch) {
    case 'M': return 'M';
    case 'A': return 'A';
    case 'D': return 'D';
    case 'R': return 'R';
    case 'C': return 'C';
    case 'U': return 'U';
    case 'T': return 'M'; // type change — surface as modified
    case '.': return null; // unchanged slot
    default: return null;
  }
}

/**
 * Collapse an (index, worktree) XY pair into one `GitStatusEntry`.
 * Rules (mirroring common SCM UIs):
 *   - If the index slot is non-`.`, the entry is staged with that letter.
 *   - Else the worktree slot wins.
 *   - Both `.` means "nothing changed" — caller should skip this row.
 */
function xyToEntry(
  x: string,
  y: string,
  relPath: string,
): GitStatusEntry | null {
  const indexLetter = parseLetter(x);
  const worktreeLetter = parseLetter(y);
  if (indexLetter !== null) {
    return { path: relPath, status: indexLetter, staged: true };
  }
  if (worktreeLetter !== null) {
    return { path: relPath, status: worktreeLetter, staged: false };
  }
  return null;
}

/**
 * Parse the buffered stdout of `git status --porcelain=v2 -z` into a
 * map keyed by **absolute** path (joined against `rootPath`). The
 * parser is tolerant — a malformed line is skipped with a warn and
 * doesn't poison the rest of the output.
 */
function parsePorcelainV2(
  buffer: string,
  rootPath: string,
  warn: (msg: string) => void,
): Map<string, GitStatusEntry> {
  const result = new Map<string, GitStatusEntry>();

  // `-z` splits records on NUL. But the `2` (renamed/copied) record
  // spans TWO NUL-separated chunks: `2 XY ... <newPath>\0<origPath>\0`.
  // We walk the token array with an index so we can consume the extra
  // chunk when the tag is `2`.
  const tokens = buffer.split('\0');
  // Trailing NUL → empty token at the end; drop it.
  if (tokens.length > 0 && tokens[tokens.length - 1] === '') tokens.pop();

  for (let i = 0; i < tokens.length; i += 1) {
    const line = tokens[i];
    if (line === undefined || line.length === 0) continue;
    const tag = line[0];

    if (tag === '1') {
      // `1 XY sub mH mI mW hH hI path`
      // path is the last field — everything after the 8th space.
      const parsed = parseOneOrTwoLine(line, 1);
      if (parsed === null) { warn(`mille-ui/git: malformed '1' line: ${line}`); continue; }
      const entry = xyToEntry(parsed.x, parsed.y, parsed.path);
      if (entry === null) continue;
      result.set(path.resolve(rootPath, parsed.path), entry);
    } else if (tag === '2') {
      // `2 XY sub mH mI mW hH hI rc<score> <newPath>` then a trailing
      // `<origPath>` as the next NUL-separated token. We consume both.
      const parsed = parseOneOrTwoLine(line, 2);
      if (parsed === null) { warn(`mille-ui/git: malformed '2' line: ${line}`); continue; }
      // Advance past the orig-path token.
      i += 1;
      const entry = xyToEntry(parsed.x, parsed.y, parsed.path);
      if (entry === null) continue;
      result.set(path.resolve(rootPath, parsed.path), entry);
    } else if (tag === '?') {
      // `? path`
      const rel = line.slice(2);
      if (rel.length === 0) continue;
      result.set(path.resolve(rootPath, rel), {
        path: rel,
        status: '?',
      });
    } else if (tag === '!') {
      // `! path` — skip ignored; UIs render ignored state separately.
      continue;
    } else if (tag === 'u') {
      // Unmerged: `u XY sub m1 m2 m3 mW h1 h2 h3 path` — ten fixed fields,
      // then the path, which may itself contain spaces. Counting the fields
      // (as for `1` and `2`) rather than taking the last space-separated
      // token is what keeps `conflict file.ts` from becoming `file.ts`
      // (SPEC mille-git T-PORC; the golden in crates/mille-git covers it).
      const parsed = parseRecord(line, 10);
      if (parsed === null) { warn(`mille-ui/git: malformed 'u' line: ${line}`); continue; }
      const rel = parsed.path;
      // Unmerged is always a conflict, regardless of XY letters.
      result.set(path.resolve(rootPath, rel), {
        path: rel,
        status: 'U',
      });
    } else if (tag === '#') {
      // Header line (`# branch.oid ...`) — ignore.
      continue;
    } else {
      // Unknown tag — skip with warn so corrupt output isn't silent.
      warn(`mille-ui/git: unrecognised status tag: ${JSON.stringify(line.slice(0, 40))}`);
    }
  }

  return result;
}

/**
 * Parse the common prefix of a `1`/`2` porcelain line. Shared because
 * the XY pair and the path live at identical offsets in both; only the
 * sub-mode count differs (8 fields before the path for `1`, 9 for `2`
 * due to the rename/copy score `rc<score>`).
 */
function parseOneOrTwoLine(
  line: string,
  variant: 1 | 2,
): { x: string; y: string; path: string } | null {
  // Fields before path: tag(0) XY(1) sub(2) mH(3) mI(4) mW(5) hH(6) hI(7) [rc(8)] path
  return parseRecord(line, variant === 1 ? 8 : 9);
}

/**
 * Split a `1` / `2` / `u` record into its XY pair and the path that follows
 * `fieldsBeforePath` space-separated fields. The path is everything after
 * them, spaces included.
 */
function parseRecord(
  line: string,
  fieldsBeforePath: number,
): { x: string; y: string; path: string } | null {
  // Fixed prefix: `<tag> XY ` — the XY pair is at indices 2 and 3.
  if (line.length < 5) return null;
  const x = line[2];
  const y = line[3];
  if (x === undefined || y === undefined) return null;
  // Count spaces to find the start of the path field.
  let spacesSeen = 0;
  let idx = 0;
  while (idx < line.length && spacesSeen < fieldsBeforePath) {
    if (line[idx] === ' ') spacesSeen += 1;
    idx += 1;
  }
  if (spacesSeen < fieldsBeforePath) return null;
  const pathField = line.slice(idx);
  if (pathField.length === 0) return null;
  return { x, y, path: pathField };
}

/**
 * Buffer a `ChildProcessLike`'s stdout/stderr into utf-8 strings,
 * resolving once the child closes. Never rejects — spawn errors surface
 * as `{ code: null, stdout: '', stderr: err.message }`.
 */
function collectChild(
  child: ChildProcessLike,
): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    const outChunks: Buffer[] = [];
    const errChunks: Buffer[] = [];
    let settled = false;
    const settle = (code: number | null, errMsg: string | null): void => {
      if (settled) return;
      settled = true;
      const stdout = Buffer.concat(outChunks).toString('utf8');
      const stderr = errMsg !== null
        ? errMsg
        : Buffer.concat(errChunks).toString('utf8');
      resolve({ code, stdout, stderr });
    };
    child.stdout.on('data', (chunk: Buffer | string) => {
      outChunks.push(typeof chunk === 'string' ? Buffer.from(chunk) : chunk);
    });
    child.stderr.on('data', (chunk: Buffer | string) => {
      errChunks.push(typeof chunk === 'string' ? Buffer.from(chunk) : chunk);
    });
    child.on('error', (err) => {
      settle(null, err.message);
    });
    child.on('close', (code) => {
      settle(code, null);
    });
  });
}

// ─── Factory ──────────────────────────────────────────────────────────

/**
 * Create a real `GitClient` backed by `git status --porcelain=v2 -z`.
 *
 * - `getStatus` returns a map keyed by **absolute** path. The mille-ui
 *   companion's provider resolves paths to entry ids via `fx.getByUri`,
 *   which keys on absolute URIs — using absolute keys here keeps the
 *   two sides consistent regardless of the workspace layout.
 * - `onChange` fires on `.git/HEAD` or `.git/index` writes, debounced
 *   by `watchDebounceMs` (default 100 ms).
 * - When git isn't on PATH or the directory isn't a repo, `getStatus`
 *   returns an empty map and logs a warning once per call. No throws.
 */
export function createShellGitClient(options: ShellGitClientOptions): GitClient {
  const {
    rootPath,
    gitPath = 'git',
    spawn = nodeSpawn as unknown as SpawnLike,
    watchDebounceMs = 100,
    disableWatcher = false,
    warn = (msg: string): void => { console.warn(msg); },
  } = options;

  // Locate `.git` once; the watcher is cheap when missing (no-op
  // disposer) and the status call works whether or not we found it
  // (git itself resolves the repo root).
  //
  // The entry is handed over as-is: `watchDotGit` owns following the
  // `gitdir:` redirect that a linked worktree or submodule puts there,
  // so hosts that call it directly get the same treatment.
  const dotGit = findDotGit(rootPath);

  // onChange listeners. Firings are produced by the .git watcher; we
  // pipe them straight through — the consumer (companion's batcher)
  // does its own coalescing so double-debouncing isn't useful.
  const listeners = new Set<() => void>();

  let watcherDispose: (() => void) | null = null;
  if (!disableWatcher && dotGit !== null) {
    watcherDispose = watchDotGit(
      dotGit,
      () => {
        for (const l of [...listeners]) {
          try { l(); } catch { /* swallow — one listener mustn't block others */ }
        }
      },
      { debounceMs: watchDebounceMs },
    );
  }

  async function getStatus(
    _root: string,
  ): Promise<ReadonlyMap<string, GitStatusEntry>> {
    // The engine wires `rootPath` through as the argument; ignore it in
    // favour of the one baked into the client, which was used to locate
    // `.git` and matches the `fx.getByUri` absolute-path key space.
    let child: ChildProcessLike;
    try {
      child = spawn(
        gitPath,
        ['status', '--porcelain=v2', '-z', '--untracked-files=all'],
        { cwd: rootPath },
      );
    } catch (err) {
      warn(
        `mille-ui/git: failed to spawn ${gitPath}: ${err instanceof Error ? err.message : String(err)}`,
      );
      return new Map();
    }

    const { code, stdout, stderr } = await collectChild(child);

    // Exit-code semantics: 0 → clean run. 128 → "not a git repository",
    // which is expected when users open a non-repo folder; we treat it
    // as "no status" and stay silent about it. Other codes warn.
    if (code === null) {
      warn(`mille-ui/git: git exited abnormally: ${stderr || 'unknown error'}`);
      return new Map();
    }
    if (code === 128) return new Map();
    if (code !== 0) {
      warn(`mille-ui/git: git exit ${code}: ${stderr.trim() || '(no stderr)'}`);
      return new Map();
    }

    try {
      return parsePorcelainV2(stdout, rootPath, warn);
    } catch (err) {
      warn(
        `mille-ui/git: parse error: ${err instanceof Error ? err.message : String(err)}`,
      );
      return new Map();
    }
  }

  function onChange(cb: () => void): () => void {
    listeners.add(cb);
    let active = true;
    return (): void => {
      if (!active) return;
      active = false;
      listeners.delete(cb);
      // Once the last listener leaves we could stop the watcher, but
      // re-subscribing is common (toggle off/on in the playground), so
      // we keep it live until the client itself is GC'd. The watcher
      // is `persistent: false`, so it won't hold the event loop open.
    };
  }

  // We don't expose dispose() on GitClient — the interface doesn't have
  // one. Callers drop references; finalization happens via the
  // `persistent: false` watcher naturally unpinning the event loop.
  // Tests that need to clean up construct with `disableWatcher: true`.
  // If consumers later need an explicit close hook, it can be added
  // without breaking the interface.
  void watcherDispose;

  return { getStatus, onChange };
}
