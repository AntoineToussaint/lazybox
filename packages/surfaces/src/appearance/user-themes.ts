/**
 * User themes: files in the Runner's themes folder. The Runner reads them
 * (JSON or TOML) and hands over the parsed document; this checks it against
 * the token catalogue and says exactly what is wrong, line by line, so the
 * person can fix the file while Settings shows the result.
 */
import { BUILTIN_BY_ID, validTokenValue, type ThemeDefinition } from './themes';
import { TOKENS, TOKEN_BY_NAME } from './tokens';
import { DEFAULT_TYPOGRAPHY, typographyError, type Typography } from './preferences';

export type ThemeFileResult = { file: string; theme?: ThemeDefinition; errors: string[]; warnings: string[] };

/** The id a theme file is selected by: stable while the file keeps its name. */
export const userThemeId = (file: string) => `user:${file.replace(/\.(json|toml)$/i, '')}`;

const KEYS = new Set(['name', 'appearance', 'scheme', 'extends', 'tokens', 'typography', '$schema', 'description', 'author']);

export function parseThemeDocument(file: string, document: unknown): ThemeFileResult {
  const errors: string[] = [], warnings: string[] = [];
  const result = (theme?: ThemeDefinition): ThemeFileResult => ({ file, theme: errors.length ? undefined : theme, errors, warnings });
  if (!document || typeof document !== 'object' || Array.isArray(document)) { errors.push('The file must hold one theme: a table (TOML) or an object (JSON).'); return result(); }
  const doc = document as Record<string, unknown>;
  for (const key of Object.keys(doc)) if (!KEYS.has(key)) warnings.push(`Unknown key "${key}" is ignored${suggest(key, [...KEYS])}.`);
  const name = typeof doc.name === 'string' ? doc.name.trim() : '';
  if (!name) errors.push('"name" is required: the name Settings shows, such as name = "Midnight".');
  else if (name.length > 64) errors.push('"name" is longer than 64 characters.');
  const scheme = doc.appearance ?? doc.scheme;
  if (scheme !== 'dark' && scheme !== 'light') errors.push('"appearance" must be "dark" or "light".');
  let parent: string | undefined;
  if (doc.extends !== undefined) {
    const base = BUILTIN_BY_ID.get(String(doc.extends));
    if (!base) errors.push(`"extends" must name a built-in theme: ${[...BUILTIN_BY_ID.keys()].join(', ')}.`);
    else if (base.scheme !== scheme) errors.push(`"extends" names ${base.name}, a ${base.scheme} theme, but this theme's appearance is "${String(scheme)}".`);
    else parent = base.id;
  }
  const tokens: Record<string, string> = {};
  if (doc.tokens !== undefined) {
    if (!doc.tokens || typeof doc.tokens !== 'object' || Array.isArray(doc.tokens)) errors.push('"tokens" must be a table of token = "colour".');
    else for (const [raw, value] of Object.entries(doc.tokens as Record<string, unknown>)) {
      const key = raw.replace(/^--/, '');
      if (!TOKEN_BY_NAME.has(key)) { errors.push(`tokens.${raw}: there is no such token${suggest(key, TOKENS.map(t => t.name))}.`); continue; }
      const text = typeof value === 'number' ? String(value) : value;
      if (typeof text !== 'string' || !validTokenValue(key, text)) {
        errors.push(TOKEN_BY_NAME.get(key)!.kind === 'number' ? `tokens.${raw}: must be a number from 0 to 1.` : `tokens.${raw}: "${String(value)}" is not a colour. Use #rrggbb, #rrggbbaa, rgb() or hsl().`);
        continue;
      }
      tokens[key] = text;
    }
  }
  if (!errors.length && !Object.keys(tokens).length && !parent) warnings.push('No tokens are set: the theme looks exactly like the built-in one.');
  const typography: Partial<Typography> = {};
  if (doc.typography !== undefined) {
    if (!doc.typography || typeof doc.typography !== 'object') errors.push('"typography" must be a table.');
    else for (const [key, value] of Object.entries(doc.typography as Record<string, unknown>)) {
      if (!(key in DEFAULT_TYPOGRAPHY)) { errors.push(`typography.${key}: there is no such setting${suggest(key, Object.keys(DEFAULT_TYPOGRAPHY))}.`); continue; }
      const problem = typographyError(key, value);
      if (problem) errors.push(`typography.${key} ${problem}.`);
      else (typography as Record<string, unknown>)[key] = value;
    }
  }
  return result({ id: userThemeId(file), name, scheme: scheme as 'dark' | 'light', extends: parent, tokens, typography, source: { kind: 'user', file } });
}

/** ", did you mean …?" for the closest known name, if close enough. */
function suggest(key: string, known: string[]): string {
  let best = '', score = Infinity;
  for (const name of known) { const d = editDistance(key.toLowerCase(), name.toLowerCase()); if (d < score) { score = d; best = name; } }
  return score <= Math.max(2, Math.floor(key.length / 3)) ? ` (did you mean "${best}"?)` : '';
}
function editDistance(a: string, b: string): number {
  const row = Array.from({ length: b.length + 1 }, (_, i) => i);
  for (let i = 1; i <= a.length; i++) {
    let previous = row[0]; row[0] = i;
    for (let j = 1; j <= b.length; j++) { const kept = row[j]; row[j] = Math.min(row[j] + 1, row[j - 1] + 1, previous + (a[i - 1] === b[j - 1] ? 0 : 1)); previous = kept; }
  }
  return row[b.length];
}

/** A starting file for a new theme: TOML, every base token, the rest listed as comments. */
export function themeTemplate(name: string, scheme: 'dark' | 'light', values: Record<string, string>): string {
  const base = TOKENS.filter(t => t.base), rest = TOKENS.filter(t => !t.base);
  const line = (n: string) => `${n.includes('-') ? `"${n}"` : n} = "${values[n]}"`;
  return [
    `# A Lazybox theme. Save it in this folder; Lazybox reloads it as you edit.`,
    `# Every token is in Settings → Appearance → Token reference.`,
    `name = "${name.replaceAll('"', "'")}"`,
    `appearance = "${scheme}"`,
    `extends = "${scheme}"`,
    ``,
    `[tokens]`,
    `# The base colours: everything else derives from these.`,
    ...base.map(t => line(t.name)),
    ``,
    `# Uncomment to set a derived token yourself.`,
    ...rest.map(t => `# ${line(t.name)}`),
    ``,
    `[typography]`,
    `# density = "comfortable"   # compact, comfortable or relaxed`,
    `# chatFontSize = 14`,
    `# chatLineHeight = 1.5`,
    ``,
  ].join('\n');
}
