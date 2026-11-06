// The napi `Drive` behind the `drive` feature (crates/mille-binding/src/drive.rs)
// and its entry, `@vibecook/mille/drive`.
//
// The feature is on by default, so the ordinary dev build carries the class
// and the native half runs. A `--no-default-features` binary skips it, and
// what remains is the feature-off contract: no class, no throw, an
// `EUNSUPPORTED` that says why. `MILLE_EXPECT_DRIVE=1` (CI's test-drive job)
// turns that skip into a failure.

import { strict as assert } from 'node:assert';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import * as root from '../dist/index.js';
import { buildIdentity } from '../dist/index.js';
import {
  Drive,
  DriveError,
  isDriveError,
  isNativeDriveAvailable,
  objectRelpath,
} from '../dist/drive.js';
import { tryParseDriveReason } from '../dist/drive/error.js';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { needsDrive, putText, tempDir, waitFor } from './fixtures/drive-fixture.mjs';

const dist = resolve(dirname(fileURLToPath(import.meta.url)), '../dist');

test('isNativeDriveAvailable agrees with the features the loaded binary reports', () => {
  const features = buildIdentity().nativeFeatures;
  assert.ok(Array.isArray(features));
  assert.equal(isNativeDriveAvailable(), features.includes('drive'));
  if (process.env.MILLE_EXPECT_DRIVE === '1') {
    assert.equal(isNativeDriveAvailable(), true, 'MILLE_EXPECT_DRIVE=1 needs a drive build');
  }
});

test('the root barrel does not export the drive', () => {
  for (const name of [
    'Drive',
    'DriveError',
    'isNativeDriveAvailable',
    'syncDrive',
    'createMemoryDriveNetwork',
    'objectRelpath',
  ]) {
    assert.equal(name in root, false, `@vibecook/mille must not export ${name}`);
  }
});

/** Every import specifier in a built ESM file (comments may name others). */
function importsOf(file) {
  const src = readFileSync(file, 'utf8');
  const specs = [];
  for (const re of [
    /(?:^|[\s;}])(?:import|export)\b[^'"]*?from\s*['"]([^'"]+)['"]/g,
    /import\s*\(\s*['"]([^'"]+)['"]\s*\)/g,
    /(?:^|[\s;}])import\s*['"]([^'"]+)['"]/g,
  ]) {
    let m;
    while ((m = re.exec(src)) !== null) specs.push(m[1]);
  }
  return specs;
}

test('the entry loads the binary as exactly ./native.js, and nothing under it does', () => {
  // A host that bundles mille swaps that one specifier, imported from mille's
  // dist, for its own loader (VibeField's files worker). A drive module
  // reaching native-loader.js, or native.js by any other path, would bypass
  // the swap and search for a binary the bundle does not have.
  const entry = importsOf(join(dist, 'drive.js'));
  assert.ok(entry.includes('./native.js'), `drive.js imports ${entry.join(', ')}`);
  assert.equal(
    entry.some((spec) => spec.includes('native-loader')),
    false,
  );
  for (const name of ['engine.js', 'error.js', 'layout.js', 'memory.js', 'sync.js', 'types.js']) {
    const specs = importsOf(join(dist, 'drive', name));
    const loads = specs.filter((spec) => /native-loader|native\.js$/.test(spec));
    assert.deepEqual(loads, [], `drive/${name} must not load the binary`);
  }
});

test('DRIVE| reasons parse into DriveError; anything else is not one', () => {
  const err = tryParseDriveReason('DRIVE|EMISMATCH|/d/objects/ab/x|hashes to y: a|b');
  assert.ok(err instanceof DriveError);
  assert.equal(err.code, 'EMISMATCH');
  assert.equal(err.path, '/d/objects/ab/x');
  assert.equal(err.message, 'hashes to y: a|b');
  assert.equal(isDriveError(err), true);

  const noPath = tryParseDriveReason('DRIVE|ENOSPC||the disk is full');
  assert.equal(noPath.code, 'ENOSPC');
  assert.equal(noPath.path, undefined);

  assert.equal(tryParseDriveReason('DRIVE|EBOGUS||x').code, 'EUNKNOWN');
  assert.equal(tryParseDriveReason('GIT|EINVAL||x'), null);
  assert.equal(tryParseDriveReason('DRIVE|EINVAL'), null);
  assert.equal(tryParseDriveReason(undefined), null);
  assert.equal(isDriveError(new Error('plain')), false);
});

test('objectRelpath names <2 hex>/<64 hex> and refuses anything but a digest', () => {
  const digest = 'ab'.padEnd(64, '0');
  assert.equal(objectRelpath(digest), `ab/${digest}`);
  for (const bad of ['', 'AB'.padEnd(64, '0'), 'ab'.padEnd(63, '0'), '../'.padEnd(64, '0'), 7]) {
    assert.throws(
      () => objectRelpath(bad),
      (e) => isDriveError(e) && e.code === 'EINVAL',
    );
  }
});

test('without the drive feature, open refuses and says why', () => {
  if (isNativeDriveAvailable()) return;
  assert.throws(
    () => Drive.open({ root: join(dist, 'never-opened'), device: 'a' }),
    (e) => isDriveError(e) && e.code === 'EUNSUPPORTED' && /no-default-features/.test(e.message),
  );
});

test('put, add a version, list it, build it and find its objects', needsDrive(), async () => {
  const dir = tempDir();
  const drive = Drive.open({ root: join(dir, 'drive'), device: 'desk-a' });
  try {
    const put = await putText(drive, dir, 'hello.txt', 'hello\n');
    assert.match(put.object.blake3, /^[0-9a-f]{64}$/);
    assert.equal(put.object.bytes, 6);
    assert.ok(put.how === 'linked' || put.how === 'copied', put.how);
    assert.equal(drive.has(put.object.blake3), true);

    // The public layout: a peer's path is its objectsRoot joined with
    // objectRelpath, and that is where the drive keeps it.
    const objectPath = drive.objectPath(put.object.blake3);
    assert.equal(objectPath, join(drive.objectsRoot(), objectRelpath(put.object.blake3)));
    assert.equal(readFileSync(objectPath, 'utf8'), 'hello\n');
    assert.equal(drive.objectPath('0'.repeat(64)), null);

    const again = await putText(drive, dir, 'same.txt', 'hello\n');
    assert.equal(again.how, 'existed');

    const files = [{ path: 'docs/hello.txt', ...put.object }];
    const version = drive.addVersion('notes', files);
    assert.match(version, /^[0-9a-f]{64}$/);

    const [item, ...rest] = drive.items();
    assert.equal(rest.length, 0);
    assert.equal(item.id, 'notes');
    assert.equal(item.current, version);
    assert.equal(item.kept, false);
    assert.equal(item.versions.length, 1);
    assert.equal(item.versions[0].held, true);
    assert.equal(item.versions[0].device, 'desk-a');
    assert.deepEqual(item.versions[0].files, files);
    assert.deepEqual(drive.item('notes'), item);
    assert.equal(drive.item('absent'), null);
    assert.deepEqual(drive.wants(), []);

    const built = await drive.build('notes', version);
    assert.equal(readFileSync(join(built, 'docs', 'hello.txt'), 'utf8'), 'hello\n');
    assert.equal(await drive.build('notes', version), built);

    const exported = drive.exportDescriptor();
    assert.equal(exported.kind, 'drive');
    assert.equal(exported.access, 'read-only');
    assert.equal(exported.followSymlinks, false);
    assert.equal(exported.roots.length, 1);

    assert.equal(await drive.verify(put.object.blake3), true);
  } finally {
    drive.dispose();
    removeTempDir(dir, 'drive smoke');
  }
});

test('a drive refuses what it cannot keep, as DriveError codes', needsDrive(), async () => {
  const dir = tempDir();
  const drive = Drive.open({ root: join(dir, 'drive'), device: 'desk-a' });
  try {
    writeFileSync(join(dir, 'x.txt'), 'x');
    await assert.rejects(
      drive.putFile(join(dir, 'x.txt'), { expect: 'f'.repeat(64) }),
      (e) => isDriveError(e) && e.code === 'EMISMATCH',
    );
    assert.equal(drive.has('f'.repeat(64)), false);

    const missing = [{ path: 'a.txt', blake3: 'e'.repeat(64), bytes: 1 }];
    assert.throws(
      () => drive.addVersion('notes', missing),
      (e) => isDriveError(e) && e.code === 'ENOENT',
    );
    assert.throws(
      () => drive.setKept('nothing-here', true),
      (e) => isDriveError(e) && e.code === 'EUNKNOWNITEM',
    );
    assert.throws(
      () => drive.addVersion('Not:An/Id', []),
      (e) => isDriveError(e) && e.code === 'EINVAL',
    );
    assert.throws(
      () => Drive.open({ root: join(dir, 'drive'), device: 'desk-a' }),
      (e) => isDriveError(e) && e.code === 'EBUSY',
    );
  } finally {
    drive.dispose();
    removeTempDir(dir, 'drive refusals');
  }
});

test(
  'buildInto leaves out what the host provides, and refuses to clobber',
  needsDrive(),
  async () => {
    const dir = tempDir();
    const drive = Drive.open({ root: join(dir, 'drive'), device: 'desk-a' });
    try {
      const a = await putText(drive, dir, 'a.txt', 'a');
      const b = await putText(drive, dir, 'b.txt', 'b');
      const files = [
        { path: 'a.txt', ...a.object },
        { path: 'app.json', ...b.object },
      ];
      const version = drive.addVersion('app', files);
      const dest = join(dir, 'out');
      await drive.buildInto('app', version, dest, { mode: 'copy', except: ['app.json'] });
      assert.equal(readFileSync(join(dest, 'a.txt'), 'utf8'), 'a');
      assert.equal(existsSync(join(dest, 'app.json')), false);

      await assert.rejects(
        drive.buildInto('app', version, dest),
        (e) => isDriveError(e) && e.code === 'EEXIST',
      );
      await drive.buildInto('app', version, dest, { replace: true });
      assert.equal(readFileSync(join(dest, 'app.json'), 'utf8'), 'b');
    } finally {
      drive.dispose();
      removeTempDir(dir, 'drive buildInto');
    }
  },
);

test('a local write is heard, and dispose releases the folder', needsDrive(), async () => {
  const dir = tempDir();
  const rootDir = join(dir, 'drive');
  const drive = Drive.open({ root: rootDir, device: 'desk-a' });
  try {
    const put = await putText(drive, dir, 'n.txt', 'n');
    drive.addVersion('notes', [{ path: 'n.txt', ...put.object }]);
    const heard = [];
    const off = drive.onChange((change) => heard.push(change));
    drive.setMeta('notes', 'title', 'Notes');
    await waitFor(() => heard.length > 0, { what: 'the setMeta change' });
    off();
    assert.deepEqual(heard[0], { kind: 'local', items: ['notes'] });
    assert.equal(drive.item('notes').meta.title, 'Notes');
  } finally {
    drive.dispose();
  }
  drive.dispose();
  assert.throws(
    () => drive.items(),
    (e) => isDriveError(e) && e.code === 'EINVAL' && /disposed/.test(e.message),
  );
  await assert.rejects(
    drive.build('notes', '0'.repeat(64)),
    (e) => isDriveError(e) && /disposed/.test(e.message),
  );

  const reopened = Drive.open({ root: rootDir, device: 'desk-a' });
  try {
    assert.equal(reopened.item('notes').meta.title, 'Notes');
  } finally {
    reopened.dispose();
    removeTempDir(dir, 'drive dispose');
  }
});
