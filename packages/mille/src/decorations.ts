// DecorationStore holds provider-supplied decorations per EntryId.
// Bumps decorationVersion (separate from treeVersion) on every change.
// Consumers subscribing to 'change:decorations' observe these bumps
// without having to re-read entry data.
//
// Per SPEC §4.9.11, decorations have their own version channel so
// SCM status churn (a git checkout flipping 500 files) doesn't
// invalidate viewport caches keyed on treeVersion.

import type { EntryId } from './client.js';

export interface Decoration {
  /**
   * A bare status GLYPH for this row (`'M'`, `'A'`, `'D'`), rendered on the
   * row's type ramp in a fixed-width slot so a column of them aligns.
   * Distinct from `badge`, which is a padded pill for a COUNT — the UI
   * styles the two differently and screen readers read a letter as
   * "status M" and a numeric badge as "N problems".
   */
  readonly letter?: string;
  readonly badge?: string; // 1-2 char glyph
  readonly color?: string; // theme token
  readonly tooltip?: string;
  readonly propagate?: boolean; // bubble to ancestor folders
}

export interface DecorationProviderEntry {
  readonly id: EntryId;
}

export interface DecorationProvider {
  readonly id: string;
  onDidChange(listener: (ids: readonly EntryId[]) => void): { dispose(): void };
  provide(
    entry: DecorationProviderEntry,
  ): Decoration | null | Promise<Decoration | null>;
}

/**
 * Per-provider decoration storage keyed by EntryId. Reads return the
 * merged list across all providers for a given entry; writes target a
 * single provider slot.
 *
 * Version bumps are driven by the caller via `bump()` so a provider
 * can batch a large changeset into a single bump (e.g. git checkout
 * flipping N files produces a single decorationVersion increment).
 */
export class DecorationStore {
  // entryId -> providerId -> Decoration
  private readonly decorations = new Map<number, Map<string, Decoration>>();
  private _version = 0;
  private readonly changeListeners = new Set<
    (changedIds: readonly number[]) => void
  >();

  get version(): number {
    return this._version;
  }

  /** Merged decorations across all providers for `id`. Empty array when none. */
  getMerged(id: number): readonly Decoration[] {
    const perProvider = this.decorations.get(id);
    if (!perProvider || perProvider.size === 0) return [];
    return [...perProvider.values()];
  }

  /**
   * Insert or clear a decoration for a single provider slot. Returns
   * `true` when the stored value actually changed (useful for building
   * the `changedIds` list passed to `bump()`).
   */
  setForProvider(
    providerId: string,
    id: number,
    decoration: Decoration | null,
  ): boolean {
    const perProvider = this.decorations.get(id);
    const hadValue = perProvider?.has(providerId) ?? false;

    if (decoration === null) {
      if (!hadValue) return false;
      perProvider!.delete(providerId);
      if (perProvider!.size === 0) this.decorations.delete(id);
      return true;
    }

    if (!perProvider) {
      const next = new Map<string, Decoration>();
      next.set(providerId, decoration);
      this.decorations.set(id, next);
      return true;
    }
    // Replacing an existing entry still counts as a change (shape may differ).
    perProvider.set(providerId, decoration);
    return true;
  }

  /**
   * Drop every decoration contributed by `providerId`. Returns the
   * affected entry ids so callers can feed them into `bump()`.
   */
  removeProvider(providerId: string): number[] {
    const affected: number[] = [];
    for (const [id, perProvider] of this.decorations) {
      if (perProvider.delete(providerId)) {
        affected.push(id);
        if (perProvider.size === 0) this.decorations.delete(id);
      }
    }
    return affected;
  }

  /**
   * Bump the decoration version and notify listeners. A zero-length
   * changedIds list is a no-op so callers can unconditionally call
   * bump() after a batch without worrying about whether anything
   * actually changed.
   */
  bump(changedIds: readonly number[]): void {
    if (changedIds.length === 0) return;
    this._version++;
    for (const l of this.changeListeners) l(changedIds);
  }

  onChange(listener: (changedIds: readonly number[]) => void): {
    dispose(): void;
  } {
    this.changeListeners.add(listener);
    return {
      dispose: () => {
        this.changeListeners.delete(listener);
      },
    };
  }
}
