# mille changelog

## [0.3.4](https://github.com/vibecook-dev/mille/compare/v0.3.3...v0.3.4) (2026-08-09)


### Features

* add Electron mesh file browser demo ([815e445](https://github.com/vibecook-dev/mille/commit/815e4451923a3b7d5b7177339798ea342f7fddfc))


### Bug Fixes

* **ui:** hydrate remote folder expansions ([8b0e759](https://github.com/vibecook-dev/mille/commit/8b0e7593a7583bcd3f743086a239bbf58c09a631))

## [0.3.3](https://github.com/vibecook-dev/mille/compare/v0.3.2...v0.3.3) (2026-08-03)


### Features

* **truffle:** publish mille-truffle on the shared release version line ([#40](https://github.com/vibecook-dev/mille/issues/40)) ([80d6593](https://github.com/vibecook-dev/mille/commit/80d65939f498612e9ba46dff99e081d01ea0f48e))

## [0.3.2](https://github.com/vibecook-dev/mille/compare/v0.3.1...v0.3.2) (2026-07-31)


### Bug Fixes

* **ci:** test the release PR before it can break main ([#37](https://github.com/vibecook-dev/mille/issues/37)) ([b1631df](https://github.com/vibecook-dev/mille/commit/b1631df35f3343e8861112730bf5e015c9cc2534))
* **test:** admit the empty file git leaves when killed mid-restore ([#39](https://github.com/vibecook-dev/mille/issues/39)) ([5be7dfe](https://github.com/vibecook-dev/mille/commit/5be7dfe9a9a8b48d82f65d0787ca480f90d12319))

## [0.3.1](https://github.com/vibecook-dev/mille/compare/v0.3.0...v0.3.1) (2026-07-30)

Windows was building but never running. Every test job targeted
`ubuntu-latest`; Windows appeared only in the build matrix, which compiles a
`.node` and uploads it without loading it. The one thing that executed on
Windows was the post-release published-package smoke test — 94 lines that load
the binary and perform a single walk. So the watcher, the host/renderer port
protocol and the mutation paths shipped unexercised there, and three defects
were living in that gap.

### Remote workspaces (`@vibecook/mille-truffle`, new — first published as 0.3.2)

- **A client can connect, survive a disconnect, and reconnect.**
  `connectMille(mesh, { peer, exportId })` dials, runs the open handshake, and
  returns a `RemoteFileExplorer` that owns one live session and swaps it
  underneath the caller on reconnect — so a dropped connection does not
  invalidate the reference an application is holding.
  Two behaviours are load-bearing. A dropped connection leaves the last mirror
  snapshot readable through `getSnapshot()`, because the user was looking at
  something and a stale tree beats a blank one; `remote.explorer` still throws
  while offline, so new work fails fast rather than hanging. And queued
  mutations are never replayed — a write that never returned a result frame
  did not happen, and re-issuing it after a gap could duplicate a rename or
  clobber a file changed in the interim.
  Backoff is exponential with symmetric jitter, clamped so a large jitter
  cannot push a retry past the ceiling. `ACCESS_DENIED`, `PROTOCOL_MISMATCH`
  and `LIMIT_EXCEEDED` are terminal and never retried: the answer will not
  change, and a client redialling a denial looks like a brute-force attempt to
  whoever reads the server log. Reconnecting to a _replaced_ host emits
  `identityReset`, since every `EntryId` the caller holds then refers to
  nothing — or worse, to something else.
  Verified across a real tailnet end to end with both public APIs: connect,
  browse, mutate, a read-only export refusing writes with `EROFS`, read-write
  on a read-only export refused outright, then the server killed and replaced
  mid-session — `stale → reconnecting → online`, one `identityReset`, tree
  usable again. Thirteen checks.
- **A mille workspace can now be served to another tailnet device.**
  `serveMille(mesh, { exports })` binds a Truffle `mesh.net` listener, runs the
  open handshake, authorizes the peer, and attaches the accepted socket to a
  `FileExplorerHost` — so a client on another machine drives the same explorer
  API it uses locally, with no per-file network RPC. Exports are named and
  server-defined: a client asks for `"work"`, never for a path, which is why
  traversal is not a request-time concern. Roots are canonicalized once at
  startup and a misconfigured export fails the service at boot rather than
  surfacing as a confusing denial later.
  The ordering inside an accepted connection is the security property: the
  verified Tailscale peer id is readable synchronously at accept, and no host,
  engine, or filesystem handle comes into existence until authorization has
  passed. An unknown export and a forbidden one return byte-identical
  rejections, because anything else turns the service into an enumerator of
  its own exports. A socket with no verified identity is refused, an
  `authorize` callback that throws is a denial, and asking read-write of a
  read-only export is rejected rather than quietly downgraded.
  One host is shared per distinct export configuration, keyed on roots and
  explorer options but never on identity — who you are decides whether you may
  attach, not which engine you attach to. It stays warm for five minutes after
  its last session so a reconnect keeps the same `EntryId`s.
  Verified against a real tailnet, not only a fake mesh: two ephemeral nodes,
  a denied export refused with no host created, an authorized peer browsing
  and mutating, and PR 3's policy still returning `EACCES` for host-global
  calls across the wire. That run caught a defect the fake could not — mille's
  default `initialWalk: 'full'` is a no-op meaning "the consumer calls
  `populateFromRoots` itself", which a remote peer cannot do, so every served
  workspace would have sat empty forever. Served exports now default to
  `roots-only`.

### Engine (`@vibecook/mille`)

- **A restricted session could read outside the workspace through a symlink or
  junction** — and a read-only remote session on an export explicitly
  configured `followSymlinks: false` was the exact case that leaked. The
  walker honours that setting: it listed a junction as a symlink entry and
  never descended it. `resolvePath` did not. It resolves through the operating
  system and hydrates whatever it finds, inserting the target as a child of
  the link, so the entry ended up structurally inside the tree and physically
  outside it — after which `readFile` served the real file. Two subsystems,
  each correct on its own, disagreeing about what "do not follow symlinks"
  means. Reproduced with a Windows junction, which needs no elevation to
  create: `resolvePath('escape-link/secret.txt')` returned an id and reading
  it returned the contents of a file outside the export.
  Path resolution now refuses to cross a symlink for any session that is not
  local-admin. `resolvePath` and `findVisiblePrefix` return `null` — the same
  answer as "does not exist", so the boundary cannot be probed — and
  id-bearing mutations reject with `EACCES`, checking every id argument
  because a move names two and because an id is just a number on the wire that
  could arrive from a guess or from an entry another session hydrated into the
  shared store. Seeing a link is still fine; traversing through one is not.
  Admin sessions are deliberately exempt: they already hold the raw explorer
  through `host.local` and can read anything the process can.
  Worth recording how this surfaced. The test skipped on Windows at first,
  because directory symlinks need Developer Mode — the same
  building-but-never-running shape the Windows readiness work existed to fix.
  Falling back to a junction, which AC-008 names explicitly and which requires
  no privilege, made it run and fail immediately.
- **`ExplorerSessionContext` accepts explicit `undefined`** — its optional
  fields were declared `?:` without `| undefined`, which under
  `exactOptionalPropertyTypes` is not the same thing. Every real caller builds
  the context from values that are already `string | undefined` (a Truffle
  socket's `remotePeerId` is exactly that), so constructing one required a
  conditional spread per field.
- **Sessions now have permissions, and the host enforces them** — a session
  attached with `attachChannel` carries an `ExplorerSessionPolicy`, and every
  mutation and call is gated against it before native dispatch. The whole
  matrix lives in two lookup tables in `src/channel/policy.ts` rather than in
  conditionals spread through the dispatch switch, because "which of the forty
  entry points did we forget to gate" should not be answerable only by
  grepping. Read-only sessions get `EROFS` on writes; the host-global controls
  — undo, projection settings, workspace roots, workspace resync, external
  import via `copyFromPath`, client-pushed decorations — are denied to remote
  sessions by default and opened only by an explicit per-flag grant, because
  each of them changes what _every_ session sees. `attachPort` is unchanged and
  still local-admin, so in-process consumers see no difference.
  Three consequences worth naming separately. Capabilities are masked per
  session (`Capability.Readonly` in, `ReadWrite`/`Trash`/`AtomicWrite` out) and
  `PortFileExplorer.capabilities()` now exists to read them — masking a value
  no client could fetch would have been decorative. Transfer progress is routed
  to the session that owns the operation instead of broadcast: `OP_PROGRESS`
  detail carries source and destination paths, so the old fan-out told every
  attached window what every other one was copying. And operation ids are
  claimed per session — a second session using an in-flight id is refused
  `EEXIST`, cancelling someone else's operation is refused indistinguishably
  from cancelling a nonexistent one, and claims are released on success,
  failure, completion, and session close. Entry `resync` is rate-limited to 10
  per minute per session on a sliding window, which is per-session so a noisy
  peer cannot starve its neighbour.
- **File payloads stopped round-tripping through JSON number arrays** —
  `readFile` returned `Array.from(buf)` and `writeFile` sent `Array.from(data)`,
  which inflated every byte into a decimal integer plus a comma. Both now carry
  the `Uint8Array` itself: structured clone preserves it and the framed codec
  ships it as a raw attachment. Both sides accept either form, so a new client
  and an old host — or the reverse — keep working. `ErrorCode` gains `EFBIG`
  for the per-request size limit the remote control stream will enforce.
- **The host/client protocol now runs over a byte stream** — `@vibecook/mille/node`
  adds `createFramedStreamHostChannel` / `createFramedStreamClientChannel`, which
  carry the existing protocol over any Node `Duplex`. A real `FileExplorerHost`
  serving a real `PortFileExplorer` across paired `PassThrough`s is covered
  end-to-end: handshake, roots, expansion, viewport rows, RPC, and abrupt
  transport loss. Swap the pipes for a socket and that is a remote workspace.
  The wire format is a 20-byte header (`MLLE`, big-endian, versioned major and
  minor) plus UTF-8 JSON metadata and raw binary attachments. Payloads matter
  here: snapshots and deltas already carry bincode buffers, and base64-ing them
  through JSON would inflate every one. Instead the encoder swaps each binary
  view for a `{$mille:'bin',i}` placeholder and appends the exact bytes — honouring
  `byteOffset`/`byteLength`, so a 10-byte subarray of a 1000-byte buffer ships 10
  bytes rather than the whole backing allocation. The decoder validates the
  header before allocating anything, so a frame claiming 4 GiB of attachments is
  rejected having buffered only the 20 bytes that actually arrived; a fuzzer
  mutates every byte of a valid frame and asserts the only failure mode is a
  clean protocol error. Fragmentation is arbitrary — one frame across 40,000
  single-byte chunks and 50 frames in one chunk both decode, and a property test
  re-partitions a stream at random cut points. Writes queue in the channel and go
  to the stream one at a time, stopping when `write()` returns false, so
  `bufferedBytes` measures what we are holding rather than what the OS took, and
  the hard limit closes the connection instead of growing without bound.
  A malformed frame retires only that session; the shared host and its other
  sessions keep serving.
- **The host and client were welded to MessagePort for no reason** — both sides
  only ever needed an ordered, reliable, message-oriented pipe with a close
  notification, but that requirement was never named, so `postMessage` calls
  and port lifecycle were spread through `host.ts` and `client-port.ts`. They
  now talk through an `ExplorerChannel`, with `createMessagePortHostChannel` /
  `createMessagePortClientChannel` as the compatibility path:
  `attachPort(port)` is a wrapper over the new `attachChannel(channel, context)`
  and `connectFileExplorer(port)` over `connectFileExplorerChannel(channel)`.
  Existing consumers need no migration and the wire format is unchanged — this
  is the seam a framed Node Duplex (and through it a remote workspace) plugs
  into. Two things improved on the way through. `adaptPort` was duplicated
  verbatim in the host and the client; it is now one function. And its Node
  `MessagePort` branch has always carried a no-op `removeEventListener` with a
  `TODO(7.10)`, which meant a detached session could still observe messages —
  the channel now gates delivery on its own state, so a closed channel
  delivers nothing even when the underlying listener could not be detached.
  A closed channel also rejects the client's in-flight calls and emits a
  `connection` event rather than leaving callers hanging; the last mirror
  snapshot stays readable. Session policy types ship with it but enforce
  nothing yet.
- **Every file save on Windows arrived as a warning, a pair-window late** —
  `ReadDirectoryChangesW` reports a content write as `FILE_ACTION_MODIFIED`
  without distinguishing data from metadata, so notify surfaces `Modify(Any)`.
  That fell into the classifier's catch-all and became `RawEvent::Any`, and
  `Any` is not merely "ambiguous": it is the only channel `RenamePairer`
  accepts. Every save was therefore queued as a possible rename half, held for
  the pair window, then flushed as `RenameDegraded` — consumers listening for
  `changed` never fired, and got a `WRENAMEDEGRADED` warning instead. Measured
  through the public API: modify now reports `changed` in ~49 ms (the debounce)
  where it previously reported a warning. Reconciliation was never affected —
  `Modified` and `Unknown` take the same `reconcile_nearest_parent`.
  Classifying `Modify(Any)` alone regressed directory renames, because Windows
  can report `Modify(Name(..))` and `Modify(Any)` for one path in a single
  batch and the coalescer's `Modified`-wins arm then consumed the rename's
  "from" half, leaving the destination with no known descendants; the coalescer
  keeps the pairing channel intact on Windows for that reason. Both halves are
  pinned by tests that assert the Windows and non-Windows mappings explicitly.
- **The documented synchronization points were timed, not acknowledged** —
  `mutate`, `updateProjectionSettings`, `reorderRoots`, `updateWorkspaceRoots`
  and `refreshWorkspaceRoots` all promise in comments that every attached
  mirror is current before the initiating client observes completion, but each
  flushed with a single `setImmediate`. That is a guess about when the peer
  runs; it held on an idle Linux runner and lost on Windows, where a second
  client's mirror was reliably one version behind when the initiator's promise
  resolved. They now use the acknowledged flush that `resync` already had. Six
  tests across five files covered this invariant and had never run on a
  platform that could fail it.
- **A client that never acknowledges no longer slows every mutation** — the
  change above introduced this: a session that handshakes but predates the
  `ack` frame cannot satisfy the wait, so each mutation paid the full fallback,
  measured at ~1012 ms per rename. Sessions that time out are now marked and
  skipped by later synchronization points, and any `ack` restores standing, so
  a momentarily busy renderer recovers rather than being written off. One
  bounded probe, then ~1 ms.
- **A created file's name could not be reused after deleting it on Windows** —
  the undo journal pinned an open handle to every file it created, held for the
  life of the journal entry. That pin exists because POSIX recycles an inode as
  soon as its last link goes, and an open descriptor is what keeps the number
  from being handed to a different file. Windows needs none of it: an NTFS file
  id carries a sequence number that advances when the MFT record is reused, so
  the id cannot come to mean a different file — the journal's own notes said as
  much. The handle was only ever the means of reading the id there. Its cost
  was real: on Windows a delete with a live handle can only mark the file
  delete-pending, so the name stays in the directory and cannot be reused.
  Create a file through mille, delete it in Explorer, and creating a file of
  the same name failed with `EPERM` while the undo entry lived. Windows now
  records the id at create time and closes the handle; `undo` still refuses a
  replaced file. Verified by holding an exclusive (`share_mode(0)`) open
  against a just-created file: previously `ERROR_SHARING_VIOLATION`, now
  succeeds. Only reproducible on Windows builds without POSIX-semantics
  deletes, which is why a Server 2022 runner caught it and a Windows 11
  developer machine did not.
- **Windows filesystem errors reported `EUNKNOWN`** — error mapping fell back
  to `io::ErrorKind` on Windows, which has no category for
  `ERROR_SHARING_VIOLATION`: a file held by another process (an open editor, a
  scanner) reported `EUNKNOWN` despite `EMBEDDING.md` documenting `EBUSY` for
  exactly that case. Win32 status codes now map explicitly — sharing and lock
  violations, disk-full, write-protect, invalid name, aborted operation and
  reparse-resolution failure among them.

### Tooling & CI

- **The suite runs on Windows** — a `test-windows` job runs fmt, clippy, the
  Rust suite, the full JS suite, and repeats the watcher and port
  synchronization regressions ten times each. It carries an explicit
  `timeout-minutes`, because the failure mode that concealed the port defect
  was a test process that completed its tests and then never exited.
- **Line endings are pinned** — `.editorconfig` declared `end_of_line = lf`
  but nothing enforced it at checkout, so a Windows clone (Git's default
  `core.autocrlf=true`) materialized the tree as CRLF. That broke
  `pnpm format:check` and `cargo fmt --check` wholesale — `rustfmt.toml` sets
  `newline_style = "Unix"` — and left the committed, generated `tokens.css`
  dirty after every build. A `.gitattributes` pins the checkout and
  `tokens.css` generation moved to a script that normalizes line endings, so
  the artifact is byte-identical on every platform.
- **`pnpm test` runs outside a globbing shell** — the package test scripts were
  spelled `node --test test/*.test.mjs`, which depends on the _shell_ expanding
  the glob. pnpm runs package scripts through the platform shell, and neither
  cmd.exe nor PowerShell expands globs, so on Windows Node received the pattern
  verbatim and exited before running anything. Node only learned to expand
  globs itself in v21 and this repo targets v20, so the three packages now
  share a small runner that expands it explicitly.
- **Test files no longer assume POSIX** — `smoke.test.mjs` and
  `decode.test.mjs` hardcoded a `.node` candidate list covering only darwin
  and linux-gnu; the first failed at import on Windows and musl, and the
  second silently skipped its only live round-trip while reporting green. Both
  now derive the binary name from the host. Also fixed: POSIX basename
  splitting on real temp paths, Unix errno literals, rooted-but-not-absolute
  workspace paths, a symlink fixture that needs Windows Developer Mode, and a
  git fixture that inherited the developer's `core.autocrlf`.


### Features

* **host:** carry the explorer protocol over a byte stream ([1b93246](https://github.com/vibecook-dev/mille/commit/1b93246c0079532d19bafab6b1d7a74964d774f5))
* **host:** give sessions permissions and enforce them host-side ([bcf9c93](https://github.com/vibecook-dev/mille/commit/bcf9c936ab19c0d293e98ade564e60e4ffe4a8e0))
* **host:** make the host/client transport an interface, not a MessagePort ([b67b5d8](https://github.com/vibecook-dev/mille/commit/b67b5d842fb866510758629e4ee2a540d835c74d))
* **truffle:** add connectMille and the reconnect facade ([34cdb82](https://github.com/vibecook-dev/mille/commit/34cdb8294c056fa386cca620b1ea053bfd5e45aa))
* **truffle:** serve a mille workspace to another tailnet device ([8f06cbe](https://github.com/vibecook-dev/mille/commit/8f06cbe91ee4594b331b6147bda46814f30e9736))


### Bug Fixes

* **binding:** silence the Unix-side dead code the pin split created ([801543b](https://github.com/vibecook-dev/mille/commit/801543b37875d688275132d682e1128462798420))
* **binding:** stop the undo journal pinning created files open on Windows ([7b276b4](https://github.com/vibecook-dev/mille/commit/7b276b4ef06d4846fc62c767f23e03e40cc5646e))
* **ci:** untrack the native binaries, and fail the build if they return ([#35](https://github.com/vibecook-dev/mille/issues/35)) ([8356024](https://github.com/vibecook-dev/mille/commit/8356024eac69f02992f283b5e687aee72c5fc175))
* **core:** map Win32 status codes so a locked file reports EBUSY ([f10a1ee](https://github.com/vibecook-dev/mille/commit/f10a1eee1c299705794e237895c7626a12abc2d8))
* **core:** report Windows content writes as changes, not degraded renames ([0895952](https://github.com/vibecook-dev/mille/commit/08959520a3ddd8fa313a0579c79c7a0d49865cac))
* **git:** keep decorations live inside linked worktrees ([#33](https://github.com/vibecook-dev/mille/issues/33)) ([f32c00e](https://github.com/vibecook-dev/mille/commit/f32c00e19fa21697db4f21a6a089ffea273e44bc))
* **host:** make the documented sync points acknowledged, not timed ([a5352af](https://github.com/vibecook-dev/mille/commit/a5352afb4c98078e279eb78078bef1df5cfa0813))
* **host:** stop path resolution crossing a symlink out of the workspace ([#30](https://github.com/vibecook-dev/mille/issues/30)) ([e84b3b4](https://github.com/vibecook-dev/mille/commit/e84b3b4b0c003f242fbe3019120ecd1a7766ad1e))
* **test:** expand the test glob ourselves so pnpm test runs on Windows ([40c5487](https://github.com/vibecook-dev/mille/commit/40c5487bd3d12b1064139c8698022f8735da794d))

## 0.3.0 — 2026-07-25

Ships Phases 4.4, 5 and 6.1–6.3, and closes a defect that made every prior
release unsafe to embed: a Rust panic aborted the host process outright. If you
are on 0.2.x inside an Electron app, this is the upgrade that stops an
unexpected filesystem edge case from taking the whole editor with it.

### Engine (`@vibecook/mille`)

- **A native panic no longer kills the host process** — mille loads into
  someone else's Electron main process, and until now any Rust panic took that
  process down with it via SIGABRT: no catchable error, no crash handler,
  nothing flushed. Two independent causes, both measured before being fixed.
  `[profile.release]` carried `panic = "abort"`, so a release build exited 134
  for both a sync and an async entry point; abort also defeats tokio's own
  task-level capture, which is why async calls recovered in a debug build and
  died in release. And none of the 67 callable `#[napi]` entry points opted
  into `catch_unwind` (napi-rs makes it per-function opt-in), so panics unwound
  out of the generated `extern "C"` shim — an abort by definition, measured at
  134 even in debug. The release profile now unwinds and every entry point
  carries the attribute; a panic arrives in JS as a normal catchable error.
  `buildIdentity()` gains `nativePanicStrategy` so an embedder can assert this
  about the binary it actually loaded.
- **Filesystem provider boundary (Phase 6.1)** — new subpath
  `@vibecook/mille/provider` with `FileSystemProvider` runtime, capability
  helpers (bits **and** method presence → `EUNSUPPORTED`), memfs with
  cycle-safe rename/copy + scoped watch, single-flight tree refresh,
  shadow-safe registry, latency/offline wrappers, and platform path helpers
  (drive/UNC/Unicode). Watcher-driven refreshes coalesce, while an explicit
  `refresh()` is serialized behind any in-flight walk — so `await writeFile()`
  → `await refresh()` never resolves with a tree read before the write.
  Local `FileExplorer` unchanged; native `registerProvider` still deferred.
- **`resync` is an acknowledged synchronization point** — the host used to
  flush deltas and wait one `setImmediate`, which is not observable evidence
  that a peer applied anything; the guarantee held on an idle machine and lost
  under load. Deltas flushed by `resync` / `resyncWorkspace` now carry
  `ackRequested`, clients reply with an `ack` frame once applied, and the call
  resolves when every attached session has confirmed. Additive and
  version-compatible: a client that never acks (or predates the frame) is
  covered by a 1 s fallback, which degrades to the old behaviour rather than
  hanging. Ordinary churn is unchanged — no acks are requested, so the hot
  path stays one-way.

  The ack frame alone was not enough, and the fallback hid the remainder for a
  while: `applyDelta` **assigned** the incoming `treeVersion` instead of
  advancing to it. A delta emitted only to carry markers (subtree
  resynced/dirty, root changes, decorations) reports an empty ChangeSet's
  version, which lags whatever the periodic tick already delivered — so such a
  delta dragged an up-to-date mirror _backwards_, and the regressed mirror
  acked the stale version. `resync` then waited for a target no ack could
  reach and fell through to the 1 s timeout, returning as though it had
  synchronized. It reproduced about one run in twenty on an idle machine.
  The fix is that mirror versions are now monotonic — one `Math.max`.
  `applyViewportPatch` had always advanced this way; `applyDelta` had not.
  Deltas deliberately keep reporting the ChangeSet's version rather than the
  host's current one: understating is the safe direction, since a monotonic
  mirror cannot be dragged backwards by it, while overstating would ship a
  version whose entries are not in that delta and let a client ack content it
  never received.

- **Scoped provider invalidation** — a watcher event invalidates the directory
  whose listing it changed, and the walk re-reads only those directories,
  returning every subtree with no dirty descendant by reference. Adding one
  file to a 6-directory tree costs **2 provider calls instead of 38**;
  `bench:provider` gates the call count. `refresh()` remains a full rebuild
  (the recovery path for a missed event), as does the first walk or a burst
  touching more than 64 directories.
- **Bounded-concurrency provider walk** — a full walk overlaps provider calls
  under a shared cap (`concurrency`, default 8) instead of awaiting each
  `stat` / `readDirectory` in series. On a 38-call tree behind a 5 ms provider
  it drops from ~222 ms to ~39 ms.
- **Provider copy collisions** — `copy` takes `{ overwrite }` and fails with
  `EEXIST` when the destination exists. Copying a file used to clobber the
  destination silently while copying a directory threw; wrappers now forward
  the option instead of dropping it.
- **Offline gate covers live watchers** — a watcher created while online stops
  delivering events once the provider is marked offline, and resumes on
  reconnect. Previously "offline" only rejected new calls.
- **`parsePlatformPath` honors its `platform` argument** — passing `'posix'`
  no longer falls through to Windows drive/UNC parsing (POSIX permits `\` and
  `:` in names).

### UI (`@vibecook/mille-ui`)

- **Live announcer (Phase 6.3)** — `@vibecook/mille-ui/a11y`
  `createLiveAnnouncer` coalesces and throttles `aria-live` feedback.
- **`VERSION` no longer lies** — the exported constant read `'0.1.0'` in all
  three entry points that declare it, while the package shipped as 0.2.1.
  Nothing compared the two, so it drifted three releases. Now matched to
  `package.json` and guarded by a test.

### Versioning

- The Rust crates and the npm packages now share one version. `buildInfo()`
  used to report `crateVersion: '0.1.0'` from a 0.2.1 package, which made
  build identity in a bug report ambiguous.

## 0.2.1 — 2026-07-12

Explorer correctness + soft-duotone icons + docs site. No public-API breaks.

### Engine (`@vibecook/mille`)

- **Expand gitignored folders.** Expanding a walk-root that is itself
  ignored (e.g. `node_modules`, `out/`) no longer clears
  `read_children_path`, so Project-view “show ignored” folders populate.
- **Symlink expandability.** Walker records `symlinkTargetIsDir` from
  target metadata; `hasChildren` treats directory-target symlinks as
  expandable. UI/mirror `isExpandableEntry` matches (pnpm package
  links open as folders).
- **Project-view visible rows.** Folders-first sibling rank; default
  visibility keeps ignored/hidden entries while still hiding `.git` and
  `.DS_Store`. Library-root / symlink data attributes for chrome styling.

### UI (`@vibecook/mille-ui`)

- **Soft-duotone icon theme** — `duotoneIconTheme` via
  `@vibecook/mille-ui/icons/duotone` (also re-exported from
  `@vibecook/mille-ui/icons`). Compact filled folders + language-accent
  file chips; playground default.
- **Row layout / virtualizer.** Dropped `position: relative !important`
  on rows so absolute + `translateY` virtualization no longer doubles
  vertical gaps. VCS badges render before the name; library-root and
  symlink markers for IDE chrome.

### Docs & playground

- Static product site (`docs/index.html`) + API reference (`docs/api.html`).
- Icon theme comparison page (`docs/icons-preview.html`).
- Playground reshaped as a JetBrains-style Project tool window
  (density, library roots, gear settings, duotone default).

## 0.2.0 — 2026-04-25

Engine correctness + Track A completion. Builds on v0.1; no breaking API
changes for in-process consumers. The port wire protocol gains optional
`roots` on delta frames and adds `decorations` / `decorationChanged` frame
types — old clients keep working, new fields ignored if unused.

### Engine (`@vibecook/mille`)

- **Roots in deltas (B1).** `DeltaMsg.roots?` carries root-set updates so a
  client mirror that handshakes before the walker has populated the root
  no longer ends up with `roots=[]` forever. `populateFromRoots`-before-
  `ready` workarounds can be removed.
- **Lazy list-on-expand (B2).** New `ExplorerOptions.initialWalk?:
'full' | 'roots-only' | 'none'` (default `'full'` for back-compat).
  `host.handleSetExpanded` now triggers shallow walks for newly-expanded
  folders whose direct children aren't yet in the store. Tree renders in
  <200 ms with `'roots-only'` even on huge repos.
- **Symlink-aware ignore (B3).** Walker applies gitignore rules on the
  DirEntry name before resolving symlinks, so pnpm-style
  `node_modules → central store` symlinks are correctly skipped.
  Walks are now O(tracked-files) on pnpm monorepos instead of
  O(tracked + store).
- **Port-side decoration pipeline (Phase A1).** New `decorations` and
  `decorationChanged` frame types. `PortFileExplorer` implements
  `registerDecorationProvider` with batched push semantics; the host
  merges into its `DecorationStore` and fans out to every connected
  client. Other clients see the same git/lint badges (by design).
- **Host-level `registerDecorationProvider`.** Decoration providers can
  be registered against the host (not just per-port-client), letting an
  embedder install one git provider that fans out to every renderer.
- **NAPI-undefined guards.** `getByUri` and provider-edge paths handle
  `undefined` returns from the binding without crashing.

### UI (`@vibecook/mille-ui`)

- **Shell-based `createShellGitClient` (B4 / A2).** Spawns
  `git status --porcelain=v2 -z`, watches `.git/HEAD` and `.git/index`
  via `node:fs.watch` (100 ms debounce). Now exported from
  `@vibecook/mille-ui/git/node` (Node-only entrypoint) so the browser
  bundle stays free of `node:child_process`.
- **Material Icon Theme bundle (B5 / A3).** `loadMaterialIconTheme()`
  returns the real bundle. Built at publish time from the upstream
  `material-extensions/vscode-material-icon-theme` repo (MIT) via
  `scripts/build-material.mjs`. See `NOTICES.md` for attribution.
- **Imperative `FileTreeRef` handle (B6).** `forwardRef` on `FileTree`
  exposes `revealPath` / `revealId` / `scrollToRow` / `clearSelection`
  / `clearFilter` / `clearClipboard` / `focusFilter`. New
  `useFileTreeRef` hook for nested consumers.
- **Headless bundle trim (B8).** Headless entry now ships logic hooks
  - ARIA primitives without the styled-row chrome. Bundle dropped from
    21.69 KB → 12.46 KB gzip (SPEC §12 target was 12 KB; landed within
    the 13 KB fail-on-regression boundary). `size-limit` now fails CI on
    regression.

### Playground (`apps/playground`)

- **Folder picker + recent folders dropdown (B7).** Open-folder button
  becomes a dropdown of up to 10 recent projects (persisted to
  `app.getPath('userData')/recent-folders.json`) plus "Browse…".
- Removed the decoration no-op shim — git and agent-rules toggles now
  render real badges via the port-side pipeline.
- Switched to `initialWalk: 'roots-only'`; dropped the
  `populateFromRoots`-before-`ready` workaround and the temporary
  `excludeGlobs` workaround for pnpm symlinks.

### Known gaps (carried into v0.2.x / v0.3)

- Headless bundle is 12.46 KB gzip vs the 12 KB SPEC §12 target — within
  the regression boundary but not under the aspirational floor.
- Full Logic-hook + View split per `MILLE_UI_SPEC` §4.9 is partial; the
  remaining row primitives are not yet split.
- Playwright perf guardrails + visual regression baselines deferred.
- Content search is still a separate package (not yet built).
- Remote FS providers (SSH, zip, memfs) reserved in API; implementations
  deferred.
- `AbortSignal` on async mutations still partial (napi-rs 3.x `!Send`).
- Windows parent-directory fsync (POSIX-only today).

## 0.1.0 — 2026-04-19

First release. Local-mode `@vibecook/mille` with:

### Rust core (`mille-core`)

- `EntryStore` with `ArcSwap` snapshot rotation, `BTreeMap` + summary caches
- Cross-platform walker (`jwalk` + `ignore` + compact folders)
- Watcher (`notify` + debouncer + rename pairing + volatile throttling)
- Crash-resume (atomic bincode write + fsync parent + `events_since` diff)
- Fuzzy search via `nucleo`
- `ChangeSet` accumulator for Phase 7 session deltas

### NAPI binding (`mille-binding`)

- `FileExplorer` class with typed `ExplorerOptions`
- `MirrorSnapshot` with `roots` / `getById` / `directChildCount` /
  `hasChildren` / `visibleRows` / `visibleRowCount`
- Mutations (`create` / `rename` / `move` / `delete` / `copy` / `readFile` /
  `readText` / `writeFile` / `readFileStream`)
- Eight event channels via `ThreadsafeFunction`
- `AbortSignal` plumbing (partial — see v0.1.x)
- Bincode `Buffer` bulk-return path

### TS client (`@vibecook/mille`)

- Per-platform optional-deps loader
- `FileSystemError` + `isFileSystemError`
- Bincode decoder for bulk rows
- Typed `FileExplorer` wrapper with `wrap()`-based error reconstruction
- `useFileExplorerSnapshot` React hook
- Host / client split: `createFileExplorerHost` + `connectFileExplorer`
- Full IPC protocol (handshake + snapshot + delta + mutate/call + dispose)
- Mutation ordering guarantees (SPEC §5.1 — delta-before-result)
- Coarse / dirty / resynced subtree plumbing
- Client-side `ViewportMirror` with frozen `MirrorSnapshot`, cache-miss
  placeholders, `mirrorCap` eviction
- Decoration providers with scoped `change:decorations` events

### Tests

- 204 `mille-core` tests (unit + integration + proptests)
- 197 package tests (unit + integration + `fast-check` proptests)

### Known gaps (tracked for v0.1.x / v0.2)

- Content search is a separate package (not yet built)
- Remote FS providers (SSH, zip, memfs) reserved in API; implementations
  deferred
- `AbortSignal` on async mutations (napi-rs 3.x `!Send` constraint)
- Windows parent-directory fsync (POSIX-only today)
- Example Electron + Playwright apps (patterns in `EMBEDDING.md`)
