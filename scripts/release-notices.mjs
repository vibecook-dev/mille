#!/usr/bin/env node

// Writes packages/mille-<platform>/THIRD_PARTY_NOTICES.txt for all eight platform packages, each
// from the crate graph that platform's addon actually links. Run by release.yml's publish job,
// after cargo-about is on PATH; the file is build output and is never committed.
//
// Why per platform: the graphs differ. mio, notify and the windows-* crates are cfg-gated, and
// cargo-about evaluates every target in its config at once, so one shared list would credit, say,
// the Windows crates in the macOS package. Each run therefore gets its own config, the committed
// scripts/notices/about.toml with exactly one `targets` entry prepended.
//
// What it reads: crates/mille-binding with its default features (the `drive` feature on, `git`
// off), which is exactly what `pnpm build:napi` builds for the release. Build- and
// dev-dependencies are excluded (about.toml), and so are mille's own path crates, which
// cargo-about skips by default. They are mille's code, under the LICENSE every package already
// carries.
//
// Failing closed: `--fail` makes cargo-about exit non-zero on any crate whose licence it cannot
// determine, and a notices file that is missing, empty or names no crate fails the job here. A
// `files` entry in package.json does not fail `npm publish` when the file is absent, so without
// this check a broken generator would publish the packages exactly as 0.3.5 did, with no
// notices.
//
// Usage: node scripts/release-notices.mjs

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const readJson = (path) => JSON.parse(readFileSync(join(root, path), 'utf8'));

// The package directory's suffix and the Rust triple napi builds it from (release.yml's matrix).
const PLATFORMS = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-arm64-gnu': 'aarch64-unknown-linux-gnu',
  'linux-arm64-musl': 'aarch64-unknown-linux-musl',
  'linux-x64-gnu': 'x86_64-unknown-linux-gnu',
  'linux-x64-musl': 'x86_64-unknown-linux-musl',
  'win32-arm64-msvc': 'aarch64-pc-windows-msvc',
  'win32-x64-msvc': 'x86_64-pc-windows-msvc',
};

const NOTICES = 'THIRD_PARTY_NOTICES.txt';

function fail(message) {
  console.error(`::error::${message}`);
  process.exit(1);
}

// The table above must name exactly the triples napi publishes, or a ninth platform would ship
// with no notices and nothing would say so.
const engine = readJson('packages/mille/package.json');
const published = [...engine.napi.triples.additional].sort();
const listed = Object.values(PLATFORMS).sort();
if (JSON.stringify(published) !== JSON.stringify(listed)) {
  fail(`napi.triples.additional (${published}) is not this script's table (${listed})`);
}

const aboutToml = readFileSync(join(root, 'scripts/notices/about.toml'), 'utf8');
if (/^\s*targets\s*=/m.test(aboutToml)) {
  fail('scripts/notices/about.toml sets `targets`; this script sets one per platform');
}
const template = join(root, 'scripts/notices/notices.hbs');
const manifest = join(root, 'crates/mille-binding/Cargo.toml');
const aboutVersion = execFileSync('cargo', ['about', '--version'], { encoding: 'utf8' }).trim();
const scratch = mkdtempSync(join(process.env.RUNNER_TEMP || tmpdir(), 'mille-notices-'));

for (const [suffix, triple] of Object.entries(PLATFORMS)) {
  const dir = `packages/mille-${suffix}`;
  const pkg = readJson(`${dir}/package.json`);
  if (!pkg.files?.includes(NOTICES)) fail(`${dir}/package.json does not publish ${NOTICES}`);

  const config = join(scratch, `about-${suffix}.toml`);
  writeFileSync(config, `targets = [${JSON.stringify(triple)}]\n\n${aboutToml}`);
  const out = join(scratch, `notices-${suffix}.txt`);
  console.log(`\n${pkg.name}: cargo about generate for ${triple}`);
  execFileSync(
    'cargo',
    [
      'about',
      'generate',
      '--locked',
      '--fail',
      '--manifest-path',
      manifest,
      '--config',
      config,
      '--output-file',
      out,
      template,
    ],
    { cwd: root, stdio: 'inherit' },
  );

  const body = existsSync(out) ? readFileSync(out, 'utf8') : '';
  const crates = (body.match(/^ {2}\S+ \d+\.\d+\.\d+\S*/gm) ?? []).length;
  // napi is linked into every addon, so a list without it is not this addon's.
  if (crates === 0 || !/^ {2}napi \d/m.test(body)) {
    fail(`${pkg.name}: cargo-about wrote no crate list for ${triple} (${body.length} bytes)`);
  }

  const header = [
    `Third-party notices for ${pkg.name} ${pkg.version}`,
    '',
    `${pkg.main} is built for ${triple}`,
    'from crates/mille-binding with its default features, and links the Rust',
    'crates listed below. Each licence is given once, with every crate it covers.',
    "mille's own code is MIT; see LICENSE.",
    '',
    `Generated at release time by ${aboutVersion} (scripts/release-notices.mjs).`,
    '',
    '',
  ].join('\n');
  const text = header + body;
  writeFileSync(join(root, dir, NOTICES), text);
  const sha = createHash('sha256').update(text).digest('hex');
  console.log(`${dir}/${NOTICES}: ${crates} crate entries, ${text.length} bytes, sha256 ${sha}`);
}

const count = Object.keys(PLATFORMS).length;
console.log(`\nThird-party notices written for ${count} platform packages.`);
