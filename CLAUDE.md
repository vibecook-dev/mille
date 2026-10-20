# mille

Native file-explorer primitives for Electron IDEs. Rust walks/watches/searches the tree, TypeScript
freezes viewport snapshots, React renders hundreds of thousands of rows. Ships as `@vibecook/mille`
(engine), `@vibecook/mille-ui` (React `FileTree`) and `@vibecook/mille-truffle` (the same host served
over a tailnet), with 8 per-platform NAPI binaries. Every published package and Rust crate carries the
same version; `scripts/check-release-versions.mjs` compares them all against `packages/mille`.

## Layout

```
crates/mille-core/     Pure Rust engine, zero NAPI — walker, watcher, store, snapshot, search
crates/mille-binding/  napi-rs cdylib; marshaling + the undo journal and watch runtime
crates/mille-bench/    Criterion benches + fixture-tree generators
packages/mille/        TS client: host/renderer split, wire protocol, mirror, provider SDK
packages/mille-ui/     React FileTree, icon themes, decoration providers, commands
packages/mille-truffle/ serveMille/connectMille — the same host over a tailnet, not a MessagePort
packages/mille-*/      8 platform wrapper packages (.node payload only)
apps/playground/       Electron reference embedding; also the a11y and watcher-bench harness
docs/                  Hand-written GitHub Pages HTML (no generator)
planning/              Parity assessment + phased roadmap
```

## Architecture

Four layers, one direction:

1. **`EntryStore`** (`crates/mille-core/src/store.rs`) is the single writer. Mutations take a write
   lock, clone the current snapshot, mutate, walk ancestors to fix summaries, then `ArcSwap::store`.
2. **`StoreSnapshot`** (`crates/mille-core/src/snapshot.rs`) is the immutable read side plus the whole
   projection layer — visibility, compact folders, file nesting, sort, and the `visible_rows` viewport
   queries. Readers never lock; `snapshot()` is one atomic load.
3. **`ChangeSet`** (`crates/mille-core/src/changes.rs`) is the delta contract, and it is small but
   central. Four _distinct_ id sets — `changed_ids`, `child_set_changed`, `subtree_roots_changed`,
   `reparented_ids` — plus `projection_changed` and a `from_version`/`to_version` pair. The point is
   that a session intersects `expanded ∩ changed` as an index lookup, not a cross product.
4. **Host/renderer** (`packages/mille/src/host.ts`, `client-port.ts`) talk over an
   **`ExplorerChannel`** (`src/channel/types.ts`), not Electron IPC. The channel is an _interface_ —
   a MessagePort (`channel/message-port.ts`) is one implementation and a framed byte stream
   (`src/stream/`, exported from `./node`) is the other, which is what lets `@vibecook/mille-truffle`
   put the renderer on a different machine without the host knowing. The host owns the one native
   explorer, the watcher, and per-session viewport state; the renderer owns an LRU mirror and
   publishes a frozen `ClientMirrorSnapshot` whose identity advances exactly once per applied frame,
   so `useSyncExternalStore` can gate renders on `===`.

   Sessions carry an access level (`admin` / `read-write` / `read-only`) enforced **host-side** in
   `channel/policy.ts`, not by the caller. Operations with process-global reach — undo, projection
   settings, decorations, `resyncWorkspace`, root mutation — sit behind `allow*` flags that default
   off, because one peer must not change what every other session sees. Note the back-compat default:
   a session constructed with **no context at all is full admin**, so the permissive case is the one
   you get by forgetting. `mille-truffle` always passes a policy (`sessionPolicyFor` returns bare
   `{ access }`, granting no flags).

Wire schema is `packages/mille/src/protocol.ts` (`{v, type, body}`, `PROTOCOL_VERSION = 1`, exact-match).
Entry payloads are bincode `ArrayBuffer`s (`entry-codec.ts`, `child-list-codec.ts`); only decorations
are JSON. Full entry records ship only for rows in the viewport window — child _id lists_ ship whole so
the client can project any window.

Events reach JS through per-channel threadsafe functions in `crates/mille-binding/src/events.rs`
(8 channels, statically typed, no `Box<dyn Any>`), never a port. `emit_*` drops the read lock before
calling, or a callback re-entering `on`/`off` deadlocks.

`FileExplorer` in `crates/mille-binding/src/explorer.rs` is 4.2k lines and 66% of the binding crate —
it holds the class lifecycle plus copy/move/delete recursion. Read the `policy_gate` comment near
`explorer.rs:164`; it is the best explanation of the concurrency reasoning in the repo.

## Build

```bash
pnpm install
pnpm --filter @vibecook/mille run build:napi:debug   # debug .node; `pnpm build` uses --release (lto=fat, slow)
pnpm --filter @vibecook/mille run build:ts
pnpm --filter @vibecook/mille-ui run build           # tsc + scripts/build-tokens.mjs
```

`packages/mille/src/native.ts` prefers a local `mille.<triple>.node` sitting next to the package root
over the platform package, so a dev build wins automatically. `buildIdentity().source` reports which
one loaded.

## Test

```bash
cargo test --workspace --exclude mille-binding   # the exclusion is mandatory — cdylib, no test binary
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
pnpm -r --if-present test
pnpm -r --if-present typecheck
pnpm format:check                                # not run by CI, and red at HEAD — see below
node scripts/axe-check.mjs                       # real axe in the live playground, both themes
```

- **TS tests import from `dist/`, not `src/`.** Build before testing or you test stale code.
- Runner is `node:test` + `node:assert/strict`. No vitest, no jest. Tests are flat `test/*.test.mjs`.
- `scripts/run-tests.mjs` expands the glob itself — shells on Windows don't, Node's own glob needs v21,
  and bare `node --test` recursion picks up fixtures.
- **There is no JS/TS linter, and Prettier is not a gate either.** `pnpm lint` resolves to nothing,
  and no workflow runs `pnpm format:check` — `tsc --noEmit` is the only automated JS/TS check. Run
  `format:check` locally and it fails on ~60 files that predate the pinned Prettier 3.8.3, so treat a
  red result as pre-existing drift: check only the files you touched. `.prettierignore` excludes
  `packages/mille-*/`, which catches `mille-ui` and `mille-truffle` too, not just the 8 platform
  packages.
- `bench/*.mjs` are gated regression harnesses, not curiosities — they assert invariants and exit
  non-zero. Some run in CI. Budgets override via `MILLE_*_BUDGET_MS`.
- Temp-dir teardown goes through `scripts/test-temp.mjs`, which warns rather than throws (Windows
  delete-pending).

CI (`.github/workflows/ci.yml`) gates a PR on five jobs: `test-rust`, `test-js`, `test-release-profile`
(the release profile once carried `panic = "abort"`, which debug builds can never expose), `test-git`
(the `git` cargo feature on — clippy over the binding and the whole JS suite against a
`build:napi:git:probe` binary, with `MILLE_EXPECT_GIT=1` so a binary without the engine fails instead
of skipping; no other job compiles the binding's git half), and `test-windows`. The 8-triple `build`
matrix compiles binaries but never loads them — that gap is why `test-windows` and
`smoke-published.yml` exist.

## Conventions

- LF everywhere, enforced in the **working tree** by `.gitattributes` (`* text=auto eol=lf`), not just
  the repo. Generated `packages/mille-ui/tokens.css` is normalized at build time and CI asserts it is
  byte-identical.
- Prettier: 100 cols, single quotes, semicolons, trailing commas. rustfmt: 100 cols, Unix newlines.
- Rust toolchain is **pinned to 1.97.1** in `rust-toolchain.toml`; upgrading is the documented 3-step
  change in that file (channel → clippy fixes → the `dtolnay/rust-toolchain@` refs in every workflow).
- `panic = "unwind"` in `[profile.release]` is load-bearing, not a default. `abort` turns a Rust panic
  into a SIGABRT that kills the host editor with no catchable error. Every exported fn is
  `#[napi(catch_unwind)]`; `test/panic-boundary.test.mjs` proves it against the shipped profile.
- Conventional Commits with a scope, and subjects state a _consequence_: `fix(core): report Windows
content writes as changes, not degraded renames`. PRs use Summary + Test Plan.
- **Comment culture:** this repo writes long "why" comments in config, CI, and at non-obvious code
  boundaries. Match that register — see `Cargo.toml:80`, `ci.yml:96-121`, `run-tests.mjs`,
  `explorer.rs:164`.

## Gotchas

- **`SPEC §x` citations resolve to two different documents, one of which you do not have.** The older
  `§4.x` / `§18.3` references in the Rust sources, CI and CHANGELOG point at the
  `research/file-explorer/` documents `CONTRIBUTING.md` places outside the repo — unresolvable here.
  The remote-workspace ones (`§12.4`, `§16.2`, `§19.1`, …) in `packages/mille/src/channel/`,
  `src/stream/` and `packages/mille-truffle/` point at the tracked `draft/mille-truffle-spec.md`.
  That file has **no markdown headings** — it is a doc export — so grep the bare number
  (`grep -n "^ *16\.2" draft/mille-truffle-spec.md`), not a `##`.
- `crossbeam-channel` is declared in `Cargo.toml` and used nowhere.
- `Fs`/`RealFs` in `mille-core/src/fs.rs` is aspirational — `RealFs` returns `Unsupported`; real I/O
  goes through `tokio::fs`/`std::fs` in the binding.
- `SymlinkPolicy::Smart` is the default but is currently behaviorally identical to `Never`; the
  dev/inode dedup it promises is a TODO at `walker.rs:32`.
- `EntryId` is capped at 2^53 so it survives as a JS number, and allocation fails rather than wraps.
- Mille-core is entirely synchronous OS threads. tokio appears only in the binding.

## Where the work is

`planning/IDE_EXPLORER_PARITY_PLAN.md` — phases 0–5 complete, 6.1 landed, **Phase 6 (provider and
platform depth) is live**. `planning/IDE_EXPLORER_PARITY_ASSESSMENT.md` is the gap analysis behind it.

**Remote workspaces are built and shipped.** `draft/mille-truffle-spec.md` is the design;
`planning/REMOTE_WORKSPACE_PLAN.md` is what Truffle 0.7.6 actually guarantees plus the two findings
that changed the spec's security model (authorize on the **Tailscale node id**, never the
self-declared ULID — it is `null` on a raw TCP accept); `packages/mille-truffle/ACCEPTANCE.md` carries
the `AC-001`…`AC-012` coverage map and the two-device run that discharged `AC-002`. All six planned
PRs landed and `@vibecook/mille-truffle@0.3.2` is on npm, versioned in lock-step with `@vibecook/mille`.

Still open there: `mesh.quic` content multiplexing (deferred, not blocked), and in-app grants by user
email, which Truffle cannot support today because the napi accept path flattens
`display_name || login_name || dns_name` into one string.
