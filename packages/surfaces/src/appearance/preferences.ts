/**
 * What a person chose in Settings → Appearance, how it is stored, and what
 * it amounts to once the theme's own suggestions and the defaults fill in
 * what they left alone. Pure: no DOM, no storage.
 */
import { parseColor } from './color';
import { BUILTIN_BY_ID, currentThemeId, validTokenValue, type ThemeDefinition } from './themes';
import type { Scheme } from './tokens';

export type Mode = 'system' | 'light' | 'dark';
export type Density = 'compact' | 'comfortable' | 'relaxed';
export type Motion = 'system' | 'reduce' | 'full';

/** Text and spacing a theme may suggest and a person may set. */
export type Typography = {
  uiFont: string;
  uiFontSize: number;
  codeFont: string;
  codeFontSize: number;
  ligatures: boolean;
  density: Density;
  chatFontSize: number;
  chatLineHeight: number;
  chatParagraphGap: number;
  /** Characters per line in the chat, 0 for the full width. */
  chatMaxWidth: number;
};
export type AppearancePreferences = {
  mode: Mode;
  darkTheme: string;
  lightTheme: string;
  accent?: string;
  tokens?: Record<string, string>;
  /** Only what the person set: the rest follows the theme, then the defaults. */
  typography: Partial<Typography>;
  minContrast: number;
  motion: Motion;
  /** The user themes in use, as last read, so every window can apply them without the Runner. */
  userThemes?: Record<string, ThemeDefinition>;
};

export const SYSTEM_UI_FONT = "ui-sans-serif,-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif";
export const SYSTEM_CODE_FONT = 'ui-monospace,SFMono-Regular,Menlo,monospace';
/** Bundled with the app (@fontsource-variable/jetbrains-mono): command output reads in it unless a code font is set. */
export const CONSOLE_FONT = "'JetBrains Mono Variable'";
export const DEFAULT_TYPOGRAPHY: Typography = {
  uiFont: '', uiFontSize: 13, codeFont: '', codeFontSize: 12, ligatures: false, density: 'comfortable',
  chatFontSize: 14, chatLineHeight: 1.5, chatParagraphGap: 8, chatMaxWidth: 0,
};
/** Chat spacing each density brings with it. */
export const DENSITY_PRESETS: Record<Density, Pick<Typography, 'chatLineHeight' | 'chatParagraphGap'>> = {
  compact: { chatLineHeight: 1.35, chatParagraphGap: 6 },
  comfortable: { chatLineHeight: 1.5, chatParagraphGap: 8 },
  relaxed: { chatLineHeight: 1.7, chatParagraphGap: 12 },
};
export const DEFAULT_PREFERENCES: AppearancePreferences = { mode: 'system', darkTheme: 'dark', lightTheme: 'light', typography: {}, minContrast: 4.5, motion: 'system' };
export const CONTRAST_CHOICES = [0, 3, 4.5, 7];

/** Allowed range of each number, so a bad file or a stale build never breaks the window. */
export const TYPOGRAPHY_LIMITS: Record<'uiFontSize' | 'codeFontSize' | 'chatFontSize' | 'chatLineHeight' | 'chatParagraphGap' | 'chatMaxWidth', [number, number]> = {
  uiFontSize: [11, 16], codeFontSize: [10, 20], chatFontSize: [11, 22], chatLineHeight: [1.1, 2.2], chatParagraphGap: [0, 32], chatMaxWidth: [0, 200],
};
const FONT = /^[\w\s,'"\-.]{1,200}$/;
/** A font-family list made only of names, quotes, commas and spaces. */
export function validFont(value: unknown): value is string { return typeof value === 'string' && (value === '' || (FONT.test(value) && !/[;{}()<>\\]/.test(value))); }

/** Checks one typography value; returns an error message or ''. */
export function typographyError(key: string, value: unknown): string {
  switch (key) {
    case 'uiFont': case 'codeFont': return validFont(value) ? '' : 'must be a font list such as "Inter, sans-serif"';
    case 'ligatures': return typeof value === 'boolean' ? '' : 'must be true or false';
    case 'density': return value === 'compact' || value === 'comfortable' || value === 'relaxed' ? '' : 'must be compact, comfortable or relaxed';
    default: {
      const limits = TYPOGRAPHY_LIMITS[key as keyof typeof TYPOGRAPHY_LIMITS];
      if (!limits) return 'is not a setting';
      return typeof value === 'number' && Number.isFinite(value) && value >= limits[0] && value <= limits[1] ? '' : `must be a number from ${limits[0]} to ${limits[1]}`;
    }
  }
}
export function cleanTypography(input: unknown): Partial<Typography> {
  if (!input || typeof input !== 'object') return {};
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(input)) if (key in DEFAULT_TYPOGRAPHY && !typographyError(key, value)) out[key] = value;
  return out as Partial<Typography>;
}

/** Reads stored preferences, keeping every valid field and dropping the rest. */
export function parsePreferences(text: string | null | undefined): AppearancePreferences {
  if (!text || text.length > 1 << 20) return { ...DEFAULT_PREFERENCES };
  let raw: Record<string, unknown>;
  try { raw = JSON.parse(text); } catch { return { ...DEFAULT_PREFERENCES }; }
  if (!raw || typeof raw !== 'object') return { ...DEFAULT_PREFERENCES };
  const out: AppearancePreferences = { ...DEFAULT_PREFERENCES, typography: cleanTypography(raw.typography) };
  if (raw.mode === 'system' || raw.mode === 'light' || raw.mode === 'dark') out.mode = raw.mode;
  if (typeof raw.darkTheme === 'string' && raw.darkTheme.length < 200) out.darkTheme = currentThemeId(raw.darkTheme);
  if (typeof raw.lightTheme === 'string' && raw.lightTheme.length < 200) out.lightTheme = currentThemeId(raw.lightTheme);
  if (typeof raw.accent === 'string' && parseColor(raw.accent)) out.accent = raw.accent;
  if (raw.tokens && typeof raw.tokens === 'object') {
    const tokens = Object.fromEntries(Object.entries(raw.tokens as Record<string, unknown>).filter(([k, v]) => typeof v === 'string' && validTokenValue(k, v))) as Record<string, string>;
    if (Object.keys(tokens).length) out.tokens = tokens;
  }
  if (typeof raw.minContrast === 'number' && raw.minContrast >= 0 && raw.minContrast <= 21) out.minContrast = raw.minContrast;
  if (raw.motion === 'system' || raw.motion === 'reduce' || raw.motion === 'full') out.motion = raw.motion;
  if (raw.userThemes && typeof raw.userThemes === 'object') {
    const themes: Record<string, ThemeDefinition> = {};
    for (const [id, theme] of Object.entries(raw.userThemes as Record<string, ThemeDefinition>)) {
      if (theme && typeof theme.name === 'string' && (theme.scheme === 'dark' || theme.scheme === 'light') && theme.tokens && typeof theme.tokens === 'object') {
        themes[id] = { id, name: theme.name, scheme: theme.scheme, extends: typeof theme.extends === 'string' ? theme.extends : undefined, tokens: Object.fromEntries(Object.entries(theme.tokens).filter(([k, v]) => validTokenValue(k, v))), typography: cleanTypography(theme.typography), source: theme.source };
      }
    }
    if (Object.keys(themes).length) out.userThemes = themes;
  }
  return out;
}
export function serializePreferences(prefs: AppearancePreferences): string { return JSON.stringify(prefs); }

/** The theme a window shows, given the system's scheme. */
export function selectedThemeId(prefs: AppearancePreferences, system: Scheme): string {
  const scheme = prefs.mode === 'system' ? system : prefs.mode;
  return scheme === 'dark' ? prefs.darkTheme : prefs.lightTheme;
}
export function findTheme(id: string, prefs: AppearancePreferences, scheme: Scheme): ThemeDefinition {
  return BUILTIN_BY_ID.get(id) || prefs.userThemes?.[id] || BUILTIN_BY_ID.get(scheme)!;
}

/** Typography in force: the person's, then the theme's suggestion, then the default. */
export function effectiveTypography(prefs: AppearancePreferences, theme?: ThemeDefinition): Typography {
  const fromTheme = theme?.typography || {};
  const density = prefs.typography.density ?? fromTheme.density ?? DEFAULT_TYPOGRAPHY.density;
  return { ...DEFAULT_TYPOGRAPHY, ...DENSITY_PRESETS[density], ...fromTheme, ...prefs.typography, density };
}
