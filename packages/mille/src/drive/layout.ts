// The drive's one public on-disk fact: where an object lives under its
// holder's objects root. mille-drive's `object_relpath` is the authority
// (crates/mille-drive/src/digest.rs, README "On disk"); this is its mirror,
// so a host composes a remote path as the holder's `objectsRoot()` joined with
// `objectRelpath(blake3)` and never re-derives the layout itself.

import { DriveError } from './error.js';
import type { Blake3Hex } from './types.js';

const BLAKE3_HEX = /^[0-9a-f]{64}$/;

/** Whether `value` is a blake3 digest as the drive spells one. */
export function isBlake3Hex(value: unknown): value is Blake3Hex {
  return typeof value === 'string' && BLAKE3_HEX.test(value);
}

/**
 * An object's path relative to its holder's `objectsRoot()`:
 * `<first 2 hex>/<64 hex>`, always `/`-separated (Windows takes `/` too, and
 * `path.join` normalises it). Throws `EINVAL` for anything but 64 lowercase
 * hex characters, so a host never asks a peer for a path the drive did not
 * name.
 */
export function objectRelpath(blake3: Blake3Hex): string {
  if (!isBlake3Hex(blake3)) {
    throw new DriveError('EINVAL', `not a blake3 digest (64 lowercase hex): ${String(blake3)}`);
  }
  return `${blake3.slice(0, 2)}/${blake3}`;
}
