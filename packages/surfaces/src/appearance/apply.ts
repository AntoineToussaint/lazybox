/**
 * The window's appearance, applied: one <style> with every token, a few
 * attributes on <html> (scheme, density, motion), and a store that Monaco,
 * the terminal and diagrams subscribe to. It is applied once from the last
 * known preferences when this module loads, before anything renders, so a
 * window never flashes the wrong theme.
 */
import { resolveTheme, themeCss, type ResolvedTheme } from './themes';
import { CONSOLE_FONT, effectiveTypography, findTheme, parsePreferences, selectedThemeId, SYSTEM_CODE_FONT, SYSTEM_UI_FONT, type AppearancePreferences, type Typography } from './preferences';
import type { Scheme } from './tokens';

export type AppearanceState = { preferences: AppearancePreferences; theme: ResolvedTheme; typography: Typography; reducedMotion: boolean; system: Scheme };
/** Where the last applied preferences are kept for the next window to start with. */
export const APPEARANCE_CACHE_KEY = 'lazybox:appearance:last';

const hasDom = typeof document !== 'undefined' && typeof window !== 'undefined';
const listeners = new Set<() => void>();
let state: AppearanceState | undefined;

function media(query: string): MediaQueryList | undefined {
  return hasDom && typeof window.matchMedia === 'function' ? window.matchMedia(query) : undefined;
}
export function systemScheme(): Scheme { return media('(prefers-color-scheme: light)')?.matches ? 'light' : 'dark'; }
const systemReducedMotion = () => !!media('(prefers-reduced-motion: reduce)')?.matches;

/** Everything a set of preferences amounts to, for a window under `system`. */
export function computeAppearance(preferences: AppearancePreferences, system: Scheme = systemScheme(), reducedMotionDefault = systemReducedMotion()): AppearanceState {
  const scheme = preferences.mode === 'system' ? system : preferences.mode;
  const definition = findTheme(selectedThemeId(preferences, system), preferences, scheme);
  const theme = resolveTheme(definition, { accent: preferences.accent, tokens: preferences.tokens }, preferences.minContrast);
  const reducedMotion = preferences.motion === 'reduce' || (preferences.motion === 'system' && reducedMotionDefault);
  return { preferences, theme, typography: effectiveTypography(preferences, definition), reducedMotion, system };
}

/** The stylesheet for a computed appearance: tokens, fonts, sizes. */
export function appearanceCss({ theme, typography: t }: AppearanceState): string {
  const vars = [
    `--font-ui:${t.uiFont || SYSTEM_UI_FONT}`,
    `--font-mono:${t.codeFont || SYSTEM_CODE_FONT}`,
    // Command output: the person's code font, else the bundled JetBrains Mono (log-view.css loads it).
    `--font-console:${t.codeFont || `${CONSOLE_FONT},${SYSTEM_CODE_FONT}`}`,
    `--text-scale:${t.uiFontSize / 13}`,
    `--code-scale:${t.codeFontSize / 12}`,
    `--code-ligatures:${t.ligatures ? 'normal' : 'none'}`,
    `--chat-font-size:${t.chatFontSize}px`,
    `--chat-line-height:${t.chatLineHeight}`,
    `--chat-paragraph-gap:${t.chatParagraphGap}px`,
    `--chat-max-width:${t.chatMaxWidth > 0 ? `${t.chatMaxWidth}ch` : 'none'}`,
  ];
  // html:root outranks the stylesheets' own :root defaults, whatever order they load in.
  return themeCss(theme).replace(/^:root\{/, `html:root{${vars.join(';')};`);
}

function paint(next: AppearanceState) {
  if (!hasDom) return;
  let style = document.getElementById('lazybox-appearance') as HTMLStyleElement | null;
  if (!style) { style = document.createElement('style'); style.id = 'lazybox-appearance'; document.head.appendChild(style); }
  const css = appearanceCss(next);
  if (style.textContent !== css) style.textContent = css;
  // Keep it last, so it wins over stylesheets that load after it at the same specificity.
  if (style.nextElementSibling) document.head.appendChild(style);
  const root = document.documentElement;
  root.dataset.theme = next.theme.id;
  root.dataset.scheme = next.theme.scheme;
  root.dataset.density = next.typography.density;
  root.dataset.motion = next.reducedMotion ? 'reduce' : 'full';
  let meta = document.querySelector('meta[name="theme-color"]');
  if (!meta) { meta = document.createElement('meta'); meta.setAttribute('name', 'theme-color'); document.head.appendChild(meta); }
  meta.setAttribute('content', next.theme.tokens.surface);
}

/** Applies preferences to this window and tells subscribers. */
export function applyAppearance(preferences: AppearancePreferences): AppearanceState {
  let next: AppearanceState;
  try { next = computeAppearance(preferences); }
  catch { next = computeAppearance(parsePreferences(null)); }
  state = next;
  paint(next);
  try {
    localStorage.setItem(APPEARANCE_CACHE_KEY, JSON.stringify(preferences));
    localStorage.setItem('lazybox:appearance:background', next.theme.tokens.bg);
  } catch { /* private mode: the next window starts from the defaults */ }
  for (const listener of listeners) listener();
  return next;
}
export function getAppearance(): AppearanceState {
  if (!state) state = computeAppearance(readCachedPreferences());
  return state;
}
export function subscribeAppearance(listener: () => void): () => void { listeners.add(listener); return () => listeners.delete(listener); }
export function readCachedPreferences(): AppearancePreferences {
  try { return parsePreferences(hasDom ? localStorage.getItem(APPEARANCE_CACHE_KEY) : null); } catch { return parsePreferences(null); }
}

if (hasDom) {
  applyAppearance(readCachedPreferences());
  // System appearance and reduced motion follow the OS live.
  const follow = () => { if (state) applyAppearance(state.preferences); };
  media('(prefers-color-scheme: light)')?.addEventListener('change', follow);
  media('(prefers-reduced-motion: reduce)')?.addEventListener('change', follow);
}
