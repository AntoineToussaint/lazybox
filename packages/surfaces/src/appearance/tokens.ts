/**
 * The design tokens: every colour the interface uses, by meaning. CSS reads
 * them as custom properties (`var(--surface)`); a theme sets some of them and
 * the rest derive from those, so a user theme can be four lines long and
 * still reach every surface, diff, chip and diagram.
 *
 * Order matters for people reading the Settings reference: groups are shown
 * in this order, and each family reads from strongest to weakest.
 */
import { adjust, lightness, mix, readableOn } from './color';

export type Scheme = 'dark' | 'light';
export type TokenGroup = 'Surfaces' | 'Lines' | 'Text' | 'Accent' | 'Status' | 'Code' | 'Diff' | 'Terminal colours' | 'Charts' | 'Effects';
/**
 * How a token is held to the minimum contrast: `body` text reaches the full
 * setting, a `hint` (faint text, ANSI black) at most 3:1, `none` is never
 * changed (surfaces, lines, fills).
 */
export type ContrastClass = 'body' | 'hint' | 'none';
export type Get = (name: string) => string;
export type TokenSpec = {
  name: string;
  group: TokenGroup;
  description: string;
  kind: 'color' | 'number';
  /** Set by every theme author who wants a distinct look; the rest derive. */
  base?: boolean;
  derive?: (get: Get, scheme: Scheme) => string;
  contrast: ContrastClass;
  /** The backgrounds this token is read on, for the contrast check. */
  on?: string[];
};

const textOn = ['surface', 'surface-raised', 'surface-sunken'];
const t = (name: string, group: TokenGroup, description: string, rest: Partial<TokenSpec> = {}): TokenSpec =>
  ({ name, group, description, kind: 'color', contrast: 'none', ...rest });
const dark = (scheme: Scheme) => scheme === 'dark';

/** The status and accent families share one shape. */
export const FAMILIES = ['accent', 'ok', 'warn', 'danger', 'info'] as const;
export type Family = typeof FAMILIES[number];
const familyNames: Record<Family, string> = { accent: 'Accent', ok: 'Success', warn: 'Warning', danger: 'Danger', info: 'Information' };
function family(f: Family): TokenSpec[] {
  const group: TokenGroup = f === 'accent' ? 'Accent' : 'Status';
  const label = familyNames[f];
  return [
    t(f, group, `${label}: icons, links, indicators and fills.`, { base: true, contrast: 'body', on: textOn }),
    t(`${f}-soft`, group, `${label} text that reads a little lighter.`, { contrast: 'body', on: textOn, derive: g => mix(g(f), g('text'), 0.22) }),
    t(`${f}-strong`, group, `${label} text at full emphasis.`, { contrast: 'body', on: textOn, derive: g => mix(g(f), g('text'), 0.55) }),
    t(`${f}-bg`, group, `${label} tinted background (chips, notices).`, { derive: (g, s) => mix(g('surface'), g(f), dark(s) ? 0.12 : 0.09) }),
    t(`${f}-bg-strong`, group, `${label} tinted background, selected or hovered.`, { derive: (g, s) => mix(g('surface'), g(f), dark(s) ? 0.2 : 0.17) }),
    t(`${f}-line-subtle`, group, `${label} quiet border.`, { derive: (g, s) => mix(g('surface'), g(f), dark(s) ? 0.27 : 0.25) }),
    t(`${f}-line`, group, `${label} border.`, { derive: (g, s) => mix(g('surface'), g(f), dark(s) ? 0.38 : 0.45) }),
    t(`${f}-solid`, group, `${label} solid fill behind white text (primary and destructive buttons).`, { derive: (g, s) => adjust(g(f), dark(s) ? { l: 0.54, scaleC: 1.6 } : { l: 0.5, scaleC: 1.1 }) }),
    t(`${f}-solid-hover`, group, `${label} solid fill, hovered.`, { derive: (g, s) => adjust(g(`${f}-solid`), { l: lightness(g(`${f}-solid`)) + (dark(s) ? 0.045 : -0.045) }) }),
    t(`on-${f}`, group, `Text on a ${label.toLowerCase()} fill.`, { derive: g => readableOn(g(f)) }),
    t(`on-${f}-solid`, group, `Text on a ${label.toLowerCase()} solid fill.`, { derive: g => readableOn(g(`${f}-solid`)) }),
  ];
}

export const TOKENS: TokenSpec[] = [
  t('bg', 'Surfaces', 'Behind everything: the window background.', { base: true }),
  t('surface-sunken', 'Surfaces', 'Recessed areas: the sidebar, wells, code.', { derive: g => mix(g('surface'), g('bg'), 0.5) }),
  t('surface', 'Surfaces', 'The main canvas: pages, panels and the chat.', { base: true }),
  t('surface-raised', 'Surfaces', 'Cards, inputs, menus and dialogs.', { base: true }),
  t('surface-hover', 'Surfaces', 'A row or control under the pointer.', { derive: (g, s) => mix(g('surface-raised'), g('text'), dark(s) ? 0.025 : 0.04) }),
  t('surface-active', 'Surfaces', 'A pressed control or an active row.', { derive: (g, s) => mix(g('surface-raised'), g('text'), dark(s) ? 0.045 : 0.065) }),
  t('surface-selected', 'Surfaces', 'The selected row, tab or item.', { derive: (g, s) => mix(g('surface'), g('text'), dark(s) ? 0.075 : 0.09) }),
  t('surface-strong', 'Surfaces', 'Strong neutral fills: scroll thumbs, toggles, badges.', { derive: (g, s) => mix(g('surface'), g('text'), dark(s) ? 0.13 : 0.15) }),
  t('line-subtle', 'Lines', 'Dividers between rows.', { derive: g => mix(g('line'), g('surface'), 0.35) }),
  t('line', 'Lines', 'Borders of panels, cards and inputs.', { base: true }),
  t('line-strong', 'Lines', 'Borders that must stand out: focused or hovered controls.', { derive: g => mix(g('line'), g('text'), 0.05) }),
  t('line-bold', 'Lines', 'The strongest neutral border.', { derive: g => mix(g('line'), g('text'), 0.11) }),
  t('text-strong', 'Text', 'Titles and text at full emphasis.', { contrast: 'body', on: textOn, derive: (g, s) => mix(g('text'), dark(s) ? '#ffffff' : '#000000', 0.6) }),
  t('text', 'Text', 'Body text.', { base: true, contrast: 'body', on: textOn }),
  t('text-soft', 'Text', 'Body text a step quieter.', { contrast: 'body', on: textOn, derive: g => mix(g('text'), g('text-muted'), 0.2) }),
  t('text-secondary', 'Text', 'Secondary text: descriptions, previews.', { contrast: 'body', on: textOn, derive: g => mix(g('text'), g('text-muted'), 0.4) }),
  t('text-tertiary', 'Text', 'Labels and metadata.', { contrast: 'body', on: textOn, derive: g => mix(g('text'), g('text-muted'), 0.6) }),
  t('text-subtle', 'Text', 'Quiet labels.', { contrast: 'body', on: textOn, derive: g => mix(g('text'), g('text-muted'), 0.8) }),
  t('text-muted', 'Text', 'Muted text: timestamps, hints, placeholders.', { base: true, contrast: 'body', on: textOn }),
  t('text-faint', 'Text', 'Faint text: decorations, separators.', { contrast: 'hint', on: textOn, derive: g => mix(g('text-muted'), g('surface'), 0.22) }),
  t('text-disabled', 'Text', 'Disabled text and icons.', { contrast: 'none', derive: g => mix(g('text-muted'), g('surface'), 0.42) }),
  ...family('accent'),
  t('selection', 'Accent', 'Selected text and editor selections.', { derive: (g, s) => mix(g('surface'), g('accent'), dark(s) ? 0.32 : 0.25) }),
  t('focus', 'Accent', 'The keyboard focus ring.', { derive: g => g('accent') }),
  ...family('ok'),
  ...family('warn'),
  ...family('danger'),
  ...family('info'),
  t('running', 'Status', 'Working agents and running commands.', { base: true, contrast: 'body', on: textOn }),
  t('code-bg', 'Code', 'Behind code blocks and the console.', { derive: g => g('surface-sunken') }),
  t('code-text', 'Code', 'Code that has no highlighting.', { contrast: 'body', on: ['code-bg'], derive: g => g('text-soft') }),
  t('syntax-keyword', 'Code', 'Keywords and tags.', { contrast: 'body', on: ['code-bg'], derive: g => mix(g('accent'), g('danger'), 0.4) }),
  t('syntax-string', 'Code', 'Strings.', { contrast: 'body', on: ['code-bg'], derive: g => g('ok') }),
  t('syntax-number', 'Code', 'Numbers and literals.', { contrast: 'body', on: ['code-bg'], derive: g => g('warn') }),
  t('syntax-comment', 'Code', 'Comments.', { contrast: 'body', on: ['code-bg'], derive: g => g('text-muted') }),
  t('syntax-type', 'Code', 'Types and built-ins.', { contrast: 'body', on: ['code-bg'], derive: g => g('info') }),
  t('syntax-attr', 'Code', 'Attributes, properties and titles.', { contrast: 'body', on: ['code-bg'], derive: g => g('accent-soft') }),
  t('syntax-function', 'Code', 'Function names.', { contrast: 'body', on: ['code-bg'], derive: g => mix(g('info'), g('ok'), 0.5) }),
  t('diff-add-bg', 'Diff', 'Added lines.', { derive: (g, s) => mix(g('surface'), g('ok'), dark(s) ? 0.09 : 0.12) }),
  t('diff-add-text', 'Diff', 'Text of added lines.', { contrast: 'body', on: ['diff-add-bg'], derive: g => g('ok-strong') }),
  t('diff-add-mark', 'Diff', 'The + sign and added counts.', { contrast: 'body', on: ['diff-add-bg', 'surface'], derive: g => g('ok') }),
  t('diff-word-add', 'Diff', 'The changed words inside an added line.', { derive: (g, s) => mix(g('surface'), g('ok'), dark(s) ? 0.38 : 0.3) }),
  t('diff-del-bg', 'Diff', 'Removed lines.', { derive: (g, s) => mix(g('surface'), g('danger'), dark(s) ? 0.09 : 0.1) }),
  t('diff-del-text', 'Diff', 'Text of removed lines.', { contrast: 'body', on: ['diff-del-bg'], derive: g => g('danger-strong') }),
  t('diff-word-del', 'Diff', 'The changed words inside a removed line.', { derive: (g, s) => mix(g('surface'), g('danger'), dark(s) ? 0.38 : 0.25) }),
  t('diff-del-mark', 'Diff', 'The − sign and removed counts.', { contrast: 'body', on: ['diff-del-bg', 'surface'], derive: g => g('danger') }),
  ...ansi(),
  ...logSources(),
  ...[1, 2, 3, 4, 5, 6, 7].map(n => t(`chart-${n}`, 'Charts', `Chart series ${n}.`, { derive: g => g(['info', 'warn', 'ok', 'accent', 'danger', 'running', 'syntax-type'][n - 1]) })),
  t('chart-muted', 'Charts', 'Reserved or unused parts of a chart.', { derive: g => g('text-faint') }),
  t('shadow', 'Effects', 'The colour of shadows and backdrops.', { derive: (_g, s) => dark(s) ? '#000000' : '#16161f' }),
  { name: 'shadow-strength', group: 'Effects', description: 'How strong shadows and backdrops are, 0–1.', kind: 'number', contrast: 'none', derive: (_g, s) => dark(s) ? '1' : '0.35' },
];

function ansi(): TokenSpec[] {
  const names = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];
  const normal = (g: Get, i: number) => [g('line-bold'), g('danger'), g('ok'), g('warn'), g('info'), g('accent'), mix(g('info'), g('ok'), 0.5), g('text-soft')][i];
  const bright = (g: Get, i: number) => i === 0 ? g('text-faint') : i === 7 ? g('text-strong') : mix(normal(g, i), g('text'), 0.3);
  return [
    ...names.map((n, i) => t(`ansi-${i}`, 'Terminal colours', `ANSI ${n} in command output.`, { contrast: i === 0 ? 'hint' : 'body', on: ['code-bg'], derive: g => normal(g, i) })),
    ...names.map((n, i) => t(`ansi-${i + 8}`, 'Terminal colours', `ANSI bright ${n}.`, { contrast: i === 0 ? 'hint' : 'body', on: ['code-bg'], derive: g => bright(g, i) })),
  ];
}

/**
 * The source column of a multi-service log (`api/web  > …`, `web-1  | …`): each
 * source keeps one of these. Hues avoid red and yellow, which say error and
 * warning there; lightness follows the theme's information colour.
 */
function logSources(): TokenSpec[] {
  // Six, as the console's source colours count them (LOG_SOURCE_COLOURS in log-lines.ts).
  return [195, 300, 150, 250, 340, 60].map((h, i) => t(`log-source-${i + 1}`, 'Terminal colours', `Source ${i + 1} of a multi-service log: its name in the console and its filter chip.`,
    { contrast: 'body', on: ['code-bg'], derive: g => adjust(g('info'), { h, c: 0.11 }) }));
}

export const TOKEN_NAMES = new Set(TOKENS.map(spec => spec.name));
export const TOKEN_BY_NAME = new Map(TOKENS.map(spec => [spec.name, spec]));
