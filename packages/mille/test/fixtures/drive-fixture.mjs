// Shared helpers for the drive tests (`drive*.test.mjs`).

import { mkdtempSync, realpathSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { isNativeDriveAvailable } from '../../dist/drive.js';

/** A fresh temp directory, canonical (macOS's /var is /private/var). */
export function tempDir(prefix = 'mille-drive-') {
  return realpathSync(mkdtempSync(join(tmpdir(), prefix)));
}

/** `node:test` options: skip unless the loaded binary carries the drive. */
export function needsDrive() {
  return isNativeDriveAvailable() ? {} : { skip: 'built without the drive feature' };
}

/** Write `text` to `dir/name` and put it in `drive`. Resolves to the outcome. */
export async function putText(drive, dir, name, text) {
  const path = join(dir, name);
  writeFileSync(path, text);
  return drive.putFile(path);
}

/**
 * Poll `predicate` until it is truthy. Polling, not a promise on a listener:
 * the drive's listeners are weak and do not keep the event loop alive, and a
 * timer does.
 */
export async function waitFor(
  predicate,
  { timeoutMs = 10_000, intervalMs = 20, what = 'condition' } = {},
) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await predicate();
    if (value) return value;
    if (Date.now() > deadline) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
}
