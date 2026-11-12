#!/usr/bin/env node

// No published addon may name the machine that built it.
//
// rustc writes every dependency's panic location and `file!()` as the absolute path it compiled
// from, and nothing in the release build told it otherwise: `@vibecook/mille-darwin-arm64@0.3.5`
// carries `/Users/runner/.cargo/registry/src/…` 284 times (`strings -a`, 2026-11-07). That is the
// CI runner's home directory, shipped inside every application that loads mille, and a host whose
// release gate refuses build-machine paths in what it ships cannot adopt the published packages at
// all. VibeField's gate does, which is why it still builds mille from a pinned checkout.
//
//   node scripts/release-paths.mjs rustflags    print RUSTFLAGS for `pnpm build:napi`: whatever
//                                                RUSTFLAGS (or CARGO_BUILD_RUSTFLAGS) already
//                                                holds, then one --remap-path-prefix per root
//   node scripts/release-paths.mjs check FILE…  exit 1 if any FILE still names a root or a CI
//                                                runner's home directory
//
// Why RUSTFLAGS, and not a profile option or a config file. Cargo's `trim-paths` profile option
// would say all of this in one line, but it is unstable at the pinned 1.97.1, and a stable
// toolchain refuses `cargo-features`. A committed `.cargo/config.toml` cannot hold the roots, which
// differ per runner and per container. And napi runs cargo itself: @napi-rs/cli composes
// `RUSTFLAGS ?? CARGO_BUILD_RUSTFLAGS` and appends its own flags (musl, strip), so RUSTFLAGS is the
// one door that keeps napi's. The door has two edges, and both are refused rather than built
// around: RUSTFLAGS splits on whitespace (a root with a space in it), and cargo reads
// CARGO_ENCODED_RUSTFLAGS before RUSTFLAGS (ours would be ignored) and lets RUSTFLAGS replace a
// CARGO_TARGET_<TRIPLE>_RUSTFLAGS or any config file's rustflags (theirs would be dropped).
//
// The roots: the checkout (→ /mille; the target directory and every build script's OUT_DIR sit
// inside it, and CARGO_TARGET_DIR is added when it points elsewhere), CARGO_HOME (→ /cargo: the
// registry, where all 284 came from, and git checkouts) and RUSTUP_HOME (→ /rustup: the
// toolchain). Each as given and as its real path, since a home reached through a symlink would
// otherwise slip past the one rustc saw. rustc applies the LAST mapping that matches, so they go
// shortest first and a root nested inside another keeps its own name.
//
// The check reads the addon the build just wrote, before it is uploaded, so a path that slips
// through fails that target's job instead of shipping. It looks for every root it remapped, in both
// separator spellings, plus the home directories GitHub's hosted runners use, which would catch a
// root this script never knew about (a container build whose paths differ from the host's). On a
// failure it prints the fragment it found and the bytes around it, never the environment.

import { createHash } from 'node:crypto';
import { existsSync, readFileSync, realpathSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** Home directories of GitHub's hosted runners (macOS, Linux, Windows) and the registry path. */
const RUNNER_MARKERS = [
  '/Users/runner/',
  '/home/runner/',
  '\\Users\\runneradmin\\',
  '/Users/runneradmin/',
  '/.cargo/registry/',
  '\\.cargo\\registry\\',
];

function real(path) {
  try {
    return realpathSync.native(path);
  } catch {
    return null;
  }
}

/** `[from, to]` pairs, each root as given and as its real path, shortest first. */
export function remapPairs(env = process.env) {
  const home = homedir();
  const roots = [
    [repoRoot, '/mille'],
    [env.CARGO_HOME || join(home, '.cargo'), '/cargo'],
    [env.RUSTUP_HOME || join(home, '.rustup'), '/rustup'],
  ];
  if (env.CARGO_TARGET_DIR) roots.push([resolve(env.CARGO_TARGET_DIR), '/target']);
  const pairs = new Map();
  for (const [from, to] of roots) {
    for (const path of [resolve(from), real(from)]) {
      if (!path) continue;
      // A root of `/` (or a bare drive) would rewrite every path in the binary, the standard
      // library's `/rustc/<commit>/…` included, and match every byte in the check.
      if (dirname(path) === path) throw new Error(`refusing to remap a filesystem root: ${path}`);
      if (/\s/.test(path)) {
        throw new Error(
          `${path} holds whitespace, which RUSTFLAGS splits on; build from a path without it`,
        );
      }
      pairs.set(path, to);
    }
  }
  return [...pairs].sort(([a], [b]) => a.length - b.length);
}

function rustflags(env = process.env) {
  if (env.CARGO_ENCODED_RUSTFLAGS !== undefined) {
    throw new Error(
      'CARGO_ENCODED_RUSTFLAGS is set; cargo reads it before RUSTFLAGS and would ignore the remap',
    );
  }
  const targeted = Object.keys(env).filter((key) =>
    /^CARGO_TARGET_[A-Z0-9_]+_RUSTFLAGS$/.test(key),
  );
  if (targeted.length > 0) {
    throw new Error(`${targeted.join(', ')} would be replaced by RUSTFLAGS; fold them in first`);
  }
  // napi runs cargo from packages/mille, and cargo discovers config from its working directory up,
  // then CARGO_HOME. RUSTFLAGS replaces every config file's rustflags, so one that sets any is a
  // build this script would silently change.
  const configs = [];
  for (let dir = join(repoRoot, 'packages', 'mille'); ; dir = dirname(dir)) {
    configs.push(join(dir, '.cargo', 'config.toml'), join(dir, '.cargo', 'config'));
    if (dirname(dir) === dir) break;
  }
  const cargoHome = env.CARGO_HOME || join(homedir(), '.cargo');
  configs.push(join(cargoHome, 'config.toml'), join(cargoHome, 'config'));
  const setting = configs.filter(
    (file) => existsSync(file) && /rustflags/.test(readFileSync(file, 'utf8')),
  );
  if (setting.length > 0) {
    throw new Error(`${setting.join(', ')} set rustflags, which RUSTFLAGS would replace`);
  }
  const existing = (env.RUSTFLAGS ?? env.CARGO_BUILD_RUSTFLAGS ?? '').split(/\s+/).filter(Boolean);
  const remap = remapPairs(env).map(([from, to]) => `--remap-path-prefix=${from}=${to}`);
  return [...existing, ...remap].join(' ');
}

/**
 * Every spelling of every root and marker the check looks for. A root is looked for with a
 * separator after it, as any path inside it is written: a container's checkout is just `/build`,
 * and the bare word would match text that names no path.
 */
export function needles(env = process.env) {
  const out = new Set(RUNNER_MARKERS);
  for (const [from] of remapPairs(env)) {
    const spellings = new Set([from, from.replaceAll('\\', '/'), from.replaceAll('/', '\\')]);
    for (const spelling of spellings) {
      out.add(`${spelling}/`);
      out.add(`${spelling}\\`);
    }
  }
  return [...out];
}

function printable(buf) {
  return [...buf].map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.')).join('');
}

function check(files, env = process.env) {
  if (files.length === 0) throw new Error('check: name at least one file');
  const wanted = needles(env);
  let failed = false;
  let missing = false;
  for (const file of files) {
    if (!existsSync(file)) {
      console.error(`::error::${file} does not exist; the build wrote no addon where expected`);
      missing = true;
      continue;
    }
    const bytes = readFileSync(file);
    const sha = createHash('sha256').update(bytes).digest('hex');
    console.log(`${file}: ${bytes.length} bytes, sha256 ${sha}`);
    for (const needle of wanted) {
      const pattern = Buffer.from(needle, 'utf8');
      let count = 0;
      let first = -1;
      for (let at = bytes.indexOf(pattern); at !== -1; at = bytes.indexOf(pattern, at + 1)) {
        if (first === -1) first = at;
        count += 1;
      }
      if (count === 0) continue;
      failed = true;
      const context = printable(bytes.subarray(first, first + pattern.length + 64));
      console.error(`::error::${file} names ${JSON.stringify(needle)} ${count}×, e.g. ${context}`);
    }
  }
  if (failed) {
    console.error('\nA build-machine path survived the remap; see scripts/release-paths.mjs.');
  }
  if (failed || missing) process.exit(1);
  console.log(`No build-machine path in ${files.length} file(s); ${wanted.length} fragments read.`);
}

const isMain = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const [command, ...rest] = process.argv.slice(2);
  try {
    if (command === 'rustflags') console.log(rustflags());
    else if (command === 'check') check(rest);
    else throw new Error('usage: release-paths.mjs rustflags | check FILE…');
  } catch (error) {
    console.error(`::error::${error.message}`);
    process.exit(1);
  }
}
