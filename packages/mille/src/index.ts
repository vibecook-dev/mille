// @vibecook/mille — entry point
//
// Phase 6 layers a typed client on top of the raw napi-rs binding.
// Commit 6.1 exposed the loader + smoke-test version(). Commits 6.4-6.7
// add FileSystemError, the FileExplorer wrapper, MirrorSnapshot, and
// the React adapter.

import { native, nativeLoadInfo, type NativeLoadSource } from './native.js';
import { PROTOCOL_VERSION } from './protocol.js';

export { native };
export { FileSystemError, isFileSystemError } from './errors.js';
export type { ErrorCode } from './errors.js';
export { decodeBulkRows } from './decode.js';
export type { VisibleRow as DecodedVisibleRow } from './decode.js';
export { FileExplorer, MirrorSnapshot } from './client.js';
export type {
  CollisionPolicy,
  DestinationProbe,
  DestinationProbeStatus,
  DirectoryLoadState,
  Entry,
  EntryId,
  EventName,
  ExplorerOptions,
  Decoration,
  Disposable,
  SearchHit,
  SearchOptions,
  ResyncOptions,
  ProgressiveResyncOptions,
  TransferOptions,
  TransferProgress,
  Uri,
  VisibleRow,
  VisibleRowCount,
  VisibleRowsOptions,
} from './client.js';
export type { UndoDescriptor, UndoKind, UndoResult } from './undo.js';
export { normalizeUndoDescriptor, normalizeUndoKind, normalizeUndoResult } from './undo.js';
export { DecorationStore } from './decorations.js';
export type { DecorationProvider } from './decorations.js';
export { createFileExplorerHost } from './host.js';
export type { FileExplorerHost, MessagePortLike } from './types.js';
export {
  connectFileExplorer,
  connectFileExplorerChannel,
  PortFileExplorer,
  PortMirrorSnapshot,
} from './client-port.js';
export type { ExplorerConnectionEvent } from './client-port.js';
export {
  createMessagePortClientChannel,
  createMessagePortHostChannel,
  isExplorerChannel,
} from './channel/message-port.js';
export { LOCAL_ADMIN_POLICY, resolveSessionContext } from './channel/types.js';
// Exported so a transport layer can pre-check for a better error message.
// Host-side enforcement remains authoritative regardless (SPEC §12.3).
export {
  MAX_OWNED_OPERATIONS,
  RESYNC_LIMIT,
  RESYNC_WINDOW_MS,
  authorizeCall,
  authorizeCancel,
  authorizeDecorations,
  authorizeMutation,
  effectiveCapabilities,
} from './channel/policy.js';
export type { PolicyVerdict } from './channel/policy.js';
export type {
  ExplorerChannel,
  ExplorerChannelCloseCode,
  ExplorerChannelCloseEvent,
  ExplorerChannelLogger,
  ExplorerChannelState,
  ExplorerClientChannel,
  ExplorerHostChannel,
  ExplorerSessionAccess,
  ExplorerSessionContext,
  ExplorerSessionPolicy,
  ResolvedSessionContext,
} from './channel/types.js';
export {
  EXPLORER_SETTINGS_VERSION,
  EXPLORER_SETTINGS_LIMITS,
  DEFAULT_EXPLORER_SETTINGS,
  parseExplorerSettings,
  serializeExplorerSettings,
  resolveExplorerSettings,
} from './explorer-settings.js';
export type {
  ExplorerSortBy,
  ExplorerProjectionSettings,
  ExplorerSettingsOverride,
  ExplorerWorkspaceSettings,
  ExplorerSettingsDocument,
  ResolvedExplorerSettings,
} from './explorer-settings.js';

/**
 * Returns the fx-binding version string (the napi-rs crate version).
 * Smoke-test hook: consumers that just want to confirm the binary
 * loaded can call this without constructing a FileExplorer.
 */
export function version(): string {
  return native.version();
}

export interface BuildIdentity {
  readonly packageVersion: string;
  readonly nativeVersion: string;
  readonly nativeProfile: string;
  readonly nativeTarget: string;
  /**
   * `'unwind'` or `'abort'` for the loaded binary. An `'abort'` build cannot
   * turn a native panic into a catchable error — the whole host process takes
   * SIGABRT instead. Embedders running their own crash triage should record
   * this; `'unknown'` means the binary predates the field.
   */
  readonly nativePanicStrategy: string;
  readonly protocolVersion: number;
  readonly platform: NodeJS.Platform;
  readonly arch: string;
  readonly source: NativeLoadSource;
  readonly resolvedPath: string;
}

let cachedBuildIdentity: BuildIdentity | null = null;

/**
 * Describe the exact TypeScript package and native artifact loaded by this
 * process. Benchmarks and bug reports should record this payload so a stale
 * local `.node` file cannot masquerade as the current source implementation.
 */
export function buildIdentity(): BuildIdentity {
  if (cachedBuildIdentity !== null) return cachedBuildIdentity;
  const nativeInfo = native.buildInfo?.() ?? {
    crateVersion: native.version(),
    profile: 'unknown',
    target: `${process.platform}-${process.arch}`,
    panicStrategy: 'unknown',
  };
  cachedBuildIdentity = Object.freeze({
    packageVersion: nativeLoadInfo.packageVersion,
    nativeVersion: nativeInfo.crateVersion,
    nativeProfile: nativeInfo.profile,
    nativeTarget: nativeInfo.target,
    nativePanicStrategy: nativeInfo.panicStrategy ?? 'unknown',
    protocolVersion: PROTOCOL_VERSION,
    platform: process.platform,
    arch: process.arch,
    source: nativeLoadInfo.source,
    resolvedPath: nativeLoadInfo.resolvedPath,
  });
  return cachedBuildIdentity;
}
