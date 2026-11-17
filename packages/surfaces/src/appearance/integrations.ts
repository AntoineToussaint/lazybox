/**
 * Themes for the components that draw their own colours: the terminal
 * (xterm), the editor (Monaco) and diagrams (Mermaid). Each is derived from
 * the window's tokens, so they change with the theme like everything else.
 */
import { mix, parseColor, parseOklabColor, toHex } from './color';
import { SYSTEM_CODE_FONT } from './preferences';
import { BUILTIN_BY_ID, resolveTheme } from './themes';
import type { AppearanceState } from './apply';
import type { Scheme } from './tokens';

export function terminalTheme({ theme: { tokens: t } }: AppearanceState) {
  return {
    background: t['code-bg'], foreground: t['code-text'], cursor: t.accent, cursorAccent: t['code-bg'], selectionBackground: t.selection,
    black: t['ansi-0'], red: t['ansi-1'], green: t['ansi-2'], yellow: t['ansi-3'], blue: t['ansi-4'], magenta: t['ansi-5'], cyan: t['ansi-6'], white: t['ansi-7'],
    brightBlack: t['ansi-8'], brightRed: t['ansi-9'], brightGreen: t['ansi-10'], brightYellow: t['ansi-11'], brightBlue: t['ansi-12'], brightMagenta: t['ansi-13'], brightCyan: t['ansi-14'], brightWhite: t['ansi-15'],
  };
}
export function codeFont({ typography }: AppearanceState): { family: string; size: number; ligatures: boolean } {
  return { family: typography.codeFont || SYSTEM_CODE_FONT, size: typography.codeFontSize, ligatures: typography.ligatures };
}

const bare = (hex: string) => hex.replace(/^#/, '').slice(0, 6);
/** A Monaco theme: syntax rules and editor chrome from the tokens. */
export function monacoTheme({ theme }: AppearanceState) {
  const t = theme.tokens;
  return {
    base: (theme.scheme === 'dark' ? 'vs-dark' : 'vs') as 'vs-dark' | 'vs',
    inherit: true,
    rules: [
      { token: '', foreground: bare(t['code-text']) },
      { token: 'comment', foreground: bare(t['syntax-comment']), fontStyle: 'italic' },
      { token: 'keyword', foreground: bare(t['syntax-keyword']) },
      { token: 'string', foreground: bare(t['syntax-string']) },
      { token: 'number', foreground: bare(t['syntax-number']) },
      { token: 'type', foreground: bare(t['syntax-type']) },
      { token: 'type.identifier', foreground: bare(t['syntax-type']) },
      { token: 'attribute.name', foreground: bare(t['syntax-attr']) },
      { token: 'tag', foreground: bare(t['syntax-keyword']) },
      { token: 'function', foreground: bare(t['syntax-function']) },
      { token: 'variable', foreground: bare(t['code-text']) },
    ],
    colors: {
      'editor.background': t['code-bg'],
      'editor.foreground': t['code-text'],
      'editorLineNumber.foreground': t['text-faint'],
      'editorLineNumber.activeForeground': t['text-secondary'],
      'editor.lineHighlightBackground': t['surface-hover'],
      'editor.selectionBackground': t.selection,
      'editor.inactiveSelectionBackground': mix(t.selection, t['code-bg'], 0.4),
      'editorCursor.foreground': t.accent,
      'editorIndentGuide.background1': t['line-subtle'],
      'editorWidget.background': t['surface-raised'],
      'editorWidget.border': t['line-strong'],
      'input.background': t['surface-sunken'],
      'diffEditor.insertedLineBackground': t['diff-add-bg'],
      'diffEditor.removedLineBackground': t['diff-del-bg'],
      // Monaco keeps its own context menu: it reads like Lazybox's.
      'menu.background': t['surface-hover'], 'menu.foreground': t.text, 'menu.border': t['line-strong'],
      'menu.selectionBackground': t['surface-strong'], 'menu.selectionForeground': t['text-strong'], 'menu.separatorBackground': t.line,
    },
  };
}

/** Which token colours each of Mermaid's theme variables. */
const MERMAID_COLOURS = {
  background: 'code-bg',
  primaryColor: 'accent-bg-strong', primaryTextColor: 'text', primaryBorderColor: 'accent-line',
  secondaryColor: 'info-bg-strong', secondaryTextColor: 'text', secondaryBorderColor: 'info-line',
  tertiaryColor: 'surface-raised', tertiaryTextColor: 'text', tertiaryBorderColor: 'line-strong',
  lineColor: 'text-muted', textColor: 'text', mainBkg: 'accent-bg-strong', nodeBorder: 'accent-line',
  clusterBkg: 'surface-raised', clusterBorder: 'line-strong', titleColor: 'text-strong', edgeLabelBackground: 'surface-raised',
  noteBkgColor: 'warn-bg', noteTextColor: 'warn-strong', noteBorderColor: 'warn-line',
  actorBkg: 'accent-bg-strong', actorBorder: 'accent-line', actorTextColor: 'text', signalColor: 'text-secondary', signalTextColor: 'text',
  errorBkgColor: 'danger-bg', errorTextColor: 'danger-strong',
} as const;

/**
 * A colour Mermaid can read, or undefined. Mermaid's colour parser (khroma)
 * reads hex, rgb() and hsl() only and throws on anything else — oklch(),
 * color-mix(), var(), a name — which fails the whole diagram, not one fill.
 */
export function mermaidColor(value: unknown): string | undefined {
  if (typeof value !== 'string') return undefined;
  const rgba = parseColor(value) ?? parseOklabColor(value);
  return rgba && toHex(rgba);
}

const builtIn: Partial<Record<Scheme, Record<string, string>>> = {};
/** The built-in theme of the scheme: where a colour Mermaid cannot read is taken from. */
function builtInTokens(scheme: Scheme): Record<string, string> {
  return builtIn[scheme] ??= resolveTheme(BUILTIN_BY_ID.get(scheme)!).tokens;
}

/**
 * Mermaid's base theme, coloured from the tokens. Every colour is handed over
 * as hex; one Mermaid could not read falls back to the built-in theme's value
 * for that variable, so a theme never costs the diagram.
 */
export function mermaidTheme({ theme, typography }: AppearanceState): Record<string, string | boolean> {
  const t = theme.tokens, safe = builtInTokens(theme.scheme);
  const colours = Object.fromEntries(Object.entries(MERMAID_COLOURS).map(([name, token]) => [name, mermaidColor(t[token]) ?? safe[token]]));
  return {
    darkMode: theme.scheme === 'dark',
    // The diagram is drawn as an image: it gets a font name itself, not a CSS
    // variable. Mermaid's own default unless the person chose an interface font.
    ...(typography.uiFont ? { fontFamily: typography.uiFont } : {}),
    ...colours,
  };
}
