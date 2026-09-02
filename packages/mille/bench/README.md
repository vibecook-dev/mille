# Mille core benchmarks

## Progressive directory hydration

`pnpm bench:progressive-directory` creates a 10,000-entry real directory and
gates time-to-first-partial-page, authoritative completion, and the number of
host delta frames. The frame gate enforces the adaptive publication schedule:
the first page stays small, while later pages grow to avoid repeatedly cloning
an ever-larger immutable snapshot. Override the fixture and budgets with
`MILLE_PROGRESSIVE_DIRECTORY_COUNT`,
`MILLE_PROGRESSIVE_DIRECTORY_BATCH`,
`MILLE_PROGRESSIVE_DIRECTORY_FIRST_PAGE_BUDGET_MS`, and
`MILLE_PROGRESSIVE_DIRECTORY_COMPLETE_BUDGET_MS`. The derived frame limit can
be overridden with `MILLE_PROGRESSIVE_DIRECTORY_MAX_DELTA_FRAMES`.

The pathological 100,000-sibling stress gate is:

```sh
MILLE_PROGRESSIVE_DIRECTORY_COUNT=100000 \
MILLE_PROGRESSIVE_DIRECTORY_COMPLETE_BUDGET_MS=30000 \
pnpm bench:progressive-directory
```

## Viewport retention

```sh
pnpm bench:viewport
```

The harness handshakes with only the root entry, installs 50,000 authoritative
ordered child IDs plus the first 64-row viewport, and then moves that viewport
200 times against a mirror capped at 4,096 full entries. Every move must retain
all mounted rows, stay within the cap, and eventually evict the first cold
window. It reports root hydration, structural-metadata installation, and
viewport-patch encoding plus decode/apply p50/p95/max latency. It also compares
the average binary payload with the equivalent JSON and requires at least a 60%
size reduction. A MessageChannel sub-benchmark reports median structured-clone
plus decode time for JSON and binary patches using the production decoders.

Arguments can override the fixture:

```sh
pnpm bench:viewport -- --entries 100000 --cap 8192 --viewport 100 --moves 500
```

Timing values are reporters; the retention, eviction, and capacity invariants
are hard gates that exit non-zero.

## File nesting

```sh
pnpm bench:nesting
```

The native harness creates one 50,001-entry directory containing 20,000 source
and companion-file pairs plus 10,000 unrelated files. It verifies projected
cardinality and disclosure state, reports the cold plan, then measures
projected-child lookup plus a 200-row viewport over 40 warm samples. The
default gates are 100 ms for the cold plan and 10 ms warm p95. Override fixture
size or budgets with `MILLE_NESTING_PAIRS`,
`MILLE_NESTING_UNRELATED`, `MILLE_NESTING_COLD_BUDGET_MS`, and
`MILLE_NESTING_P95_BUDGET_MS`.

## Live projection reconfiguration

```sh
pnpm bench:reconfigure
```

The harness builds a 50,001-entry wide directory and alternates full display
policies 20 times, including visibility, sibling ordering, 15,000 nesting
pairs, and configured exclusions that reclassify 20,000 entries per toggle. It
reports add/remove exclusion latency, aggregate atomic native update latency,
the first 200-row ready projection, and the idempotent no-op path. Default p95
gates are 125 ms update, 160 ms ready, and 0.1 ms no-op. Fixture size and
budgets use the `MILLE_RECONFIGURE_*` environment variables.

## Locale collation

```sh
pnpm bench:locale
```

The native harness builds a 30,004-entry Unicode-heavy directory and alternates
English and Swedish BCP-47 collation 20 times. Every sample verifies the
locale-specific `z`/`å` relationship, numeric `file2`/`file10` ordering, the
atomic tree version, and a ready 200-row viewport. It reports native update and
ready-projection median/p95 latency, with default p95 gates of 90/100 ms.
Fixture size, samples, and budgets use the `MILLE_LOCALE_*` environment
variables.

## Duplicate-basename workspace roots

```sh
pnpm bench:multi-root
```

The harness creates two configured roots whose basename is `workspace`, gives
each a distinct child, and alternates 500 lazy list operations by root identity.
Every sample must return only the intended root's child. It reports
identity-to-path resolution plus bounded depth-1 list latency and enforces a
default 5 ms p95 gate. Sample count and budget use
`MILLE_MULTI_ROOT_SAMPLES` and `MILLE_MULTI_ROOT_P95_BUDGET_MS`.

## Live workspace-root reorder

```sh
pnpm bench:root-reorder
```

The harness indexes 32 configured roots with 1,024 files each (32,800 entries)
and alternates their full order for 100 measured samples. Each sample includes
the native immutable-snapshot publish plus a public `roots()` observation and
verifies exact order and tree version. It separately measures idempotent
same-order calls and enforces a default 8 ms reorder p95 gate. Root count,
entries per root, samples, and budget use `MILLE_ROOT_REORDER_ROOTS`,
`MILLE_ROOT_REORDER_ENTRIES_PER_ROOT`, `MILLE_ROOT_REORDER_SAMPLES`, and
`MILLE_ROOT_REORDER_P95_BUDGET_MS`.

## Live workspace-root add/remove

```sh
pnpm bench:root-churn
```

The harness keeps a 32,768-entry root indexed while rotating a second root
through 16 candidates for 100 measured replacements. Every sample includes
filesystem validation, one immutable native publication, subtree/index
removal, lazy root insertion, and public snapshot verification. It separately
measures identical-list no-ops and enforces a 16 ms replacement p95 gate.
Fixture size, candidates, samples, and budget use `MILLE_ROOT_CHURN_*`.

## Workspace-root availability

```sh
pnpm bench:root-availability
```

The harness repeatedly disconnects and restores an 8,193-entry workspace
across 30 measured cycles. It verifies stable root identity, complete stale
subtree eviction, lazy recovery, and version-free healthy refreshes. Both
disappearance and recovery enforce a 16 ms p95 gate; fixture size, warmups,
samples, and budget use `MILLE_ROOT_AVAILABILITY_*`.

## Cross-root subtree move

```sh
pnpm bench:cross-root
```

The harness alternates an 8,193-entry directory subtree between two workspace
roots for 30 measured moves. Each sample includes collision probing, the
filesystem rename, one identity-preserving immutable store publication,
complete reverse-path rewrite, and public path verification. It enforces a
16 ms p95 gate; fixture size, warmups, samples, and budget use
`MILLE_CROSS_ROOT_*`.

## Transfer progress

```sh
pnpm bench:transfer-progress
```

The harness imports a 256-file external directory with `operationId` progress
enabled for 12 measured samples (2 warmups). Every sample requires at least one
`OP_PROGRESS` warning. Default p95 gate is 250 ms; fixture size, samples, and
budget use the `MILLE_TRANSFER_PROGRESS_*` environment variables.

## Collision policy

```sh
pnpm bench:collision
```

The harness alternates skip and overwrite copies of a colliding file for 40
measured samples (5 warmups), verifying destination content each time. Default
p95 gate is 5 ms for both policies; samples, warmups, and budget use the
`MILLE_COLLISION_*` environment variables.

## External path import

```sh
pnpm bench:copy-from-path
```

The harness imports one absolute file and a 256-file external directory into a
fresh inbox per sample (20 samples, 3 warmups). Every sample verifies payload
content on disk. Default p95 gates are 50 ms for a single file and 200 ms for
the directory tree; fixture size, warmups, samples, and budgets use the
`MILLE_COPY_FROM_PATH_*` environment variables.

## Authoritative subtree resync

```sh
pnpm bench:resync
```

The native harness builds a temporary 5,100-file workspace, replaces 100 files
between each of 10 measured samples, and recursively reconciles the root
through the public API. Every sample must converge to the exact disk child
count. After three cache warmups, twenty unchanged scans must preserve the tree
version. Reports include the maximum, while gates use nearest-rank p95. Default
p95 gates are 100 ms for churn and 50 ms for a no-op; fixture size, warmups,
sample count, and budgets use the `MILLE_RESYNC_*` environment variables.

The related UI collapse-state gate lives in `@vibecook/mille-ui`:

```sh
pnpm bench:collapse
```

It removes expanded descendants from controlled React state across a
100,000-sibling tree and a 10,000-level chain. Thirty samples enforce default
p95 budgets of 15 ms wide and 8 ms deep. Override the fixture or budgets with
the `MILLE_COLLAPSE_*` environment variables.

The scoped-search handoff has a separate path-materialization gate:

```sh
pnpm bench:search-scope
```

It measures 100,000 single-folder requests and 100 batches containing 1,000
selected folders. Every batch verifies exact target cardinality. Default p95
budgets are 0.02 ms for one scope and 5 ms for the multi-scope batch; fixture,
sample, and budget overrides use the `MILLE_SEARCH_SCOPE_*` variables.
