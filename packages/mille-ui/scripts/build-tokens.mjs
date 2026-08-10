// Generate the published `tokens.css` from the theme sources.
//
// Extracted from an inline `node -e` in package.json so the line-ending
// normalization below can carry an explanation.
//
// `tokens.css` is a generated file that is also committed. The generator
// concatenated its two CRLF-on-Windows sources with a literal '\n', so a
// Windows build produced a mixed-ending file that differed from the committed
// copy and left the tree dirty after every `pnpm build`. Normalizing here
// makes the artifact identical on every platform regardless of how the
// sources were checked out — `.gitattributes` pins the checkout, and this
// pins the build.

import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';

/** Read a file and normalize CRLF/CR to LF. */
function readLf(path) {
  return readFileSync(path, 'utf8').replace(/\r\n?/g, '\n');
}

const tokens = readLf('src/theme/tokens.css');
const focus = readLf('src/theme/focus.css');
const combined = `${tokens}\n${focus}`;

mkdirSync('dist', { recursive: true });
writeFileSync('dist/tokens.css', combined);
writeFileSync('tokens.css', combined);

// Opt-in CSS themes ship as-is. Unlike `tokens.css` above these are authored
// files rather than concatenations, so `.gitattributes`' working-tree LF is
// already the whole story and there is nothing to normalize.
mkdirSync('dist/theme', { recursive: true });
copyFileSync('src/theme/minimal.css', 'dist/theme/minimal.css');
copyFileSync('src/theme/vibefield.css', 'dist/theme/vibefield.css');
