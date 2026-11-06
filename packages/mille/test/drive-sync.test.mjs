// Two native drives in one process, joined by createMemoryDriveNetwork and
// syncDrive: the TypeScript loop over the Rust sync core, end to end. The
// core's own cases (mismatched pulls, chunked updates, a three-drive relay)
// are `cargo test -p mille-drive`; this proves the loop and the transport
// seam carry them.

import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { createMemoryDriveNetwork, Drive, syncDrive } from '../dist/drive.js';
import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { needsDrive, putText, tempDir, waitFor } from './fixtures/drive-fixture.mjs';

/** Two drives, `a` and `b`, synced over one memory network. */
function pair(dir) {
  const network = createMemoryDriveNetwork();
  const a = Drive.open({ root: join(dir, 'a'), device: 'desk-a' });
  const b = Drive.open({ root: join(dir, 'b'), device: 'desk-b' });
  const syncA = syncDrive(a, network.transportFor('a', a), { tickMs: 100 });
  const syncB = syncDrive(b, network.transportFor('b', b), { tickMs: 100 });
  const close = () => {
    syncA.dispose();
    syncB.dispose();
    a.dispose();
    b.dispose();
  };
  return { network, a, b, syncA, syncB, close };
}

/** The version `id` of `item` on `drive`, once its objects are held there. */
function heldVersion(drive, item, id) {
  return drive.item(item)?.versions.find((v) => v.id === id && v.held) ?? null;
}

test('a version made on one drive is held, verified, on the other', needsDrive(), async () => {
  const dir = tempDir();
  const { a, b, syncA, syncB, close } = pair(dir);
  try {
    const put = await putText(a, dir, 'notes.md', '# notes\n');
    const version = a.addVersion('notes', [{ path: 'notes.md', ...put.object }]);

    await waitFor(() => heldVersion(b, 'notes', version), { what: "b holding a's version" });
    assert.equal(b.item('notes').current, version);
    assert.equal(b.item('notes').versions[0].device, 'desk-a');
    assert.equal(await b.verify(put.object.blake3), true);
    assert.equal(readFileSync(b.objectPath(put.object.blake3), 'utf8'), '# notes\n');

    const built = await b.build('notes', version);
    assert.equal(readFileSync(join(built, 'notes.md'), 'utf8'), '# notes\n');

    await syncA.settled();
    await syncB.settled();
    await waitFor(() => syncB.status.state === 'idle', { what: "b's sync to go idle" });
    assert.deepEqual(syncB.status.peers, ['a']);
    assert.equal(syncB.status.wants, 0);
  } finally {
    close();
    removeTempDir(dir, 'drive sync');
  }
});

test('Keep on one drive is kept on the other', needsDrive(), async () => {
  const dir = tempDir();
  const { a, b, close } = pair(dir);
  try {
    const put = await putText(a, dir, 'app.js', 'export {};\n');
    const version = a.addVersion('app', [{ path: 'app.js', ...put.object }]);
    await waitFor(() => heldVersion(b, 'app', version), { what: 'b holding the app' });

    b.setKept('app', true);
    await waitFor(() => a.item('app')?.kept === true, { what: "b's Keep reaching a" });
    b.setHidden('app', true);
    await waitFor(() => a.item('app')?.hidden === true, { what: "b's Hide reaching a" });
  } finally {
    close();
    removeTempDir(dir, 'drive keep');
  }
});

test('a cut link catches up when it is restored', needsDrive(), async () => {
  const dir = tempDir();
  const { network, a, b, syncB, close } = pair(dir);
  try {
    const first = await putText(a, dir, 'v1.txt', 'one');
    const v1 = a.addVersion('doc', [{ path: 'doc.txt', ...first.object }]);
    await waitFor(() => heldVersion(b, 'doc', v1), { what: 'b holding v1' });

    network.partition('a', 'b', true);
    await waitFor(() => syncB.status.peers.length === 0, { what: 'b seeing a go' });
    const second = await putText(a, dir, 'v2.txt', 'two');
    const v2 = a.addVersion('doc', [{ path: 'doc.txt', ...second.object }]);
    // Long enough for several ticks: nothing may cross a cut link.
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(b.item('doc').current, v1);

    network.partition('a', 'b', false);
    await waitFor(() => heldVersion(b, 'doc', v2), { what: 'b catching up to v2' });
    assert.equal(b.item('doc').current, v2);
    const built = await b.build('doc', v2);
    assert.equal(readFileSync(join(built, 'doc.txt'), 'utf8'), 'two');
  } finally {
    close();
    removeTempDir(dir, 'drive partition');
  }
});
