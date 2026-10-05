# `@vibecook/mille-ui/git`

Git decoration companion for the `mille-ui` file tree. Registers a
`DecorationProvider` on the engine that paints per-entry badges (`M`,
`A`, `?`, …) and propagates a muted variant to ancestor folders so
"something changed below me" surfaces at a glance.

## Surfaces

- `registerGitDecorations({ fx, client, rootPath, … })` — wires a
  host-supplied `GitClient` into the engine. Returns a handle with
  `dispose()` and `refresh()`.
- `createShellGitClient({ rootPath })` — real `GitClient` that shells
  out to the host's `git` binary (Phase B4). Parses
  `git status --porcelain=v2 -z --untracked-files=all` and watches
  `.git/HEAD` + `.git/index` for refresh.
- `watchDotGit(dotGit, onChange, { debounceMs })` — lower-level watcher
  used by the shell client. Exposed so hosts that hand-roll a client
  (e.g. a `simple-git` wrapper) can reuse the refresh signal. Accepts
  either a gitdir or a `.git` entry carrying a `gitdir:` redirect.
- `resolveGitDir(dotGit)` — follows that redirect. Exported for hosts
  that need the real gitdir for their own bookkeeping.

## Engine client vs shell client

Two backends satisfy the same `GitClient` contract.

| | Engine (`createGitServiceClient`) | Shell (`createShellGitClient`) |
|--|--|--|
| Status | gix in the native `GitEngine`, one queue per worktree | `git status --porcelain=v2 -z` per call |
| Freshness | explorer `'event'` hints + gitdir watch (worktree-only saves included) | `.git/HEAD` + `.git/index` only |
| Where | any process holding a `GitService` (renderer-safe adapter) | Node only (`/git/node`) |
| Needs | a `.node` built with `build:napi:git` | a `git` binary |

The engine path, as a host wires it (SPEC mille-git §17.2, §19.1):

```ts
import { bindExplorer, createGitService, createInProcessGitTransport,
         isNativeGitAvailable } from '@vibecook/mille/git';        // Node / utility only
import { createGitServiceClient, registerGitDecorations } from '@vibecook/mille-ui/git';

if (isNativeGitAvailable()) {
  const transport = await createInProcessGitTransport({ allowedRoots: [rootPath] });
  const service = createGitService(transport);
  await service.open(rootPath);
  bindExplorer(transport, host.local);              // worktree saves → status
  registerGitDecorations({ fx: host.local, client: createGitServiceClient(service, rootPath),
                           rootPath, registrar: (p) => host.registerDecorationProvider(p) });
} else {
  // feature-off binaries: keep the shell client
}
```

- **Types live here, the engine does not.** `GitService`, `GitTransport` and every
  snapshot / diff / log type are exported from `@vibecook/mille-ui/git` and are
  renderer-safe; `@vibecook/mille/git` holds the native engine and the factories and is
  never imported by renderer code. `test-types/git-service-compat.ts` fails `typecheck`
  if the engine's private copy of the contract drifts from this one.
- **The adapters** — `createGitServiceClient`, `createGitServiceHistoryClient`,
  `createGitServiceScmClient` (`engine-client.ts`) — speak only `GitService`, so they
  work over an in-process transport or a MessagePort one. The `GitClient` collapses XY
  exactly as the shell client does and drops rows outside the explorer root; history
  reads `readObject` / `readWorktreeFile`; `revert` restores **index** bytes. Construct
  them with the same spelling of the root that `open` was given.
- **The shell clients stay** (≥ one major after the engine ships, §24) for binaries built
  without the `git` feature and hosts without a native engine.

## Shell client

```ts
import {
  createShellGitClient,
  registerGitDecorations,
} from '@vibecook/mille-ui/git';

const client = createShellGitClient({ rootPath: '/path/to/repo' });
const handle = registerGitDecorations({ fx, client, rootPath: '/path/to/repo' });
// …later
handle.dispose();
```

### Requirements

- A `git` binary on `PATH`. Override via `gitPath: '/opt/homebrew/bin/git'`
  if the host ships its own binary or if PATH isn't reliable (Electron
  renderers sometimes don't inherit the shell's PATH).
- Node 20+ (the companion is ESM-only and uses `node:child_process` +
  `node:fs`).

### Degradation

Decoration is a *nice-to-have*; failures never crash the host.

- If the directory isn't a git repo, git exits with code 128 and the
  client returns an empty map silently.
- If the git binary is missing, spawn fails and the client warns once
  via `console.warn` (overridable via the `warn` option) and returns an
  empty map.
- If porcelain output is malformed, each bad line is skipped with a
  warn; well-formed lines still land.

### What's watched

`watchDotGit` covers the two files that flip on every interesting git
state change:

| File         | Flips on                                              |
|--------------|-------------------------------------------------------|
| `.git/HEAD`  | branch switch, detached-HEAD move, `git reset`        |
| `.git/index` | `git add`, `git rm`, `git commit`, `git stash push`   |

Both are located relative to the **resolved** gitdir. In a linked
worktree or a submodule, `.git` is a file holding `gitdir: <path>` and
those two files live at the far end of that redirect, per worktree —
`watchDotGit` follows it before attaching anything.

Worktree-only edits (a plain `save` in an editor) are **not** observed
by this watcher — they don't change the index. If you need
modified-on-save badge freshness, call `handle.refresh()` from your
editor's save hook, or pair the shell client with a broader
filesystem watcher (chokidar, etc.).

### Deferred / v0.3

- **isomorphic-git fallback.** Browsers and sandboxed hosts can't
  spawn subprocesses. A future `createIsomorphicGitClient` will plug
  into the same `GitClient` contract without requiring `git` on the
  host — see https://isomorphic-git.org/.
- **Worktree list liveness.** `HEAD` and `index` are per-worktree and
  now resolve correctly, but `git worktree add` / `remove` mutates
  `<common>/worktrees/`, which nothing here watches. A host showing a
  live list of worktrees needs its own watch on that directory; the
  decoration path does not care.
- **Per-entry watchers.** For million-file repos, debouncing on the
  index is cheap; scanning the whole output on every tick isn't. A
  future pass will diff two consecutive porcelain outputs so only the
  changed ids are notified.
