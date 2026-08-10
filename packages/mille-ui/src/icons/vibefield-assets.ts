// Inline SVG sources for the VibeField icon set.
//
// One weight, one grid, no fills: every path is `fill="none"
// stroke="currentColor"`, so a glyph inherits the row's text color through
// every state and carries no color of its own. A file's *type* is not a
// status, and the moment icons carry hue they start competing with the
// decoration letters that do mean something.
//
// WHY THESE ARE DRAWN THE WAY THEY ARE
//
// The tree renders a glyph at `1em` of a 12px row — 12 device-independent
// pixels, 0.75 of a unit on this 16-unit grid. That size decides everything.
// A per-language badge tucked inside a page outline does not survive it: the
// outline reads and the badge turns to mush, so every file ends up looking
// like "generic page", only blurrier. So for an identified kind the mark IS
// the icon, with no page silhouette competing for the same twelve pixels, and
// the page shape is kept for the two things that really are pages — prose,
// and the file we could not identify.
//
// `stroke-width: 1.5` lands at ~1.1px on a 12px row, which is the thinnest
// stroke that still holds its shape there.
//
// Provenance: hand-drawn monoline glyphs, bespoke to this package and
// licensed under its MIT license.

const SVG_OPEN =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" ' +
  'stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">';
const SVG_CLOSE = '</svg>';

function wrap(inner: string): string {
  return `${SVG_OPEN}${inner}${SVG_CLOSE}`;
}

/** A solid dot. Small circles read as blobs when stroked at this size. */
function dot(cx: number, cy: number, r: number): string {
  return `<circle cx="${cx}" cy="${cy}" r="${r}" fill="currentColor" stroke="none"/>`;
}

// ── The page: body with a cut corner, plus the fold that makes the cut
//    read as paper rather than as a chipped rectangle ──────────────────
const PAGE_BODY =
  '<path d="M5.4 2.2h3.4l3 3v7.9a.9.9 0 0 1-.9.9H5.4a.9.9 0 0 1-.9-.9V3.1a.9.9 0 0 1 .9-.9Z"/>';
const PAGE_FOLD = '<path d="M8.8 2.2v2.1a.9.9 0 0 0 .9.9h2.1"/>';

// ── Generic file ────────────────────────────────────────────────────
export const VF_FILE_SVG = wrap(PAGE_BODY + PAGE_FOLD);

// ── Prose. Two rules, 2.8 units apart — the widest spacing the page
//    admits, and the tightest that still shows daylight at 12px ───────
export const VF_DOC_SVG = wrap(
  `${PAGE_BODY}${PAGE_FOLD}<path d="M6.5 8.8h3.9M6.5 11.6h3.9"/>`,
);

// ── Code: the chevron pair, full height, nothing between them. The
//    classic slash is dropped deliberately — at 12px it closes the gap
//    to the chevrons and the three marks merge into a smudge ──────────
export const VF_CODE_SVG = wrap('<path d="M6.6 5 3.4 8l3.2 3M9.4 5l3.2 3-3.2 3"/>');

// ── Styling: a droplet. Braces were drawn first and rejected on the
//    evidence — at 12px the pair closes into one squiggle and reads as
//    parentheses, and it competes with the chevrons besides. A
//    stylesheet is the file that decides how things look, so one closed
//    drop says it in a shape that survives the size ────────────────────
export const VF_STYLE_SVG = wrap(
  '<path d="M8 2.4c2.7 3.1 4.1 5.3 4.1 6.9a4.1 4.1 0 0 1-8.2 0c0-1.6 1.4-3.8 4.1-6.9Z"/>',
);

// ── Configuration: two rails with a setting on each. A gear at this
//    size is a grey circle; rails stay legible because they run the
//    full width of the grid ─────────────────────────────────────────────
export const VF_CONFIG_SVG = wrap(
  `<path d="M3.2 6h9.6M3.2 10h9.6"/>${dot(6.4, 6, 1.6)}${dot(9.6, 10, 1.6)}`,
);

// ── Media ───────────────────────────────────────────────────────────
export const VF_MEDIA_SVG = wrap(
  '<path d="M2.6 4.6a1.4 1.4 0 0 1 1.4-1.4h8a1.4 1.4 0 0 1 1.4 1.4v6.8a1.4 1.4 0 0 1-1.4 1.4H4a1.4 1.4 0 0 1-1.4-1.4Z"/>' +
    '<path d="m2.6 11.6 3.6-3.6 2.4 2.2 1.6-1.4 2.6 2.2"/>' +
    dot(10.1, 6.1, 1.15),
);

// ── Lockfile. The one file in the tree you must not hand-edit, which
//    is worth seeing before you open it, so it does not fold into
//    config despite being written in json or yaml ────────────────────
export const VF_LOCK_SVG = wrap(
  '<path d="M4.2 7.5h7.6a.9.9 0 0 1 .9.9v4.1a.9.9 0 0 1-.9.9H4.2a.9.9 0 0 1-.9-.9V8.4a.9.9 0 0 1 .9-.9Z"/>' +
    '<path d="M5.9 7.5V5.9a2.1 2.1 0 0 1 4.2 0v1.6"/>',
);

// ── Secrets. "This file holds credentials" is worth knowing before you
//    open it, let alone before you hand it to an agent.
//    One tooth, not two: at 12px a second notch lands within a stroke
//    width of the first and the bit reads as a thickened line ──────────
export const VF_ENV_SVG = wrap(
  '<circle cx="5.7" cy="10.3" r="2.7"/><path d="m7.6 8.4 5.2-5.2M10.7 5.3l1.6 1.6"/>',
);

// ── Git's own plumbing ──────────────────────────────────────────────
export const VF_GIT_SVG = wrap(
  '<path d="M5 5.6v4.9"/><path d="M11 7.9v.7a3.2 3.2 0 0 1-3.2 3.2H6.4"/>' +
    dot(5, 4, 1.6) +
    dot(5, 12, 1.6) +
    dot(11, 6.3, 1.6),
);

// ── Folders ─────────────────────────────────────────────────────────
export const VF_FOLDER_SVG = wrap(
  '<path d="M2.2 5.1a1.3 1.3 0 0 1 1.3-1.3h2.9l1.7 1.7h4.4a1.3 1.3 0 0 1 1.3 1.3v5.3a1.3 1.3 0 0 1-1.3 1.3H3.5a1.3 1.3 0 0 1-1.3-1.3Z"/>',
);

export const VF_FOLDER_OPEN_SVG = wrap(
  '<path d="M2.2 12V5.1a1.3 1.3 0 0 1 1.3-1.3h2.9l1.7 1.7h4.4a1.3 1.3 0 0 1 1.3 1.3v1"/>' +
    '<path d="M2.4 12.6 4 8.5a1.2 1.2 0 0 1 1.1-.7h8.6a1.2 1.2 0 0 1 1.1 1.6l-1.4 3.6a1.2 1.2 0 0 1-1.1.8H3.5a1.2 1.2 0 0 1-1.1-1.2Z"/>',
);
