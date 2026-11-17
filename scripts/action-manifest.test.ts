import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { manifestPath, renderManifest } from './action-manifest';
import { actionDefinitions } from '../packages/surfaces/src/actions';
import { layoutPanelKinds } from '../packages/surfaces/src/layout-presets';

test('the action manifest is the registry, rendered (npm run generate:manifest)', () => {
  let committed = '';
  try { committed = readFileSync(manifestPath, 'utf8'); } catch { /* missing: reported below */ }
  assert.equal(committed, renderManifest(), 'services/runner/code/internal/power/actions.manifest.json is stale: run `npm run generate:manifest` from the repository root');
});

test('hooks may run only actions that show, focus or arrange', () => {
  const safe = actionDefinitions.filter(d => (d as { safe?: boolean }).safe).map(d => d.id);
  for (const id of ['layout.maximize', 'layout.cycle', 'inbox.open', 'changes.show']) assert.ok(safe.includes(id as never), id);
  // Settings, starting or stopping work, running code, permissions, deleting: never.
  for (const id of ['settings.open', 'appearance.toggle', 'chat.stop', 'chat.clear', 'chat.compact', 'chat.permissions', 'code.run-last', 'review.codex', 'workspace.new', 'conversation.new', 'layout.reset-all', 'layout.reset', 'layout.import', 'layout.close', 'voice.dictate'])
    assert.ok(!safe.includes(id as never), id);
});

test('the layout panel kinds are the panel registry\'s', () => {
  const source = readFileSync(new URL('../packages/surfaces/src/PanelRegistry.tsx', import.meta.url), 'utf8');
  const block = source.slice(source.indexOf('export const builtinPanels'), source.indexOf('};', source.indexOf('export const builtinPanels')));
  const kinds = [...block.matchAll(/^\s{2}([a-z]+): \{ title:/gm)].map(m => m[1]);
  assert.deepEqual([...layoutPanelKinds], kinds);
});
