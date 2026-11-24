import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import type { SDKMessage } from '@anthropic-ai/claude-agent-sdk';
import { nativeOptions } from './options.js';
import { SettingsWatch, asksBySettings, settingsNotInEffect } from './settings-effect.js';

// What Claude Code 2.1.330 (SDK 0.3.330) sends when settings did not load.
const listed = 'These settings did not load, so nothing in them is in effect, including hooks, permission rules and sandbox settings, though environment variables they already set can stay:\n  SDK inline settings (permissions.ask): Invalid value';
const warning = (content: string) => ({ type: 'system', subtype: 'informational', tag: 'settings_not_in_effect', level: 'warning', content, uuid: 'u', session_id: 's' }) as unknown as SDKMessage;

test('Claude’s settings_not_in_effect warning is read; other informational lines are not', () => {
  assert.equal(settingsNotInEffect(warning(listed)), listed);
  assert.match(settingsNotInEffect(warning(''))!, /not in effect/);
  assert.equal(settingsNotInEffect({ type: 'system', subtype: 'informational', level: 'warning', content: 'x', tag: 'artifact_resume' } as unknown as SDKMessage), undefined);
  assert.equal(settingsNotInEffect({ type: 'assistant', message: { content: [] } } as unknown as SDKMessage), undefined);
});

test('approvals rest on the worker’s settings in ask mode and for the help assistant only', () => {
  assert.equal(asksBySettings(nativeOptions({ cwd: '/f', permission: 'ask' }, {})), true);
  assert.equal(asksBySettings(nativeOptions({ cwd: '/f', purpose: 'help' }, {})), true);
  for (const permission of ['yolo', 'read-only', '']) assert.equal(asksBySettings(nativeOptions({ cwd: '/f', permission, browser: true }, {})), false, permission);
});

test('a session whose approvals rest on the worker’s settings stops when Claude did not apply settings', () => {
  let stopped = 0;
  const reported: unknown[] = [];
  const watch = new SettingsWatch(true, () => { stopped++; }, f => reported.push(f));
  assert.equal(watch.pass({ type: 'system', subtype: 'init' } as unknown as SDKMessage), true);
  assert.equal(watch.pass(warning(listed)), false);
  assert.equal(stopped, 1);
  assert.equal(reported.length, 0);
  assert.equal(watch.end?.status, 'sdk_error');
  assert.match(watch.end!.text, /stopped rather than run a command without asking you/);
  assert.ok(watch.end!.text.includes('SDK inline settings (permissions.ask)'), 'Claude’s own list of what did not load');
  // Nothing after it is read: no turn goes on under the weakened rules.
  assert.equal(watch.pass({ type: 'assistant', message: { content: [] } } as unknown as SDKMessage), false);
  assert.equal(stopped, 1);
});

test('any other session reports the warning as a failure and goes on', () => {
  let stopped = 0;
  const reported: { status: string; text: string }[] = [];
  const watch = new SettingsWatch(false, () => { stopped++; }, f => reported.push(f));
  assert.equal(watch.pass(warning(listed)), true);
  assert.equal(stopped, 0);
  assert.equal(watch.end, undefined);
  assert.equal(reported.length, 1);
  assert.equal(reported[0].status, 'provider_error');
  assert.ok(reported[0].text.includes(listed));
});
