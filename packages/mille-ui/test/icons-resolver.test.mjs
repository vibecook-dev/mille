// Phase 9.2 — icon resolver precedence tests.
//
// Exercises the full VS Code precedence order (fileNames →
// languageIds → fileExtensions with longest-match → default) plus
// folder resolution across the (expanded × rootLevel) matrix, plus
// the per-resolver memo behavior, plus resolution against the
// default theme.
//
// Also contains a smoke render of <FileIcon> against the default
// theme (Phase 9.6): sets up happy-dom globals, renders the
// component via React 19's renderToString, and asserts an <svg>
// tag is present in the output.

import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { test, before, after } from 'node:test';
import { strict as assert } from 'node:assert';
import { Window } from 'happy-dom';
import { createIconResolver } from '../dist/icons/resolver.js';
import { defaultIconTheme } from '../dist/icons/default-theme.js';

const KIND_FILE = 0;
const KIND_DIR = 1;

function fileEntry(name, languageId) {
  const e = { name, kind: KIND_FILE };
  if (languageId !== undefined) e.languageId = languageId;
  return e;
}

function dirEntry(name) {
  return { name, kind: KIND_DIR };
}

const theme = {
  id: 'test-theme',
  iconDefinitions: {
    _file: { iconPath: '/icons/file.svg' },
    _folder: { iconPath: '/icons/folder.svg' },
    _folder_open: { iconPath: '/icons/folder-open.svg' },
    _root: { iconPath: '/icons/root.svg' },
    _root_open: { iconPath: '/icons/root-open.svg' },
    _ts: { iconPath: '/icons/ts.svg' },
    _test_ts: { iconPath: '/icons/test-ts.svg' },
    _pkg: { iconPath: '/icons/package.svg' },
    _py_lang: { iconPath: '/icons/py-lang.svg' },
    _src_folder: { iconPath: '/icons/src-folder.svg' },
  },
  file: '_file',
  folder: '_folder',
  folderExpanded: '_folder_open',
  rootFolder: '_root',
  rootFolderExpanded: '_root_open',
  fileExtensions: { ts: '_ts', 'test.ts': '_test_ts' },
  fileNames: { 'package.json': '_pkg' },
  languageIds: { python: '_py_lang' },
  folderNames: { src: '_src_folder' },
};

test('resolver: fileNames wins over fileExtensions', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(fileEntry('package.json'));
  assert.equal(res.iconId, '_pkg');
});

test('resolver: fileNames lookup is case-insensitive (lowercases input)', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(fileEntry('PACKAGE.JSON'));
  assert.equal(res.iconId, '_pkg');
});

test('resolver: languageIds wins over fileExtensions when no fileName match', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(fileEntry('foo.ts', 'python'));
  assert.equal(res.iconId, '_py_lang');
});

test('resolver: fileExtensions picks the LONGEST compound match', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(fileEntry('foo.test.ts'));
  // test.ts must beat ts.
  assert.equal(res.iconId, '_test_ts');
});

test('resolver: fileExtensions falls back to shorter ext when longer is absent', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(fileEntry('foo.thing.ts'));
  assert.equal(res.iconId, '_ts');
});

test('resolver: default file icon when no match', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(fileEntry('NOEXT'));
  assert.equal(res.iconId, '_file');
});

test('resolver: folder default', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(dirEntry('anything'));
  assert.equal(res.iconId, '_folder');
});

test('resolver: folderExpanded when expanded', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(dirEntry('anything'), { expanded: true });
  assert.equal(res.iconId, '_folder_open');
});

test('resolver: rootFolder when rootLevel (not expanded)', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(dirEntry('anything'), { rootLevel: true });
  assert.equal(res.iconId, '_root');
});

test('resolver: rootFolderExpanded when expanded+rootLevel', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(dirEntry('anything'), { expanded: true, rootLevel: true });
  assert.equal(res.iconId, '_root_open');
});

test('resolver: folderNames wins over folder defaults', () => {
  const r = createIconResolver(theme, 'light');
  const res = r(dirEntry('src'));
  assert.equal(res.iconId, '_src_folder');
});

test('resolver: returns assetUrl and fontColor on ResolvedIcon', () => {
  const themed = {
    id: 't',
    iconDefinitions: { _file: { iconPath: '/f.svg', fontColor: '#0f0' } },
    file: '_file',
  };
  const r = createIconResolver(themed, 'light');
  const res = r(fileEntry('x'));
  assert.equal(res.assetUrl, '/f.svg');
  assert.equal(res.fontColor, '#0f0');
});

test('resolver: returns inlineSvg when the definition uses inlineSvg', () => {
  const themed = {
    id: 't',
    iconDefinitions: { _file: { inlineSvg: '<svg />' } },
    file: '_file',
  };
  const r = createIconResolver(themed, 'light');
  const res = r(fileEntry('x'));
  assert.equal(res.inlineSvg, '<svg />');
});

test('resolver: falls back to root defaults when variant slice omits a field', () => {
  const split = {
    id: 's',
    iconDefinitions: { _file: { iconPath: '/root.svg' } },
    file: '_file',
    dark: { iconDefinitions: {} },
  };
  const r = createIconResolver(split, 'dark');
  const res = r(fileEntry('x'));
  assert.equal(res.iconId, '_file');
});

test('resolver: dark variant iconDefinitions override root for same id', () => {
  const split = {
    id: 's',
    iconDefinitions: { _file: { iconPath: '/light.svg' } },
    file: '_file',
    dark: {
      iconDefinitions: { _file: { iconPath: '/dark.svg' } },
    },
  };
  const light = createIconResolver(split, 'light');
  const dark = createIconResolver(split, 'dark');
  assert.equal(light(fileEntry('x')).assetUrl, '/light.svg');
  assert.equal(dark(fileEntry('x')).assetUrl, '/dark.svg');
});

test('resolver: memoizes identical (name, kind, languageId) triples', () => {
  const r = createIconResolver(theme, 'light');
  const a = r(fileEntry('foo.test.ts'));
  const b = r(fileEntry('foo.test.ts'));
  assert.strictEqual(a, b);
});

test('resolver: default theme resolves common files correctly', () => {
  const r = createIconResolver(defaultIconTheme, 'light');
  assert.equal(r(fileEntry('app.ts')).iconId, '_ts');
  assert.equal(r(fileEntry('app.tsx')).iconId, '_ts');
  assert.equal(r(fileEntry('app.js')).iconId, '_js');
  assert.equal(r(fileEntry('data.json')).iconId, '_json');
  assert.equal(r(fileEntry('README.md')).iconId, '_readme');
  assert.equal(r(fileEntry('Cargo.lock')).iconId, '_lock');
  assert.equal(r(fileEntry('.env')).iconId, '_env');
  assert.equal(r(fileEntry('.gitignore')).iconId, '_git');
  assert.equal(r(fileEntry('photo.png')).iconId, '_image');
  assert.equal(r(fileEntry('unknown')).iconId, '_file');
  assert.equal(r(dirEntry('src')).iconId, '_folder');
  assert.equal(r(dirEntry('src'), { expanded: true }).iconId, '_folder_open');
});

test('resolver: returns null when theme has no default file and no match', () => {
  const bare = { id: 'bare', iconDefinitions: {} };
  const r = createIconResolver(bare, 'light');
  assert.equal(r(fileEntry('x')), null);
});

// ─── Phase 9.6 — FileIcon smoke render ────────────────────────────

let win;
before(() => {
  win = new Window();
  globalThis.window = win;
  globalThis.document = win.document;
  globalThis.HTMLElement = win.HTMLElement;
});

after(async () => {
  delete globalThis.window;
  delete globalThis.document;
  delete globalThis.HTMLElement;
  await win.happyDOM.close();
});

test('FileIcon: renders an inline <svg> for a file using default theme', async () => {
  const { FileIcon } = await import('../dist/icons/FileIcon.js');
  const { renderToString } = await import('react-dom/server');
  const { createElement } = await import('react');
  const entry = { name: 'foo.ts', kind: 0 };
  const html = renderToString(createElement(FileIcon, { entry }));
  assert.match(html, /<svg/, `expected rendered output to contain <svg>: ${html}`);
  // inline SVG is injected via dangerouslySetInnerHTML within a <span>
  assert.match(html, /<span/);
});

test('FileIcon: renders a folder icon for a directory entry', async () => {
  const { FileIcon } = await import('../dist/icons/FileIcon.js');
  const { renderToString } = await import('react-dom/server');
  const { createElement } = await import('react');
  const entry = { name: 'src', kind: 1 };
  const html = renderToString(createElement(FileIcon, { entry }));
  assert.match(html, /<svg/);
});

test('FileIcon: respects explicit appearance=dark prop', async () => {
  const { FileIcon } = await import('../dist/icons/FileIcon.js');
  const { renderToString } = await import('react-dom/server');
  const { createElement } = await import('react');
  const splitTheme = {
    id: 'split',
    iconDefinitions: { _f: { inlineSvg: '<svg data-variant="root"/>' } },
    file: '_f',
    dark: { iconDefinitions: { _f: { inlineSvg: '<svg data-variant="dark"/>' } } },
  };
  const html = renderToString(
    createElement(FileIcon, {
      entry: { name: 'x', kind: 0 },
      theme: splitTheme,
      appearance: 'dark',
    }),
  );
  assert.match(html, /data-variant="dark"/);
});

test('FileIcon: returns null when the theme cannot resolve and has no default', async () => {
  const { FileIcon } = await import('../dist/icons/FileIcon.js');
  const { renderToString } = await import('react-dom/server');
  const { createElement } = await import('react');
  const bare = { id: 'bare', iconDefinitions: {} };
  const html = renderToString(
    createElement(FileIcon, { entry: { name: 'x', kind: 0 }, theme: bare }),
  );
  assert.equal(html, '');
});


test('the published icon preview matches every exported theme and its resolved icons', () => {
  const generator = fileURLToPath(new URL('../scripts/build-icons-preview.mjs', import.meta.url));
  execFileSync(process.execPath, [generator, '--check']);
});
