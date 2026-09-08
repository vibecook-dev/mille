// Test utilities entry point. Re-exports the scriptable fake engine
// used across the mille-ui test suite. See
// MILLE_UI_PLAN.md Phase 1.1 and MILLE_UI_SPEC.md §18.

export const VERSION = '0.3.4'; // x-release-please-version

export { createFakeEngine, createFakeSnapshot } from './testing/createFakeEngine.js';
export type {
  FakeEngine,
  FakeEngineCalls,
  FakeMirrorSnapshot,
  FakeSnapshotInit,
} from './testing/createFakeEngine.js';
