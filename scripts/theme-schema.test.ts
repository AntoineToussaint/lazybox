// The theme schema the CLI ships (`lazybox schema theme`) is the one the
// token catalogue generates. Regenerate with:
//   node --import tsx scripts/theme-schema.test.ts --write
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { themeSchema, themeSchemaText } from '../packages/surfaces/src/appearance/theme-schema';
import { TOKENS } from '../packages/surfaces/src/appearance/tokens';

const path = new URL('../services/runner/code/internal/themes/theme.schema.json', import.meta.url);
if (process.argv.includes('--write')) writeFileSync(path, themeSchemaText());

test('the shipped theme schema is the generated one', () => {
  assert.equal(readFileSync(path, 'utf8'), themeSchemaText(), 'run: node --import tsx scripts/theme-schema.test.ts --write');
});

test('every token is in it, and nothing points at a web address', () => {
  const schema = themeSchema() as { $id: string; properties: { tokens: { properties: Record<string, unknown> } } };
  assert.deepEqual(Object.keys(schema.properties.tokens.properties), TOKENS.map(t => t.name));
  assert.match(schema.$id, /^urn:lazybox:/);
});
