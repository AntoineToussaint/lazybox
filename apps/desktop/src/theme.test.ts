import { describe, expect, it } from "vitest";
import { type ThemeColors, luminance, terminalTheme } from "./theme";

const dark: ThemeColors = {
  accent: "#7dcfff",
  hover: "#f7768e",
  success: "#9ece6a",
  warn: "#e0af68",
  error: "#f7768e",
  text_strong: "#c0caf5",
  text_dim: "#7a82a7",
  chrome: "#3a4060",
  fill: "#292e42",
  surface: "#1a1d2e",
};

const light: ThemeColors = {
  accent: "#1a6ec4",
  hover: "#c13574",
  success: "#23864e",
  warn: "#9f6a00",
  error: "#c13574",
  text_strong: "#1c2030",
  text_dim: "#606880",
  chrome: "#c4c9d6",
  fill: "#dadfe9",
  surface: "#f7f8fa",
};

describe("terminalTheme", () => {
  it("maps background and foreground straight from the palette", () => {
    const theme = terminalTheme(dark);
    expect(theme.background).toBe(dark.surface);
    expect(theme.foreground).toBe(dark.text_strong);
    expect(theme.cursor).toBe(dark.accent);
  });

  it("keeps ANSI black darker than white for a dark theme", () => {
    const theme = terminalTheme(dark);
    expect(luminance(theme.black)).toBeLessThan(luminance(theme.white));
    // On a dark theme the darker pole is the surface itself.
    expect(theme.black).toBe(dark.surface);
    expect(theme.white).toBe(dark.text_strong);
  });

  it("does not invert ANSI black/white under a light theme", () => {
    const theme = terminalTheme(light);
    // The bug: black === surface makes black text vanish on a light
    // background. Black must stay the dark pole (the foreground here).
    expect(luminance(theme.black)).toBeLessThan(luminance(theme.white));
    expect(theme.black).toBe(light.text_strong);
    expect(theme.white).toBe(light.surface);
    expect(theme.background).toBe(light.surface);
  });
});
