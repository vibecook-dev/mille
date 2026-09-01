# Embedding `@vibecook/mille`

This document is the one-stop integration guide. It walks from a zero-knowledge
Node script to a production Electron + React integration. Every code sample is
copy-pasteable; patterns assume v0.1 (see section 9 for what v0.1 deliberately
does not ship).

For the authoritative public type surface see `api.d.ts` at the package root.
For architecture & design, see the monorepo `README.md`.

---

## Table of contents

1. [Minimum viable usage (Node)](#1-minimum-viable-usage-node)
2. [Electron integration (UtilityProcess pattern)](#2-electron-integration-utilityprocess-pattern)
3. [React integration](#3-react-integration)
4. [Decoration providers](#4-decoration-providers)
5. [Error handling](#5-error-handling)
6. [Packaging for distribution](#6-packaging-for-distribution)
7. [Performance tuning](#7-performance-tuning)
8. [Troubleshooting](#8-troubleshooting)
9. [What is not in v0.1 (roadmap)](#9-what-is-not-in-v01-roadmap)
10. [Explorer settings](#10-explorer-settings)

---

## 1. Minimum viable usage (Node)

The library is usable from a plain Node script. The native `.node` binary loads
eagerly the first time the package is imported; every subsequent call is
in-process and cheap.

```ts
import { FileExplorer } from '@vibecook/mille';

const fx = new FileExplorer({
  roots: ['/absolute/path/to/workspace'],
  respectIgnore: true, // parse .gitignore/.ignore/.rgignore
  followSymlinks: 'smart', // canonicalize + dedupe; see api.d.ts
  watchDebounceMs: 75, // default
});

// Constructor does NOT walk the filesystem. Call populateFromRoots()
// exactly once to seed the EntryStore. Returns total entry count.
const total = await fx.populateFromRoots();
console.log(`loaded ${total} entries`);

// Every read goes through an immutable snapshot. Identity is stable
// until the tree or decoration version bumps.
const snap = fx.getSnapshot();
for (const root of snap.roots()) {
  console.log(root.id, root.name);
}

// Always dispose. Stops the watcher thread and releases Rust resources.
await fx.dispose();
```

Multiple roots may share a filesystem basename. Mille resolves every entry
through an exact bidirectional path index, so reads, mutations, URI lookup, and
lazy prefetch remain scoped to the intended root rather than the first matching
name. The reference `FileTree` gives duplicate root rows stable ordinal labels
such as `workspace (1)` and `workspace (2)` while preserving their real names
for commands and persisted paths.

### Line-by-line

- `new FileExplorer({ roots })` — cheap constructor; no I/O. Rejects non-absolute
  root paths via `FileSystemError` only when you later read or mutate.
- `await fx.populateFromRoots()` — performs the parallel walk, populates the
  EntryStore, and returns the total entry count. May throw `FileSystemError`
  (`ENOENT`, `EACCES`, `ELOOP`) on a dead root; partial walks are committed —
  entries found before the error remain in the store.
- `fx.getSnapshot()` — returns a `MirrorSnapshot`. Cheap (struct handle, no
  copy). Safe to cache in refs; safe to read during React render; safe to diff
  with `===`.
- `snap.roots()` — returns a fresh `readonly Entry[]` each call. Cache at a
  call-site if you iterate repeatedly.
- `fx.dispose()` — idempotent. Stops the watcher, drains outstanding async
  tasks, then releases the native handle. Further method calls throw.

### Getting by id

The snapshot is the read surface; the explorer is the write surface.

```ts
const snap = fx.getSnapshot();
const entry = snap.getById(42); // null if unknown
const count = snap.directChildCount(42); // null if folder not yet walked
const hasKids = snap.hasChildren(42);
const firstLevelIds = snap.childrenOf(42); // in id-allocation order
```

### Iterating visible rows

For virtualizers, prefer `visibleRows`. The snapshot computes the flat slice
from your expansion set + viewport window:

```ts
const expanded = new Set<number>([1, 5, 9]);
const rows = snap.visibleRows({
  expanded,
  offset: 0,
  limit: 100,
  includeIgnored: false,
});
for (const row of rows) {
  const indent = '  '.repeat(row.depth);
  const caret = row.hasChildren ? (row.isExpanded ? 'v ' : '> ') : '  ';
  console.log(`${indent}${caret}${row.name}`);
}
```

For viewports larger than ~100 rows, use the bincode bulk path:

```ts
const rowsBulk = snap.visibleRowsBulk({ expanded, offset: 0, limit: 1000 });
// One Buffer hop, decoded in TS. Same row shape, same fields.
```

See `src/mirror-snapshot.ts` for the exact slicing semantics; cache-miss rows
emit `pending: true` placeholders at the correct depth.

---

## 2. Electron integration (UtilityProcess pattern)

The native `.node` binary must load in a process that can spawn threads and
talk to the filesystem. **Don't load it in the renderer.** The supported
deployment shape is:

```
Electron main   ── spawns ──▶  UtilityProcess (runs fx-host.js)
     │                                │
     │    MessagePort (transferred)   │
     └────────────────────────────────┘
                  │
                  │  postMessage / MessageChannelMain
                  │
            BrowserWindow renderer
                  (runs connectFileExplorer(port))
```

The UtilityProcess owns the single native `FileExplorer`; each renderer window
gets its own `MessagePort` into the host and its own session (expansion set,
viewport, knownIds). Four files total: `main.ts`, `fx-host.js`, `preload.ts`,
`renderer.ts`.

### 2.1 Spawn the UtilityProcess from main

```ts
// main.ts
import { app, BrowserWindow, MessageChannelMain, utilityProcess } from 'electron';
import { join } from 'node:path';

app.whenReady().then(async () => {
  // Fork the utility — this process loads the native .node binary.
  const fxProcess = utilityProcess.fork(join(__dirname, 'fx-host.js'), [], {
    serviceName: 'mille-file-explorer',
    env: {
      ...process.env,
      WORKSPACE_ROOT: '/absolute/path/to/workspace',
    },
  });

  fxProcess.on('exit', (code) => {
    console.error(`[mille] fx utility exited with code ${code}`);
  });

  const win = new BrowserWindow({
    webPreferences: {
      preload: join(__dirname, 'preload.js'),
      contextIsolation: true,
      sandbox: true,
    },
  });
  await win.loadFile('index.html');

  // One MessageChannel per renderer. Transfer port1 into the utility
  // and port2 into the renderer; the two ends talk directly afterwards.
  const { port1, port2 } = new MessageChannelMain();
  fxProcess.postMessage({ type: 'attach' }, [port1]);
  win.webContents.postMessage('fx-port', null, [port2]);
});
```

### 2.2 UtilityProcess host script

The utility script receives ports from main and attaches each to the host.

```js
// fx-host.js (runs inside the UtilityProcess — plain CJS for simplicity)
const { createFileExplorerHost } = require('@vibecook/mille/host');

let host;

(async () => {
  host = await createFileExplorerHost({
    roots: [process.env.WORKSPACE_ROOT],
    respectIgnore: true,
    followSymlinks: 'smart',
    watchDebounceMs: 75,
  });
  // Populate eagerly so the first renderer handshake returns a full tree.
  // For very large workspaces, defer this and let `setExpanded` drive it.
  await host.local.populateFromRoots();

  process.parentPort.on('message', (evt) => {
    const msg = evt.data;
    if (msg && msg.type === 'attach' && evt.ports && evt.ports.length > 0) {
      // Each attached port becomes its own renderer session.
      host.attachPort(evt.ports[0]);
    }
  });
})().catch((err) => {
  console.error('[mille] fx-host bootstrap failed:', err);
  process.exit(1);
});

// Clean shutdown — main sends SIGTERM on quit.
process.on('exit', () => {
  if (host) void host.dispose();
});
```

Note: `host.local` is the same `FileExplorer` instance; same-process consumers
(SCM extensions, indexers) running alongside the host in the utility can use it
directly without going through a port.

### 2.3 Preload receives the port

The preload script bridges `ipcRenderer.on('fx-port', …)` into a promise the
renderer can `await`.

```ts
// preload.ts
import { contextBridge, ipcRenderer } from 'electron';

// Resolve with the MessagePort main sent via postMessage.
let portResolver: ((port: MessagePort) => void) | null = null;
const portReady = new Promise<MessagePort>((resolve) => {
  portResolver = resolve;
});

ipcRenderer.on('fx-port', (event) => {
  // event.ports is (MessagePort | MessagePortMain)[]; cast to DOM shape.
  portResolver?.(event.ports[0] as unknown as MessagePort);
  portResolver = null;
});

contextBridge.exposeInMainWorld('mille', {
  getFilePort: (): Promise<MessagePort> => portReady,
});
```

`contextBridge` strips prototypes, so exposing the bare promise works but
methods are a cleaner surface to evolve.

### 2.4 Renderer connects

```ts
// renderer.ts
import { connectFileExplorer } from '@vibecook/mille/client';

declare global {
  interface Window {
    mille: { getFilePort(): Promise<MessagePort> };
  }
}

async function main(): Promise<void> {
  const port = await window.mille.getFilePort();
  const fx = await connectFileExplorer(port, {
    mirrorCap: 20_000, // LRU size for the renderer-side mirror
    prefetchRows: 200, // overscan hint for the host
  });

  console.log('tree version', fx.getTreeVersion());

  fx.on('change', () => {
    const snap = fx.getSnapshot();
    // Re-render from snap. Identity advances on every delta; gate with `===`.
  });

  // Typical expansion — ids come from a previous snapshot read.
  fx.setExpanded({ add: [1, 7, 42] });

  // Viewport hint. Fire-and-forget; the host uses it to bound which
  // child payloads land in subsequent deltas.
  fx.setViewport({ offset: 0, limit: 200, overscan: 50 });
}

void main();
```

`connectFileExplorer` resolves once the host completes the handshake and the
initial snapshot lands. The returned `PortFileExplorer` exposes the same
mutation surface as the local `FileExplorer` (rename, move, delete, readText,
writeFile, …); every call round-trips over the port and resolves on the
matching `mutateResult` frame. Rejections arrive as typed `FileSystemError`.

---

## 3. React integration

Consumers of the local `FileExplorer` (e.g. scripts or same-process extensions)
can drive React via the built-in `useFileExplorerSnapshot` hook. The hook is a
thin `useSyncExternalStore` wrapper — the snapshot is the external store value.

### 3.1 Hook import

Hooks live on the `/react` subpath so the main entry stays free of the React
peer dependency.

```ts
import { useFileExplorerSnapshot, useFileExplorerTreeSnapshot } from '@vibecook/mille/react';
```

- `useFileExplorerSnapshot(fx)` — re-renders on tree OR decoration bumps.
  Default for viewport renderers.
- `useFileExplorerTreeSnapshot(fx)` — re-renders on tree bumps only. Useful
  for scanners/indexers that do not show decoration overlays.

### 3.2 Virtualized tree view

This example uses `@tanstack/react-virtual` with the snapshot's `visibleRows`
slice. Expansion state lives in React; everything else lives in the mirror.

```tsx
// TreeView.tsx
import { useFileExplorerSnapshot } from '@vibecook/mille/react';
import type { FileExplorer } from '@vibecook/mille';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useMemo, useRef, useState } from 'react';

const ROW_HEIGHT = 24;

export function TreeView({ fx }: { fx: FileExplorer }): JSX.Element {
  const snap = useFileExplorerSnapshot(fx);
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(() => new Set());

  const count = useMemo(() => snap.visibleRowCount(expanded), [snap, expanded]);
  const total = count.known + count.pendingExpansions.size;

  const parentRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: total,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 10,
  });

  const virtualItems = virtualizer.getVirtualItems();
  const firstVisible = virtualItems[0]?.index ?? 0;
  const lastVisible = virtualItems[virtualItems.length - 1]?.index ?? 0;
  const windowOffset = firstVisible;
  const windowLimit = Math.max(1, lastVisible - firstVisible + 1);

  const rows = useMemo(
    () =>
      snap.visibleRows({
        expanded,
        offset: windowOffset,
        limit: windowLimit,
      }),
    [snap, expanded, windowOffset, windowLimit],
  );

  const toggle = (id: number): void => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  return (
    <div ref={parentRef} style={{ height: 600, overflow: 'auto', fontFamily: 'monospace' }}>
      <div
        style={{
          height: virtualizer.getTotalSize(),
          width: '100%',
          position: 'relative',
        }}
      >
        {virtualItems.map((vrow) => {
          const row = rows[vrow.index - windowOffset];
          if (!row) return null;
          const caret = row.hasChildren ? (row.isExpanded ? 'v' : '>') : ' ';
          return (
            <div
              key={row.id}
              onClick={() => toggle(row.id)}
              style={{
                position: 'absolute',
                top: 0,
                left: 0,
                width: '100%',
                transform: `translateY(${vrow.start}px)`,
                height: ROW_HEIGHT,
                paddingLeft: row.depth * 16,
                cursor: 'pointer',
                whiteSpace: 'nowrap',
              }}
            >
              <span style={{ opacity: 0.6, marginRight: 4 }}>{caret}</span>
              <span>{row.name}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
```

Notes:

- `visibleRowCount` returns `{ known, pendingExpansions }`. Adding the two
  gives the virtualizer an honest total even when some expanded children
  haven't arrived yet; the slice emits `pending: true` placeholder rows at
  those positions (see `row.pending` to render a skeleton).
- Sorting: the snapshot sorts children by name with id tiebreak. Custom sort
  is a v0.2 concern — render-side re-sort is fine for now.
- The hook returns a `MirrorSnapshot`. Identity changes only when the tree or
  decoration version advances; React's `useSyncExternalStore` gates re-renders
  on that identity via `===`.

### 3.3 Port-backed React (renderer-side)

The `PortFileExplorer` returned from `connectFileExplorer` exposes
`on('change', listener)` and `getSnapshot(): ClientMirrorSnapshot`. Wire it up
via plain `useSyncExternalStore` — there is no dedicated hook for the port
variant in v0.1:

```tsx
import { useSyncExternalStore } from 'react';
import type { PortFileExplorer } from '@vibecook/mille';

export function usePortSnapshot(fx: PortFileExplorer) {
  return useSyncExternalStore(
    (cb) => {
      const sub = fx.on('change', cb);
      return () => sub.dispose();
    },
    () => fx.getSnapshot(),
    () => fx.getSnapshot(),
  );
}
```

The returned `ClientMirrorSnapshot` implements the same read surface as
`MirrorSnapshot`: `roots()`, `getById`, `hasChildren`, `directChildCount`,
`visibleRows`, `visibleRowCount`, `getDecorations`.

---

## 4. Decoration providers

Decorations are provider-supplied overlays (git status, lint problems, etc).
Each provider is identified by a string id; the store merges every provider's
contribution per-entry. Providers are driven by their own change source; the
explorer calls `provide()` only for the ids the provider reports as changed.

`getDecorations(id)` on the snapshot returns the merged list for one entry.

### 4.1 Wiring a git decorator

```ts
import type { DecorationProvider, FileExplorer } from '@vibecook/mille';

// Assume gitWatcher is your own watcher: emits 'status' with paths and
// supports `getStatus(path): 'modified' | 'added' | 'clean' | ...`.
declare const gitWatcher: {
  getStatus(path: string): Promise<string>;
  on(event: 'status', listener: (paths: readonly string[]) => void): { dispose(): void };
};

// Map from absolute path -> EntryId. Maintain this alongside tree
// deltas: snapshot.getById doesn't support path lookups in v0.1.
declare const pathToId: Map<string, number>;

function buildGitDecorator(fx: FileExplorer): DecorationProvider {
  return {
    id: 'git',
    onDidChange(listener) {
      // Called by FileExplorer once at register time with a listener
      // that expects an array of changed EntryIds. Forward git events
      // through that listener.
      const sub = gitWatcher.on('status', (paths) => {
        const ids: number[] = [];
        for (const p of paths) {
          const id = pathToId.get(p);
          if (id !== undefined) ids.push(id);
        }
        if (ids.length > 0) listener(ids);
      });
      return { dispose: () => sub.dispose() };
    },
    async provide({ id }) {
      // Find the path for this id via your own map.
      const entry = fx.getSnapshot().getById(id);
      if (!entry) return null;
      // Reconstruct the path however you track it — out of scope here.
      const path = /* your mapping */ '';
      const status = await gitWatcher.getStatus(path);
      switch (status) {
        case 'modified':
          return { badge: 'M', color: 'scm.modified', propagate: true };
        case 'added':
          return { badge: 'A', color: 'scm.added', propagate: true };
        default:
          return null; // clears this provider's contribution for the id
      }
    },
  };
}

const sub = fx.registerDecorationProvider(buildGitDecorator(fx));

// Read decorations from the snapshot — fast, precomputed.
const decos = fx.getSnapshot().getDecorations(42); // readonly Decoration[]

// Stop and clear all decorations contributed by this provider.
sub.dispose();
```

### 4.2 Semantics

- `provide()` may return `null` to clear this provider's slot for the entry.
- `provide()` errors are swallowed — a buggy provider cannot crash the explorer.
- `fx.getDecorationVersion()` bumps once per batch. Subscribe to
  `'change:decorations'` to react to decoration-only churn without
  re-rendering the tree:

  ```ts
  const s = fx.on('change:decorations', (ids: readonly number[]) => {
    // re-render overlays for `ids` only
  });
  ```

- The client-port side does not yet ship decorations over the wire. If your
  decorator runs in the renderer (e.g. a git status stream from the main
  process), register it against a `FileExplorer` in the same process; the
  `PortFileExplorer`'s snapshot returns an empty decoration list.

---

## 5. Error handling

Expected filesystem failures surface as `FileSystemError`, a subclass of
`Error` shaped after VS Code's. Unexpected failures (bugs, native panics)
surface as plain `Error` — rethrow those.

```ts
import { FileSystemError, isFileSystemError } from '@vibecook/mille';

try {
  await fx.rename(entryId, 'new-name.txt');
} catch (e) {
  if (isFileSystemError(e)) {
    console.error(`[fs] code=${e.code} path=${e.path ?? '(none)'} message=${e.message}`);
    if (e.code === 'EEXIST') {
      // handle collision
    }
  } else {
    throw e; // unknown — rethrow
  }
}
```

### 5.1 Error codes

| Code           | Fires when                                                                    |
| -------------- | ----------------------------------------------------------------------------- |
| `EACCES`       | Permission denied on read/write/delete/rename.                                |
| `ENOENT`       | Path does not exist. Common on stale ids after external deletion.             |
| `EEXIST`       | Target already exists on create/copy/move without overwrite.                  |
| `EISDIR`       | Operation expected a file but found a directory (e.g. `readFile` on a dir).   |
| `ENOTDIR`      | Operation expected a directory but found a file.                              |
| `ELOOP`        | Symlink cycle detected during traversal or canonicalization.                  |
| `ENOSPC`       | Write failed — disk full.                                                     |
| `EROFS`        | Write attempted on a read-only filesystem or mount.                           |
| `EBUSY`        | Resource locked (Windows: file in use by another process).                    |
| `EINVAL`       | Bad arguments (invalid name, wrong kind for op, malformed frame on the wire). |
| `ECANCELED`    | Operation cancelled — disposed explorer, cancelled stream, closed session.    |
| `EUNSUPPORTED` | Capability not advertised (e.g. stream read on a provider without `Stream`).  |
| `EUNKNOWN`     | Catch-all for anything the binding couldn't classify.                         |

### 5.2 PortFileExplorer

`PortFileExplorer`'s methods reject with `FileSystemError` the same way. On a
session-level failure (e.g. malformed handshake) the handshake promise rejects
and every pending request rejects with `ECANCELED` or the originating code.

---

## 6. Packaging for distribution

### 6.1 electron-builder config

The native binary must remain outside the ASAR archive and the macOS hardened
runtime needs the JIT entitlement (napi-rs uses `mmap` + dynamic loading).

```json
// electron-builder.json — excerpt
{
  "asar": true,
  "asarUnpack": ["**/*.node"],
  "mac": {
    "hardenedRuntime": true,
    "entitlements": "build/entitlements.mac.plist",
    "entitlementsInherit": "build/entitlements.mac.plist",
    "gatekeeperAssess": false
  },
  "win": {
    "target": "nsis",
    "signAndEditExecutable": true
  },
  "linux": {
    "target": ["AppImage", "deb"]
  }
}
```

```xml
<!-- build/entitlements.mac.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>com.apple.security.cs.allow-jit</key>
    <true/>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <true/>
    <key>com.apple.security.cs.disable-library-validation</key>
    <true/>
  </dict>
</plist>
```

Notary: the native `.node` file inside `app.asar.unpacked` is signed as part
of the Electron bundle signing step. No separate codesign invocation is
needed when `hardenedRuntime: true`.

### 6.2 Per-platform optional-deps

`@vibecook/mille` distributes the native binary via napi-rs's
optional-dependencies pattern. The main package's `optionalDependencies`
declare every supported triple; `npm install` resolves only the ones whose
`os`/`cpu`/`libc` constraints match the host.

Supported triples:

| Triple             | Platform       | Arch  | libc  |
| ------------------ | -------------- | ----- | ----- |
| `darwin-arm64`     | macOS          | arm64 | —     |
| `darwin-x64`       | macOS          | x64   | —     |
| `linux-arm64-gnu`  | Linux          | arm64 | glibc |
| `linux-arm64-musl` | Linux (Alpine) | arm64 | musl  |
| `linux-x64-gnu`    | Linux          | x64   | glibc |
| `linux-x64-musl`   | Linux (Alpine) | x64   | musl  |
| `win32-arm64-msvc` | Windows        | arm64 | msvc  |
| `win32-x64-msvc`   | Windows        | x64   | msvc  |

Install simply with:

```
npm install @vibecook/mille
```

npm/pnpm/yarn all support optional-deps resolution. For CI cross-compilation
(e.g. building a Linux AppImage from macOS), force all triples:

```
npm install --include=optional @vibecook/mille
```

The loader in `src/native.ts` picks the right triple at runtime by
inspecting `process.platform` + `process.arch` + libc flavor.

---

## 7. Performance tuning

Sensible defaults ship for every knob; start there. Reach for these only after
profiling.

### 7.1 Construction options

- `watchDebounceMs` — default **75ms**. Lower (≈25ms) for editor-like latency;
  raise (≈200ms) on battery-sensitive laptops. The value caps how quickly a
  burst of filesystem events becomes observable on the wire.
- `maxCachedEntries` — default **500_000**. Caps the EntryStore. When
  exceeded, the least-recently-touched subtree becomes lazy-only — reads
  still work; they just take an extra walk. Drop to ~50k for a background
  indexer that only needs shallow metadata.
- `walkerConcurrency` — defaults to `num_cpus`. Pin lower on shared CI.
- `compactFolders` — default **true**. Collapses `a/b/c` single-child chains
  into one row; the row's `pathSegments` field carries the original segments.
  Disable only if you need per-level UI.

### 7.2 Snapshot reads

- For viewports ≤ 100 rows, `visibleRows` is the right call. Each row is
  marshaled as a NAPI struct.
- For viewports > 100 rows, switch to `visibleRowsBulk`. One Buffer hop, one
  TS decode pass (see `src/decode.ts`). Identical row shape.
- `directChildCount` is O(1) off a precomputed cache on the snapshot. Prefer
  it over `childrenOf(id).length` when you only need the number.

### 7.3 Expansion & viewport

- `fx.setExpanded({ add, remove })` — batch every toggle inside a single tick;
  the port sends one frame per call.
- `fx.setViewport({ offset, limit, overscan })` — fire-and-forget. The host
  uses it to bound which child payloads land in subsequent deltas; keeping it
  up-to-date with your virtualizer's window reduces wire chatter.

### 7.4 Large monorepos

`populateFromRoots` walks every root eagerly. For a 200k-entry monorepo that's
~200-400ms on an M1. If that's too long for your startup budget:

- Keep `populateFromRoots` in the utility process, not the main process — it
  won't block the UI in either case.
- Or skip the eager walk entirely and drive it lazily: render from the roots
  only, call `fx.setExpanded({ add: [id] })` when the user expands a folder,
  and let the host's delta stream bring children in. Viability depends on
  how much of the tree your UI shows by default.

---

## 8. Troubleshooting

### 8.1 `Error: failed to load native binary`

The optional-dep for your triple was not installed. Symptoms:

- `npm install` ran on a machine whose triple isn't in the supported list.
- `--ignore-optional` or `--no-optional` was set in CI.
- A lockfile from a different platform was reused without reinstalling.

Fix: `npm install --include=optional @vibecook/mille`, or add the right
triple via `--force-install-optional` in your package manager.

### 8.2 `FileSystemError ENOTSUP encoding`

`readText` in v0.1 only supports UTF-8. Pass no `encoding` argument, or use
`readFile` and decode yourself:

```ts
const bytes = await fx.readFile(id);
const text = new TextDecoder('utf-16le').decode(bytes);
```

### 8.3 Slow or missing events on Linux with > 8k files

The inotify `max_user_watches` limit is hit. The explorer emits a
`'warning'` event with `code: 'INOTIFY_LIMIT'` when it detects the condition:

```ts
fx.on('warning', (w: { code: string; detail?: string }) => {
  if (w.code === 'INOTIFY_LIMIT') {
    // surface to the user — they likely need to raise the sysctl
  }
});
```

Remediation (on the host): raise the limit.

```
sudo sysctl fs.inotify.max_user_watches=524288
```

### 8.4 Renderer hangs on `connectFileExplorer`

The handshake never completed. Most common causes:

- The utility process crashed on boot. Check `fxProcess.on('exit', ...)` in
  main — usually an unhandled rejection in `fx-host.js`.
- The preload never forwarded the port. Verify `ipcRenderer.on('fx-port', …)`
  actually fires and receives `event.ports[0]`.
- `contextIsolation: false` + sandbox inconsistencies. Keep
  `contextIsolation: true` and the `contextBridge.exposeInMainWorld` pattern.

### 8.5 `ENOENT` on an id that just existed

Another process (or your own code) deleted the entry between snapshot read
and method call. Ids are stable across renames but not resurrection: after
`ENOENT` on a stale id, re-read `getSnapshot()` and query the path by a
fresh traversal.

---

## 9. What is not in v0.1 (roadmap)

The following are explicitly **deferred** for v0.1. Do not build around them
yet — shape may change.

- **Remote FS providers (native scheme dispatch)** — SSH, zip, S3, etc.
  The `FileSystemProvider` + `registerProvider` surface in `api.d.ts` remains
  the long-term native contract. **Phase 6.1** ships a TypeScript provider
  runtime at `@vibecook/mille/provider` with:
  - `createMemoryFileSystemProvider` (non-local test/demo backend)
  - capability gating (`withCapabilityGate`, `providerSupports`,
    `describeUnsupported` → `EUNSUPPORTED`)
  - registry, latency / offline wrappers, and `createProviderTreeSession`
    for renderable trees without native scheme dispatch
  - platform path helpers (`parsePlatformPath`, UNC/drive, Unicode NFC)
  Native `registerProvider` wiring is still deferred; local `file:` trees
  continue to use `FileExplorer`.
- **Watchman optional backend** — v0.1 uses notify-rs's per-platform default
  (inotify / FSEvents / ReadDirectoryChangesW). A Watchman adapter is
  tracked for later.
- **Full `AbortSignal` on every async method** — `signal` is accepted on the
  TS signatures but currently ignored for native I/O (`readFile`, `readText`,
  `readFileStream`). napi-rs 3.x has a `!Send` constraint on async fns with
  references that is blocking the restructure. Streams already cancel on
  generator close; explicit-signal support lands once the constraint is
  resolved.
- **Content search** — filename search ships in this package; content search
  (ripgrep-style) will be a separate `@vibecook/mille-search` package that
  consumes this one.
- **Full example Electron app** — a minimal `examples/electron` scaffold (the
  patterns in section 2 turned into a runnable app) lands in a post-v0.1 PR.
- **Sort on the snapshot** — `sort`/`sortDir` on `VisibleRowsOptions` is
  declared in `api.d.ts` but not yet honored by the client-side slice. Sort
  renderer-side in the meantime.
- **Path-keyed lookups on the snapshot** — `getByPath` does not exist yet.
  Maintain your own path↔id map by listening to the delta stream.
- **Decorations over the wire** — `PortFileExplorer.getSnapshot().getDecorations`
  always returns `[]`. Register providers in the same process that owns the
  `FileExplorer` (utility process or local Node).
- **`writeSnapshot` / crash-resume** — `snapshotPath` is accepted on
  `ExplorerOptions` but the resume path is a Phase 12 concern.

---

## Appendix: import cheatsheet

```ts
// Local-mode (Node, utility process)
import { FileExplorer, MirrorSnapshot } from '@vibecook/mille';
import { FileSystemError, isFileSystemError, type ErrorCode } from '@vibecook/mille';
import type {
  Entry,
  EntryId,
  VisibleRow,
  VisibleRowCount,
  VisibleRowsOptions,
  Decoration,
  DecorationProvider,
  ExplorerOptions,
  Disposable,
  EventName,
  SearchHit,
  SearchOptions,
  Uri,
} from '@vibecook/mille';

// UtilityProcess host
import { createFileExplorerHost } from '@vibecook/mille/host';
import type { FileExplorerHost, MessagePortLike } from '@vibecook/mille';

// Renderer
import { connectFileExplorer, PortFileExplorer, PortMirrorSnapshot } from '@vibecook/mille/client';

// React
import { useFileExplorerSnapshot, useFileExplorerTreeSnapshot } from '@vibecook/mille/react';
```

## 10. Explorer settings

Mille exposes a versioned settings document separately from
`ExplorerOptions`, allowing a host to store global defaults plus workspace and
root overrides before constructing or reconfiguring an explorer:

```ts
import {
  parseExplorerSettings,
  resolveExplorerSettings,
  serializeExplorerSettings,
} from '@vibecook/mille';

const document = parseExplorerSettings(await storage.get('explorer.settings'));
const settings = resolveExplorerSettings(document, workspaceId, rootId);
await storage.set('explorer.settings', serializeExplorerSettings(document!));
```

The resolved shape includes sort mode, case/locale behavior, folders-on-top,
hidden/ignored visibility, compact folders, exclude globs, and nesting rules.
The parser migrates the pre-release flat shape and bounds hostile or stale
workspace data.

Pass the resolved record at construction to apply native ordering:

```ts
const fx = new FileExplorer({
  roots: ['/workspace'],
  settings: resolveExplorerSettings(document, workspaceId, rootId),
});
```

Name, type/extension, modified-time, case-sensitive, locale-aware, and
folders-on-top ordering are applied in the native store and preserved across
the host/port boundary. Modified-time order is newest first. A non-null
`locale` accepts a BCP-47 locale such as `en`, `sv`, or `es-u-co-trad`;
numeric filename ordering remains enabled. Case-insensitive locale sorting
distinguishes accents but ignores case, while case-sensitive sorting also uses
case at the tertiary comparison level. Equal collation weights fall back to
Mille's deterministic natural order. Invalid locale tags reject construction
or live reconfiguration without publishing a partial tree version.

Hidden and ignored visibility is also applied at the snapshot boundary, so
rows, ID-only projections, counts, indexes, typeahead, and port viewports agree.
The default Project view shows project dotfiles and ignored/excluded artifacts,
while suppressing `.git` and common OS metadata. Per-query
`includeIgnored: true` requests the completely unfiltered view.

`settings.excludeGlobs` and the legacy top-level `excludeGlobs` option are
de-duplicated and layered onto repository ignore rules. They mark matching
entries ignored and stop eager descent into matching directories during full,
lazy, and watcher-reconciliation walks. Set `showIgnoredFiles: false` to hide
those entries from the projection.

With `compactFolders: true`, single-directory chains below workspace roots are
projected as one row. The row keeps the leaf directory's stable ID and exposes
the full label in `pathSegments` (for example `['src', 'main', 'java']`).

`fileNestingPatterns` projects generated or companion files below a sibling
source file:

```ts
const settings = {
  ...DEFAULT_EXPLORER_SETTINGS,
  fileNestingPatterns: {
    '*.ts': ['${capture}.test.ts', '${capture}.js'],
    'package.json': ['package-lock.json'],
  },
};
```

A parent pattern accepts zero or one `*`; `${capture}` in each child template
is replaced with the matched stem and resolves to an exact sibling filename.
Rules and child lists are bounded by the settings parser. When multiple parents
could claim the same file, current sibling order then normalized rule order
wins. Nesting is one level deep, never creates synthetic IDs, and never changes
filesystem mutation paths. Local snapshots and port clients use the same
authoritative projected child lists.

Display projection settings can be changed without rebuilding or rewalking the
explorer:

```ts
await fx.updateProjectionSettings({
  ...settings,
  showHiddenFiles: false,
  compactFolders: true,
  excludeGlobs: ['dist/', '*.generated.ts'],
});
```

The update publishes one atomic tree version covering sort mode, case
sensitivity, locale, folders-on-top, hidden/ignored visibility, compact
folders, and file nesting. Changing `excludeGlobs` reclassifies every indexed
entry in the same publication without losing repository-ignore provenance.
Future lazy walks, mutations, and watcher reconciliation use the new rules;
descendants of a newly un-excluded directory remain bounded and hydrate when it
is expanded. Existing snapshots remain immutable. On a port client, the
promise resolves only after every attached mirror has received the new
projection.

Workspace-root display labels and ordering are independent from filesystem
identity. Give `FileTree` a presentation-only resolver when product names
should differ from directory basenames:

```tsx
<FileTree
  fx={fx}
  ariaLabel="Files"
  rootLabel={(root, { index, duplicateIndex, duplicateCount }) =>
    duplicateCount > 1
      ? `${index + 1}. ${root.name} (${duplicateIndex + 1})`
      : `${index + 1}. ${root.name}`
  }
/>
```

Without a resolver, duplicate basenames render as `workspace (1)`,
`workspace (2)`. The underlying `Entry.name` and indexed path are unchanged.
To change live display order, pass every current root ID exactly once:

```ts
const roots = fx.getSnapshot().roots();
await fx.reorderRoots(roots.map((root) => root.id).reverse());
```

An identical order is a version-free no-op. Invalid permutations reject
atomically. A local explorer returns synchronously; a port explorer resolves
after all attached mirrors have received the new ordered root list. Retained
snapshots keep their original order.

Replace root membership without rebuilding the explorer:

```ts
await fx.updateWorkspaceRoots(['/workspace/app', { scheme: 'file', path: '/workspace/shared' }]);
```

Retained paths keep their entry IDs. Added directories appear immediately as
lazy root entries and hydrate when expanded; removed roots, known descendants,
path indexes, expansion state, and remote-mirror records disappear in the same
tree publication. Passing `[]` produces an empty workspace, and re-adding a
removed path assigns a fresh identity. Duplicate, overlapping, missing, and
non-directory roots reject without changing configuration or tree version.
Port calls resolve only after every attached mirror receives the membership
change. Watch registrations, exclude matching, and later path resolution all
use the replacement list.

`FileTree` treats the `FileExplorer` object as the identity boundary for all
uncontrolled UI state. Replacing `fx` starts a fresh tree session even when the
new engine reuses entry IDs or version numbers; expansion, selection, focus,
navigation, rename, clipboard, drag, scroll, and structural caches reset. In
contrast, calling `updateWorkspaceRoots()` on the same `fx` retains state for
the entry IDs the engine retains, as described above. Controlled props remain
owned by the caller and should be re-scoped when their engine changes.

Configured roots that disappear or become unreadable remain in the tree with
their original ID and `EntryKind.Unavailable`; known descendants are removed
so stale files cannot be opened. Re-stat roots without walking their contents:

```ts
await fx.refreshWorkspaceRoots();
```

When a root returns, the same entry becomes a lazy directory again. Local
watcher deletion/recreation hints use the same transition, while
`refreshWorkspaceRoots()` is the deterministic fallback for disconnected
mounts and permission changes. Port refreshes resolve after all attached
mirrors are current. The default `FileTree` renders unavailable roots as
disabled, non-draggable folder rows with no disclosure affordance.

Use authoritative reconciliation when files may have changed without a usable
watch event:

```ts
await fx.resync(folderId, { recursive: true }); // complete subtree
await fx.resync(fileId); // containing directory
await fx.resyncWorkspace(); // every configured root
```

`resync()` uses the same bounded scanner and policy gates as watcher recovery.
A directory refresh without `recursive: true` reconciles its immediate
children; a file refresh reconciles its containing directory so additions,
deletions, and renames beside that file are not missed. `resyncWorkspace()`
walks every configured root. Local calls emit at most one tree change, and a
no-op preserves the tree version. Port calls resolve only after every attached
mirror has received the authoritative result.

Moves and copies deny cross-root transfers unless the call opts in:

```ts
await fx.move(entryId, destinationFolderId, undefined, {
  crossRoot: true,
  collision: 'rename',
});
```

`collision` defaults to `'error'` and returns `EEXIST` before changing disk.
Other policies:

| Policy | Behavior |
| --- | --- |
| `error` | Fail when the destination name exists (including case-only siblings) |
| `rename` | Choose a free `copy` / `copy N` suffix |
| `overwrite` | Replace the destination file or directory |
| `skip` | Leave the destination untouched and succeed |
| `merge` | For directories, merge children; files overwrite |

The same options apply to `copy`, `move`, and `copyFromPath`. Destination names
must be single path components (no `..` / separators). Destination parents are
containment-checked after symlink resolution so workspace roots cannot be
escaped. Self-overwrite is rejected. Hosts that need UI prompts can use
`FileTree` `dragDrop.onCollision` (invoked only when `probeDestination` reports
a collision) and `onDropError` for failed imports.

### Progress and cancellation

Long recursive copies accept an `operationId` (and optional `reportProgress`)
on `TransferOptions`. While the operation runs the explorer emits warnings:

| Code | Meaning |
| --- | --- |
| `OP_PROGRESS` | `detail` JSON: `{ operationId, phase, done, total, path? }` |
| `OP_COMPLETE` | finished successfully or failed (`status`, `done`, `total`) |
| `OP_CANCELLED` | cancelled cooperatively (`status: "cancelled"`) |

```ts
const sub = fx.on('warning', (w) => {
  if (w.code === 'OP_PROGRESS') console.log(JSON.parse(w.detail!));
});
await fx.copyFromPath(src, parentId, undefined, {
  operationId: 'import-1',
  // or: signal: abortController.signal  // auto-generates an operation id
});
fx.cancelOperation('import-1'); // cooperative; checked between recursive steps
```

Cancelled **create** copies best-effort remove the partial destination.
**Overwrite** copies stage into a sibling path and only swap on success, so a
cancelled overwrite restores/keeps the original destination. Hosts using
`createFileExplorerHost` forward `OP_*` warnings to every attached port client.

### Trash and undo

`delete(id)` defaults to **`trash: true`**: the entry is soft-deleted into a
**managed recycle directory outside the workspace**
(`$TMPDIR/mille-recycle/<workspace-hash>/…`) so restore cannot be hijacked by
in-tree symlinks and never pollutes the file tree. Pass `{ trash: false }` for
a permanent delete.

```ts
await fx.delete(fileId); // soft-delete (default, undoable)
await fx.delete(fileId, { trash: false }); // permanent

fx.canUndo(); // boolean — undoable stack only
fx.peekUndo(); // next undoable op, or null
fx.lastMutation(); // most recent op, including non-undoable with reason
await fx.undo(); // reverse create / rename / move / soft-delete
```

| Operation | Undo behavior |
| --- | --- |
| `create` | Removes the path only if store identity + size still match |
| `rename` / same-root `move` | Moves back when identity and free destination allow |
| overwrite `move` | **Not undoable** — reported via `lastMutation().reason` |
| soft `delete` | Restores from managed recycle |
| permanent `delete` | **Not undoable** — `lastMutation()` explains why |

`create` / `rename` refuse to clobber existing destinations (`EEXIST`). Failed
`undo` leaves the journal entry so the user can fix conflicts and retry.
Same-volume directory moves preserve the complete known subtree identity;
cross-device moves return `EUNSUPPORTED` without partial store mutation until
the Phase 4 copy/delete fallback lands. Progress, cancellation, and undo
remain later Phase 4 work.

### External import (`copyFromPath`)

OS drag-in and any host-driven import use:

```ts
await fx.copyFromPath('/absolute/source/path', destinationFolderId, undefined, {
  collision: 'rename',
});
```

Files and directories copy recursively with content preserved. Failures surface
as structured `FileSystemError`s and never create empty placeholder files.
Partial directory copies best-effort remove the incomplete destination.
`FileTree` external drops require `copyFromPath` and report per-item failures
instead of silently inventing empty entries. `FileTree` drag/drop defaults to
`crossRoot: false` and forwards collision policy for both internal and external
transfers.

Counts, indexes, typeahead, host viewports, and port mirrors use the same leaf
identity. In roots-only mode the host follows the chain with bounded depth-1
reads and stops at the first branch; it does not eagerly walk the subtree.
