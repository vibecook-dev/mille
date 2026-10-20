// A native panic must reach JS as a catchable error, never as a dead process.
//
// mille loads into someone else's Electron main process. A panic that aborts
// takes their editor down with it — no error boundary, no crash report, no
// chance to flush unsaved buffers. Two independent things have to hold for a
// panic to be survivable, and each was broken in a different way:
//
//   1. The binary must be built with `panic = "unwind"`. `[profile.release]`
//      carried `panic = "abort"`, which turns every panic into SIGABRT before
//      any handler runs. Measured: sync AND async probes both exited 134.
//   2. Sync `#[napi]` entry points must opt into `catch_unwind`. Without it a
//      panic unwinds out of the generated `extern "C"` shim, which Rust
//      defines as an abort (`panic_cannot_unwind`). Measured: exit 134 even in
//      a debug build, where the profile already unwinds.
//
// The async path is worth probing separately because it fails differently:
// tokio catches panics in its own task machinery, so async entry points
// recovered cleanly in debug while dying in release. That divergence is the
// reason this test insists on running against the shipped profile in CI —
// a debug-only run would have called (2) fixed and missed (1) entirely.
//
// The probes are behind the `panic-probe` cargo feature and are absent from
// published artifacts. Build with `pnpm --filter @vibecook/mille
// build:napi:probe` (or `:release`) to exercise the behavioural half.

import { strict as assert } from 'node:assert';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { existsSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { buildIdentity } from '../dist/index.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

const nativePath = buildIdentity().resolvedPath;
const native = require(nativePath);
const hasProbes = typeof native.__panicProbeSync === 'function';
// Only a `--features panic-probe,git` build (`build:napi:git:probe`) has these.
const hasGitProbes = typeof native.__gitPanicProbeSync === 'function';

// Run the probe in a child process: if the guard regresses, the callee dies,
// and a child is the only way to observe that without taking the suite down.
function runProbe(which) {
  const script = join(__dirname, 'fixtures', 'panic-probe-child.cjs');
  assert.ok(existsSync(script), `missing probe fixture: ${script}`);
  try {
    const stdout = execFileSync(process.execPath, [script, nativePath, which], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return { exitCode: 0, stdout };
  } catch (err) {
    return {
      exitCode: err.status,
      signal: err.signal,
      stdout: err.stdout ?? '',
    };
  }
}

test('the release profile is declared to unwind', async () => {
  // This one reads the manifest rather than the loaded binary, and that is
  // deliberate. `[profile.dev]` unwinds no matter what the release profile
  // says, so asserting `nativePanicStrategy` against the debug binary CI
  // loads would pass even with `panic = "abort"` restored — vacuous exactly
  // where it needs to bite. The shipped profile is the one at risk, so check
  // the declaration that governs it.
  const { readFile } = await import('node:fs/promises');
  const manifest = await readFile(join(__dirname, '..', '..', '..', 'Cargo.toml'), 'utf8');
  const releaseSection = manifest.split(/^\[profile\.release\]$/m)[1] ?? '';
  const body = releaseSection.split(/^\[/m)[0];

  assert.ok(releaseSection, 'no [profile.release] section found in Cargo.toml');
  assert.match(
    body,
    /^panic\s*=\s*"unwind"$/m,
    'panic=abort makes every native panic SIGABRT the host process, and defeats ' +
      'catch_unwind and tokio task capture along with it',
  );
});

test('the loaded binary reports its panic strategy', () => {
  // Strong when run against a release artifact, tautological against debug.
  // Kept because it is the only assertion that holds for a *published* .node,
  // where the manifest is not available to inspect.
  const strategy = buildIdentity().nativePanicStrategy;
  assert.equal(strategy, 'unwind', `loaded binary reports panic=${strategy}`);
});

test(
  'a panic in a sync entry point becomes a catchable JS error',
  { skip: !hasProbes && 'built without --features panic-probe' },
  () => {
    const { exitCode, signal, stdout } = runProbe('sync');

    assert.equal(
      signal ?? null,
      null,
      `child died on ${signal} instead of throwing — the panic was not caught`,
    );
    assert.equal(exitCode, 0, `child exited ${exitCode}; stdout: ${stdout}`);
    assert.match(stdout, /^CAUGHT: /m);
    assert.match(stdout, /synchronous panic across the napi boundary/);
  },
);

test(
  'a panic in an async entry point rejects instead of aborting',
  { skip: !hasProbes && 'built without --features panic-probe' },
  () => {
    const { exitCode, signal, stdout } = runProbe('async');

    assert.equal(
      signal ?? null,
      null,
      `child died on ${signal} instead of rejecting — the panic was not caught`,
    );
    assert.equal(exitCode, 0, `child exited ${exitCode}; stdout: ${stdout}`);
    assert.match(stdout, /^CAUGHT: /m);
    assert.match(stdout, /asynchronous panic across the napi boundary/);
  },
);

// SPEC mille-git §18.4: the git module must not be the one exception path. The
// sync probe panics in `mille-git`'s own code; the async one panics inside a
// task on the git engine's runtime — where a gix panic in a status or diff
// would happen — and must come back as an `EUNKNOWN` rejection, not an abort.
test(
  'a panic inside mille-git (sync) becomes a catchable JS error',
  { skip: !hasGitProbes && 'built without --features panic-probe,git' },
  () => {
    const { exitCode, signal, stdout } = runProbe('git-sync');

    assert.equal(signal ?? null, null, `child died on ${signal} — the git panic was not caught`);
    assert.equal(exitCode, 0, `child exited ${exitCode}; stdout: ${stdout}`);
    assert.match(stdout, /^CAUGHT: /m);
    assert.match(stdout, /synchronous panic inside mille-git/);
  },
);

test(
  'a panic on the git engine runtime rejects with GIT|EUNKNOWN',
  { skip: !hasGitProbes && 'built without --features panic-probe,git' },
  () => {
    const { exitCode, signal, stdout } = runProbe('git-async');

    assert.equal(signal ?? null, null, `child died on ${signal} — the git panic was not caught`);
    assert.equal(exitCode, 0, `child exited ${exitCode}; stdout: ${stdout}`);
    assert.match(stdout, /^CAUGHT: GIT\|EUNKNOWN\|\|mille-git task panicked/m);
  },
);

test('every callable napi entry point opts into catch_unwind', async () => {
  // The behavioural probes above only cover the two functions they call. This
  // is the part that generalises: a new `#[napi]` method added without the
  // attribute is a new way to kill the host, and nothing else would catch it.
  const { readFile, readdir } = await import('node:fs/promises');
  const srcDir = join(__dirname, '..', '..', '..', 'crates', 'mille-binding', 'src');
  const files = (await readdir(srcDir)).filter((f) => f.endsWith('.rs'));

  const unguarded = [];
  for (const file of files) {
    const lines = (await readFile(join(srcDir, file), 'utf8')).split('\n');
    for (let i = 0; i < lines.length; i += 1) {
      const attr = lines[i].trim();
      if (!attr.startsWith('#[napi')) continue;

      // Look past any further attributes/comments to the declaration itself;
      // `#[napi(object)]` on a struct is a type, not an entry point.
      let j = i + 1;
      while (j < lines.length) {
        const t = lines[j].trim();
        if (t.startsWith('#[') || t.startsWith('//') || t === '') {
          j += 1;
          continue;
        }
        break;
      }
      const decl = j < lines.length ? lines[j].trim() : '';
      if (!/^(pub )?(async )?fn /.test(decl)) continue;
      if (attr.includes('catch_unwind')) continue;
      unguarded.push(`${file}:${i + 1} ${decl.replace(/\s*\{.*$/, '')}`);
    }
  }

  assert.deepEqual(
    unguarded,
    [],
    `napi entry points missing catch_unwind (a panic here aborts the host):\n  ${unguarded.join('\n  ')}`,
  );
});
