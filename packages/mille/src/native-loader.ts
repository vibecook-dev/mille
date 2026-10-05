// Native binary discovery, separated from `native.ts` so it can be asked
// without throwing.
//
// `native.ts` throws at import time when no binary loads — the right call for
// the FileExplorer entry points, which cannot do anything without one. The git
// entry (`@vibecook/mille/git`) must not: `isNativeGitAvailable()` is the
// question "can this process use the native git engine?", and SPEC mille-git
// §17.5 requires it to answer `false` rather than throw so a host can fall
// back to the shell client. Both read the same lookup from here.

import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

function isMusl(): boolean {
  // glibc-linked Node populates `glibcVersionRuntime` in the process
  // report header; musl-linked Node leaves it undefined. This is the
  // same heuristic @napi-rs/cli's loader uses.
  try {
    const report = (
      process as unknown as {
        report?: { getReport(): { header?: { glibcVersionRuntime?: string } } };
      }
    ).report?.getReport();
    return !report?.header?.glibcVersionRuntime;
  } catch {
    return false;
  }
}

export type NativeLoadSource = 'local' | 'platform-package';

export interface LoadedNative {
  readonly module: unknown;
  readonly source: NativeLoadSource;
  readonly resolvedPath: string;
}

function loadLocal(): LoadedNative | null {
  // Dev build path: the `.node` is emitted next to api.d.ts (one dir up
  // from dist/). napi-rs names Linux/Windows binaries with an ABI suffix
  // (`-gnu` / `-musl` / `-msvc`); Darwin is just `platform-arch`.
  const base = `mille.${process.platform}-${process.arch}`;
  const names = [
    `${base}.node`,
    `${base}-gnu.node`,
    `${base}-musl.node`,
    `${base}-msvc.node`,
  ];
  const roots = [join(__dirname, '..'), join(__dirname, '..', '..')];
  for (const root of roots) {
    for (const name of names) {
      const c = join(root, name);
      if (existsSync(c)) {
        return { module: require(c), source: 'local', resolvedPath: c };
      }
    }
  }
  return null;
}

function loadFromPlatformPackage(): LoadedNative | null {
  // Order matters only for linux: musl must be probed before gnu
  // because musl binaries can't dlopen into glibc Node and vice versa.
  const triples: Record<string, Record<string, string[]>> = {
    darwin: { arm64: ['darwin-arm64'], x64: ['darwin-x64'] },
    win32: { x64: ['win32-x64-msvc'], arm64: ['win32-arm64-msvc'] },
    linux: {
      x64: isMusl() ? ['linux-x64-musl'] : ['linux-x64-gnu'],
      arm64: isMusl() ? ['linux-arm64-musl'] : ['linux-arm64-gnu'],
    },
  };
  const attempts = triples[process.platform]?.[process.arch] ?? [];
  for (const triple of attempts) {
    try {
      const request = `@vibecook/mille-${triple}`;
      const resolvedPath = require.resolve(request);
      return {
        module: require(resolvedPath),
        source: 'platform-package',
        resolvedPath,
      };
    } catch {
      // Each per-triple package is an optionalDependency, so missing
      // modules are expected on mismatched hosts. Fall through.
    }
  }
  return null;
}

let memo: { readonly value: LoadedNative | null } | null = null;

/**
 * The native binary this package would load, or `null` when none is found.
 * Memoized. Still throws when a binary is found but fails to load (an ABI or
 * architecture mismatch) — callers that must not throw catch that.
 */
export function tryLoadNative(): LoadedNative | null {
  if (memo === null) {
    memo = { value: loadLocal() ?? loadFromPlatformPackage() };
  }
  return memo.value;
}
