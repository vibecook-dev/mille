#!/usr/bin/env node

// Every publishable artifact in this repo ships under one version, and that
// version lives in roughly fifteen files. Release Please writes most of them
// from `release-please-config.json`; anything it does not know about silently
// rots until a release breaks `main`.
//
// That has now happened twice. `packages/mille-ui/src/index.ts` drifted three
// releases stale (see `packages/mille-ui/test/version-export.test.mjs`), and
// 0.3.1 shipped with `build-identity.test.mjs` still asserting '0.3.0' — red on
// `main` five minutes after the release PR merged.
//
// The durable fix is not to enumerate today's offenders in a test. It is to
// pick one file as the source of truth and compare every other location to it,
// so the expected value can never itself go stale:
//
//   packages/mille/package.json  ->  everything else
//
// A hardcoded '0.3.1' anywhere in this script would reintroduce exactly the bug
// it exists to catch. There is deliberately not one.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const read = (path) => readFileSync(join(root, path), 'utf8');
const readJson = (path) => JSON.parse(read(path));

// `@vibecook/mille` is the package the native loader reports as its own
// identity (`readPackageVersion()` in `packages/mille/src/native.ts`), which
// makes it the version users actually observe at runtime. Everything else
// follows it.
const expected = readJson('packages/mille/package.json').version;
const errors = [];

// The root manifest is private, but Release Please treats it as the release
// anchor and bumps it, so a mismatch here means the release itself is broken.
//
// Deliberately absent: `apps/playground`, which is private and pinned at 0.0.0.
// It is not published, so forcing it onto the release version would create
// churn without protecting a consumer.
//
// `packages/mille-truffle` was in that same sentence until it was published.
// It kept its own 0.1.0 line while it was `private: true`, and that would have
// been a quiet trap once it went public: `pnpm -r publish` skips a version the
// registry already has, so a frozen version number does not fail a release —
// it just silently never ships the fix. It is on the release line now.
const jsonPackages = [
  'package.json',
  'packages/mille-ui/package.json',
  'packages/mille-truffle/package.json',
  'packages/mille-darwin-arm64/package.json',
  'packages/mille-darwin-x64/package.json',
  'packages/mille-linux-arm64-gnu/package.json',
  'packages/mille-linux-arm64-musl/package.json',
  'packages/mille-linux-x64-gnu/package.json',
  'packages/mille-linux-x64-musl/package.json',
  'packages/mille-win32-arm64-msvc/package.json',
  'packages/mille-win32-x64-msvc/package.json',
];

for (const path of jsonPackages) {
  const actual = readJson(path).version;
  if (actual !== expected) errors.push(`${path}: expected ${expected}, found ${actual ?? 'none'}`);
}

// Rust versions are inherited from `[workspace.package]`, so the manifest has
// exactly one version line to check.
const workspaceVersion = read('Cargo.toml').match(/^version = "([^"]+)"/m)?.[1];
if (workspaceVersion !== expected) {
  errors.push(`Cargo.toml: expected ${expected}, found ${workspaceVersion ?? 'none'}`);
}

// `Cargo.lock` records the local crates too, and a stale entry there fails the
// `--locked` builds the release matrix runs — long after the release PR merged.
const lock = read('Cargo.lock');
for (const name of ['mille-core', 'mille-git', 'mille-binding', 'mille-bench']) {
  const actual = lock.match(
    new RegExp(`\\[\\[package\\]\\]\\r?\\nname = "${name}"\\r?\\nversion = "([^"]+)"`),
  )?.[1];
  if (actual !== expected) {
    errors.push(`Cargo.lock ${name}: expected ${expected}, found ${actual ?? 'none'}`);
  }
}

// Hand-written literals in shipped source. These are the ones with no build
// step to derive them and no package manager to rewrite them, which is exactly
// why they are the ones that rot.
const versionLiterals = [
  'packages/mille-ui/src/index.ts',
  'packages/mille-ui/src/testing.ts',
  'packages/mille-ui/src/tailwind-preset.ts',
];

for (const path of versionLiterals) {
  const actual = read(path).match(/export const VERSION = '([^']+)';/)?.[1];
  if (actual === undefined) {
    errors.push(`${path}: no VERSION export found`);
  } else if (actual !== expected) {
    errors.push(`${path}: expected ${expected}, found ${actual}`);
  }
}

// On a tag build, prove the tag and the tree agree before anything reaches npm.
// `release.yml` checks out the tag, so a mismatch here means the tag was cut
// against the wrong commit.
const ref = process.env.GITHUB_REF_NAME;
if (ref?.startsWith('v') && ref !== `v${expected}`) {
  errors.push(`release tag ${ref} does not match package version ${expected}`);
}

if (errors.length > 0) {
  console.error('[release:check] version consistency failed:');
  for (const error of errors) console.error(`  - ${error}`);
  console.error(
    '\nSource of truth is packages/mille/package.json. If this fired on a release PR,' +
      '\nthe location is missing from release-please-config.json "extra-files".',
  );
  process.exit(1);
}

console.log(`[release:check] all publishable artifacts are version ${expected}`);
