// Access composition (SPEC §16.1).
//
// Effective access is the minimum of everything that has an opinion: what
// the export is configured for, what the client asked for, whether the peer
// is on the allow-list, and what the application's callback says.
//
// One rule is not a minimum and is worth stating separately: a client that
// asks for read-write on a read-only export is **rejected**, not quietly
// downgraded. Silently handing back a read-only session would leave the UI
// offering edits that fail later, and would hide a genuine misconfiguration
// from whoever set the export up.

import type { ExplorerSessionPolicy } from '@vibecook/mille';

import type { AuthorizeMillePeerContext, ResolvedExport, RemoteAccess } from './types.js';

export type AuthorizeOutcome =
  | { readonly ok: true; readonly access: RemoteAccess }
  | { readonly ok: false; readonly reason: string };

export interface AuthorizeInput {
  readonly export: ResolvedExport;
  /** Verified identity from the accepted socket, or null when absent. */
  readonly peerId: string | null;
  readonly peerName?: string | undefined;
  readonly requestedAccess: RemoteAccess;
  readonly authorize?:
    | ((context: AuthorizeMillePeerContext) => boolean | Promise<boolean>)
    | undefined;
}

export async function authorizePeer(input: AuthorizeInput): Promise<AuthorizeOutcome> {
  const { export: ex, peerId, requestedAccess } = input;

  // Truffle documents `remotePeerId` as nullable and says never to gate on
  // it — meaning an absent id must not be treated as permission. Fail closed.
  if (peerId === null || peerId.length === 0) {
    return { ok: false, reason: 'peer identity unavailable on the accepted socket' };
  }

  if (ex.allowedPeerIds !== undefined && !ex.allowedPeerIds.includes(peerId)) {
    return { ok: false, reason: `peer ${peerId} is not on the export allow-list` };
  }

  if (requestedAccess === 'read-write' && ex.access === 'read-only') {
    return { ok: false, reason: 'read-write requested on a read-only export' };
  }

  if (input.authorize !== undefined) {
    let verdict: boolean;
    try {
      verdict = await input.authorize({
        peerId,
        peerName: input.peerName,
        exportId: ex.id,
        requestedAccess,
        configuredAccess: ex.access,
      });
    } catch (err) {
      // §17.3 — an exception is a denial, never an accidental allow.
      return { ok: false, reason: `authorize callback threw: ${(err as Error).message}` };
    }
    if (!verdict) return { ok: false, reason: 'authorize callback denied the peer' };
  }

  // Minimum of configured and requested.
  const access: RemoteAccess =
    ex.access === 'read-only' || requestedAccess === 'read-only' ? 'read-only' : 'read-write';
  return { ok: true, access };
}

/**
 * The mille session policy an accepted remote peer runs under.
 *
 * Every `allow*` flag is left unset, which mille's PR 3 tables read as
 * denied: undo, projection settings, workspace roots, workspace resync,
 * external import and client decorations all touch host-global state shared
 * with every other session (§16.2, §16.4). Phase 1 does not open them, and
 * an export cannot opt in — that would need per-session engine state that
 * does not exist yet.
 */
export function sessionPolicyFor(
  access: RemoteAccess,
  maxFileBytes?: number,
): ExplorerSessionPolicy {
  return maxFileBytes === undefined ? { access } : { access, maxFileBytes };
}
