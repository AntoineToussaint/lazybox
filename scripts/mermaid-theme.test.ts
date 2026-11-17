// Diagrams get colours Mermaid's parser (khroma: hex, rgb(), hsl()) reads,
// whatever the theme holds; one it cannot read falls back for that variable
// alone, so a theme can never cost the whole diagram.
import test from 'node:test';
import assert from 'node:assert/strict';
import { parseOklabColor, toHex } from '../packages/surfaces/src/appearance/color.ts';
import { mermaidColor, mermaidTheme } from '../packages/surfaces/src/appearance/integrations.ts';
import { BUILTIN_BY_ID, BUILTIN_THEMES, resolveTheme } from '../packages/surfaces/src/appearance/themes.ts';
import { computeAppearance, type AppearanceState } from '../packages/surfaces/src/appearance/apply.ts';
import { DEFAULT_PREFERENCES } from '../packages/surfaces/src/appearance/preferences.ts';

// khroma's own patterns (node_modules/khroma/dist/color/{hex,rgb}.js), so "accepted" means Mermaid accepts it.
const KHROMA_HEX = /^#((?:[a-f0-9]{2}){2,4}|[a-f0-9]{3})$/i;
const accepted = (value: unknown) => typeof value === 'string' && KHROMA_HEX.test(value);

test('oklch() and oklab() convert to sRGB; anything else is not a colour here', () => {
  assert.equal(toHex(parseOklabColor('oklch(1 0 0)')!), '#ffffff');
  assert.equal(toHex(parseOklabColor('oklch(0% 0 0)')!), '#000000');
  assert.equal(toHex(parseOklabColor('oklab(0.627955 0.224863 0.125846)')!), '#ff0000');
  assert.equal(toHex(parseOklabColor('oklch(62.8% 0.2577 29.23deg / 50%)')!), '#ff000080');
  // Out of gamut: brought inside sRGB, not refused.
  assert.ok(KHROMA_HEX.test(toHex(parseOklabColor('oklch(0.7 0.4 150)')!)));
  for (const bad of ['oklch(0.7 0.1)', 'oklch(a b c)', 'color-mix(in srgb, red, blue)', 'var(--x)', 'red', '']) assert.equal(parseOklabColor(bad), undefined, bad);
});

test('mermaidColor hands Mermaid hex for every syntax it can convert, and nothing for the rest', () => {
  assert.equal(mermaidColor('#ABC'), '#aabbcc');
  assert.equal(mermaidColor('rgba(0 0 255 / 50%)'), '#0000ff80');
  assert.equal(mermaidColor('hsl(120, 100%, 25%)'), '#008000');
  assert.equal(mermaidColor('oklch(1 0 0)'), '#ffffff');
  for (const bad of ['color-mix(in srgb, red 40%, blue)', 'var(--accent)', 'rebeccapurple', 'url(x)', '', undefined, 3]) assert.equal(mermaidColor(bad), undefined, String(bad));
});

const withTokens = (state: AppearanceState, tokens: Record<string, string>): AppearanceState => ({ ...state, theme: { ...state.theme, tokens: { ...state.theme.tokens, ...tokens } } });

test('every built-in theme gives Mermaid only colours it reads', () => {
  for (const theme of BUILTIN_THEMES) {
    const state = computeAppearance({ ...DEFAULT_PREFERENCES, mode: theme.scheme, [theme.scheme === 'dark' ? 'darkTheme' : 'lightTheme']: theme.id }, theme.scheme, false);
    const vars = mermaidTheme(state);
    assert.equal(vars.darkMode, theme.scheme === 'dark');
    for (const [name, value] of Object.entries(vars)) if (name !== 'darkMode' && name !== 'fontFamily') assert.ok(accepted(value), `${theme.id}: ${name} = ${value}`);
  }
});

test('a token Mermaid cannot read falls back to the built-in value for that variable only', () => {
  const state = computeAppearance({ ...DEFAULT_PREFERENCES, mode: 'dark' }, 'dark', false);
  const safe = resolveTheme(BUILTIN_BY_ID.get('dark')!).tokens;
  const vars = mermaidTheme(withTokens(state, {
    'accent-bg-strong': 'color-mix(in srgb, var(--accent) 30%, transparent)',
    'warn-bg': 'var(--surface)',
    'text': 'oklch(0.9 0.02 280)',
    'info-line': 'rgb(10 20 30)',
  }));
  assert.equal(vars.primaryColor, safe['accent-bg-strong'], 'color-mix() falls back');
  assert.equal(vars.actorBkg, safe['accent-bg-strong']);
  assert.equal(vars.noteBkgColor, safe['warn-bg'], 'var() falls back');
  assert.equal(vars.textColor, toHex(parseOklabColor('oklch(0.9 0.02 280)')!), 'oklch() converts');
  assert.equal(vars.secondaryBorderColor, '#0a141e', 'rgb() converts');
  assert.equal(vars.lineColor, state.theme.tokens['text-muted'], 'the others are untouched');
  for (const [name, value] of Object.entries(vars)) if (name !== 'darkMode') assert.ok(accepted(value), `${name} = ${value}`);
  // A light theme falls back to light values, not dark ones.
  const light = computeAppearance({ ...DEFAULT_PREFERENCES, mode: 'light' }, 'light', false);
  assert.equal(mermaidTheme(withTokens(light, { 'code-bg': 'var(--x)' })).background, resolveTheme(BUILTIN_BY_ID.get('light')!).tokens['code-bg']);
});

test('the interface font is handed over only when the person chose one', () => {
  const state = computeAppearance({ ...DEFAULT_PREFERENCES, mode: 'dark' }, 'dark', false);
  assert.equal('fontFamily' in mermaidTheme(state), false);
  assert.equal(mermaidTheme({ ...state, typography: { ...state.typography, uiFont: 'Inter Variable' } }).fontFamily, 'Inter Variable');
});
