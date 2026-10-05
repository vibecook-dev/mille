// Session permission tables and capability masking (SPEC §12.3, §12.4, §16.2).
//
// PR 1 shipped `ExplorerSessionPolicy` as a type and enforced nothing. This
// is the enforcement, kept as a pair of lookup tables rather than
// conditionals scattered through the dispatch switch — the point of §23.3
// is that the whole matrix is readable in one place, because "which of the
// forty entry points did we forget to gate" is not a question anyone should
// have to answer by grepping.
//
// The gating is host-side and authoritative. A Truffle server may pre-check
// for a nicer error, but a permissive client, a future transport, or a bug
// in either must not be able to reach past this.

import type { ErrorCode } from '../errors.js';
import type { ExplorerSessionPolicy, ResolvedSessionContext } from './types.js';

export type PolicyVerdict =
  | { readonly allowed: true }
  | { readonly allowed: false; readonly code: ErrorCode; readonly message: string };

const ALLOW: PolicyVerdict = { allowed: true };

/**
 * How an operation is gated.
 *
 * - `always` — available to every session; the export's root boundary is
 *   the only constraint.
 * - `write` — refused for read-only sessions with `EROFS`, which is the one
 *   denial the spec names a code for (§19.2).
 * - a policy flag — admin always passes; anyone else needs the flag set
 *   explicitly. These guard state that is *host-global* today (undo stack,
 *   projection settings, workspace roots), so one remote peer flipping them
 *   would change what every other session sees.
 */
type Gate = 'always' | 'write' | { readonly flag: FlagName };

type FlagName = Exclude<keyof ExplorerSessionPolicy, 'access'>;

/** SPEC §16.2, mutation half. */
const MUTATION_GATES: Readonly<Record<string, Gate>> = {
  create: 'write',
  rename: 'write',
  move: 'write',
  delete: 'write',
  copy: 'write',
  writeFile: 'write',
  readFile: 'always',
  readText: 'always',
  // External import can name any path on the serving machine, which would
  // walk straight out of the export boundary. Denied by default even for
  // read-write.
  copyFromPath: { flag: 'allowExternalImport' },
  undo: { flag: 'allowUndo' },
};

/** SPEC §16.2, call half. */
const CALL_GATES: Readonly<Record<string, Gate>> = {
  getTreeVersion: 'always',
  capabilities: 'always',
  resolvePath: 'always',
  findVisiblePrefix: 'always',
  probeDestination: 'always',
  // Ownership is enforced separately; the table only says "not forbidden
  // outright". See `authorizeCancel`.
  cancelOperation: 'always',
  // Rate-limited per session rather than denied. See `checkResyncRate`.
  resync: 'always',
  resyncWorkspace: { flag: 'allowWorkspaceResync' },
  canUndo: { flag: 'allowUndo' },
  peekUndo: { flag: 'allowUndo' },
  lastMutation: { flag: 'allowUndo' },
  updateProjectionSettings: { flag: 'allowProjectionMutation' },
  reorderRoots: { flag: 'allowWorkspaceRootMutation' },
  updateWorkspaceRoots: { flag: 'allowWorkspaceRootMutation' },
  refreshWorkspaceRoots: { flag: 'allowWorkspaceRootMutation' },
  // A full scan of the shared tree per call; bounded below, and opt-in for
  // remote sessions so an export can be browsable without being searchable.
  search: { flag: 'allowSearch' },
};

function evaluate(
  ctx: ResolvedSessionContext,
  name: string,
  gate: Gate | undefined,
): PolicyVerdict {
  // An unlisted name is not a policy question — the dispatch switch will
  // reject it as unknown. Failing closed here would turn "typo" into
  // "permission denied", which is a worse error message.
  if (gate === undefined) return ALLOW;
  if (gate === 'always') return ALLOW;

  const { access } = ctx.policy;
  if (access === 'admin') return ALLOW;

  if (gate === 'write') {
    if (access === 'read-only') {
      return {
        allowed: false,
        code: 'EROFS',
        message: `${name} is not permitted on a read-only session`,
      };
    }
    return ALLOW;
  }

  if (ctx.policy[gate.flag] === true) return ALLOW;
  return {
    allowed: false,
    code: 'EACCES',
    message: `${name} is not permitted for this session`,
  };
}

export function authorizeMutation(ctx: ResolvedSessionContext, op: string): PolicyVerdict {
  return evaluate(ctx, op, MUTATION_GATES[op]);
}

export function authorizeCall(ctx: ResolvedSessionContext, method: string): PolicyVerdict {
  return evaluate(ctx, method, CALL_GATES[method]);
}

/** Client-pushed decorations write into the shared store (SPEC §16.2). */
export function authorizeDecorations(ctx: ResolvedSessionContext): PolicyVerdict {
  return evaluate(ctx, 'decorations', { flag: 'allowClientDecorations' });
}

// ─── Capability masking (SPEC §12.4) ────────────────────────────────────

// Mirrors `Capability` in api.d.ts. Duplicated as plain numbers because
// that declaration is a `const enum` in a .d.ts and is not importable here.
const CAP_READ_WRITE = 1 << 0;
const CAP_READONLY = 1 << 2;
const CAP_TRASH = 1 << 3;
const CAP_ATOMIC_WRITE = 1 << 4;

/**
 * What a session is told it can do, which must match what the host will
 * actually let it do.
 *
 * A read-only session that saw `ReadWrite` would render enabled rename and
 * delete affordances and only discover the truth on `EROFS`. Masking is a
 * UI-honesty measure — the server still rejects the operation either way.
 */
export function effectiveCapabilities(ctx: ResolvedSessionContext, native: number): number {
  if (ctx.policy.access !== 'read-only') return native;
  return (native & ~(CAP_READ_WRITE | CAP_TRASH | CAP_ATOMIC_WRITE)) | CAP_READONLY;
}

// ─── Operation ownership (SPEC §16.3) ───────────────────────────────────

/** Max concurrent owned operations per session (SPEC §20.2). */
export const MAX_OWNED_OPERATIONS = 64;

/** Hits one channel `search` returns when the caller names no limit. */
export const DEFAULT_SEARCH_LIMIT = 100;

/** The most hits one channel `search` can return; larger limits are clamped. */
export const MAX_SEARCH_LIMIT = 1000;

/** Longest query a channel `search` accepts. A filename is at most 255 bytes. */
export const MAX_SEARCH_QUERY_LENGTH = 1024;

/** Entry resync budget per session (SPEC §20.2): 10 per minute. */
export const RESYNC_LIMIT = 10;
export const RESYNC_WINDOW_MS = 60_000;

/**
 * Cancel is the one call where the table is not enough: a session may cancel
 * its own operations, an admin may cancel anything, and everyone else gets
 * `EACCES` — including for an id that exists but belongs to someone else.
 * The message deliberately does not distinguish "unknown" from "not yours",
 * so an unprivileged peer cannot probe for live operation ids.
 */
export function authorizeCancel(
  ctx: ResolvedSessionContext,
  owned: ReadonlySet<string>,
  operationId: string,
): PolicyVerdict {
  if (ctx.policy.access === 'admin') return ALLOW;
  if (owned.has(operationId)) return ALLOW;
  return {
    allowed: false,
    code: 'EACCES',
    message: 'no such operation for this session',
  };
}
