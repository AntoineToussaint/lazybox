import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import type { SDKMessage } from '@anthropic-ai/claude-agent-sdk';
import { nativeOptions } from './options.js';
import { SettingsWatch, asksBySettings, settingsNotInEffect } from './settings-effect.js';
import { drift, settingsCases } from './settings-capture.js';
import { loadFixture, workspace } from './settings-fixture.js';

// What the bundled Claude Code prints for each case, as committed. These tests
// never start the binary: `npm run test:claude-settings` checks the fixture
// against it, as a gate of its own (settings-capture.ts).
const real = loadFixture().warnings;
const listed = real['inline did not load (permissions)'];
const warning = (content: string) => ({ type: 'system', subtype: 'informational', tag: 'settings_not_in_effect', level: 'warning', content, uuid: 'u', session_id: 's' }) as unknown as SDKMessage;

test('the fixture holds every case the check captures, and the check names what drifted', () => {
  assert.deepEqual(Object.keys(real).sort(), settingsCases.map(c => c.name).sort());
  assert.deepEqual(drift(loadFixture(), { ...real }), []);
  const changed = drift(loadFixture(), { ...real, 'inline entry skipped (permissions)': 'reworded' });
  assert.equal(changed.length, 1);
  assert.match(changed[0], /^inline entry skipped \(permissions\):/);
  assert.equal(drift(loadFixture(), {}).length, settingsCases.length, 'a case the binary no longer prints is drift too');
});

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

test('a session whose approvals rest on the worker’s settings stops when Claude did not apply them', () => {
  let stopped = 0;
  const reported: unknown[] = [];
  const watch = new SettingsWatch(true, () => { stopped++; }, f => reported.push(f));
  assert.equal(watch.pass({ type: 'system', subtype: 'init' } as unknown as SDKMessage), true);
  assert.equal(watch.pass(warning(listed)), false);
  assert.equal(stopped, 1);
  assert.equal(reported.length, 0);
  assert.equal(watch.end?.status, 'sdk_error');
  assert.match(watch.end!.text, /stopped rather than run a command without asking you/);
  assert.ok(watch.end!.text.includes('--settings (inline JSON) (permissions.ask)'), 'Claude’s own list of what did not load');
  // Nothing after it is read: no turn goes on under the weakened rules.
  assert.equal(watch.pass({ type: 'assistant', message: { content: [] } } as unknown as SDKMessage), false);
  assert.equal(stopped, 1);
});

// The same text with the label Claude gives settings sent with applyFlagSettings,
// and the line it writes when a source has more problems than it lists.
const asApplied = (text: string) => text.split('--settings (inline JSON)').join('SDK inline settings');
const truncated = () => real['inline entry skipped (permissions)'].replace(/\n {2}--settings \(inline JSON\)[^\n]*/, '\n  … and 4 more in --settings (inline JSON)');
// Claude's general wording, which names no source (from Claude Code 2.1.330).
const general = 'Some settings did not load or were skipped, so they are not in effect, though environment variables they already set can stay. These may include hooks, permission rules and sandbox settings. Check the settings this app passes and the settings files Claude Code loads, then start Claude Code again.';

test('Lazybox’s own permission settings rejected, or an unknown source, stop an ask session', () => {
  for (const [what, content] of [
    ['inline did not load (permissions)', real['inline did not load (permissions)']],
    ['inline did not load (other setting): the whole source, ask rules included', real['inline did not load (other setting)']],
    ['inline entry skipped (permissions)', real['inline entry skipped (permissions)']],
    ['the same, sent with applyFlagSettings', asApplied(real['inline entry skipped (permissions)'])],
    ['more inline entries than listed', truncated()],
    ['Claude did not say which settings', general],
  ] as const) {
    let stopped = 0;
    const watch = new SettingsWatch(true, () => { stopped++; }, () => assert.fail(`${what}: reported instead of stopped`));
    assert.equal(watch.pass(warning(content)), false, what);
    assert.equal(stopped, 1, what);
    assert.equal(watch.end?.status, 'sdk_error', what);
  }
});

test('an ask session goes on, with a notice naming the file, when only other settings were rejected', () => {
  for (const [what, content, named] of [
    ['the person’s project file did not load', real['project file did not load'], `${workspace}/.claude/settings.json`],
    ['an entry of the person’s local file was skipped', real['local file entry skipped (permissions)'], `${workspace}/.claude/settings.local.json`],
    ['an inline setting that is not a permission was skipped', real['inline entry skipped (hooks)'], 'hooks.Nope'],
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
