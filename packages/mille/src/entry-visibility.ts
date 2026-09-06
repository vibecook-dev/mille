import type { Entry } from './client.js';

/** Shared by physical directory listings and renderer row projection.
 * Matches mille-core's VisibilityPolicy; includeIgnored reveals everything,
 * including hidden files and OS/VCS noise, regardless of explorer settings. */
export function isVisibleEntry(
  entry: Pick<Entry, 'name' | 'isHidden' | 'isIgnored'>,
  includeIgnored: boolean,
  showHiddenFiles: boolean,
  showIgnoredFiles: boolean,
): boolean {
  if (includeIgnored) return true;
  const name = entry.name;
  if (name === '.DS_Store' || name === 'Thumbs.db' || name === 'desktop.ini' || name === '.git') {
    return false;
  }
  return (showHiddenFiles || !entry.isHidden) && (showIgnoredFiles || !entry.isIgnored);
}
