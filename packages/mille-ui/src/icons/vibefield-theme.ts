// The VibeField icon theme — a category set, paired with
// `@vibecook/mille-ui/theme/vibefield.css`.
//
// Eleven glyphs, and the small count is the design rather than a stub. The
// drawing rationale lives in `vibefield-assets.ts`; the mapping rationale is
// here: what the categories separate is what changes how you *treat* a row in
// a reading rail — code, styling, prose, configuration, media, a lockfile you
// must not hand-edit, a file holding credentials, git's own plumbing — never
// which language the code happens to be written in. At a 12px row the icon
// column has room for one honest distinction per row, and "is this mine to
// edit?" is a better use of it than "is this Go or Rust?", which the name
// already says.
//
// Per-language glyphs are one map away for anyone who disagrees: add
// definitions and point `fileExtensions` at them. Nothing else changes.
//
// NO NAMED FOLDERS, deliberately. `folderNames` is supported by the resolver
// and unused here, because a mark drawn inside a folder silhouette at 12px is
// the same mush the set exists to avoid — and the signal it would carry is
// usually available honestly elsewhere: a derived directory arrives ignored
// and wears the muted ramp, which says "not your code" more legibly than any
// glyph this size.

import type { IconTheme } from './types.js';
import {
  VF_CODE_SVG,
  VF_CONFIG_SVG,
  VF_DOC_SVG,
  VF_ENV_SVG,
  VF_FILE_SVG,
  VF_FOLDER_OPEN_SVG,
  VF_FOLDER_SVG,
  VF_GIT_SVG,
  VF_LOCK_SVG,
  VF_MEDIA_SVG,
  VF_STYLE_SVG,
} from './vibefield-assets.js';

export const vibefieldIconTheme: IconTheme = {
  id: 'vibefield',

  iconDefinitions: {
    _file: { inlineSvg: VF_FILE_SVG },
    _doc: { inlineSvg: VF_DOC_SVG },
    _code: { inlineSvg: VF_CODE_SVG },
    _style: { inlineSvg: VF_STYLE_SVG },
    _config: { inlineSvg: VF_CONFIG_SVG },
    _media: { inlineSvg: VF_MEDIA_SVG },
    _lock: { inlineSvg: VF_LOCK_SVG },
    _env: { inlineSvg: VF_ENV_SVG },
    _git: { inlineSvg: VF_GIT_SVG },
    _folder: { inlineSvg: VF_FOLDER_SVG },
    _folder_open: { inlineSvg: VF_FOLDER_OPEN_SVG },
  },

  file: '_file',
  folder: '_folder',
  folderExpanded: '_folder_open',

  fileExtensions: {
    // Code — one mark, every language. See the note above.
    ts: '_code',
    tsx: '_code',
    mts: '_code',
    cts: '_code',
    js: '_code',
    jsx: '_code',
    mjs: '_code',
    cjs: '_code',
    rs: '_code',
    swift: '_code',
    py: '_code',
    pyi: '_code',
    go: '_code',
    rb: '_code',
    java: '_code',
    kt: '_code',
    c: '_code',
    h: '_code',
    cpp: '_code',
    hpp: '_code',
    cs: '_code',
    php: '_code',
    lua: '_code',
    sh: '_code',
    bash: '_code',
    zsh: '_code',
    fish: '_code',
    sql: '_code',
    wgsl: '_code',
    glsl: '_code',
    vert: '_code',
    frag: '_code',
    wasm: '_code',
    html: '_code',
    htm: '_code',
    xml: '_code',
    vue: '_code',
    svelte: '_code',
    astro: '_code',

    css: '_style',
    scss: '_style',
    sass: '_style',
    less: '_style',
    pcss: '_style',

    md: '_doc',
    markdown: '_doc',
    mdx: '_doc',
    txt: '_doc',
    rst: '_doc',
    adoc: '_doc',
    pdf: '_doc',
    csv: '_doc',

    json: '_config',
    jsonc: '_config',
    json5: '_config',
    yaml: '_config',
    yml: '_config',
    toml: '_config',
    ini: '_config',
    cfg: '_config',
    conf: '_config',
    properties: '_config',
    plist: '_config',
    xcconfig: '_config',
    editorconfig: '_config',
    npmrc: '_config',
    nvmrc: '_config',
    prettierrc: '_config',

    png: '_media',
    jpg: '_media',
    jpeg: '_media',
    gif: '_media',
    webp: '_media',
    svg: '_media',
    ico: '_media',
    avif: '_media',
    bmp: '_media',
    icns: '_media',
    mp3: '_media',
    wav: '_media',
    flac: '_media',
    mp4: '_media',
    mov: '_media',
    webm: '_media',
    woff: '_media',
    woff2: '_media',
    ttf: '_media',
    otf: '_media',

    lock: '_lock',

    // The resolver yields a dotfile's post-dot name as a one-part extension,
    // so these match `.env` and `.gitignore` without a fileNames entry each.
    env: '_env',
    gitignore: '_git',
    gitattributes: '_git',
    gitmodules: '_git',
    gitkeep: '_git',
  },

  fileNames: {
    // Exact names outrank extensions, so a lockfile reads as a lockfile
    // rather than as the json or yaml it happens to be written in.
    'package-lock.json': '_lock',
    'pnpm-lock.yaml': '_lock',
    'yarn.lock': '_lock',
    'bun.lockb': '_lock',
    'cargo.lock': '_lock',
    'package.resolved': '_lock',

    dockerfile: '_config',
    makefile: '_config',
    justfile: '_config',
    procfile: '_config',
    '.dockerignore': '_config',

    license: '_doc',
    'license.md': '_doc',
    notice: '_doc',
    '.env': '_env',
    '.env.local': '_env',
    '.env.example': '_env',
    '.env.development': '_env',
    '.env.production': '_env',
  },
};
