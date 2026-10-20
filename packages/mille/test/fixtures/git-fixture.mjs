// Temp-repo helpers for the git engine tests (SPEC mille-git §28).
//
// Every repository is created with the real `git` CLI inside a fresh temp
// directory and configured locally — no signing, an explicit hooks directory,
// no autocrlf/fsmonitor — so a developer's global git config cannot change
// what a test observes. Tests skip cleanly when `git --version` fails.

import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, realpathSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';

import { buildIdentity } from '../../dist/index.js';
import { isNativeGitAvailable } from '../../dist/git/index.js';

export const HAS_GIT = (() => {
  try {
    execFileSync('git', ['--version'], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
})();

export const NATIVE_GIT = isNativeGitAvailable();

/**
 * `{ skip }` for a test that needs a git-enabled `.node` and a git CLI.
 * Under `MILLE_EXPECT_GIT=1` (the CI job that builds `build:napi:git:probe`)
 * a missing engine is a failure, not a skip — otherwise that job could
 * silently test the default binary and report green.
 */
export function needsGit() {
  if (!NATIVE_GIT) {
    if (process.env.MILLE_EXPECT_GIT === '1') {
      throw new Error(
        `MILLE_EXPECT_GIT=1 but the loaded binary has no git engine (${buildIdentity().resolvedPath})`,
      );
    }
    return { skip: 'native binary built without the git feature' };
  }
  if (!HAS_GIT) return { skip: 'git not on PATH' };
  return {};
}

const GIT_ENV = (() => {
  const env = { ...process.env, GIT_TERMINAL_PROMPT: '0' };
  for (const key of ['GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_COMMON_DIR']) {
    delete env[key];
  }
  return env;
})();

export function git(cwd, ...args) {
  const result = spawnSync('git', args, { cwd, encoding: 'utf8', env: GIT_ENV });
  if (result.status !== 0) {
    throw new Error(`git ${args.join(' ')} failed in ${cwd}: ${result.stderr}`);
  }
  return result.stdout;
}

/** `git` with `input` on stdin (`mktree`, `hash-object --stdin`). */
export function gitStdin(cwd, args, input) {
  const result = spawnSync('git', args, { cwd, encoding: 'utf8', env: GIT_ENV, input });
  if (result.status !== 0) {
    throw new Error(`git ${args.join(' ')} failed in ${cwd}: ${result.stderr}`);
  }
  return result.stdout;
}

/** A realpath'd temp directory (macOS `/var` → `/private/var`). */
export function tempDir(prefix = 'mille-git-') {
  return realpathSync(mkdtempSync(join(tmpdir(), prefix)));
}

export function write(root, rel, contents) {
  const target = join(root, ...rel.split('/'));
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, contents);
}

/** `git init` + local config in `dir` (created if missing). */
export function initRepo(dir) {
  mkdirSync(dir, { recursive: true });
  git(dir, 'init', '-q');
  for (const [key, value] of [
    ['user.name', 'Mille Test'],
    ['user.email', 'mille@example.invalid'],
    ['commit.gpgsign', 'false'],
    ['tag.gpgsign', 'false'],
    ['core.autocrlf', 'false'],
    ['core.fsmonitor', 'false'],
    ['core.untrackedCache', 'false'],
  ]) {
    git(dir, 'config', key, value);
  }
  const hooks = join(dir, '.git', 'hooks');
  mkdirSync(hooks, { recursive: true });
  git(dir, 'config', 'core.hooksPath', hooks);
  return dir;
}

/** A repo with `files` committed in one commit. Returns its realpath root. */
export function makeRepo(files = { 'a.ts': 'a\n' }, prefix) {
  const root = tempDir(prefix);
  initRepo(root);
  for (const [rel, contents] of Object.entries(files)) write(root, rel, contents);
  git(root, 'add', '-A');
  git(root, 'commit', '-q', '-m', 'initial');
  return root;
}

/** Resolve when `predicate()` holds, polling; reject after `timeoutMs`. */
export async function waitFor(predicate, { timeoutMs = 5000, intervalMs = 20, what = 'condition' } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await predicate();
    if (value) return value;
    if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
}

/** The collapsed `XY` string of a snapshot row (`.M`, `M.`, `.?`, `UU` …). */
export function xyOf(snapshot, path) {
  const entry = snapshot.entries.find((e) => e.path === path);
  return entry === undefined ? null : `${entry.xy.index}${entry.xy.worktree}`;
}
