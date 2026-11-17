import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { renderShortcuts, shortcutsPage } from './docs-shortcuts';
import { actionDefinitions } from '../packages/surfaces/src/actions';

test('the keyboard shortcut page is the action registry, rendered (npm run docs:shortcuts)', () => {
  let committed = '';
  try { committed = readFileSync(shortcutsPage, 'utf8'); } catch { /* missing: reported below */ }
  assert.equal(committed, renderShortcuts(), 'docs/reference/keyboard-shortcuts.md is stale: run `npm run docs:shortcuts` from the repository root');
});
test('every action is on the page, with its keys as each platform reads them', () => {
  const page = renderShortcuts();
  for (const definition of actionDefinitions) {
    if ((definition as { series?: string }).series) continue;
    assert.ok(page.includes(`| ${definition.title} |`), definition.id);
  }
  assert.ok(page.includes('| Go to workspace 1…9 | `⌘1` … `⌘9` | `Ctrl+1` … `Ctrl+9` |'));
  assert.ok(page.includes('| Command palette | `⌘K` or `⇧⌘P` or `]] p` | `Ctrl+K` or `Ctrl+Shift+P` or `]] p` |'));
});
