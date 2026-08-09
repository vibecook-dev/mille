// AC-008: traversal, symlink/junction escape, and external import.
//
// These had no coverage. `followSymlinks: false` was enforced at config
// validation and never actually attacked, and nothing checked that a remote
// peer cannot name a path outside the export.
//
// The structural defence is that a client never sends a root — it names an
// export, and roots are canonicalized once at startup. So the attack surface
// is narrow: the few calls that do take a path-ish argument, plus anything on
// disk that points outward. Both are tested here rather than argued about.

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
  symlinkSync,
  realpathSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, sep } from 'node:path';
import { PassThrough, Duplex } from 'node:stream';
import { EventEmitter } from 'node:events';

import { serveMille, connectMille, resolveExport, ExportConfigError } from '../dist/index.js';

async function waitFor(predicate, label, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const v = predicate();
    if (v !== null && v !== undefined && v !== false) return v;
    await new Promise((r) => setTimeout(r, 25));
  }
  throw new Error(`timed out waiting for ${label}`);
}

function socketPair() {
  const a2b = new PassThrough();
  const b2a = new PassThrough();
  const server = Duplex.from({ readable: a2b, writable: b2a });
  const client = Duplex.from({ readable: b2a, writable: a2b });
  for (const s of [server, client, a2b, b2a]) s.on('error', () => {});
  server.remotePeerId = 'nSEC00001CNTRL';
  server.remotePeerName = 'security peer';
  return { server, client };
}

class FakeMeshServer extends EventEmitter {
  #listener;
  port;
  constructor(listener) {
    super();
    this.#listener = listener;
  }
  listen(port) {
    this.port = port;
    queueMicrotask(() => this.emit('listening'));
    return this;
  }
  close(cb) {
    this.closed = true;
    if (cb) queueMicrotask(cb);
    return this;
  }
  accept(socket) {
    this.#listener(socket);
  }
}

function loopbackMesh() {
  const mesh = { net: {}, servers: [] };
  mesh.net.createServer = (l) => {
    const s = new FakeMeshServer(l);
    mesh.servers.push(s);
    return s;
  };
  mesh.net.connect = () => {
    const { server, client } = socketPair();
    queueMicrotask(() => {
      if (mesh.servers[0] && !mesh.servers[0].closed) mesh.servers[0].accept(server);
      client.emit('connect');
    });
    return client;
  };
  return mesh;
}

/**
 * A sandbox with an exported root and a sibling directory that must stay
 * unreachable. `secret.txt` outside the export is the thing an escape would
 * expose.
 */
function sandbox() {
  const base = realpathSync.native(mkdtempSync(join(tmpdir(), 'mille-sec-')));
  const root = join(base, 'exported');
  const outside = join(base, 'outside');
  mkdirSync(root);
  mkdirSync(outside);
  writeFileSync(join(root, 'inside.txt'), 'fine\n');
  writeFileSync(join(outside, 'secret.txt'), 'must not be reachable\n');
  return { base, root, outside };
}

async function serveRoot(root, access = 'read-write', maxFileBytes) {
  const mesh = loopbackMesh();
  const server = await serveMille(mesh, {
    exports: {
      work: {
        label: 'Guarded',
        roots: [root],
        access,
        ...(maxFileBytes === undefined ? {} : { maxFileBytes }),
        explorer: { initialWalk: 'roots-only', watchDebounceMs: 40 },
      },
    },
  });
  const remote = await connectMille(mesh, {
    peer: 'fake',
    exportId: 'work',
    access,
    reconnect: false,
  });
  const rootId = await waitFor(() => remote.explorer.getSnapshot().roots()[0]?.id, 'root id');
  return { server, remote, rootId };
}

// ─── path traversal ─────────────────────────────────────────────────────

test('AC-008: resolvePath cannot escape the export with .. segments', async () => {
  const { base, root } = sandbox();
  let s;
  try {
    s = await serveRoot(root);
    const attempts = [
      '../outside/secret.txt',
      '../../outside/secret.txt',
      './../outside/secret.txt',
      'subdir/../../outside/secret.txt',
      '..',
      '../',
    ];
    for (const attempt of attempts) {
      const id = await s.remote.explorer.resolvePath(attempt);
      assert.equal(id, null, `${JSON.stringify(attempt)} must not resolve`);
    }
  } finally {
    if (s) {
      await s.remote.close();
      await s.server.close();
    }
    removeTempDir(base);
  }
});

test('AC-008: an absolute path outside the export does not resolve', async () => {
  const { base, root, outside } = sandbox();
  let s;
  try {
    s = await serveRoot(root);
    for (const attempt of [
      join(outside, 'secret.txt'),
      outside,
      base,
      process.platform === 'win32' ? 'C:\\Windows\\win.ini' : '/etc/passwd',
    ]) {
      const id = await s.remote.explorer.resolvePath(attempt);
      assert.equal(id, null, `${JSON.stringify(attempt)} must not resolve`);
    }
  } finally {
    if (s) {
      await s.remote.close();
      await s.server.close();
    }
    removeTempDir(base);
  }
});

test('AC-008: mixed separators and encoding tricks do not resolve', async () => {
  const { base, root } = sandbox();
  let s;
  try {
    s = await serveRoot(root);
    for (const attempt of [
      '..\\outside\\secret.txt',
      '..%2Foutside%2Fsecret.txt',
      '%2e%2e/outside/secret.txt',
      `..${sep}outside${sep}secret.txt`,
      'inside.txt/../../outside/secret.txt',
    ]) {
      const id = await s.remote.explorer.resolvePath(attempt);
      assert.equal(id, null, `${JSON.stringify(attempt)} must not resolve`);
    }
    // The control: a legitimate in-export path still works, so the tests
    // above are not passing merely because resolvePath returns null always.
    const ok = await s.remote.explorer.resolvePath('inside.txt');
    assert.notEqual(ok, null, 'a real in-export path still resolves');
  } finally {
    if (s) {
      await s.remote.close();
      await s.server.close();
    }
    removeTempDir(base);
  }
});

// ─── symlink / junction escape ──────────────────────────────────────────

/**
 * Create a directory link, preferring the strongest form the platform allows.
 *
 * Windows refuses directory symlinks without Developer Mode or elevation, but
 * allows **junctions** unprivileged — and AC-008 names junction escape
 * explicitly, so falling back to one is testing the thing that actually
 * matters on Windows rather than skipping the platform where reparse points
 * are the real risk.
 */
function linkDir(target, path) {
  for (const type of ['dir', 'junction']) {
    try {
      symlinkSync(target, path, type);
      return type;
    } catch {
      /* try the next form */
    }
  }
  return null;
}

test('AC-008: a symlink or junction inside the export is not followed out of it', async (t) => {
  const { base, root, outside } = sandbox();
  const link = join(root, 'escape-link');
  const kind = linkDir(outside, link);
  if (kind === null) {
    removeTempDir(base);
    t.skip('platform allows neither directory symlinks nor junctions');
    return;
  }
  t.diagnostic(`link type: ${kind}`);

  let s;
  try {
    s = await serveRoot(root);
    await s.remote.explorer.setExpanded({ add: [s.rootId], remove: [] });
    s.remote.explorer.setViewport({ offset: 0, limit: 200, overscan: 8 });

    const expanded = new Set([s.rootId]);
    await waitFor(
      () =>
        s.remote.explorer
          .getSnapshot()
          .visibleRows({ offset: 0, limit: 200, expanded })
          .some((r) => r.name === 'inside.txt'),
      'export listing',
    );

    // The link may be listed — that is fine, it exists. What must not happen
    // is the contents beyond it becoming reachable.
    const rows = s.remote.explorer
      .getSnapshot()
      .visibleRows({ offset: 0, limit: 200, expanded });
    assert.equal(
      rows.some((r) => r.name === 'secret.txt'),
      false,
      'content behind the symlink must not be listed',
    );

    // And naming it explicitly must not resolve either.
    for (const attempt of ['escape-link/secret.txt', join('escape-link', 'secret.txt')]) {
      assert.equal(
        await s.remote.explorer.resolvePath(attempt),
        null,
        `${attempt} must not resolve through the link`,
      );
    }
  } finally {
    if (s) {
      await s.remote.close();
      await s.server.close();
    }
    removeTempDir(base);
  }
});

test('AC-008: an export cannot be configured to follow symlinks', () => {
  const { base, root } = sandbox();
  try {
    // SEC-003 is enforced at config time, so a misconfiguration cannot even
    // start the service — it is not a runtime check that might be missed.
    for (const value of [true, 'smart']) {
      assert.throws(
        () => resolveExport('w', { label: 'W', roots: [root], access: 'read-only', followSymlinks: value }),
        ExportConfigError,
        `followSymlinks: ${JSON.stringify(value)} must be rejected`,
      );
    }
    // Explicit false and omitted are both fine.
    assert.ok(resolveExport('w', { label: 'W', roots: [root], access: 'read-only' }));
    assert.ok(
      resolveExport('w', {
        label: 'W',
        roots: [root],
        access: 'read-only',
        followSymlinks: false,
      }),
    );
  } finally {
    removeTempDir(base);
  }
});

test('AC-008: a symlinked export root is canonicalized, not served as the link', () => {
  const { base, root } = sandbox();
  const linkToRoot = join(base, 'link-to-exported');
  if (linkDir(root, linkToRoot) === null) {
    removeTempDir(base);
    return;
  }
  try {
    const ex = resolveExport('w', { label: 'W', roots: [linkToRoot], access: 'read-only' });
    // Canonicalizing at startup is what makes every later containment check
    // meaningful; serving the link verbatim would compare against a path
    // that resolves elsewhere.
    assert.equal(ex.roots.length, 1);
    assert.equal(
      realpathSync.native(ex.roots[0]),
      realpathSync.native(root),
      'the root was resolved through the link',
    );
  } finally {
    removeTempDir(base);
  }
});

// ─── external import ────────────────────────────────────────────────────

test('AC-008: copyFromPath is denied to a remote session even read-write', async () => {
  const { base, root, outside } = sandbox();
  let s;
  try {
    s = await serveRoot(root, 'read-write');
    // External import is the one mutation that can name an arbitrary path on
    // the serving machine, so it is denied by default at every access level
    // below admin — this is the check that keeps the export boundary from
    // being bypassed by a legitimate-looking write.
    await assert.rejects(
      s.remote.explorer.copyFromPath(join(outside, 'secret.txt'), s.rootId, 'stolen.txt'),
      (err) => {
        assert.equal(err.code, 'EACCES', `expected EACCES, got ${err.code}`);
        return true;
      },
    );

    const expanded = new Set([s.rootId]);
    const names = s.remote.explorer
      .getSnapshot()
      .visibleRows({ offset: 0, limit: 200, expanded })
      .map((r) => r.name);
    assert.equal(names.includes('stolen.txt'), false, 'nothing was imported');
  } finally {
    if (s) {
      await s.remote.close();
      await s.server.close();
    }
    removeTempDir(base);
  }
});

test('AC-008: a read-only export refuses every write path', async () => {
  const { base, root } = sandbox();
  let s;
  try {
    s = await serveRoot(root, 'read-only');
    const id = await waitFor(
      () => s.remote.explorer.resolvePath('inside.txt'),
      'inside.txt resolves',
    );

    await assert.rejects(
      s.remote.explorer.create(s.rootId, 'new.txt', 0),
      (e) => e.code === 'EROFS',
      'create',
    );
    await assert.rejects(
      s.remote.explorer.rename(id, 'renamed.txt'),
      (e) => e.code === 'EROFS',
      'rename',
    );
    await assert.rejects(s.remote.explorer.delete(id), (e) => e.code === 'EROFS', 'delete');
    await assert.rejects(
      s.remote.explorer.writeFile(id, new Uint8Array([1, 2, 3])),
      (e) => e.code === 'EROFS',
      'writeFile',
    );
    // Reads still work — read-only means read-only, not useless.
    const bytes = await s.remote.explorer.readFile(id);
    assert.equal(new TextDecoder().decode(bytes), 'fine\n');
  } finally {
    if (s) {
      await s.remote.close();
      await s.server.close();
    }
    removeTempDir(base);
  }
});

test('advertised maxFileBytes is enforced for reads and writes without killing the session', async () => {
  const { base, root } = sandbox();
  writeFileSync(join(root, 'too-big.txt'), '0123456789');
  let s;
  try {
    s = await serveRoot(root, 'read-write', 8);
    const largeId = await waitFor(
      () => s.remote.explorer.resolvePath('too-big.txt'),
      'large file resolves',
    );
    const smallId = await waitFor(
      () => s.remote.explorer.resolvePath('inside.txt'),
      'small file resolves',
    );

    await assert.rejects(s.remote.explorer.readFile(largeId), (error) => error.code === 'EFBIG');
    await assert.rejects(s.remote.explorer.readText(largeId), (error) => error.code === 'EFBIG');
    await assert.rejects(
      s.remote.explorer.writeFile(smallId, new TextEncoder().encode('123456789')),
      (error) => error.code === 'EFBIG',
    );
    assert.equal(readFileSync(join(root, 'inside.txt'), 'utf8'), 'fine\n', 'denied write changed nothing');

    await s.remote.explorer.writeFile(smallId, new TextEncoder().encode('ok'));
    assert.equal(readFileSync(join(root, 'inside.txt'), 'utf8'), 'ok');
    assert.equal(
      new TextDecoder().decode(await s.remote.explorer.readFile(smallId)),
      'ok',
      'the same session remains usable after EFBIG',
    );
  } finally {
    if (s) {
      await s.remote.close();
      await s.server.close();
    }
    removeTempDir(base);
  }
});
