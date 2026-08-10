// Internal barrel for the icon subsystem. The public entry is
// `src/icons.ts` (which re-exports from here). Consumers should never
// import `./icons/*` directly; go through `@vibecook/mille-ui/icons`.

export { defaultIconTheme } from './default-theme.js';
export { duotoneIconTheme } from './duotone-theme.js';
export { minimalIconTheme } from './minimal-theme.js';
export { vibefieldIconTheme } from './vibefield-theme.js';
export { FileIcon, type FileIconProps } from './FileIcon.js';
export { loadIconTheme, type LoadIconThemeOptions } from './load.js';
export { createIconResolver } from './resolver.js';
export { sanitizeSvg, SvgSanitizationError } from './sanitize.js';
export { validateIconTheme, IconThemeValidationError } from './schema.js';
export type {
  IconDefinition,
  IconResolver,
  IconResolverOptions,
  IconTheme,
  IconThemeTokens,
  ResolvedIcon,
} from './types.js';
