// MessagePort vs loopback WebSocket — the same host, the same client, two
// transports.
//
// The question this answers (VibeField FS-D2): does carrying mille's channel
// over a WebSocket, instead of a MessagePort, cost enough to matter? Each run
// attaches one fresh session per transport to one walked host and times four
// phases from the client's side:
//
//   attach  dial → explorer ready (WebSocket: TCP + upgrade + mille.remote
//           open with a credential + the explorer handshake)
//   expand  open the root folder → every child id known, first 50 rows hydrated
//   page    a 200-row viewport deep in the folder → all 200 rows hydrated
//   bulk    a viewport over the whole folder → every entry record hydrated
//           (the 10,000-entry snapshot: the largest single frame mille sends)
//
// Runs interleave the transports and alternate which goes first, and the
// 1-minute load average is recorded beside each, because this runs on shared
// machines: the per-phase ratio is the finding, not the absolute times.
// The MessagePort here is an in-process MessageChannel — the cheapest a
// MessagePort ever is — so the ratio is an upper bound on what the WebSocket
// costs relative to it.
//
// Hard gates: every phase completes on both transports with identical rows.
// Timings are reporters unless MILLE_TRANSPORT_WS_RATIO_BUDGET is set, which
// then gates every phase's median ratio.

import { strict as assert } from 'node:assert';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { loadavg, tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { MessageChannel } from 'node:worker_threads';

import { WebSocketServer, WebSocket as WsClient } from 'ws';

import { removeTempDir } from '../../../scripts/test-temp.mjs';
import { createFileExplorerHost } from '../dist/host.js';
import { connectFileExplorer, connectFileExplorerChannel } from '../dist/client-port.js';
import { createMessagePortHostChannel } from '../dist/channel/message-port.js';
import { serveWebSocketSession } from '../dist/node.js';
import { connectWebSocketChannel } from '../dist/remote-client.js';

const ENTRIES = Number(process.env.MILLE_TRANSPORT_ENTRIES ?? 10_000);
const PAGE = Number(process.env.MILLE_TRANSPORT_PAGE ?? 200);
const RUNS = Number(process.env.MILLE_TRANSPORT_RUNS ?? 7);
// Halfway down the folder, but never so deep the page runs past its end.
const PAGE_OFFSET = Math.max(0, Math.min(Math.floor(ENTRIES / 2), ENTRIES + 1 - PAGE));
assert.ok(PAGE <= ENTRIES + 1, `a ${PAGE}-row page needs at least ${PAGE - 1} entries`);
const PHASE_TIMEOUT_MS = Number(process.env.MILLE_TRANSPORT_PHASE_TIMEOUT_MS ?? 30_000);
// `ws` (default) or `global`: Node's own WHATWG WebSocket, where it has one.
const WS_CLIENT = process.env.MILLE_TRANSPORT_WS_CLIENT ?? 'ws';
const ClientSocket = WS_CLIENT === 'global' ? globalThis.WebSocket : WsClient;
if (typeof ClientSocket !== 'function') {
  throw new Error(`MILLE_TRANSPORT_WS_CLIENT=${WS_CLIENT}: no such WebSocket client here`);
}
const RATIO_BUDGET =
  process.env.MILLE_TRANSPORT_WS_RATIO_BUDGET === undefined
    ? undefined
    : Number(process.env.MILLE_TRANSPORT_WS_RATIO_BUDGET);
const reportIndex = process.argv.indexOf('--report');
const reportPath = reportIndex === -1 ? undefined : process.argv[reportIndex + 1];

const TICKET = 'bench-ticket';
const PHASES = ['attach', 'expand', 'page', 'bulk'];
const POLICY = { access: 'read-only', allowSearch: true };

// ─── fixture ────────────────────────────────────────────────────────────

const sandbox = mkdtempSync(join(tmpdir(), 'mille-transport-bench-'));
const root = join(sandbox, 'workspace');
mkdirSync(root);
for (let i = 0; i < ENTRIES; i += 1) {
  writeFileSync(join(root, `file-${String(i).padStart(6, '0')}.txt`), '');
}

const host = await createFileExplorerHost({ roots: [root], watchDebounceMs: 60_000 });
await host.local.populateFromRoots();

const wss = new WebSocketServer({
  host: '127.0.0.1',
  port: 0,
  maxPayload: 32 * 1024 * 1024,
  perMessageDeflate: false,
});
await once(wss, 'listening');
wss.on('connection', (socket) =>
  serveWebSocketSession(socket, {
    authorize: ({ credential }) => (credential === TICKET ? { host, policy: POLICY } : null),
  }),
);
const url = `ws://127.0.0.1:${wss.address().port}`;

// ─── transports ─────────────────────────────────────────────────────────

const clientOptions = { mirrorCap: ENTRIES * 2 + 64 };

/** Attach over an in-process MessageChannel, under the same remote policy. */
async function attachMessagePort() {
  const { port1, port2 } = new MessageChannel();
  host.attachChannel(createMessagePortHostChannel(port1), { kind: 'remote', policy: POLICY });
  const fx = await connectFileExplorer(port2, clientOptions);
  return {
    fx,
    bytes: () => null,
    close: async () => {
      await fx.dispose();
      port2.close();
    },
  };
}

/** Dial the loopback server, open with the ticket, run the same client. */
async function attachWebSocket() {
  const raw =
    ClientSocket === WsClient
      ? new WsClient(url, { perMessageDeflate: false })
      : new ClientSocket(url);
  let received = 0;
  raw.addEventListener('message', (event) => {
    received += event.data.byteLength;
  });
  const conn = await connectWebSocketChannel(raw, { exportId: 'bench', credential: TICKET });
  const fx = await connectFileExplorerChannel(conn.channel, clientOptions);
  return {
    fx,
    bytes: () => {
      const value = received;
      received = 0;
      return value;
    },
    close: () => fx.dispose(),
  };
}

const TRANSPORTS = { mp: attachMessagePort, ws: attachWebSocket };

// ─── one measured session ───────────────────────────────────────────────

/** Resolve once `predicate` holds, re-checking on every published snapshot. */
function until(fx, predicate, label) {
  if (predicate()) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      sub.dispose();
      reject(new Error(`${label} did not complete within ${PHASE_TIMEOUT_MS}ms`));
    }, PHASE_TIMEOUT_MS);
    const sub = fx.on('change', () => {
      if (!predicate()) return;
      clearTimeout(timer);
      sub.dispose();
      resolve();
    });
  });
}

function hydrated(fx, expanded, offset, limit) {
  const rows = fx.getSnapshot().visibleRows({ offset, limit, expanded });
  return rows.length === limit && rows.every((row) => row.pending !== true);
}

async function measureSession(kind) {
  const times = {};
  const bytes = {};
  let started = performance.now();
  const session = await TRANSPORTS[kind]();
  const { fx } = session;
  await until(fx, () => fx.getSnapshot().roots().length > 0, 'roots');
  times.attach = performance.now() - started;
  bytes.attach = session.bytes();

  const rootId = fx.getSnapshot().roots()[0].id;
  const expanded = new Set([rootId]);
  const total = ENTRIES + 1;

  started = performance.now();
  fx.setViewport({ offset: 0, limit: 50 });
  await fx.setExpanded({ add: [rootId], remove: [] });
  await until(
    fx,
    // The whole child-id list has arrived (entries ship only for the viewport).
    () => fx.getSnapshot().directChildCount(rootId) === ENTRIES && hydrated(fx, expanded, 0, 50),
    'expand',
  );
  times.expand = performance.now() - started;
  bytes.expand = session.bytes();

  started = performance.now();
  fx.setViewport({ offset: PAGE_OFFSET, limit: PAGE });
  await until(fx, () => hydrated(fx, expanded, PAGE_OFFSET, PAGE), 'page');
  times.page = performance.now() - started;
  bytes.page = session.bytes();
  const pageNames = fx
    .getSnapshot()
    .visibleRows({ offset: PAGE_OFFSET, limit: PAGE, expanded })
    .map((row) => row.name);

  started = performance.now();
  fx.setViewport({ offset: 0, limit: total });
  await until(fx, () => hydrated(fx, expanded, 0, total), 'bulk');
  times.bulk = performance.now() - started;
  bytes.bulk = session.bytes();
  const bulkCount = fx.getSnapshot().visibleRows({ offset: 0, limit: total, expanded }).length;

  await session.close();
  return { times, bytes, pageNames, bulkCount };
}

// ─── run ────────────────────────────────────────────────────────────────

function stats(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  const median = sorted.length % 2 === 1 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
  return { median, min: sorted[0], max: sorted[sorted.length - 1] };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 50));
const fmt = (ms) => `${ms.toFixed(2).padStart(8)} ms`;

const samples = { mp: [], ws: [] };
const runs = [];
let referencePage;
try {
  // One discarded warm-up per transport: JIT, the ws server's first upgrade.
  await measureSession('mp');
  await measureSession('ws');
  await settle();

  for (let run = 0; run < RUNS; run += 1) {
    const order = run % 2 === 0 ? ['mp', 'ws'] : ['ws', 'mp'];
    const record = { run: run + 1, order: order.join('→') };
    for (const kind of order) {
      const load = loadavg()[0];
      const result = await measureSession(kind);
      assert.equal(result.bulkCount, ENTRIES + 1, `${kind}: bulk viewport hydrated every row`);
      referencePage ??= result.pageNames;
      assert.deepEqual(result.pageNames, referencePage, `${kind}: the page holds the same rows`);
      samples[kind].push(result);
      record[kind] = { load1: Number(load.toFixed(2)), ...result.times };
      if (kind === 'ws') record.wsBytes = result.bytes;
      await settle();
    }
    runs.push(record);
  }
} finally {
  for (const client of wss.clients) client.terminate();
  await new Promise((resolve) => wss.close(() => resolve()));
  await host.dispose();
  removeTempDir(sandbox);
}

const summary = {};
for (const phase of PHASES) {
  const mp = stats(samples.mp.map((s) => s.times[phase]));
  const ws = stats(samples.ws.map((s) => s.times[phase]));
  summary[phase] = { mp, ws, ratio: ws.median / mp.median };
}
const wsBytes = {};
for (const phase of PHASES) wsBytes[phase] = stats(samples.ws.map((s) => s.bytes[phase])).median;

console.log(
  `transport-compare: ${ENTRIES} entries, ${PAGE}-row page at offset ${PAGE_OFFSET}, ` +
    `${RUNS} interleaved runs (Node ${process.version}, ${WS_CLIENT} WebSocket client)`,
);
// Both transports pay the same host and client work, so the median
// difference isolates the transport better than the ratio does.
console.log('phase    transport    median          min          max    ws/mp    ws−mp');
for (const phase of PHASES) {
  const { mp, ws, ratio } = summary[phase];
  const delta = ws.median - mp.median;
  console.log(`${phase.padEnd(8)} MessagePort ${fmt(mp.median)} ${fmt(mp.min)} ${fmt(mp.max)}`);
  console.log(
    `${''.padEnd(8)} WebSocket   ${fmt(ws.median)} ${fmt(ws.min)} ${fmt(ws.max)}  ` +
      `${ratio.toFixed(2)}x ${delta >= 0 ? '+' : ''}${delta.toFixed(2)} ms`,
  );
}
console.log(
  `WebSocket payload bytes received (median): ${PHASES.map((p) => `${p} ${wsBytes[p]}`).join(', ')}`,
);
console.log('1-minute load beside each run (MessagePort / WebSocket):');
for (const r of runs) {
  console.log(`  run ${r.run} (${r.order}): ${r.mp.load1} / ${r.ws.load1}`);
}

if (reportPath !== undefined) {
  writeFileSync(
    reportPath,
    `${JSON.stringify({ entries: ENTRIES, page: PAGE, wsClient: WS_CLIENT, runs, summary, wsBytes }, null, 2)}\n`,
  );
}

if (RATIO_BUDGET !== undefined) {
  for (const phase of PHASES) {
    assert.ok(
      summary[phase].ratio <= RATIO_BUDGET,
      `${phase}: WebSocket median is ${summary[phase].ratio.toFixed(2)}x MessagePort ` +
        `(budget ${RATIO_BUDGET}x)`,
    );
  }
}
