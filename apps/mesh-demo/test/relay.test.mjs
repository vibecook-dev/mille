import { EventEmitter } from 'node:events';
import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { relayExplorerChannel } from '../out/main/relay.mjs';

class FakeChannel {
  state = 'open';
  bufferedBytes = 0;
  sent = [];
  #messages = new Set();
  #closes = new Set();

  send(message) {
    this.sent.push(message);
  }
  drain() {
    return Promise.resolve();
  }
  onMessage(listener) {
    this.#messages.add(listener);
    return { dispose: () => this.#messages.delete(listener) };
  }
  onClose(listener) {
    this.#closes.add(listener);
    return { dispose: () => this.#closes.delete(listener) };
  }
  close(reason) {
    if (this.state === 'closed') return;
    this.state = 'closed';
    for (const listener of [...this.#closes]) listener({ code: 'LOCAL_CLOSE', reason });
  }
  dispose() {
    this.close('disposed');
  }
  receive(message) {
    for (const listener of [...this.#messages]) listener(message);
  }
}

class FakePort extends EventEmitter {
  posted = [];
  started = false;
  closed = false;
  postMessage(message) {
    this.posted.push(message);
  }
  start() {
    this.started = true;
  }
  close() {
    if (this.closed) return;
    this.closed = true;
    this.emit('close');
  }
}

test('relay forwards explorer frames in both directions and tears down cleanly', () => {
  const channel = new FakeChannel();
  const port = new FakePort();
  let portClosed = 0;
  const relay = relayExplorerChannel(channel, port, () => {
    portClosed += 1;
  });

  assert.equal(port.started, true);
  channel.receive({ type: 'snapshot', body: { treeVersion: 1 } });
  assert.deepEqual(port.posted, [{ type: 'snapshot', body: { treeVersion: 1 } }]);

  port.emit('message', { data: { type: 'hello', body: { version: 1 } } });
  assert.deepEqual(channel.sent, [{ type: 'hello', body: { version: 1 } }]);

  relay.dispose();
  assert.equal(port.closed, true);
  assert.equal(portClosed, 0, 'intentional relay disposal does not look like a drop');
  channel.receive({ type: 'delta' });
  assert.equal(port.posted.length, 1, 'no frames arrive after disposal');
});

test('renderer port closure is surfaced so the connection manager can recover', () => {
  const channel = new FakeChannel();
  const port = new FakePort();
  let portClosed = 0;
  relayExplorerChannel(channel, port, () => {
    portClosed += 1;
  });

  port.close();
  assert.equal(portClosed, 1);
});

test('remote channel closure closes the renderer port without a false drop callback', () => {
  const channel = new FakeChannel();
  const port = new FakePort();
  let portClosed = 0;
  relayExplorerChannel(channel, port, () => {
    portClosed += 1;
  });

  channel.close('peer left');
  assert.equal(port.closed, true);
  assert.equal(portClosed, 0);
});
