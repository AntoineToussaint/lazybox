/**
 * The JSON Schema of a user theme file (user-themes.ts), generated from the
 * token catalogue so an editor completes and checks every token, built-in
 * theme and typography setting the app accepts. It ships with the CLI
 * (`lazybox schema theme`, services/runner/code/internal/themes/theme.schema.json,
 * kept identical by scripts/theme-schema.test.ts); nothing is fetched.
 */
import { TOKENS } from './tokens';
import { BUILTIN_THEMES } from './themes';
import { DEFAULT_TYPOGRAPHY, TYPOGRAPHY_LIMITS } from './preferences';

export const THEME_SCHEMA_ID = 'urn:lazybox:schema:theme:v1';

export function themeSchema(): Record<string, unknown> {
  const color = { type: 'string', description: 'A colour: #rgb, #rrggbb, #rrggbbaa, rgb(), rgba(), hsl() or hsla().', pattern: '^(#[0-9A-Fa-f]{3,8}|(rgb|rgba|hsl|hsla)\\(.*\\))$' };
  const tokens: Record<string, unknown> = {};
  for (const t of TOKENS) {
    tokens[t.name] = t.kind === 'number'
      ? { description: `${t.description} (${t.group})`, oneOf: [{ type: 'number', minimum: 0, maximum: 1 }, { type: 'string', pattern: '^(0(\\.\\d+)?|1(\\.0+)?)$' }] }
      : { ...color, description: `${t.description} (${t.group}${t.base ? ', base' : ''})` };
  }
  const typography: Record<string, unknown> = {};
  for (const key of Object.keys(DEFAULT_TYPOGRAPHY)) {
    const limits = TYPOGRAPHY_LIMITS[key as keyof typeof TYPOGRAPHY_LIMITS];
    typography[key] = key === 'uiFont' || key === 'codeFont' ? { type: 'string', description: 'A font list, such as "Inter, sans-serif"; empty for the default.' }
      : key === 'ligatures' ? { type: 'boolean' }
      : key === 'density' ? { enum: ['compact', 'comfortable', 'relaxed'] }
      : limits ? { type: 'number', minimum: limits[0], maximum: limits[1] } : {};
  }
  return {
    $schema: 'https://json-schema.org/draft/2020-12/schema',
    $id: THEME_SCHEMA_ID,
    title: 'Lazybox theme',
    description: 'A theme file in the Runner\'s themes folder (Settings → Appearance). Set the base tokens; the others derive from them.',
    type: 'object',
    required: ['name', 'appearance'],
    properties: {
      $schema: { type: 'string' },
      name: { type: 'string', minLength: 1, maxLength: 64, description: 'The name Settings shows.' },
      appearance: { enum: ['dark', 'light'], description: 'Whether this is a dark or a light theme.' },
      scheme: { enum: ['dark', 'light'], description: 'Another name for appearance.' },
      extends: { enum: [...new Set(BUILTIN_THEMES.map(t => t.id))], description: 'A built-in theme of the same appearance to start from.' },
      description: { type: 'string' },
      author: { type: 'string' },
      tokens: { type: 'object', description: 'Token = colour. A leading -- is accepted.', properties: tokens, patternProperties: { '^--': {} }, additionalProperties: false },
      typography: { type: 'object', properties: typography, additionalProperties: false },
    },
    additionalProperties: true,
  };
}

export const themeSchemaText = () => JSON.stringify(themeSchema(), null, 2) + '\n';
