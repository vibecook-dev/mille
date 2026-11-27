// Run after building mille-ui. The checked-in HTML keeps the Pages deployment
// static and lets the preview work offline without a bundler or CDN.
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { createIconResolver } from '@vibecook/mille-ui/icons';

const manifest = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));
const page = new URL('../../../docs/icons-preview.html', import.meta.url);
const labels = {
  default: 'Default monoline',
  duotone: 'Soft duotone',
  material: 'Material Icon Theme',
  minimal: 'Minimal',
  vibefield: 'VibeField',
};
const rows = [
  { name: 'src', kind: 1, expanded: true },
  { name: 'index.ts', kind: 0, nested: true },
  { name: 'styles.css', kind: 0, nested: true },
  { name: 'assets', kind: 1 },
  { name: 'package.json', kind: 0 },
  { name: 'README.md', kind: 0 },
  { name: 'yarn.lock', kind: 0 },
];

function sample(resolver, minimal) {
  return rows.map((row) => {
    const icon = resolver(row, { expanded: row.expanded ?? false });
    assert.ok(icon?.inlineSvg, `Missing SVG for ${row.name}`);
    const arrow = row.kind !== 1 ? '' : minimal ? (row.expanded ? '[-]' : '[+]') : (row.expanded ? '▾' : '▸');
    return `<li${row.nested ? ' class="nested"' : ''}><span class="arrow" aria-hidden="true">${arrow}</span>${minimal ? '' : `<span class="glyph" aria-hidden="true">${icon.inlineSvg}</span>`}${row.name}</li>`;
  }).join('\n');
}

const notices = await readFile(new URL('../NOTICES.md', import.meta.url), 'utf8');
const materialNotice = notices.split('## Material Icon Theme')[1].split('---')[0];
const materialLicense = materialNotice.slice(materialNotice.indexOf('Copyright')).replace(/^> ?/gm, '').trim();

const sections = [];
for (const key of Object.keys(manifest.exports).filter((key) => key.startsWith('./icons/'))) {
  const name = key.split('/').at(-1);
  const specifier = manifest.name + key.slice(1);
  const module = await import(specifier);
  const symbol = name === 'material' ? 'loadMaterialIconTheme' : name + 'IconTheme';
  const theme = name === 'material' ? await module[symbol]() : module[symbol];
  assert.ok(theme && labels[name], `Add a preview for ${specifier}`);
  const light = sample(createIconResolver(theme, 'light'), name === 'minimal');
  const dark = sample(createIconResolver(theme, 'dark'), name === 'minimal');
  const tree = light === dark ? `<ul>${light}</ul>` :
    `<ul class="only-light">${light}</ul><ul class="only-dark">${dark}</ul>`;
  const css = ['minimal', 'vibefield'].includes(name) ? `\nimport '@vibecook/mille-ui/theme/${name}.css';` : '';
  const load = name === 'material' ? '\nconst iconTheme = await loadMaterialIconTheme();' : '';
  sections.push(`<section id="${name}"><h2>${labels[name]}</h2>
${tree}
<pre><code>import { ${symbol} } from
  '${specifier}';${load}${css}</code></pre></section>`);
}
const html = await readFile(page, 'utf8');
const region = /<!-- generated icon previews -->[\s\S]*?<!-- end icon previews -->/;
assert.ok(region.test(html));
const result = html.replace(region, () => `<!-- generated icon previews -->\n${sections.join('\n')}\n<!-- Material Icon Theme\n${materialLicense}\n-->\n<!-- end icon previews -->`);
if (process.argv.includes('--check')) {
  assert.ok(result === html, 'Rebuild docs/icons-preview.html with node packages/mille-ui/scripts/build-icons-preview.mjs');
} else {
  await writeFile(page, result);
}
