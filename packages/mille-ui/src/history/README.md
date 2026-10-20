# `@vibecook/mille-ui/history`

File timeline + SCM mutation contracts for the explorer (Phase 5.3).

## Surfaces

| Export | Role |
|--------|------|
| `FileHistoryClient` / `ScmClient` | Host-supplied backends (git, mock, remote) |
| `runScmRevert` / `runScmCompare` / `runFileHistory` | Confirm + progress + `AbortSignal` |
| `scmHistoryCommands` | Default `scm.*` commands for the registry |
| `selectedScmTargets` / `groupScmTargetsByRoot` | Multi-root safe selection → per-root batches |
| `createMapFileHistoryClient` / `createMapScmClient` | In-memory clients for tests and demos |
| `createShellFileHistoryClient` / `createShellScmClient` | Node-only shell git (`@vibecook/mille-ui/git/node`) |

## Security

Shell clients **never** join unchecked relative paths:

```ts
import { assertPathUnderRoot, createShellScmClient } from '@vibecook/mille-ui/git/node';

// Throws on `../secret`, `/etc/passwd`, drive letters, `..` segments.
assertPathUnderRoot('/ws', 'src/a.ts'); // → 'src/a.ts'
```

Electron hosts must also reject renderer-supplied roots that are not the
active workspace (playground does this in main-process IPC).

## Multi-root revert

Destructive commands group by absolute root:

```ts
import { selectedScmTargets, groupScmTargetsByRoot } from '@vibecook/mille-ui/history';

const targets = selectedScmTargets(ctx); // rootId + rootRelativePath + rootPath
const groups = groupScmTargetsByRoot(targets);
for (const [rootPath, paths] of groups) {
  await client.revert(paths, { rootPath, signal });
}
```

Hosts implement `ScmHostHooks.resolveRootPath(rootId, rootName)` when
`ctx.workspaceRoot` alone is ambiguous.

## Commands

| Id | When |
|----|------|
| `scm.compareWithHead` | focused file |
| `scm.compareWithPrevious` | focused file |
| `scm.showHistory` | focused file |
| `scm.revert` | selection or focused file |

Register with `createCommandRegistry([...defaultCommands, ...scmHistoryCommands])`
and inject clients via `FileTree` `hostHooks` (structural `ScmHostHooks`).

## Cancellation

`ScmClient` methods accept `{ signal?: AbortSignal }`. The shell client
terminates the spawned git child on abort (not only pre-flight checks).

An aborted client call rejects with an `Error` whose `code` is `'ABORT_ERR'`,
from the shell clients and the `GitService` adapters alike (SPEC mille-git
§12.5). `runScmRevert` / `runScmCompare` report it — or any failure once their
`signal` has fired — as `ScmActionError('ECANCELED')` and the `cancelled`
progress phase, the same code a declined confirm throws.
