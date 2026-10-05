// Generated-style per-platform loader. The umbrella package's
// optionalDependencies hold the per-triple binaries; we try each
// in turn based on host os + arch + libc.
//
// Convention is modelled on @napi-rs/cli's generated loader. We differ
// in two spots:
//   1. A dev-build fallback: if a `.node` sits next to the package root
//      (monorepo layout, `napi build --output-dir .`), prefer it over
//      the installed per-triple package. This keeps `pnpm run build`
//      → `pnpm test` working without publishing platform packages.
//   2. The musl check relies on `process.report.header.glibcVersionRuntime`
//      being absent; alpine containers set it to an empty string which
//      the absence-check coerces correctly.
//
// The shape of the returned binding is defined by fx-binding's NAPI
// module — `FileExplorer`, `FileReadStream`, `version`. Phase 6's
// higher-level wrappers (commits 6.4+) layer typed surfaces on top.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { tryLoadNative } from './native-loader.js';

export type { NativeLoadSource } from './native-loader.js';

const __dirname = dirname(fileURLToPath(import.meta.url));

const loadedNative = tryLoadNative();
if (!loadedNative) {
  throw new Error(
    `@vibecook/mille: failed to load native binary for ` +
      `${process.platform}-${process.arch}. Install the matching ` +
      `@vibecook/mille-* optional dependency or run a local dev build.`,
  );
}

// Raw napi-rs shape. Phase 6's higher-level wrapper (commits 6.4+)
// re-exports these with typed signatures and FX-reason decoding.
// The `any` for classes is deliberate — their full shape is declared
// on the TS-level wrapper and we don't want the loader to lock in a
// type that drifts from the generated d.ts.
export interface NativeBinding {
  version(): string;
  buildInfo?(): {
    readonly crateVersion: string;
    readonly profile: string;
    readonly target: string;
    readonly panicStrategy?: string;
    /** Optional cargo features compiled in, e.g. `['git']`. Absent before 0.3.5. */
    readonly features?: readonly string[];
  };
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  FileExplorer: any;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  FileReadStream: any;
}

function readPackageVersion(): string {
  try {
    const packageJson = JSON.parse(
      readFileSync(join(__dirname, '..', 'package.json'), 'utf8'),
    ) as { version?: unknown };
    return typeof packageJson.version === 'string' ? packageJson.version : 'unknown';
  } catch {
    return 'unknown';
  }
}

export const native = loadedNative.module as NativeBinding;

/** Loader-side identity, combined with the native payload by `buildIdentity()`. */
export const nativeLoadInfo = Object.freeze({
  source: loadedNative.source,
  resolvedPath: loadedNative.resolvedPath,
  packageVersion: readPackageVersion(),
});
