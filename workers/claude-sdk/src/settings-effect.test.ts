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

// Claude Code 2.1.330's lists: a heading, then "  <source>[ (<path>)]: <message>" lines, then a closing line.
const notLoaded = (...lines: string[]) => ['These settings did not load, so nothing in them is in effect, including hooks, permission rules and sandbox settings, though environment variables they already set can stay:', ...lines, 'Fix each problem listed, then start Claude Code again. For settings sent with applyFlagSettings, shown as SDK inline settings, send them again instead.'].join('\n');
const skipped = (...lines: string[]) => ['These settings entries were skipped; the rest of each source is in effect:', ...lines, 'Fix each problem listed, then start Claude Code again.'].join('\n');
const userFile = '  /work/repo/.claude/settings.json: Invalid JSON';

test('only Lazybox’s own permission settings, or an unknown source, stop an ask session', () => {
  for (const [what, content] of [
    ['the inline settings did not load', notLoaded('  SDK inline settings: Invalid JSON')],
    ['an inline ask rule was skipped', skipped('  SDK inline settings (permissions.ask[2]): Unknown tool')],
    ['the inline permissions were skipped', skipped(userFile.replace(': Invalid JSON', ' (hooks): x'), '  SDK inline settings (permissions): Expected object')],
    ['more inline entries than listed', skipped('  … and 4 more in SDK inline settings')],
    ['Claude did not say which settings', 'Some settings did not load or were skipped, so they are not in effect, though environment variables they already set can stay. These may include hooks, permission rules and sandbox settings. Check the settings this app passes and the settings files Claude Code loads, then start Claude Code again.'],
  ] as const) {
    let stopped = 0;
    const watch = new SettingsWatch(true, () => { stopped++; }, () => assert.fail(`${what}: reported instead of stopped`));
    assert.equal(watch.pass(warning(content)), false, what);
    assert.equal(stopped, 1, what);
    assert.equal(watch.end?.status, 'sdk_error', what);
  }
});

test('an ask session goes on, with a warning naming the file, when only other settings were rejected', () => {
  for (const [what, content, named] of [
    ['the person’s settings file did not load', notLoaded(userFile), '/work/repo/.claude/settings.json'],
    ['entries of the person’s own files were skipped', skipped('  /work/repo/.claude/settings.local.json (permissions.allow[0]): Invalid rule', '  /Users/me/.claude/settings.json (permissions.deny): Expected array'), '/work/repo/.claude/settings.local.json'],
    ['an inline setting that is not a permission was skipped', skipped('  SDK inline settings (disableClaudeAiConnectors): Expected boolean'), 'disableClaudeAiConnectors'],
    ['a settings file in a connector-downloads folder', 'These settings files are not used: they are in a connector-downloads folder, where Claude Code saves files from claude.ai connectors:\n  /Users/me/Downloads/claude/settings.json', '/Users/me/Downloads/claude/settings.json'],
  ] as const) {
    let stopped = 0;
    const reported: { type: string; status: string; text: string }[] = [];
    const watch = new SettingsWatch(true, () => { stopped++; }, f => reported.push(f));
    assert.equal(watch.pass(warning(content)), true, what);
    assert.equal(stopped, 0, what);
    assert.equal(watch.end, undefined, what);
    assert.equal(reported.length, 1, what);
    assert.deepEqual([reported[0].type, reported[0].status], ['provider_notice', 'settings_not_in_effect'], `${what}: a notice, never a failure that faults the conversation`);
    assert.ok(reported[0].text.includes(named), `${what}: names ${named}`);
    assert.match(reported[0].text, /approval settings are in effect/, what);
  }
});

test('any other session reports the warning as a notice and goes on', () => {
  let stopped = 0;
  const reported: { type: string; status: string; text: string }[] = [];
  const watch = new SettingsWatch(false, () => { stopped++; }, f => reported.push(f));
  assert.equal(watch.pass(warning(listed)), true);
  assert.equal(stopped, 0);
  assert.equal(watch.end, undefined);
  assert.equal(reported.length, 1);
  assert.deepEqual([reported[0].type, reported[0].status], ['provider_notice', 'settings_not_in_effect']);
  assert.ok(reported[0].text.includes(listed));
});
