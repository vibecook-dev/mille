# mille-drive

A drive: immutable versions of trees, held by every replica and synced among
them. Pure Rust, synchronous, no napi and no tokio — like `mille-git`, any mille
host links it alone. The napi binding exposes it as `Drive` (cargo feature
`drive`, on by default), and `@vibecook/mille/drive` is its TypeScript entry.

It was built for VibeField's Drive (its record `draft/thinking-drive.md`, M-D1),
and it knows nothing of VibeField: trees, versions, objects and intents — never
what an item means to the host that made it.

## Laws

- **Bytes never ride the document.** The document holds names, digests, sizes
  and intents. An object's bytes are a file named by its blake3.
- **An object is immutable and verified at every hop.** A local put hashes what
  it staged; a pulled copy is hashed before it is admitted, and a mismatch is
  refused (`EMISMATCH`) and removed. `verify` re-hashes a held object.
- **One writer per object.** Publishing is first-writer-wins; every later writer
  of the same bytes learns `Existed`. Keep, Hide and a new version merge in the
  document.
- **Honest states.** Every failure carries a code a host can turn into a state:
  `ENOSPC` is "out of room", `EMISMATCH` "a copy arrived damaged",
  `EBUSY` "open elsewhere". A version whose objects are not all here says
  `held: false`; `wants()` lists what is still to come.

## On disk

```text
<root>/
  drive.json                 {format: 1, device}
  drive.loro                 the document's full snapshot, replaced atomically
  LOCK                       held exclusively by the one open Drive
  objects/<2 hex>/<64 hex>   objects by blake3, read-only
  built/<item>/<version>/    versions built as folders of hard links
  staging/                   temporaries, all named mdrive-*, swept at open
```

**`objects/` is a stable, public layout.** A puller names the file it wants as
the holder's `objects_root()` joined with `object_relpath(digest)` —
`<2 hex>/<64 hex>`, relative to the objects root — so a transport never has to
know anything else about a drive. `objects_root()` is the only folder a host may
register as a pull root: never the drive root, which holds the document and
staging. Changing this layout is a format change.

**A built file shares its object's inode**, and so does an object put by link
with its source. Every name of that inode is one file: making the object
read-only makes each name read-only (on Windows, clearing the attribute to
delete one name clears it for all of them, so `unbuild` and `gc` put it back),
and a write through any name changes the object for every name — the drive's
copy and, for a linked put, the source's. A host that derives a file in a view
(a themed `index.html`) leaves it out of the build (`build_into`'s `except`) or
replaces it by writing a new file and renaming it over, never by writing into a
link.

**One drive per root.** `Drive::open` takes `LOCK` exclusively (`flock` on Unix,
a share-nothing open on Windows) and fails `EBUSY` while another holds it.

## Ids

An item id is `[a-z0-9][a-z0-9-]{0,63}`, not a Windows device name. A version id
is 64 lowercase hex: the blake3 of the version's canonical manifest. Both are
folder names under `built/`, so neither holds `:` or `/`. A host whose own ids
do (a URL, a fold key) names the item by a digest of its id and keeps the id in
the item's meta. Attachment names and meta keys are `[a-z0-9][a-z0-9._-]{0,63}`.

A version's paths are relative, `/`-separated and portable: mille-git's safe
path rule, plus nothing Windows refuses (`< > : " | ? *`, control characters, a
segment ending in a dot or space, device names), no two paths equal ignoring
case, and no file where another path needs a folder.

The canonical manifest is the line `mille-drive manifest v1`, then one line per
file in byte order of path: `path NUL blake3-hex NUL bytes LF`. The same tree is
the same version whoever makes it, and a receiver re-derives the id: a record
that does not match its key is **quarantined** — listed, never built, held or
served.

## The document

A Loro document of root maps only (no nested containers, which fork on a
concurrent first write), and only plain values in them:

| map           | key                       | value                             |
|---------------|---------------------------|-----------------------------------|
| `versions`    | `<item>/<version>`        | JSON `{files, createdAt, device}` |
| `current`     | `<item>`                  | a version id                      |
| `kept`        | `<item>`                  | bool                              |
| `hidden`      | `<item>`                  | bool                              |
| `meta`        | `<item>/<key>`            | string                            |
| `attachments` | `<item>/<version>/<name>` | JSON `{blake3, bytes}`            |

Concurrent writes to one key converge on one value by Loro's map order. The
document is saved as a full snapshot after every change (never a shallow one),
and a drive mints a fresh Loro peer id every time it opens.

## Holding

`HoldPolicy` says what a drive holds eagerly: every item's current version, a
kept item's current version (or, with `KeptScope::All`, every version), and the
attachments of what it holds. `wants()` is what that policy names and the drive
does not hold yet. `gc` removes every object the policy, a build under `built/`
and the host's own views (`GcOptions::keep`) do not name.

## Sync

The sync core does no I/O of its own: frames in, actions out. A host owns the
transport and runs a loop — `sync_open` / `sync_close` on a peer coming and
going, `sync_handle` on each frame, `sync_local_changed` after a local write,
`sync_tick` now and then — and carries out the actions returned:

- `Send { to, frame }`: send the frame. Frames to one peer must arrive in order.
- `Pull { from, digest, into }`: fetch `from`'s `objects_root()` +
  `object_relpath(digest)`, land it at `into` or anywhere on the drive's
  volume, then call `sync_pulled(from, digest, landed)` (which verifies and
  admits it, consuming the file) or `sync_pull_failed(from, digest)`. The drive
  chooses holders and retries; a host never retries on its own.

Frames are a version byte (`1`) and a bincode body: `Hello {vv}`,
`Updates {bytes}`, `UpdatesChunk {id, seq, total, bytes}` (an update larger
than `max_frame_bytes`, reassembled in order before import), `Want {digests}`
and `Have {digests}` (at most 1024 per frame). Every import runs under
`catch_unwind`; an update that panics Loro is refused and the document reloaded
from disk.

`Transport` is the same contract in Rust, `apply_actions` the loop over it, and
`MemoryNetwork` an in-process transport for tests (with a partition and a
damaged-copy fault).

## Not in this first cut

- On-demand pulls of versions the hold policy does not name, and GC budgets.
- An update log with compaction (every change saves the full snapshot).
- mille-truffle's transport (M-D3) — VibeField implements the transport over
  its own node.
