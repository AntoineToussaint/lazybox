import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { contrast, ensureContrast, mix, normalizeColor, parseColor, readableOn } from '../packages/surfaces/src/appearance/color.ts';
import { BUILTIN_THEMES, BUILTIN_BY_ID, enforceContrast, resolveTheme, themeCss } from '../packages/surfaces/src/appearance/themes.ts';
import { TOKENS, TOKEN_BY_NAME } from '../packages/surfaces/src/appearance/tokens.ts';
import { parseThemeDocument, themeTemplate, userThemeId } from '../packages/surfaces/src/appearance/user-themes.ts';
import { DEFAULT_PREFERENCES, effectiveTypography, parsePreferences, selectedThemeId, serializePreferences } from '../packages/surfaces/src/appearance/preferences.ts';
import { appearanceCss, computeAppearance } from '../packages/surfaces/src/appearance/apply.ts';
import { literalColors, literalFontSizes, tokenizeCss } from './tokenize-css.ts';

test('colours: every accepted syntax parses, anything else is refused', () => {
  assert.equal(normalizeColor('#ABC'), '#aabbcc');
  assert.equal(normalizeColor('#aabbcc80'), '#aabbcc80');
  assert.equal(normalizeColor('rgb(255, 0, 0)'), '#ff0000');
  assert.equal(normalizeColor('rgba(0 0 255 / 50%)'), '#0000ff80');
  assert.equal(normalizeColor('hsl(120, 100%, 25%)'), '#008000');
  for (const bad of ['red', 'var(--x)', '#12', 'url(x)', 'rgb(1,2)', '#fff;}body{', 'expression(1)', 'hsl(10, 20, 30)']) assert.equal(parseColor(bad), undefined, bad);
});

test('contrast follows WCAG 2 and ensureContrast moves only as far as needed', () => {
  assert.equal(Math.round(contrast('#000000', '#ffffff')), 21);
  assert.equal(contrast('#777777', '#777777'), 1);
  const fixed = ensureContrast('#9a9a9a', '#ffffff', 4.5);
  assert.ok(contrast(fixed, '#ffffff') >= 4.5 && contrast(fixed, '#ffffff') < 4.7, `${fixed} is just enough`);
  assert.equal(ensureContrast('#111111', '#ffffff', 4.5), '#111111', 'readable colours are unchanged');
  const tinted = ensureContrast('#bdb1ed', '#ffffff', 4.5);
  assert.ok(contrast(tinted, '#ffffff') >= 4.5);
  assert.ok(parseColor(tinted)!.b > parseColor(tinted)!.g, 'the hue survives');
  assert.equal(readableOn('#ffffff'), '#111114');
  assert.equal(readableOn('#101010'), '#ffffff');
  assert.equal(mix('#000000', '#ffffff', 0), '#000000');
});

test('retired palettes leave the picker and old selections resolve to editor presets', () => {
  assert.equal(BUILTIN_THEMES.length, 12);
  assert.ok(!BUILTIN_BY_ID.has('aubergine'));
  const prefs = parsePreferences(JSON.stringify({ darkTheme: 'aubergine', lightTheme: 'paper' }));
  assert.equal(prefs.darkTheme, 'dracula');
  assert.equal(prefs.lightTheme, 'github-light-default');
  assert.equal(parseThemeDocument('custom.toml', { name: 'Custom', appearance: 'dark', extends: 'pine' }).theme?.extends, 'dracula');
});

test('every built-in theme resolves every token to a valid value', () => {
  for (const theme of BUILTIN_THEMES) {
    const { tokens } = resolveTheme(theme);
    for (const spec of TOKENS) {
      const value = tokens[spec.name];
      assert.ok(value !== undefined, `${theme.id}: ${spec.name}`);
      if (spec.kind === 'color') assert.ok(parseColor(value), `${theme.id}: ${spec.name} = ${value}`);
      else assert.ok(Number.isFinite(Number(value)), `${theme.id}: ${spec.name}`);
    }
  }
});

test('Dark keeps the look Lazybox always had: no enforcement needed, nothing moved', () => {
  const dark = BUILTIN_BY_ID.get('dark')!;
  const plain = resolveTheme(dark).tokens, enforced = resolveTheme(dark, {}, 4.5).tokens;
  for (const name of ['bg', 'surface', 'surface-raised', 'line', 'text', 'text-muted', 'accent', 'ok', 'warn', 'danger', 'syntax-keyword', 'diff-add-bg'])
    assert.equal(enforced[name], dark.tokens[name], `${name} changed`);
  assert.equal(plain.surface, '#1d1e24');
  assert.equal(plain.accent, '#bdb1ed');
});

test('body text reaches 4.5:1 on its backgrounds in every built-in theme', () => {
  for (const theme of BUILTIN_THEMES) {
    const { tokens } = resolveTheme(theme, {}, 4.5);
    for (const spec of TOKENS.filter(t => t.contrast === 'body' && t.on)) {
      for (const background of spec.on!) {
        const ratio = contrast(tokens[spec.name], tokens[background]);
        assert.ok(ratio >= 4.45, `${theme.id}: ${spec.name} ${tokens[spec.name]} on ${background} ${tokens[background]} is ${ratio.toFixed(2)}`);
      }
    }
    for (const spec of TOKENS.filter(t => t.contrast === 'hint' && t.on))
      for (const background of spec.on!) assert.ok(contrast(tokens[spec.name], tokens[background]) >= 2.95, `${theme.id}: hint ${spec.name} on ${background}`);
  }
});

test('the light theme reads well without enforcement', () => {
  const { tokens } = resolveTheme(BUILTIN_BY_ID.get('light')!);
  for (const name of ['text', 'text-secondary', 'text-muted', 'accent', 'ok', 'warn', 'danger', 'info', 'syntax-comment', 'diff-add-text', 'diff-del-text'])
    assert.ok(contrast(tokens[name], tokens.surface) >= 4.5, `${name} ${contrast(tokens[name], tokens.surface).toFixed(2)}`);
  assert.ok(contrast(tokens['on-accent-solid'], tokens['accent-solid']) >= 4.5);
});

test('a chosen accent recolours the whole accent family; an override wins over the theme', () => {
  const dark = BUILTIN_BY_ID.get('dark')!;
  const { tokens } = resolveTheme(dark, { accent: '#ff8800', tokens: { surface: '#000000', 'not-a-token': '#fff', text: 'blue' } });
  assert.equal(tokens.accent, '#ff8800');
  assert.notEqual(tokens['accent-bg'], dark.tokens['accent-bg']);
  assert.notEqual(tokens['accent-solid'], dark.tokens['accent-solid']);
  assert.equal(tokens.surface, '#000000');
  assert.equal(tokens.text, dark.tokens.text, 'an invalid override is ignored');
  assert.equal(tokens['not-a-token'], undefined);
});

test('a user theme of a few base tokens derives everything else', () => {
  const result = parseThemeDocument('midnight.toml', { name: 'Midnight', appearance: 'dark', tokens: { bg: '#05060a', surface: '#0b0d14', 'surface-raised': '#121522', line: '#252a3a', text: '#e8ecf5', 'text-muted': '#8b93a7', accent: '#5eb1ff' } });
  assert.deepEqual(result.errors, []);
  assert.equal(result.theme!.id, userThemeId('midnight.toml'));
  const { tokens } = resolveTheme(result.theme!, {}, 4.5);
  assert.ok(tokens['accent-bg'] && tokens['diff-add-bg'] && tokens['ansi-4'] && tokens['syntax-string']);
  assert.ok(contrast(tokens['text-muted'], tokens.surface) >= 4.45);
});

test('theme files say exactly what is wrong', () => {
  const bad = parseThemeDocument('broken.toml', { name: '', appearance: 'dim', extends: 'light', tokens: { acent: '#fff', surface: 'blue', 'shadow-strength': 4 }, typography: { chatLineHeight: 9, fontSize: 3 }, colour: 1 });
  const text = bad.errors.join('\n');
  assert.equal(bad.theme, undefined);
  assert.match(text, /"name" is required/);
  assert.match(text, /"appearance" must be "dark" or "light"/);
  assert.match(text, /tokens\.acent: there is no such token \(did you mean "accent"\?\)/);
  assert.match(text, /tokens\.surface: "blue" is not a colour/);
  assert.match(text, /tokens\.shadow-strength: must be a number from 0 to 1/);
  assert.match(text, /typography\.chatLineHeight must be a number from 1\.1 to 2\.2/);
  assert.match(text, /typography\.fontSize: there is no such setting/);
  assert.match(bad.warnings.join('\n'), /Unknown key "colour"/);
  assert.match(parseThemeDocument('x.json', { name: 'X', appearance: 'dark', extends: 'light' }).errors.join(), /a light theme/);
  assert.match(parseThemeDocument('x.json', [1]).errors.join(), /one theme/);
});

test('the starting template is a valid theme of the current values', () => {
  const values = resolveTheme(BUILTIN_BY_ID.get('light')!).tokens;
  const toml = themeTemplate('My "light" theme', 'light', values);
  assert.match(toml, /^name = "My 'light' theme"$/m);
  assert.match(toml, /^accent = "#6a55c2"$/m);
  assert.match(toml, /^# "accent-bg" = "/m);
  assert.match(toml, /^"text-muted" = "/m);
});

test('stored preferences: valid fields are kept, anything else falls back', () => {
  assert.deepEqual(parsePreferences('not json'), DEFAULT_PREFERENCES);
  const prefs = parsePreferences(JSON.stringify({ mode: 'light', darkTheme: 'dark-high-contrast', accent: 'nope', minContrast: 7, motion: 'reduce', tokens: { surface: '#fff', bogus: '#000', text: 'url(x)' }, typography: { chatLineHeight: 1.35, uiFont: 'Inter; } body {', chatMaxWidth: 80, density: 'tiny' } }));
  assert.equal(prefs.mode, 'light');
  assert.equal(prefs.darkTheme, 'dark-high-contrast');
  assert.equal(prefs.accent, undefined);
  assert.deepEqual(prefs.tokens, { surface: '#fff' });
  assert.deepEqual(prefs.typography, { chatLineHeight: 1.35, chatMaxWidth: 80 });
  assert.equal(selectedThemeId(prefs, 'dark'), 'light');
  assert.equal(selectedThemeId({ ...prefs, mode: 'system' }, 'dark'), 'dark-high-contrast');
  assert.deepEqual(parsePreferences(serializePreferences(prefs)), prefs);
});

test('typography: the person wins over the theme, the theme over the density preset', () => {
  const theme = { ...BUILTIN_BY_ID.get('dark')!, typography: { chatLineHeight: 1.6, chatFontSize: 15 } };
  assert.equal(effectiveTypography(DEFAULT_PREFERENCES).chatLineHeight, 1.5);
  assert.equal(effectiveTypography({ ...DEFAULT_PREFERENCES, typography: { density: 'compact' } }).chatLineHeight, 1.35);
  assert.equal(effectiveTypography({ ...DEFAULT_PREFERENCES, typography: { density: 'relaxed' } }).chatParagraphGap, 12);
  assert.equal(effectiveTypography(DEFAULT_PREFERENCES, theme).chatLineHeight, 1.6);
  assert.equal(effectiveTypography({ ...DEFAULT_PREFERENCES, typography: { chatLineHeight: 1.4 } }, theme).chatLineHeight, 1.4);
});

test('the applied stylesheet carries the tokens, fonts and chat text', () => {
  const css = appearanceCss(computeAppearance({ ...DEFAULT_PREFERENCES, mode: 'light', typography: { uiFont: 'Inter', uiFontSize: 14, chatMaxWidth: 80, ligatures: true } }, 'dark', false));
  assert.match(css, /^html:root\{/);
  assert.match(css, /color-scheme:light/);
  assert.match(css, /--font-ui:Inter;/);
  assert.match(css, /--text-scale:1\.0769/);
  assert.match(css, /--chat-max-width:80ch/);
  assert.match(css, /--code-ligatures:normal/);
  assert.match(css, /--surface:#fafafc;/);
  assert.match(themeCss(resolveTheme(BUILTIN_BY_ID.get('dark')!)), /color-scheme:dark/);
  // Nothing a preference holds can end the rule early.
  assert.doesNotMatch(appearanceCss(computeAppearance(parsePreferences(JSON.stringify({ typography: { uiFont: 'x;}html{color:red' } })), 'dark', false)), /color:red/);
});

test('minimum contrast leaves surfaces alone and moves only text', () => {
  const surfacesOnly = { ...resolveTheme(BUILTIN_BY_ID.get('dark')!).tokens, 'text-muted': '#3a3b44' };
  const out = enforceContrast(surfacesOnly, 7);
  assert.equal(out.surface, surfacesOnly.surface);
  assert.ok(contrast(out['text-muted'], out.surface) >= 6.95);
  assert.equal(TOKEN_BY_NAME.get('surface')!.contrast, 'none');
});

test('the token sweep maps literals by role and keeps print styles', () => {
  const css = tokenizeCss('.a{color:#e4e4eb;background:#1d1e24;border:1px solid #3a3b45;font-size:12px}.a:hover{box-shadow:0 1px 4px #0006}.b{background:#bdb1ed;color:#282337}.review-row.is-add{background:#18291f}.c{font:600 14px/20px var(--font-ui)}pre{font-size:12px}@media print{.p{color:#111}}');
  assert.match(css, /\.a\{color:var\(--text\);background:var\(--surface\);border:1px solid var\(--line-strong\);font-size:calc\(12px \* var\(--text-scale\)\)\}/);
  assert.match(css, /box-shadow:0 1px 4px color-mix\(in srgb,var\(--shadow\) calc\(40% \* var\(--shadow-strength\)\),transparent\)/);
  assert.match(css, /\.b\{background:var\(--accent\);color:var\(--on-accent\)\}/);
  assert.match(css, /is-add\{background:var\(--diff-add-bg\)\}/);
  assert.match(css, /font:600 calc\(14px \* var\(--text-scale\)\)\/calc\(20px \* var\(--text-scale\)\) var\(--font-ui\)/);
  assert.match(css, /pre\{font-size:calc\(12px \* var\(--code-scale\)\)\}/);
  assert.match(css, /@media print\{\.p\{color:#111\}\}/);
  // A shorthand with a unitless line height scales its size and keeps the line height.
  assert.equal(tokenizeCss('.d{font:600 10px/1.6 var(--font-ui)}'), '.d{font:600 calc(10px * var(--text-scale))/1.6 var(--font-ui)}');
  assert.deepEqual(literalFontSizes('.d{font:600 10px/1.6 var(--font-ui)}.e{font:11px var(--font-mono)}.f{font:inherit}.g{font:calc(12px * var(--text-scale))/1.5 x}'), ['10px', '11px']);
});

// The guard every stylesheet change meets: colours and text sizes are tokens,
// so themes, light mode, and text size reach everything. Run
// `node --import tsx scripts/tokenize-css.ts <file>` to convert a stylesheet.
test('no stylesheet hard-codes a colour or a pixel font size', () => {
  const root = fileURLToPath(new URL('../packages/surfaces/src', import.meta.url));
  const files = (function walk(dir: string): string[] { return readdirSync(dir).flatMap(n => { const p = join(dir, n); return statSync(p).isDirectory() ? walk(p) : p.endsWith('.css') ? [p] : []; }); })(root);
  const problems = files.flatMap(file => {
    const text = readFileSync(file, 'utf8');
    return [...literalColors(text).map(c => `${file}: colour ${c}`), ...literalFontSizes(text).map(s => `${file}: font-size ${s}`)];
  });
  assert.deepEqual(problems, [], 'Use design tokens (see packages/surfaces/src/appearance/tokens.ts) or run scripts/tokenize-css.ts on the file.');
});
