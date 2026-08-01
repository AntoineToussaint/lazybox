// Palette → xterm.js theme mapping.
//
// The Rust settings command hands the frontend a semantic palette (the
// same slots the ratatui TUI renders from). xterm.js needs concrete
// terminal colors, including the 16 ANSI entries. The mapping lives here
// as a pure function so it can be unit-tested in isolation — the tricky
// part (the ANSI black/white poles) is easy to get wrong under a light
// theme and hard to catch from the app entry.

export interface ThemeColors {
  accent: string;
  hover: string;
  success: string;
  warn: string;
  error: string;
  text_strong: string;
  text_dim: string;
  chrome: string;
  fill: string;
  surface: string;
}

function parseHex(hex: string): [number, number, number] {
  const value = hex.replace(/^#/, "");
  return [
    parseInt(value.slice(0, 2), 16),
    parseInt(value.slice(2, 4), 16),
    parseInt(value.slice(4, 6), 16),
  ];
}

// Perceived brightness (0 = black, 255 = white).
export function luminance(hex: string): number {
  const [r, g, b] = parseHex(hex);
  return 0.299 * r + 0.587 * g + 0.114 * b;
}

export interface XtermTheme {
  background: string;
  foreground: string;
  cursor: string;
  cursorAccent: string;
  selectionBackground: string;
  black: string;
  brightBlack: string;
  green: string;
  brightGreen: string;
  cyan: string;
  brightCyan: string;
  yellow: string;
  brightYellow: string;
  red: string;
  brightRed: string;
  white: string;
  brightWhite: string;
}

export function terminalTheme(colors: ThemeColors): XtermTheme {
  // ANSI black (0) and white (7) must stay absolute poles regardless of
  // whether the theme is dark or light: black is the darker of the
  // surface / foreground pair, white the lighter. A naive `black =
  // surface` inverts under a light theme, so black text vanishes on the
  // light background.
  const surfaceIsDarker =
    luminance(colors.surface) <= luminance(colors.text_strong);
  const dark = surfaceIsDarker ? colors.surface : colors.text_strong;
  const light = surfaceIsDarker ? colors.text_strong : colors.surface;
  return {
    background: colors.surface,
    foreground: colors.text_strong,
    cursor: colors.accent,
    cursorAccent: colors.surface,
    selectionBackground: colors.fill,
    black: dark,
    brightBlack: colors.text_dim,
    green: colors.success,
    brightGreen: colors.success,
    cyan: colors.accent,
    brightCyan: colors.accent,
    yellow: colors.warn,
    brightYellow: colors.warn,
    red: colors.error,
    brightRed: colors.error,
    white: light,
    brightWhite: light,
  };
}
