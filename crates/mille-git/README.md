# mille-git

The git engine behind mille's Changes / History surfaces: **gix 0.87 reads, git CLI writes**, one
operation queue per open worktree, and watches on the resolved gitdir. Pure Rust, no napi — the
binding exposes it behind its `git` feature (off by default; FileTree embedders never link gix).

The design is `draft/mille-git-spec.md` (SPEC mille-git), which is normative for §7–§24. This
crate implements its PRs 1–5; the napi binding, the TypeScript `GitService` and the playground
migration (PRs 6a–9) are later work.

## Layout

| File | Role |
| --- | --- |
| `src/lib.rs` | `GitEngine`, `GitEngineOptions`, the runtime the engine schedules on |
| `src/gitdir.rs` | `.git` walk-up and `gitdir:` redirect following (the load-bearing why-comment) |
| `src/identity.rs` | checkout-root discovery, `GitIdentity`, the CLI identity fallback |
| `src/session.rs` | per-checkout sessions: refcounted de-dupe, snapshot publish, subscribers |
| `src/queue.rs` | the per-worktree status queue: debounce, single flight, drop-and-replace, idle rescan |
| `src/watch.rs` | gitdir / common-dir / `refs` / `worktrees/` directory watches and event classification |
| `src/route.rs` | explorer OS paths → realpath longest-prefix → worktree-relative hints (§11.2.1) |
| `src/worktree_list.rs` | `git worktree list --porcelain` → `GitWorktree` |
| `src/containment.rs` | `assert_path_under_root`, `assert_safe_revision`, the `allowedRoots` gate |
| `src/status.rs` | gix status → `StatusSnapshot`, `UntrackedFiles::Files`, row cap, the §10.6 merge |
| `src/porcelain.rs` | `git status --porcelain=v2 -z` parser (the CLI fallback), golden-tested |
| `src/warning.rs` | the `'warning'` channel: `ENOGIT` once, `PARSE`, `WATCH`, `DEGRADED_CLI` |
| `src/cli.rs` | the git CLI seam: no shell, alias-guarded argv, filtered env, graceful cancel |
| `src/paths.rs` | lexical / realpath / POSIX-relative spelling helpers |
| `src/error.rs` | `GitError`, `GitErrorKind` and their `E*` wire codes |

## Test

```bash
cargo test -p mille-git          # temp repos via the real git CLI; skips cleanly without git
```

`tests/fixtures/porcelain/` holds the porcelain-v2 `-z` goldens (T-PORC). They are shared with the
JavaScript parser: `v2-mixed.z` is raw `git status` output, `v2-mixed.expected.json` the XY-preserving
entries both parsers must produce.

Every repository a test touches is created with `git init` inside a `tempfile` directory and
configured locally (no signing, explicit `core.hooksPath`) so a developer's global git config
cannot change what a test observes.

## Deviations from the spec

Each item names the section it departs from and why. None reopens a §31/§32 decision.

1. **§8.1 gix features add `sha1`.** gix 0.87 moved the hash implementation behind the `sha1`
   feature (it is in `default`, which §8.1 rightly turns off). Without it no SHA-1 repository
   opens. The pin is otherwise exactly `0.87.1` with §8.1's feature list.
2. **Appendix B `GitErrorKind` gains `Access` and `NotFound`.** Appendix A's TypeScript
   `GitErrorCode` union already carries `EACCES` and `ENOENT`; Appendix B had no Rust spelling for
   them. `Io` (anything else from the OS) crosses the wire as `EUNKNOWN`, since the union has no
   `EIO`.
3. **Appendix B `GitEngineOptions.git_path` is `Option<PathBuf>`** (`None` = probe `PATH`, then
   the §32.2 well-known paths), matching Appendix A's optional `gitPath`. A probe that finds
   nothing leaves writes failing `ENOGIT` while gix reads keep working.
4. **§9.2 step 6's CLI identity fallback is split in two.** The single
   `rev-parse --git-dir --git-common-dir --is-bare-repository --abbrev-ref HEAD` fails outright on
   an unborn branch (`HEAD` does not resolve), which would turn every fresh `git init` that gix
   cannot open into `kind: "none"`. The location query alone decides "is this a repo"; branch and
   head are separate best-effort queries.
5. **§20.3's environment allow-list is wider, and narrower.** Wider: the platform variables a child
   needs to run at all (`SYSTEMROOT`, `TEMP`, `APPDATA`… on Windows; `TMPDIR`, `LANG`, `USER` on
   POSIX) and the signing/transport agents (`SSH_AUTH_SOCK`, `GNUPGHOME`, `GPG_TTY`) that "so
   GPG/SSH/LFS work" implies. Narrower: of the host's `GIT_*`, the repository-location variables
   (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_COMMON_DIR`, …) are
   dropped — inherited from a host running inside a git hook, they would point every engine call
   at a different repository than the one being asked about.
6. **§15.1 / §20.4 argv prefix adds `config` and `rm` to the alias list, and
   `--literal-pathspecs`.** The crate spawns `git config` (performance advice) and `git rm
   --cached` (unstaging on an unborn branch, where `restore --staged` and `reset HEAD` both fail),
   and §20.4 requires an override for every spawned verb. `--literal-pathspecs` keeps a path like
   `app/[id]/page.tsx` from glob-matching `app/i/page.tsx`. The working directory is set with
   `current_dir` rather than `-C` (§15.1 allows either).
7. **`cli.rs` lands in PR 1, not PR 4.** §9.2 step 6 (the identity fallback, PR 1) and §10.3 (the
   status fallback, PR 2) both spawn git, so the spawn seam — probe, argv prefix, filtered env,
   cancellable runner — arrives with its first caller. PR 4 adds the mutations and the alias tests.
8. **`identity.worktreePath` keeps the caller's spelling of the checkout root; the session is
   keyed by its realpath.** §9.2 says to store `realpath(checkoutRoot)` for hint routing and not
   to match against a second spelling — the routing does exactly that. But the `GitClient` adapter
   builds decoration keys from `worktreePath`, and on macOS a realpath'd root
   (`/private/var/folders/…`) would not match the explorer's URIs (`/var/folders/…`), so the
   identity carries the spelling the caller used.
9. **Bare repositories are found by `gix::discover`** when the `.git` walk finds nothing: a bare
   repository has no `.git` entry to find, and §9.4 still wants `kind: "bare"` for it.
10. **The engine owns a small runtime.** `GitEngine::new` starts a two-worker tokio runtime and
    runs every operation on it, so callers can await from any executor; `GitEngine::with_runtime`
    adopts an existing one (the binding's). Dropping the engine shuts the runtime down in the
    background rather than blocking.
11. **§10.6: a hinted path covers its descendants.** "Delete every hinted path that came back
    clean" is applied to rows *under* a hinted path too, exactly as the pathspec the hint becomes
    matches them. A deleted untracked directory hinted as `gen` therefore clears `gen/out.js`
    instead of leaving it until the idle rescan. For a file hint the two readings are identical.
12. **§10.6 T-MERGE-RENAME: a `to`-only pathspec cannot see a rename, so path-limited runs widen
    their pathspec with both names of every known rename row they touch.** The spec has the rename
    reported as `{ path: b.ts, origPath: a.ts }` "whether H is `{b.ts}` or `{a.ts, b.ts}`". Under
    the pathspec `-- b.ts` the deletion side is outside the pathspec, so neither gix nor git can pair
    it: `git status --porcelain=v2 -- b.ts` prints `1 A. … b.ts` (checked against git 2.50). Two
    consequences, both handled: a *first-seen* staged rename always moves the index, so the gitdir
    `index` watch runs a full status that reports the pair and drops the old row; and a *known*
    rename (a row with `origPath` in the previous snapshot) would otherwise be downgraded to `A`
    and lose `origPath` the next time either name is hinted alone — so before a path-limited run the
    engine adds both names of any such row to the pathspec (falling back to a full run if that
    crosses the 512-path ceiling). The merge rule itself is implemented as written and unit-tested
    with an injected rename row. The binding's "hint only `to` and rely on §10.6" fallback
    (§11.2.1) is safe for the same two reasons.
13. **§10.2 `staged` is false for untracked and conflicted rows; untracked `xy` is `.?`.** The spec
    defines `staged` as "index slot !== '.'" and calls an untracked row's `xy` "unused". A concrete
    value is needed: untracked rows carry `{ index: '.', worktree: '?' }`, and conflicted rows keep
    git's pair (`UU`, `AA`, `DU`, …) in `xy` — so the formula alone would call every conflict
    "staged". Neither untracked nor unmerged paths have anything a commit would take as-is.
14. **The porcelain `u` record's path is found by counting its ten fixed fields**, as the parser
    already does for `1` and `2` records, not with `lastIndexOf(' ')` as `shell-client.ts` does.
    The TypeScript parser truncates an unmerged path containing a space (`conflict file.ts` becomes
    `file.ts`); the shared golden includes that case, so PR 7's parser lift should fix it there.
15. **Added API: `refresh(worktree, paths)` and `set_status_backend(worktree, backend)`.**
    `refresh` runs a full or path-limited status now and returns what it published (the queue in
    PR 3 makes it "hint and await"); `set_status_backend` is §21.3's per-session `statusBackend`
    flag. Both are additive.
16. **§11.2.1 step 1: a deleted path's realpath is its deepest surviving ancestor's, with the
    missing tail re-appended** — not "deleted paths stay as delivered". On macOS a deleted
    `/var/folders/…/repo/a.ts` delivered as-is never prefix-matches the checkout realpath
    `/private/var/folders/…/repo`, so every deletion under a temp or symlinked root would be
    dropped by routing.
17. **The §11.2.1 routing lives in `mille-git`** (`GitEngine::hint_os_paths`,
    `hint_coarse_os_path`) rather than in the binding's `git.rs`. It needs nothing but the
    sessions' realpath'd roots, which the engine owns; keeping it here makes T-HINT-ROUTE a cargo
    test (nested linked worktree, `/tmp` vs `/private/tmp`, overflow-under-A-only, the forbidden
    inverted rule) and leaves PR 6b's native callback a one-line forward. `hint_paths` (relative
    POSIX, Appendix B) is still the primitive both call.
18. **`GitEngineOptions.idle_rescan`** (default 5 s) makes §21.1's "idle 5 s full rescan"
    configurable, so the behaviour is testable without a five-second sleep.
19. **`status()` returns the settled snapshot; `refresh(None)` forces a run.** With the queue in
    place, `status()` returns the published snapshot when the queue is idle and otherwise waits for
    the run that settles what is already queued — so a `GitClient.getStatus` after `onChange` never
    triggers a redundant whole-repo scan. Freshness comes from hints and the gitdir watch (F3/F4).
20. **The blocking mutex (§12.1) and the diff/log read limits land with their first callers** — PR 4
    (mutations) and PR 5 (diff, log) — rather than in PR 3, so no PR ships unused machinery. PR 3's
    queue is the status half: one in flight per worktree, drop-and-replace, cross-worktree parallel
    (AC-008, measured with an injected delay).
21. **Gitdir watches are directory watches only.** `watch-dotgit.ts` watched the `HEAD` / `index`
    files as well as the directory because `fs.watch` reports file events unreliably; notify's
    directory watches report children by name on every backend, which is the half of the TS fix
    that survives atomic renames. Also watched: `<common>/refs` and `<common>/reftable`
    (recursive), the common dir itself for `packed-refs` in linked worktrees, and
    `<common>/worktrees/` — attached late when the first `git worktree add` creates it.
22. **`subscribe_status` / `subscribe_worktrees` return `Result<u64>`**: subscribing to a worktree
    that is not open is `EINVAL` rather than a silently dead id.
23. **Not implemented: a `kind: "none"` session does not upgrade itself** when its directory later
    becomes a repository (`git init`). §10.5 only requires that such a session can be subscribed to;
    it stays `none` until closed and re-opened. Watching for `.git` creation would mean watching the
    caller directory — a second worktree watch, which §11.1 forbids.

