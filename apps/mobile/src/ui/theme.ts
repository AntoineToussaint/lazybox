import { useColorScheme } from 'react-native';
import { BUILTIN_BY_ID } from '@lazybox/surfaces/models';

/**
 * The app's colours are the computer's: the built-in Dark and Light themes'
 * tokens (surfaces/appearance/themes.ts), read at build time, following the
 * phone's appearance. Only the names a phone screen needs are picked out.
 */
const pick = (id: 'dark' | 'light') => {
  const t = BUILTIN_BY_ID.get(id)!.tokens;
  return {
    scheme: id,
    bg: t['bg'], sunken: t['surface-sunken'], surface: t['surface'], raised: t['surface-raised'], pressed: t['surface-active'], selected: t['surface-selected'],
    line: t['line'], lineSubtle: t['line-subtle'],
    textStrong: t['text-strong'], text: t['text'], secondary: t['text-secondary'], muted: t['text-muted'], faint: t['text-faint'], disabled: t['text-disabled'],
    accent: t['accent'], accentBg: t['accent-bg'], accentLine: t['accent-line-subtle'], accentSolid: t['accent-solid'], onAccentSolid: t['on-accent-solid'],
    ok: t['ok'], okBg: t['ok-bg'], okSolid: t['ok-solid'],
    warn: t['warn'], warnBg: t['warn-bg'], warnLine: t['warn-line-subtle'],
    danger: t['danger'], dangerBg: t['danger-bg'], dangerSolid: t['danger-solid'], dangerLine: t['danger-line-subtle'],
    info: t['info'], infoBg: t['info-bg'],
    running: t['running'],
    codeBg: t['code-bg'], codeText: t['code-text'],
  };
};
export type Theme = ReturnType<typeof pick>;
export const themes = { dark: pick('dark'), light: pick('light') } as const;

export function useTheme(): Theme {
  return useColorScheme() === 'light' ? themes.light : themes.dark;
}

export const space = { xs: 4, s: 8, m: 12, l: 16, xl: 24 } as const;
export const radius = { s: 8, m: 12, l: 16, pill: 999 } as const;
/** Apple's minimum comfortable target. */
export const touch = 44;
export const mono = 'Menlo';
