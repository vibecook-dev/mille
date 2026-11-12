#!/usr/bin/env node

// Writes packages/mille-<platform>/THIRD_PARTY_NOTICES.txt for all eight platform packages, each
// from the crate graph that platform's addon actually links, and checks them before publish.
// The file is build output and is never committed.
//
//   node scripts/release-notices.mjs           write the eight files (release.yml's `notices`
//                                              job, with cargo-about on PATH), then verify them
//   node scripts/release-notices.mjs --verify  verify the eight files only; needs no cargo
//                                              (release.yml's `publish` job, after it downloads
//                                              them from the `notices` job)
//
// Why two jobs: the publish job holds `id-token: write`, which npm's trusted publishing
// exchanges for a publish credential, so anything running in it could publish. Writing the
// notices needs a Rust toolchain and a fetched binary, and no publish rights at all, so it runs
// in a job with read access to the repository and nothing else. Eight text files cross to the
// publish job, which checks them here before anything is stamped or published.
//
// Why per platform: the graphs differ. mio, notify and the windows-* crates are cfg-gated, and
// cargo-about evaluates every target in its config at once, so one shared list would credit, say,
// the Windows crates in the macOS package. Each run therefore gets its own config, the committed
// scripts/notices/about.toml with exactly one `targets` entry prepended.
//
// What it reads: Cargo.lock and the crate manifests, through cargo-about, for crates/mille-binding
// with its default features (the `drive` feature on, `git` off). That is exactly what
// `pnpm build:napi` builds for the release. It never reads the built addons. Build- and
// dev-dependencies are excluded (about.toml). mille's own crates are not: mille-binding,
// mille-core and mille-drive are listed under MIT with the rest, the same licence as the LICENSE
// every package already carries.
//
// Failing closed: `--fail` makes cargo-about exit non-zero on any crate whose licence it cannot
// determine, and a file that is missing, carries another package's or version's header, or lists
// no napi crate fails the job. A `files` entry in package.json does not fail `npm publish` when
// the file is absent, so without this check a broken generator would publish the packages exactly
// as 0.3.5 did, with no notices.

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

/** The first lines of a package's notices: what verify holds every file to. */
function header(pkg, triple) {
  return [
    `Third-party notices for ${pkg.name} ${pkg.version}`,
    '',
    `${pkg.main} is built for ${triple}`,
    'from crates/mille-binding with its default features, and links the Rust',
    'crates listed below.',
    'Each distinct licence text is given once, with every crate it covers.',
    "mille's own code is MIT; see LICENSE.",
  ];
}

const CRATE_LINE = /^ {2}\S+ \d+\.\d+\.\d+\S*/gm;

// The table above must name exactly the triples napi publishes, or a ninth platform would ship
// with no notices and nothing would say so.
const engine = readJson('packages/mille/package.json');
const published = [...engine.napi.triples.additional].sort();
const listed = Object.values(PLATFORMS).sort();
if (JSON.stringify(published) !== JSON.stringify(listed)) {
  fail(`napi.triples.additional (${published}) is not this script's table (${listed})`);
}
const packages = Object.entries(PLATFORMS).map(([suffix, triple]) => {
  const dir = `packages/mille-${suffix}`;
  const pkg = readJson(`${dir}/package.json`);
  if (!pkg.files?.includes(NOTICES)) fail(`${dir}/package.json does not publish ${NOTICES}`);
  return { dir, pkg, triple, file: join(root, dir, NOTICES) };
});

function generate() {
  const aboutToml = readFileSync(join(root, 'scripts/notices/about.toml'), 'utf8');
  if (/^\s*targets\s*=/m.test(aboutToml)) {
    fail('scripts/notices/about.toml sets `targets`; this script sets one per platform');
  }
  const template = join(root, 'scripts/notices/notices.hbs');
  const manifest = join(root, 'crates/mille-binding/Cargo.toml');
  const aboutVersion = execFileSync('cargo', ['about', '--version'], { encoding: 'utf8' }).trim();
  const scratch = mkdtempSync(join(process.env.RUNNER_TEMP || tmpdir(), 'mille-notices-'));

  for (const { dir, pkg, triple, file } of packages) {
    const suffix = dir.slice('packages/mille-'.length);
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
    const lines = [
      ...header(pkg, triple),
      '',
      `Generated at release time by ${aboutVersion} (scripts/release-notices.mjs).`,
      '',
      '',
    ];
    writeFileSync(file, lines.join('\n') + body);
  }
}

function verify() {
  let failed = false;
  for (const { dir, pkg, triple, file } of packages) {
    const where = `${dir}/${NOTICES}`;
    if (!existsSync(file)) {
      console.error(`::error::${where} is missing`);
      failed = true;
      continue;
    }
    const text = readFileSync(file, 'utf8');
    const want = header(pkg, triple).join('\n');
    if (!text.startsWith(want)) {
      const first = text.split('\n', 1)[0].slice(0, 120);
      console.error(`::error::${where} does not open with ${pkg.name} ${pkg.version}'s header`);
      console.error(`  its first line: ${JSON.stringify(first)}`);
      failed = true;
      continue;
    }
    // napi is linked into every addon, so a list without it is not this addon's.
    const crates = (text.match(CRATE_LINE) ?? []).length;
    if (crates === 0 || !/^ {2}napi \d/m.test(text)) {
      console.error(`::error::${where} lists no napi crate (${crates} crate entries)`);
      failed = true;
      continue;
    }
    const sha = createHash('sha256').update(text).digest('hex');
    console.log(`${where}: ${crates} crate entries, ${text.length} bytes, sha256 ${sha}`);
  }
  if (failed) process.exit(1);
  console.log(`\nThird-party notices verified for ${packages.length} platform packages.`);
}

const args = process.argv.slice(2);
if (args.length === 0) {
  generate();
  verify();
} else if (args.length === 1 && args[0] === '--verify') {
  verify();
} else {
  fail('usage: release-notices.mjs [--verify]');
}
