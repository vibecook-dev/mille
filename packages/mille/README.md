# @vibecook/mille

High-performance file explorer library for Electron IDEs. A native Rust core
(walker, watcher, ignore matcher, snapshot + crash-resume, fuzzy search) is
exposed to Node via NAPI-RS; per-platform prebuilt binaries ship through
`optionalDependencies`, so `npm install @vibecook/mille` resolves the
right binary for the host automatically.

The package exposes a typed `FileExplorer` wrapper, a renderer-side
`ViewportMirror` with frozen snapshots, a React hook
(`useFileExplorerSnapshot`), and a host/client pair
(`createFileExplorerHost` + `connectFileExplorer`) for the recommended
UtilityProcess architecture.

## Integration guide

See [**EMBEDDING.md**](./EMBEDDING.md) for the full integration walkthrough:
minimum-viable Node usage, the Electron UtilityProcess + MessagePort pattern
(main / utility host / preload / renderer end-to-end), React +
`@tanstack/react-virtual` viewports, decoration providers, error handling,
`electron-builder` packaging, performance tuning, and troubleshooting.

The public API surface is declared in [`api.d.ts`](./api.d.ts).

## Remote sessions over a WebSocket

The host and client talk over a channel, not a fixed transport, so a client
does not have to share a process — or a machine — with the host. A WebSocket
carries mille's framed codec one frame per binary message, and the
`mille.remote` handshake in front of it names an export and presents a
credential. No host session exists until your `authorize` hook has accepted
that credential, and the session's policy is the one `authorize` returns, never
what the client asked for.

### Serving: a host, a WebSocket server, a ticket check

```ts
import { WebSocketServer } from 'ws';
import { createFileExplorerHost } from '@vibecook/mille';
import { DEFAULT_LIMITS, serveWebSocketSession } from '@vibecook/mille/node';

const host = await createFileExplorerHost({ roots: ['/home/me/repo'], initialWalk: 'roots-only' });
// maxPayload lets `ws` refuse an oversized message before buffering it.
const wss = new WebSocketServer({
  host: '127.0.0.1',
  port: 0,
  maxPayload: DEFAULT_LIMITS.maxFrameBytes,
});

wss.on('connection', (socket) => {
  serveWebSocketSession(socket, {
    authorize: ({ exportId, requestedAccess, credential }) => {
      const ticket = tickets.take(credential); // yours: single use, short-lived
      if (ticket === undefined || ticket.exportId !== exportId) return null; // ACCESS_DENIED
      return { host, policy: { access: ticket.access, allowSearch: true } };
    },
  });
});
```

- **The grant is the policy.** `requestedAccess` can only narrow it: a client
  asking for read-only on a read-write grant gets read-only (and none of the
  host-global `allow*` flags); asking for read-write on a read-only grant is
  refused. `maxFileBytes` defaults to 16 MiB and is capped below one frame.
- **Refusals.** Return `null` for a uniform `ACCESS_DENIED` (an unknown export
  and a forbidden one look the same unless `diagnosticDisclosure` is on), or
  `{ refuse: 'LIMIT_EXCEEDED' | 'SERVER_SHUTTING_DOWN', reason }`. A throw is a
  denial. On a WebSocket an `ACCESS_DENIED` closes with code 4008.
- **The credential is never logged** by mille, and is scrubbed from your
  `authorize` exception messages and refusal reasons before they reach the
  logger. It rides the open request, inside the channel — not the URL, whose
  query string ends up in access logs. If you authorize at the upgrade instead
  (a subprotocol, say), check it there and ignore `credential`.
- **Bounded sessions.** The open (authorize included) must finish within
  `openTimeoutMs` (10 s); heartbeats ping after `heartbeatMs` (20 s) of
  silence and close after `idleTimeoutMs` (60 s). Heartbeats never reach the
  host. A result too large for one frame (`readText` of a big file) answers
  `EFBIG` and the session carries on. A viewport (limit plus overscan) is
  capped at the grant's `maxViewportRows`, 2,048 rows by default; a larger
  request is clamped and the client gets a `VIEWPORT_CLAMPED` warning, and
  `accepted.limits.maxViewportRows` says the cap up front. `serveRemoteSession(channel, options)`
  is the same logic on any channel — a framed Duplex, a MessagePort.
- **One host per export.** A session sees every root of the host it attaches
  to, so give each export its own host. Keep `followSymlinks: false` on any
  host that serves restricted sessions: with it on, the walker records a
  followed link as a plain directory, and the symlink boundary that keeps a
  restricted session from seeing past a link cannot tell it is one.

### Attaching from a renderer Worker

```ts
import { connectFileExplorerChannel } from '@vibecook/mille/port';
import { connectWebSocketChannel, relayExplorerChannel } from '@vibecook/mille/remote';

const { channel, accepted } = await connectWebSocketChannel('ws://127.0.0.1:41234', {
  exportId: 'repo',
  credential: ticket,
});

// Either browse here…
const fx = await connectFileExplorerChannel(channel);
// …or hand an isolated consumer nothing but a MessagePort:
relayExplorerChannel(channel, port); // the consumer runs connectFileExplorer(port)
```

`@vibecook/mille/remote` and `@vibecook/mille/port` import nothing from Node
and never load the native addon; a test walks their compiled import graphs, and
another loads them in a Worker under a hook that refuses Node builtins. On Node
20, which has no global `WebSocket`, pass `{ WebSocket }` from `ws`.

When the upstream session ends, the relay tells the port's client before
closing the port (a MessagePort cannot carry a close by itself): pending calls
reject with the upstream reason and later calls fail at once.

### Search over the channel

`fx.search(query, { limit, kinds, includeIgnored, caseSensitive })` runs on the
host's engine. Pages default to 100 hits, are clamped to 1,000, and say
`clipped` when the engine had more; each hit carries its `path` from the root.
A remote session needs `allowSearch` in its grant. Search covers what the
engine holds: on a `roots-only` host, a folder nobody has opened is not
searched until something walks it.

### Close codes

| Code               | Meaning                                                       | Channel close code         |
| ------------------ | ------------------------------------------------------------- | -------------------------- |
| 1000               | orderly close                                                 | `REMOTE_CLOSE` at the peer |
| 4002 / 4003 / 4009 | malformed frame / text message / over the frame limit         | `PROTOCOL_ERROR`           |
| 4008               | open refused                                                  | `AUTH_REJECTED`            |
| 4011               | transport fault                                               | `TRANSPORT_ERROR`          |
| 4013               | the peer stopped reading and the outbound queue hit its limit | `BACKPRESSURE`             |

Application codes are 4000 + the standard code they stand for, because a
browser's `close()` accepts nothing else; the standard codes `ws` and the
network produce (1009, 1006, …) map the same way.

`pnpm bench:transport` measures a loopback WebSocket against an in-process
MessagePort on a 10,000-entry folder: about 2 ms more to attach (TCP, upgrade,
the ticketed open) and 1–3% on everything after, with identical payloads.

## Supported platforms

| Platform          | Optional dependency |
| ---               | --- |
| macOS arm64       | `@vibecook/mille-darwin-arm64` |
| macOS x64         | `@vibecook/mille-darwin-x64` |
| Windows x64       | `@vibecook/mille-win32-x64-msvc` |
| Windows arm64     | `@vibecook/mille-win32-arm64-msvc` |
| Linux x64 glibc   | `@vibecook/mille-linux-x64-gnu` |
| Linux arm64 glibc | `@vibecook/mille-linux-arm64-gnu` |
| Linux x64 musl    | `@vibecook/mille-linux-x64-musl` |
| Linux arm64 musl  | `@vibecook/mille-linux-arm64-musl` |

## License

MIT
