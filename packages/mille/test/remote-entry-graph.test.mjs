// The renderer-facing entries must stay loadable in a browser or a Worker.
//
// `@vibecook/mille/port` and `@vibecook/mille/remote` are the two entries a
// renderer Worker imports. Neither may reach a `node:*` module, a Node
// builtin under its bare name, or the native addon loader — a bundler would
// either fail or quietly ship a polyfill, and a Worker would crash at load.
// This walks the compiled import graph from each entry, so a stray import
// anywhere underneath is caught, not just one in the entry file itself.

import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import { builtinModules } from 'node:module';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const DIST = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');

// Static `import … from`, `export … from`, side-effect `import '…'`, and
// dynamic `import('…')` with a literal specifier.
const SPECIFIER =
  /(?:\bimport|\bexport)\s[^'"`;]*?\bfrom\s*['"]([^'"]+)['"]|\bimport\s*['"]([^'"]+)['"]|\bimport\s*\(\s*['"]([^'"]+)['"]\s*\)/g;

const BUILTINS = new Set(builtinModules.flatMap((name) => [name, `node:${name}`]));

/** Every module reachable from `entry`, and every bare specifier they import. */
function walk(entry) {
  const files = new Set();
  const bare = new Map();
  const queue = [resolve(DIST, entry)];
  while (queue.length > 0) {
    const file = queue.pop();
    if (files.has(file)) continue;
    files.add(file);
    const source = readFileSync(file, 'utf8');
    for (const match of source.matchAll(SPECIFIER)) {
      const spec = match[1] ?? match[2] ?? match[3];
      if (spec.startsWith('.')) {
        queue.push(resolve(dirname(file), spec));
      } else {
        bare.set(spec, relative(DIST, file));
      }
    }
  }
  return { files: [...files].map((f) => relative(DIST, f)).sort(), bare };
}

for (const entry of ['client-port.js', 'remote-client.js']) {
  test(`${entry} reaches no node: module, Node builtin or native loader`, () => {
    const { files, bare } = walk(entry);
    assert.ok(files.length > 1, 'the walk found the entry and its imports');
    for (const [spec, from] of bare) {
      assert.ok(!BUILTINS.has(spec), `${from} imports the Node builtin ${spec}`);
      assert.ok(!spec.startsWith('node:'), `${from} imports ${spec}`);
    }
    // These are the modules that load or wrap the native addon.
    for (const nodeOnly of ['native.js', 'client.js', 'host.js', 'node.js', 'index.js']) {
      assert.ok(!files.includes(nodeOnly), `${entry} reaches ${nodeOnly}: ${files.join(', ')}`);
    }
    assert.deepEqual([...bare.keys()], [], `${entry} imports no packages at runtime`);
  });
}

test('the walker sees node: imports where they exist (it is not vacuous)', () => {
  // The root entry loads the native addon, so its graph must show both.
  const { files, bare } = walk('index.js');
  assert.ok(files.includes('native.js'), 'index.js reaches native.js');
  assert.ok(
    [...bare.keys()].some((spec) => BUILTINS.has(spec)),
    'index.js reaches a Node builtin — if this fails, the walker is broken',
  );
});
