// FileTree expand / chevron click / loading-badge integration.
//
//   A. Click the chevron → fx.setExpanded called with { add: [id] }.
//   B. After emitDelta with the children visible, DOM reflects growth.
//   C. A row with id in `pendingExpansions` renders a LoadingBadge;
//      once the fake clears `pendingExpansions`, the badge is gone.
//   D. When a command registry is attached to the provider, chevron
//      click dispatches `tree.expand` (and later `tree.collapse`).

import { test } from 'node:test';
import { strict as assert } from 'node:assert';

import { Window } from 'happy-dom';

const hdWindow = new Window({ url: 'http://localhost/' });
const hdDocument = hdWindow.document;

function installGlobal(name, value) {
  Object.defineProperty(globalThis, name, {
    value,
    configurable: true,
    writable: true,
    enumerable: false,
  });
}

installGlobal('window', hdWindow);
installGlobal('document', hdDocument);
installGlobal('navigator', hdWindow.navigator);
installGlobal('HTMLElement', hdWindow.HTMLElement);
installGlobal('Node', hdWindow.Node);
installGlobal('Element', hdWindow.Element);
installGlobal('MessageChannel', hdWindow.MessageChannel);
installGlobal('IS_REACT_ACT_ENVIRONMENT', true);
let reducedMotion = false;
const reducedMotionListeners = new Set();
hdWindow.matchMedia = (query) => ({
  matches: query === '(prefers-reduced-motion: reduce)' && reducedMotion,
  media: query,
  onchange: null,
  addEventListener: (_type, listener) => reducedMotionListeners.add(listener),
  removeEventListener: (_type, listener) => reducedMotionListeners.delete(listener),
  addListener: (listener) => reducedMotionListeners.add(listener),
  removeListener: (listener) => reducedMotionListeners.delete(listener),
  dispatchEvent: () => true,
});
function setReducedMotion(next) {
  reducedMotion = next;
  for (const listener of reducedMotionListeners) {
    listener({ matches: next, media: '(prefers-reduced-motion: reduce)' });
  }
}
if (typeof globalThis.ResizeObserver === 'undefined') {
  class ResizeObserverMock {
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  installGlobal('ResizeObserver', ResizeObserverMock);
}

const { createElement, act } = await import('react');
const { createRoot } = await import('react-dom/client');

const { FileTree, FileTreeProvider } = await import('../dist/index.js');
const { createCommandRegistry } = await import('../dist/commands.js');
const { createFakeEngine, createFakeSnapshot } = await import('../dist/testing.js');

// ─── Helpers ──────────────────────────────────────────────────────────

function makeRow(p) {
  return {
    id: p.id,
    parentId: p.parentId,
    name: p.name,
    kind: p.kind ?? 1,
    size: 0,
    mtimeMs: 0,
    ctimeMs: 0,
    isIgnored: false,
    isReadonly: false,
    isHidden: false,
    depth: p.depth,
    hasChildren: p.hasChildren ?? false,
    isExpanded: p.isExpanded ?? false,
  };
}

function makeObservers({ height = 400, width = 600 } = {}) {
  const offsetRef = { current: 0 };
  const offsetListeners = new Set();
  const observeElementRect = (_instance, cb) => {
    cb({ width, height });
    return () => {};
  };
  const observeElementOffset = (_instance, cb) => {
    offsetListeners.add(cb);
    cb(offsetRef.current, false);
    return () => offsetListeners.delete(cb);
  };
  return { observeElementRect, observeElementOffset };
}

function mount() {
  const container = hdDocument.createElement('div');
  hdDocument.body.appendChild(container);
  return { container, root: createRoot(container) };
}

function dispatchClick(el) {
  const evt = new hdWindow.MouseEvent('click', { bubbles: true, cancelable: true });
  el.dispatchEvent(evt);
}

// ─── Test A: chevron click → fx.setExpanded({ add }) ──────────────────

test('clicking a chevron calls fx.setExpanded with the row id', async () => {
  const fx = createFakeEngine();
  const rows = [
    makeRow({ id: 1, parentId: null, name: 'root', depth: 0, hasChildren: true, isExpanded: true }),
    makeRow({ id: 2, parentId: 1, name: 'folder', depth: 1, hasChildren: true, isExpanded: false }),
  ];
  fx.emitDelta(createFakeSnapshot({ rows, treeVersion: 1 }));

  const { container, root } = mount();
  const obs = makeObservers();

  await act(async () => {
    root.render(
      createElement(FileTree, {
        fx,
        ariaLabel: 'Expand',
        rowHeight: 22,
        overscan: 5,
        __testObserveElementRect: obs.observeElementRect,
        __testObserveElementOffset: obs.observeElementOffset,
      }),
    );
  });

  fx.calls.setExpanded.length = 0;

  const row = container.querySelectorAll('[role="treeitem"]')[1];
  assert.ok(row);
  const chevron = row.querySelector('button[data-mille-chevron]');
  assert.ok(chevron, 'chevron button missing');

  await act(async () => {
    dispatchClick(chevron);
  });

  assert.ok(
    fx.calls.setExpanded.length >= 1,
    `expected setExpanded recorded; got ${fx.calls.setExpanded.length}`,
  );
  const firstCall = fx.calls.setExpanded[0];
  assert.deepEqual(Array.from(firstCall.add), [2]);

  await act(async () => { root.unmount(); });
  container.remove();
});

// ─── Test B: emitDelta adding children → DOM grows ────────────────────

test('root expands by default and renders children when the engine publishes them', async () => {
  const fx = createFakeEngine();
  const initial = [
    makeRow({ id: 1, parentId: null, name: 'root', depth: 0, hasChildren: true, isExpanded: false }),
  ];
  fx.emitDelta(createFakeSnapshot({ rows: initial, treeVersion: 1 }));

  const { container, root } = mount();
  const obs = makeObservers();

  await act(async () => {
    root.render(
      createElement(FileTree, {
        fx,
        ariaLabel: 'Expand',
        rowHeight: 22,
        overscan: 5,
        __testObserveElementRect: obs.observeElementRect,
        __testObserveElementOffset: obs.observeElementOffset,
      }),
    );
  });

  assert.equal(container.querySelectorAll('[role="treeitem"]').length, 1);

  const initialViewport = fx.calls.setViewport.at(-1);
  assert.ok(initialViewport, 'the initial hydration viewport must be published');
  assert.equal(initialViewport.offset, 0);
  assert.ok(
    initialViewport.limit >= Math.ceil(400 / 22),
    `expected a screen-sized hydration window; got ${initialViewport.limit}`,
  );

  assert.ok(
    fx.calls.setExpanded.some((call) => Array.from(call.add).includes(1)),
    'expected the workspace root to be expanded automatically',
  );

  // Engine publishes the expanded snapshot with children.
  const expanded = [
    makeRow({ id: 1, parentId: null, name: 'root', depth: 0, hasChildren: true, isExpanded: true }),
    makeRow({ id: 2, parentId: 1, name: 'child-a', depth: 1, kind: 0 }),
    makeRow({ id: 3, parentId: 1, name: 'child-b', depth: 1, kind: 0 }),
  ];
  await act(async () => {
    fx.emitDelta(createFakeSnapshot({ rows: expanded, treeVersion: 2 }));
  });

  assert.equal(container.querySelectorAll('[role="treeitem"]').length, 3);
  assert.equal(
    container.querySelector('[role="tree"]')?.getAttribute('data-mille-layout-animating'),
    'true',
  );
  assert.equal(
    container.querySelector('[data-mille-row-id="2"]')?.getAttribute('data-mille-entering'),
    'true',
    'new filesystem rows should receive the enter-animation marker',
  );

  // A second filesystem update inside the 150 ms transaction cancels the
  // stale animation instead of restarting motion across the viewport.
  const burst = [
    ...expanded,
    makeRow({ id: 4, parentId: 1, name: 'child-c', depth: 1, kind: 0 }),
  ];
  await act(async () => {
    fx.emitDelta(createFakeSnapshot({ rows: burst, treeVersion: 3 }));
  });
  assert.equal(
    container.querySelector('[role="tree"]')?.getAttribute('data-mille-layout-animating'),
    null,
  );
  assert.equal(
    container.querySelector('[role="tree"]')?.getAttribute('data-mille-animation-suppressed'),
    'in-flight',
  );
  assert.equal(
    container.querySelectorAll('[data-mille-entering], [data-mille-repositioning]').length,
    0,
    'a burst update must clear stale row animation markers',
  );

  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 190));
  });
  assert.equal(
    container.querySelector('[role="tree"]')?.getAttribute('data-mille-layout-animating'),
    null,
    'layout transitions must switch off before ordinary scrolling resumes',
  );
  assert.equal(
    container.querySelector('[data-mille-row-id="2"]')?.getAttribute('data-mille-entering'),
    null,
  );

  await act(async () => { root.unmount(); });
  container.remove();
});

test('reduced motion emits no structural animation markers', async () => {
  setReducedMotion(true);
  const fx = createFakeEngine();
  const initial = [
    makeRow({ id: 20, parentId: null, name: 'root', depth: 0, hasChildren: true }),
  ];
  fx.emitDelta(createFakeSnapshot({ rows: initial, treeVersion: 1 }));
  const { container, root } = mount();
  const obs = makeObservers();

  try {
    await act(async () => {
      root.render(
        createElement(FileTree, {
          fx,
          ariaLabel: 'Reduced motion',
          rowHeight: 22,
          overscan: 5,
          __testObserveElementRect: obs.observeElementRect,
          __testObserveElementOffset: obs.observeElementOffset,
        }),
      );
    });
    await act(async () => {
      fx.emitDelta(
        createFakeSnapshot({
          rows: [
            makeRow({
              id: 20,
              parentId: null,
              name: 'root',
              depth: 0,
              hasChildren: true,
              isExpanded: true,
            }),
            makeRow({ id: 21, parentId: 20, name: 'child', depth: 1, kind: 0 }),
          ],
          treeVersion: 2,
        }),
      );
    });

    const tree = container.querySelector('[role="tree"]');
    assert.equal(tree?.getAttribute('data-mille-layout-animating'), null);
    assert.equal(tree?.getAttribute('data-mille-animation-suppressed'), 'reduced-motion');
    assert.equal(
      container.querySelectorAll('[data-mille-entering], [data-mille-repositioning]').length,
      0,
    );
  } finally {
    await act(async () => root.unmount());
    container.remove();
    setReducedMotion(false);
  }
});

test('a root arriving asynchronously expands once and stays collapsed after user action', async () => {
  const fx = createFakeEngine();
  const { container, root } = mount();
  const obs = makeObservers();

  await act(async () => {
    root.render(
      createElement(FileTree, {
        fx,
        ariaLabel: 'Async root',
        rowHeight: 22,
        overscan: 5,
        __testObserveElementRect: obs.observeElementRect,
        __testObserveElementOffset: obs.observeElementOffset,
      }),
    );
  });
  assert.equal(fx.calls.setExpanded.length, 0);

  const rows = [
    makeRow({ id: 7, parentId: null, name: 'root', depth: 0, hasChildren: true, isExpanded: false }),
  ];
  await act(async () => {
    fx.emitDelta(createFakeSnapshot({ rows, treeVersion: 1 }));
  });
  assert.ok(
    fx.calls.setExpanded.some((call) => Array.from(call.add).includes(7)),
    'expected an asynchronously discovered root to expand',
  );

  fx.calls.setExpanded.length = 0;
  const chevron = container.querySelector('button[data-mille-chevron]');
  assert.ok(chevron);
  await act(async () => { dispatchClick(chevron); });
  assert.deepEqual(fx.calls.setExpanded, [{ add: [], remove: [7] }]);

  fx.calls.setExpanded.length = 0;
  await act(async () => {
    fx.emitDelta(createFakeSnapshot({ rows, treeVersion: 2 }));
  });
  assert.equal(
    fx.calls.setExpanded.length,
    0,
    'a later snapshot must not reopen a root the user collapsed',
  );

  await act(async () => { root.unmount(); });
  container.remove();
});

// ─── Test C: pendingExpansions → LoadingBadge appears and clears ──────

test('LoadingBadge appears for rows in pendingExpansions and clears on delta', async () => {
  const fx = createFakeEngine();
  const rows = [
    makeRow({ id: 1, parentId: null, name: 'root', depth: 0, hasChildren: true, isExpanded: true }),
  ];
  fx.emitDelta(createFakeSnapshot({
    rows,
    treeVersion: 1,
    pendingExpansions: new Set([1]),
  }));

  const { container, root } = mount();
  const obs = makeObservers();

  await act(async () => {
    root.render(
      createElement(FileTree, {
        fx,
        ariaLabel: 'Loading',
        rowHeight: 22,
        overscan: 5,
        __testObserveElementRect: obs.observeElementRect,
        __testObserveElementOffset: obs.observeElementOffset,
      }),
    );
  });

  const row = container.querySelector('[role="treeitem"]');
  assert.ok(row);
  assert.ok(
    row.querySelector('[data-mille-loading-badge]'),
    'expected LoadingBadge when row is in pendingExpansions',
  );

  // Clear pending expansions.
  await act(async () => {
    fx.emitDelta(createFakeSnapshot({
      rows,
      treeVersion: 2,
      pendingExpansions: new Set(),
    }));
  });

  const rowAfter = container.querySelector('[role="treeitem"]');
  assert.ok(rowAfter);
  assert.equal(
    rowAfter.querySelector('[data-mille-loading-badge]'),
    null,
    'expected LoadingBadge cleared after delta',
  );

  await act(async () => { root.unmount(); });
  container.remove();
});

// ─── Test D: chevron click dispatches `tree.expand` via command registry

test('chevron click dispatches tree.expand through the command registry', async () => {
  const fx = createFakeEngine();
  const rows = [
    makeRow({ id: 42, parentId: null, name: 'root', depth: 0, hasChildren: true, isExpanded: false }),
  ];
  fx.emitDelta(createFakeSnapshot({ rows, treeVersion: 1 }));

  const dispatched = [];
  // Minimal registry: just record dispatches. The default commands from
  // defaults.ts call through fx.* — for this test we only care that the
  // UI dispatched at all.
  const commands = createCommandRegistry([
    {
      id: 'tree.expand',
      label: 'Expand',
      run: (_ctx, args) => { dispatched.push({ id: 'tree.expand', args }); },
    },
    {
      id: 'tree.collapse',
      label: 'Collapse',
      run: (_ctx, args) => { dispatched.push({ id: 'tree.collapse', args }); },
    },
  ]);
  commands.setContextProvider(() => ({
    fx,
    snapshot: fx.getSnapshot(),
    focusedId: null,
    focusedEntry: null,
    selectedIds: new Set(),
    selectedEntries: [],
    isMultiSelect: false,
    isRenaming: false,
    host: {},
  }));

  const { container, root } = mount();
  const obs = makeObservers();

  await act(async () => {
    root.render(
      createElement(
        FileTreeProvider,
        { fx, commands },
        createElement(FileTree, {
          ariaLabel: 'CmdExpand',
          rowHeight: 22,
          overscan: 5,
          __testObserveElementRect: obs.observeElementRect,
          __testObserveElementOffset: obs.observeElementOffset,
        }),
      ),
    );
  });

  const chevron = container.querySelector('button[data-mille-chevron]');
  assert.ok(chevron);
  await act(async () => { dispatchClick(chevron); });

  assert.equal(dispatched.length, 1);
  assert.equal(dispatched[0].id, 'tree.collapse');
  assert.deepEqual(dispatched[0].args, { id: 42 });

  // The second click expands the root again.
  await act(async () => { dispatchClick(chevron); });
  assert.equal(dispatched.length, 2);
  assert.equal(dispatched[1].id, 'tree.expand');
  assert.deepEqual(dispatched[1].args, { id: 42 });

  await act(async () => { root.unmount(); });
  container.remove();
});
