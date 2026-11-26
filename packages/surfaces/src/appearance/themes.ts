/**
 * Built-in themes and how a theme becomes the concrete token values a window
 * applies: the theme's own values, then derivations for whatever it left
 * out, then the person's overrides, then the minimum contrast.
 */
import { editorThemes } from './editor-themes';
import { contrast, ensureContrast, normalizeColor, parseColor } from './color';
import { TOKENS, TOKEN_BY_NAME, type Scheme } from './tokens';
import type { Typography } from './preferences';

export type ThemeDefinition = {
  id: string;
  name: string;
  scheme: Scheme;
  /** Built-in theme whose explicit values this one starts from. */
  extends?: string;
  tokens: Record<string, string>;
  /** Text and spacing the theme suggests; the person's own settings win. */
  typography?: Partial<Typography>;
  /** Where it came from, for Settings: built in, or a user file. */
  source?: { kind: 'builtin' } | { kind: 'user'; file: string };
};
export type ResolvedTheme = { id: string; name: string; scheme: Scheme; tokens: Record<string, string> };

/** Dark: the look Lazybox has always had, every token written out. */
const darkTokens: Record<string, string> = {
  'bg': '#121317', 'surface-sunken': '#17181d', 'surface': '#1d1e24', 'surface-raised': '#202127', 'surface-hover': '#24252d', 'surface-active': '#272830', 'surface-selected': '#2a2b33', 'surface-strong': '#34353e',
  'line-subtle': '#2c2d35', 'line': '#33343e', 'line-strong': '#3a3b45', 'line-bold': '#43444f',
  'text-strong': '#ffffff', 'text': '#e4e4eb', 'text-soft': '#d8d8e2', 'text-secondary': '#c4c6d0', 'text-tertiary': '#b0b1bd', 'text-subtle': '#9fa0ad', 'text-muted': '#92939f', 'text-faint': '#7c7d89', 'text-disabled': '#676873',
  'accent': '#bdb1ed', 'accent-soft': '#c9bdf2', 'accent-strong': '#ddd5fb', 'accent-bg': '#2b2839', 'accent-bg-strong': '#3a3550', 'accent-line-subtle': '#3d3a52', 'accent-line': '#4a4565', 'accent-solid': '#6f56c9', 'accent-solid-hover': '#7b62d6', 'on-accent': '#282337', 'on-accent-solid': '#ffffff',
  'selection': '#50496a', 'focus': '#b9afe3',
  'ok': '#8fd0a9', 'ok-soft': '#a9d6b1', 'ok-strong': '#bfe3cd', 'ok-bg': '#1b2b21', 'ok-bg-strong': '#1f3a2c', 'ok-line-subtle': '#2f4a3b', 'ok-line': '#35503b', 'ok-solid': '#2f7a4f', 'ok-solid-hover': '#38895b', 'on-ok': '#10241a', 'on-ok-solid': '#ffffff',
  'warn': '#d6b46e', 'warn-soft': '#e3c98f', 'warn-strong': '#f0dcb2', 'warn-bg': '#2b261c', 'warn-bg-strong': '#3a3220', 'warn-line-subtle': '#4a3f26', 'warn-line': '#5a4d2c', 'warn-solid': '#8a6a24', 'warn-solid-hover': '#9a782c', 'on-warn': '#241c0b', 'on-warn-solid': '#ffffff',
  'danger': '#dc9494', 'danger-soft': '#e6a3a8', 'danger-strong': '#f3c2c2', 'danger-bg': '#362328', 'danger-bg-strong': '#4a2a30', 'danger-line-subtle': '#5c3036', 'danger-line': '#6b3a43', 'danger-solid': '#813f48', 'danger-solid-hover': '#934b55', 'on-danger': '#2a1215', 'on-danger-solid': '#ffffff',
  'info': '#8fb5e0', 'info-soft': '#9fc1e8', 'info-strong': '#c4dcf5', 'info-bg': '#1f2f3d', 'info-bg-strong': '#263a4d', 'info-line-subtle': '#2f3a4a', 'info-line': '#3c5470', 'info-solid': '#3f6fa8', 'info-solid-hover': '#4a7cb6', 'on-info': '#0f1d2b', 'on-info-solid': '#ffffff',
  'running': '#97bba2',
  'code-bg': '#17181e', 'code-text': '#d4d5de',
  'syntax-keyword': '#ddaaf3', 'syntax-string': '#a5d6ad', 'syntax-number': '#efc287', 'syntax-comment': '#9398aa', 'syntax-type': '#8dced8', 'syntax-attr': '#c4b5fd', 'syntax-function': '#9cc8ef',
  'diff-add-bg': '#18291f', 'diff-add-text': '#a8dbb3', 'diff-add-mark': '#8fd0a9', 'diff-word-add': '#2f6b47', 'diff-del-bg': '#2d1c20', 'diff-del-text': '#eab0b5', 'diff-del-mark': '#ec8c94', 'diff-word-del': '#7a3440',
  'ansi-0': '#3b3d48', 'ansi-1': '#f0747a', 'ansi-2': '#8ad48b', 'ansi-3': '#e9c46a', 'ansi-4': '#6fa8f5', 'ansi-5': '#c792ea', 'ansi-6': '#5fd3dd', 'ansi-7': '#d6d8e0',
  'ansi-8': '#7d8494', 'ansi-9': '#ff9aa0', 'ansi-10': '#a8eea8', 'ansi-11': '#ffe08a', 'ansi-12': '#93c1ff', 'ansi-13': '#e0b3ff', 'ansi-14': '#8ff0f5', 'ansi-15': '#ffffff',
  'chart-1': '#579cf4', 'chart-2': '#e99557', 'chart-3': '#46bba5', 'chart-4': '#d3ae4b', 'chart-5': '#ab8cde', 'chart-6': '#b4a6c5', 'chart-7': '#799fad', 'chart-muted': '#777984',
  'shadow': '#000000', 'shadow-strength': '1',
};

/** Light: the same structure for daylight, every token written out. */
const lightTokens: Record<string, string> = {
  'bg': '#e8e8ee', 'surface-sunken': '#f1f1f5', 'surface': '#fafafc', 'surface-raised': '#ffffff', 'surface-hover': '#f0f0f4', 'surface-active': '#e8e8ef', 'surface-selected': '#e6e4f3', 'surface-strong': '#d6d6df',
  'line-subtle': '#e6e6ec', 'line': '#d9d9e1', 'line-strong': '#c6c6d0', 'line-bold': '#adadb9',
  'text-strong': '#0b0b10', 'text': '#1c1c23', 'text-soft': '#2b2b34', 'text-secondary': '#3a3a45', 'text-tertiary': '#484854', 'text-subtle': '#55555f', 'text-muted': '#5f5f6b', 'text-faint': '#6e6e79', 'text-disabled': '#9c9ca7',
  'accent': '#6a55c2', 'accent-soft': '#5b47b3', 'accent-strong': '#3e2f8a', 'accent-bg': '#f0edfb', 'accent-bg-strong': '#e4def8', 'accent-line-subtle': '#d6cef3', 'accent-line': '#b8abeb', 'accent-solid': '#6a55c2', 'accent-solid-hover': '#5a46b0', 'on-accent': '#ffffff', 'on-accent-solid': '#ffffff',
  'selection': '#d9d1f6', 'focus': '#6a55c2',
  'ok': '#1d7646', 'ok-soft': '#19683d', 'ok-strong': '#0f4a2b', 'ok-bg': '#e8f6ee', 'ok-bg-strong': '#d3eedf', 'ok-line-subtle': '#bfe3cf', 'ok-line': '#8fcaa8', 'ok-solid': '#24814f', 'ok-solid-hover': '#1d7043', 'on-ok': '#ffffff', 'on-ok-solid': '#ffffff',
  'warn': '#895f00', 'warn-soft': '#7a5400', 'warn-strong': '#583c00', 'warn-bg': '#fbf3e0', 'warn-bg-strong': '#f5e6c0', 'warn-line-subtle': '#ecd9a8', 'warn-line': '#d6b468', 'warn-solid': '#94670a', 'warn-solid-hover': '#805806', 'on-warn': '#ffffff', 'on-warn-solid': '#ffffff',
  'danger': '#b32b3b', 'danger-soft': '#a02434', 'danger-strong': '#781626', 'danger-bg': '#fcebed', 'danger-bg-strong': '#f8d7dc', 'danger-line-subtle': '#f0c2c9', 'danger-line': '#df919c', 'danger-solid': '#b8323f', 'danger-solid-hover': '#a02a36', 'on-danger': '#ffffff', 'on-danger-solid': '#ffffff',
  'info': '#2a63a6', 'info-soft': '#22568f', 'info-strong': '#173d67', 'info-bg': '#e9f1fb', 'info-bg-strong': '#d5e4f6', 'info-line-subtle': '#c2d6ef', 'info-line': '#8db0dd', 'info-solid': '#2f6cb2', 'info-solid-hover': '#275e9d', 'on-info': '#ffffff', 'on-info-solid': '#ffffff',
  'running': '#2f7a50',
  'code-bg': '#f3f3f7', 'code-text': '#2b2b34',
  'syntax-keyword': '#a3249f', 'syntax-string': '#1f7a3a', 'syntax-number': '#9c5600', 'syntax-comment': '#6a6f80', 'syntax-type': '#0b7285', 'syntax-attr': '#6a3fc0', 'syntax-function': '#2a63a6',
  'diff-add-bg': '#e6f6ea', 'diff-add-text': '#185a2c', 'diff-add-mark': '#1d7646', 'diff-word-add': '#b5e2c3', 'diff-del-bg': '#fcebec', 'diff-del-text': '#871e2a', 'diff-del-mark': '#b32b3b', 'diff-word-del': '#f4c0c7',
  'ansi-0': '#3a3b45', 'ansi-1': '#c0303f', 'ansi-2': '#227f3a', 'ansi-3': '#8f6400', 'ansi-4': '#2c63c0', 'ansi-5': '#9437b3', 'ansi-6': '#0d7a86', 'ansi-7': '#5f616e',
  'ansi-8': '#6e7080', 'ansi-9': '#cc3747', 'ansi-10': '#138036', 'ansi-11': '#926802', 'ansi-12': '#346dcd', 'ansi-13': '#a445c2', 'ansi-14': '#037b87', 'ansi-15': '#1c1c23',
  'chart-1': '#2f74d0', 'chart-2': '#c86a22', 'chart-3': '#16907a', 'chart-4': '#a47f12', 'chart-5': '#7c58c4', 'chart-6': '#8a7aa3', 'chart-7': '#4f7a8a', 'chart-muted': '#a4a5b0',
  'shadow': '#16161f', 'shadow-strength': '0.35',
};

function palette(id: string, name: string, scheme: Scheme, bg: string, surface: string, text: string, accent: string): ThemeDefinition {
  return { id, name, scheme, tokens: { bg, surface, text, accent } };
}

// Read old settings and theme files without offering retired presets for new selections.
const retiredThemes = new Map<string, string>([
  ['midnight-blue', 'dracula'],
  ['pine', 'dracula'],
  ['ember', 'dracula'],
  ['aubergine', 'dracula'],
  ['graphite', 'dracula'],
  ['deep-ocean', 'dracula'],
  ['rosewood', 'dracula'],
  ['ink', 'dracula'],
  ['paper', 'github-light-default'],
  ['glacier', 'github-light-default'],
  ['sage', 'github-light-default'],
  ['rose-quartz', 'github-light-default'],
  ['lavender-mist', 'github-light-default'],
  ['sand', 'github-light-default'],
  ['sea-glass', 'github-light-default'],
]);
export const currentThemeId = (id: string): string => retiredThemes.get(id) ?? id;

export const BUILTIN_THEMES: ThemeDefinition[] = [
  { id: 'dark', name: 'Dark', scheme: 'dark', tokens: darkTokens },
  { id: 'light', name: 'Light', scheme: 'light', tokens: lightTokens },
  {
    id: 'dark-high-contrast', name: 'Dark high contrast', scheme: 'dark', tokens: {
      'bg': '#000000', 'surface': '#08080a', 'surface-raised': '#121216', 'line': '#5b5b68', 'text': '#ffffff', 'text-muted': '#c9c9d4',
      'accent': '#cbbfff', 'ok': '#7fe3a9', 'warn': '#ffd37a', 'danger': '#ff9d9d', 'info': '#9dc9ff', 'running': '#8ee0b0',
      'selection': '#4b3f86', 'code-bg': '#000000',
    },
  },
  {
    id: 'solarized-light', name: 'Solarized light', scheme: 'light', tokens: {
      'bg': '#e6dfc8', 'surface-sunken': '#eee8d5', 'surface': '#fdf6e3', 'surface-raised': '#fffbf0', 'line': '#ddd6c1', 'text': '#073642', 'text-muted': '#586e75',
      'accent': '#5b5fb8', 'ok': '#5c6f00', 'warn': '#8a6800', 'danger': '#c0302d', 'info': '#1e6fa8', 'running': '#1b7d75',
      'code-bg': '#f5efdc', 'syntax-keyword': '#5c6f00', 'syntax-string': '#1b7d75', 'syntax-number': '#b02a6d', 'syntax-comment': '#7a8b8c', 'syntax-type': '#8a6800', 'syntax-attr': '#1e6fa8', 'syntax-function': '#5b5fb8',
    },
  },
  ...editorThemes,
  palette('light-high-contrast', 'Light high contrast', 'light', '#e7e7e7', '#ffffff', '#101010', '#243488'),
];
export const BUILTIN_BY_ID = new Map(BUILTIN_THEMES.map(theme => [theme.id, theme]));
for (const theme of BUILTIN_THEMES) theme.source = { kind: 'builtin' };

/** What a person changed on top of a theme. */
export type ThemeOverrides = { accent?: string; tokens?: Record<string, string> };

/**
 * Every token's concrete value for `theme`: its own values (and those of the
 * built-in it extends), derivations for the rest, then overrides. A chosen
 * accent recolours the whole accent family, not just one token.
 */
export function resolveTheme(theme: ThemeDefinition, overrides: ThemeOverrides = {}, minContrast = 0): ResolvedTheme {
  const parent = theme.extends ? BUILTIN_BY_ID.get(currentThemeId(theme.extends)) : undefined;
  const explicit: Record<string, string> = { ...(parent && parent.scheme === theme.scheme ? parent.tokens : {}), ...theme.tokens };
  if (overrides.accent && parseColor(overrides.accent)) {
    // A new accent replaces every accent value the theme spelled out, so the family derives from it.
    for (const spec of TOKENS) if (spec.group === 'Accent' && spec.name !== 'accent') delete explicit[spec.name];
    explicit.accent = normalizeColor(overrides.accent);
  }
  for (const [name, value] of Object.entries(overrides.tokens || {})) if (TOKEN_BY_NAME.has(name) && validTokenValue(name, value)) explicit[name] = value;
  const values: Record<string, string> = {};
  const resolving = new Set<string>();
  const get = (name: string): string => {
    if (values[name] !== undefined) return values[name];
    const spec = TOKEN_BY_NAME.get(name);
    if (!spec) throw new Error(`Unknown token ${name}`);
    if (resolving.has(name)) throw new Error(`Token ${name} derives from itself`);
    resolving.add(name);
    const own = explicit[name];
    const value = own !== undefined ? (spec.kind === 'color' ? normalizeColor(own) : String(Number(own))) : spec.derive ? spec.derive(get, theme.scheme) : fallback(name, theme.scheme);
    resolving.delete(name);
    return (values[name] = value);
  };
  for (const spec of TOKENS) get(spec.name);
  return { id: theme.id, name: theme.name, scheme: theme.scheme, tokens: minContrast > 1 ? enforceContrast(values, minContrast) : values };
}
/** A base token the theme left out comes from the built-in of its scheme. */
function fallback(name: string, scheme: Scheme): string {
  const value = (scheme === 'dark' ? darkTokens : lightTokens)[name];
  if (value === undefined) throw new Error(`No value for ${name}`);
  return value;
}
export function validTokenValue(name: string, value: string): boolean {
  const spec = TOKEN_BY_NAME.get(name);
  if (!spec || typeof value !== 'string') return false;
  if (spec.kind === 'number') { const n = Number(value); return value.trim() !== '' && Number.isFinite(n) && n >= 0 && n <= 1; }
  return parseColor(value) !== undefined;
}

/**
 * Moves each text token just far enough from its backgrounds to reach the
 * ratio: body text the full ratio, hints at most 3:1. Surfaces and fills are
 * never changed, so the theme's look stays and only unreadable text moves.
 */
export function enforceContrast(tokens: Record<string, string>, ratio: number): Record<string, string> {
  const out = { ...tokens };
  for (const spec of TOKENS) {
    if (spec.contrast === 'none' || !spec.on) continue;
    const target = spec.contrast === 'hint' ? Math.min(ratio, 3) : ratio;
    let value = out[spec.name];
    // Several backgrounds: adjust against the one it reads worst on, until all pass.
    for (let i = 0; i < spec.on.length; i++) {
      const worst = spec.on.map(bg => tokens[bg]).sort((a, b) => contrast(value, a) - contrast(value, b))[0];
      if (contrast(value, worst) >= target - 0.005) break;
      value = ensureContrast(value, worst, target);
    }
    out[spec.name] = value;
  }
  return out;
}

/** The CSS a window applies: one rule on the root element. */
export function themeCss(theme: ResolvedTheme): string {
  const lines = Object.entries(theme.tokens).map(([name, value]) => `--${name}:${value};`);
  return `:root{color-scheme:${theme.scheme};${lines.join('')}}`;
}

export const acceptedBuiltinThemeIds = [...new Set([...BUILTIN_THEMES.map(theme => theme.id), ...retiredThemes.keys()])];
