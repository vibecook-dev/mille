// Type-level drift check between the two copies of SPEC mille-git Appendix A.
//
// The published contract is this package's `src/git/service-types.ts`. The
// engine package keeps a private structural copy (`@vibecook/mille/git`'s
// `types.ts`) because it may not import this one (§8.3). Nothing at runtime
// ties them; this file does, at `typecheck` time (`tsc -p test-types`): every
// shape must be mutually assignable, and the engine's factories must accept
// and return the contract's types. Never emitted, never shipped.

import type * as engine from '@vibecook/mille/git';

import type * as contract from '../src/git/service-types.js';

type Assert<T extends true> = T;
type Mutual<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;

export type Checks = [
  Assert<Mutual<engine.GitService, contract.GitService>>,
  Assert<Mutual<engine.GitTransport, contract.GitTransport>>,
  Assert<Mutual<engine.GitTransportEvents, contract.GitTransportEvents>>,
  Assert<Mutual<engine.GitIdentity, contract.GitIdentity>>,
  Assert<Mutual<engine.StatusSnapshot, contract.StatusSnapshot>>,
  Assert<Mutual<engine.StatusEntry, contract.StatusEntry>>,
  Assert<Mutual<engine.DiffRequest, contract.DiffRequest>>,
  Assert<Mutual<engine.DiffResult, contract.DiffResult>>,
  Assert<Mutual<engine.LogRequest, contract.LogRequest>>,
  Assert<Mutual<engine.LogPage, contract.LogPage>>,
  Assert<Mutual<engine.CommitFile, contract.CommitFile>>,
  Assert<Mutual<engine.CommitRequest, contract.CommitRequest>>,
  Assert<Mutual<engine.ReadObjectRequest, contract.ReadObjectRequest>>,
  Assert<Mutual<engine.GitWorktree, contract.GitWorktree>>,
  Assert<Mutual<engine.GitWarning, contract.GitWarning>>,
  Assert<Mutual<engine.GitEngineOptions, contract.GitEngineOptions>>,
  Assert<Mutual<engine.GitErrorCode, contract.GitErrorCode>>,
  Assert<Mutual<engine.GitError, contract.GitError>>,
  Assert<Mutual<typeof engine.GIT_PROTOCOL_VERSION, typeof contract.GIT_PROTOCOL_VERSION>>,
];

// The factories, as an ADE host calls them: a contract transport in, a
// contract service out.
export function hostWiring(
  createGitService: typeof engine.createGitService,
  transport: contract.GitTransport,
): contract.GitService {
  return createGitService(transport);
}
