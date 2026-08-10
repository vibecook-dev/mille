// VibeField icon theme + CSS theme.
//
// Two things can rot here without anyone noticing, so both are pinned:
//
//  1. The maps. Adding an extension is easy; pointing it at an icon id that
//     does not exist is just as easy, and the resolver's failure mode is a
//     silent fall back to `_file` — the tree still renders, it just quietly
//     stops distinguishing anything. `validateIconTheme` is the repo's own
//     validator and already rejects dangling ids, so it does that job here.
//
//  2. The resolution ORDER. The set leans on precedence in three places that
//     would each look fine in isolation: a lockfile has to beat its own json
//     or yaml extension, a dotfile has to resolve from its post-dot name, and
//     a compound extension like `.test.ts` has to fall through to `ts`
//     rather than landing on the default.
//
// The CSS half is read from `dist/` on purpose: that also proves
// `scripts/build-tokens.mjs` still copies the theme into the published tree.

import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { vibefieldIconTheme } from '../dist/icons-vibefield.js';
import { createIconResolver } from '../dist/icons/resolver.js';
import { validateIconTheme } from '../dist/icons/schema.js';

const KIND_FILE = 0;
const KIND_DIRECTORY = 1;

function resolveName(name, kind = KIND_FILE, opts) {
  const resolver = createIconResolver(vibefieldIconTheme, 'light');
  return resolver({ name, kind }, opts);
}

test('the theme passes the icon-theme schema validator', () => {
  const validated = validateIconTheme(vibefieldIconTheme);
  assert.equal(validated.id, 'vibefield');
  assert.equal(validated.file, '_file');
  assert.equal(validated.folder, '_folder');
  assert.equal(validated.folderExpanded, '_folder_open');
});

test('every mapped id exists as a definition', () => {
  const ids = new Set(Object.keys(vibefieldIconTheme.iconDefinitions));
  const mapped = [
    ...Object.values(vibefieldIconTheme.fileExtensions ?? {}),
    ...Object.values(vibefieldIconTheme.fileNames ?? {}),
    ...Object.values(vibefieldIconTheme.folderNames ?? {}),
  ];
  for (const id of mapped) {
    assert.ok(ids.has(id), `mapped id ${id} has no definition`);
  }
});

test('glyphs carry no color of their own', () => {
  for (const [id, def] of Object.entries(vibefieldIconTheme.iconDefinitions)) {
    assert.ok(def.inlineSvg?.startsWith('<svg'), `${id} is not an inline SVG`);
    assert.equal(def.fontColor, undefined, `${id} pins a font color`);
    assert.doesNotMatch(
      def.inlineSvg,
      /#[\da-f]{3,8}\b|rgb\(|hsl\(/i,
      `${id} hardcodes a color instead of inheriting currentColor`,
    );
    assert.match(def.inlineSvg, /stroke="currentColor"/);
  }
});

test('an exact file name outranks the extension it is written in', () => {
  // The whole point of the `_lock` glyph: these are yaml and json files, and
  // reading them as config would lose the one fact worth showing.
  assert.equal(resolveName('pnpm-lock.yaml').iconId, '_lock');
  assert.equal(resolveName('package-lock.json').iconId, '_lock');
  assert.equal(resolveName('Cargo.lock').iconId, '_lock');
  // Case-insensitive: the resolver lowercases before matching.
  assert.equal(resolveName('CARGO.LOCK').iconId, '_lock');
  // …while a plain one of each still reads as configuration.
  assert.equal(resolveName('tsconfig.json').iconId, '_config');
  assert.equal(resolveName('pnpm-workspace.yaml').iconId, '_config');
});

test('dotfiles resolve from their post-dot name', () => {
  assert.equal(resolveName('.gitignore').iconId, '_git');
  assert.equal(resolveName('.gitattributes').iconId, '_git');
  assert.equal(resolveName('.env').iconId, '_env');
  assert.equal(resolveName('.env.production').iconId, '_env');
});

test('a compound extension falls through to its last part', () => {
  // `test.ts` is not mapped, so this must reach `ts` rather than `_file`.
  assert.equal(resolveName('resolver.test.ts').iconId, '_code');
  assert.equal(resolveName('field-app.d.ts').iconId, '_code');
  assert.equal(resolveName('theme.module.css').iconId, '_style');
});

test('categories separate the kinds a reading rail cares about', () => {
  assert.equal(resolveName('FileTree.tsx').iconId, '_code');
  assert.equal(resolveName('main.rs').iconId, '_code');
  assert.equal(resolveName('tokens.css').iconId, '_style');
  assert.equal(resolveName('README.md').iconId, '_doc');
  assert.equal(resolveName('Cargo.toml').iconId, '_config');
  assert.equal(resolveName('icon.png').iconId, '_media');
  // Anything unrecognized is a page, not a guess.
  assert.equal(resolveName('note.vfplugin').iconId, '_file');
  assert.equal(resolveName('LICENSE-MIT').iconId, '_file');
});

test('directories take the folder pair, expanded or not', () => {
  assert.equal(resolveName('packages', KIND_DIRECTORY).iconId, '_folder');
  assert.equal(
    resolveName('packages', KIND_DIRECTORY, { expanded: true }).iconId,
    '_folder_open',
  );
  // No folderNames map, so a well-known directory is still just a folder —
  // deliberate, and this pins it so a future addition is a decision.
  assert.equal(resolveName('node_modules', KIND_DIRECTORY).iconId, '_folder');
});

test('the published CSS theme scopes itself and respects reduced motion', async () => {
  const css = await readFile(
    new URL('../dist/theme/vibefield.css', import.meta.url),
    'utf8',
  );

  assert.match(css, /\[data-mille-theme="vibefield"\]/);
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)/);
  // Dark has to reach the theme through either stamping convention.
  assert.match(css, /\.dark \[data-mille-theme="vibefield"\]/);
  assert.match(css, /\[data-theme="dark"\] \[data-mille-theme="vibefield"\]/);
  // The menu is portaled outside the wrapper, so its rules must NOT be
  // scoped to the theme attribute or they would never match.
  assert.match(css, /^\.mille-context-menu-content \{/m);
});

test('the CSS theme stands alone without a host stylesheet', async () => {
  const css = await readFile(
    new URL('../dist/theme/vibefield.css', import.meta.url),
    'utf8',
  );

  // Every `--vf-*` read is a two-way bridge: it must carry a fallback, or
  // the theme renders as unstyled rows for anyone outside VibeField.
  const reads = css.match(/var\(--vf-[a-z0-9-]+[^)]*\)/g) ?? [];
  assert.ok(reads.length > 0, 'expected the host-token bridge to be present');
  for (const read of reads) {
    assert.match(
      read,
      /var\(--vf-[a-z0-9-]+,/,
      `${read} has no fallback, so the theme would break standalone`,
    );
  }
});
