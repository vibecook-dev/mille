import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MessageChannel } from 'node:worker_threads';

import { createFileExplorerHost } from '../dist/index.js';
import { decodeChildLists } from '../dist/child-list-codec.js';
import { decodeClientEntries } from '../dist/entry-codec.js';

function nextMatching(port, predicate, label, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      port.off('message', onMessage);
      reject(new Error(`${label} timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    const onMessage = (message) => {
      if (!predicate(message)) return;
      clearTimeout(timer);
      port.off('message', onMessage);
      resolve(message);
    };
    port.on('message', onMessage);
  });
}

test('handshake and expansion hydrate only roots plus the mounted viewport', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'mille-bounded-hydration-'));
  let host;
  const { port1, port2 } = new MessageChannel();
  const legacy = new MessageChannel();
  try {
    for (let index = 0; index < 256; index++) {
      writeFileSync(join(dir, `file-${String(index).padStart(3, '0')}.txt`), 'x');
    }
    host = await createFileExplorerHost({ roots: [dir] });
    await host.local.populateFromRoots();
    host.attachPort(port1);

    const snapshotPromise = nextMatching(
      port2,
      (message) => message?.type === 'snapshot',
      'packed handshake',
    );
    port2.postMessage({
      v: 1,
      type: 'handshake',
      body: {
        version: 1,
        clientId: 'bounded-hydration',
        options: { packedChildLists: true },
      },
    });
    const snapshot = await snapshotPromise;
    assert.ok(snapshot.body.mirror instanceof ArrayBuffer, 'handshake uses binary entries');
    assert.equal(snapshot.body.entriesJson, undefined, 'handshake omits JSON entries');
    const snapshotEntries = decodeClientEntries(snapshot.body.mirror);
    assert.equal(snapshotEntries.length, snapshot.body.roots.length, 'handshake ships roots only');

    const rootId = snapshot.body.roots[0];
    const viewportPromise = nextMatching(
      port2,
      (message) => message?.type === 'delta' && Array.isArray(message.body.viewportIds),
      'packed viewport',
    );
    port2.postMessage({
      v: 1,
      type: 'setViewport',
      body: { offset: 0, limit: 16, overscan: 0 },
    });
    await viewportPromise;

    // Root metadata can change during watcher startup, so the next delta may
    // refresh the already-known root alongside its visible children. Force
    // that timing here instead of depending on the platform's watcher speed.
    const takePendingChanges = host.local.takePendingChanges.bind(host.local);
    host.local.takePendingChanges = () => {
      host.local.takePendingChanges = takePendingChanges;
      const changes = takePendingChanges();
      return { ...changes, changedIds: [...new Set([...changes.changedIds, rootId])] };
    };

    const expansionPromise = nextMatching(
      port2,
      (message) => message?.type === 'delta' && message.body.childListsBin instanceof ArrayBuffer,
      'packed expansion',
    );
    port2.postMessage({
      v: 1,
      type: 'setExpanded',
      body: { add: [rootId], remove: [] },
    });
    const expansion = await expansionPromise;
    assert.equal(expansion.body.childLists, undefined, 'expansion omits cloned JS id arrays');
    const orderedChildren = decodeChildLists(expansion.body.childListsBin).get(rootId);
    assert.ok(orderedChildren instanceof Uint32Array, 'normal entry ids retain packed u32 storage');
    assert.equal(orderedChildren.length, 256, 'full structure arrives as compact ordered ids');
    assert.equal(expansion.body.entriesJson, undefined, 'expansion omits JSON entries');
    const expansionEntries =
      expansion.body.viewportPatch instanceof ArrayBuffer
        ? decodeClientEntries(expansion.body.viewportPatch)
        : [];
    assert.ok(
      expansionEntries.length <= 16,
      `entry payload stays inside the 16-row viewport; got ${expansionEntries.length}`,
    );
    assert.ok(
      expansionEntries.filter((entry) => entry.id !== rootId).length <= 15,
      'the root leaves room for at most 15 child records',
    );
    const viewportIds = new Set(expansion.body.viewportIds);
    for (const entry of expansionEntries) {
      assert.ok(viewportIds.has(entry.id), `entry ${entry.id} is outside the mounted viewport`);
    }

    // Protocol-v1 clients that did not advertise the packed channel continue
    // to receive the legacy object shape.
    host.attachPort(legacy.port1);
    const legacySnapshotPromise = nextMatching(
      legacy.port2,
      (message) => message?.type === 'snapshot',
      'legacy handshake',
    );
    legacy.port2.postMessage({
      v: 1,
      type: 'handshake',
      body: { version: 1, clientId: 'legacy-bounded-hydration', options: {} },
    });
    await legacySnapshotPromise;
    const legacyExpansionPromise = nextMatching(
      legacy.port2,
      (message) => message?.type === 'delta' && message.body.childLists !== undefined,
      'legacy expansion',
    );
    legacy.port2.postMessage({
      v: 1,
      type: 'setExpanded',
      body: { add: [rootId], remove: [] },
    });
    const legacyExpansion = await legacyExpansionPromise;
    assert.equal(legacyExpansion.body.childListsBin, undefined);
    assert.equal(legacyExpansion.body.childLists[String(rootId)].length, 256);
  } finally {
    port1.close();
    port2.close();
    legacy.port1.close();
    legacy.port2.close();
    await host?.dispose().catch(() => {});
    removeTempDir(dir);
  }
});
