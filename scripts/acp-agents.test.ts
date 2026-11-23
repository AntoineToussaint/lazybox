import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { ProviderAvailability, ProviderStatus } from '@lazybox/protocol';
import { agentChoices, agentIcon, agentTitle, commandLine, isAcp, keySource, parseCommandLine, parseEnvLines, sharedCredentialGroups } from '../packages/surfaces/src/providers.ts';
import { conversationDetails } from '../packages/surfaces/src/conversation-model.ts';
import { lazyboxCommands, type SlashContext } from '../packages/surfaces/src/slash-commands.ts';
import { helpChoices, providerNeeds } from '../packages/surfaces/src/help-availability.ts';

const status = (provider: string, extra: Partial<ProviderStatus> = {}) => ({ provider, title: '', state: 'ready', installed: true, path: '', version: '', source: 'path', error: '', installCommand: '', subscriptions: [], ...extra }) as ProviderStatus;
const report = (...providers: ProviderStatus[]) => ({ providers, checkedAtUnixMillis: 0n }) as ProviderAvailability;

test('every surface names an agent the same way', () => {
  assert.equal(agentTitle('claude'), 'Claude');
  assert.equal(agentTitle('codex'), 'Codex');
  assert.equal(agentTitle('acp:antigravity'), 'Google Antigravity');
  assert.equal(agentTitle('acp:hermes'), 'Hermes Agent');
  assert.equal(agentTitle('acp:gemini'), 'Gemini CLI');
  assert.equal(agentTitle('acp:my-agent'), 'My Agent');
  assert.equal(agentTitle('acp:my-agent', [{ provider: 'acp:my-agent', label: 'Team bot' }]), 'Team bot');
  assert.equal(agentTitle(undefined, [], 'the agent'), 'the agent');
  assert.equal(agentTitle('acp:'), 'Agent');
  assert.ok(isAcp('acp:goose') && !isAcp('acp:') && !isAcp('claude'));
  assert.equal(agentIcon('acp:gemini'), 'agent-gemini');
  assert.equal(agentIcon('acp:custom', status('acp:custom', { acp: { id: 'custom', icon: 'agent', command: 'x', args: [], envNames: [], homepage: '', custom: true } as never })), 'agent');
  assert.equal(agentIcon('acp:unknown'), 'agent');
});

test('the picker offers Claude, Codex and the ACP agents the Runner can start', () => {
  const profiles = [{ id: 'claude-default', provider: 'claude' }, { id: 'acp-gemini', provider: 'acp:gemini', label: 'Gemini CLI' }];
  const choices = agentChoices(profiles, report(
    status('claude', { title: 'Claude Code', version: '2.1.311' }),
    status('codex', { title: 'Codex', state: 'not_installed', installed: false }),
    status('acp:gemini', { title: 'Gemini CLI', version: '0.68.0', acp: { id: 'gemini', icon: 'agent-gemini' } as never }),
    // Not installed: left to Settings → Agents, which says how to install it.
    status('acp:goose', { title: 'Goose', state: 'not_installed', installed: false }),
  ));
  assert.deepEqual(choices.map(c => [c.provider, c.title, c.ready, c.version, c.icon]), [
    ['claude', 'Claude', true, '2.1.311', 'agent-claude'],
    ['codex', 'Codex', false, '', 'agent-codex'],
    ['acp:gemini', 'Gemini CLI', true, '0.68.0', 'agent-gemini'],
  ]);
  assert.equal(choices[1].detail, 'Not installed');
  // Without a report (an older Runner, still loading), profiles say which agents exist.
  assert.deepEqual(agentChoices(profiles).map(c => c.provider), ['claude', 'codex', 'acp:gemini']);
});

test('command lines and environments typed in Settings', () => {
  assert.deepEqual(parseCommandLine('  gemini --acp '), { command: 'gemini', args: ['--acp'] });
  assert.deepEqual(parseCommandLine(`"/Applications/My Agent/bin/agent" acp --name 'two words' a\\ b ""`), { command: '/Applications/My Agent/bin/agent', args: ['acp', '--name', 'two words', 'a b', ''] });
  assert.deepEqual(parseCommandLine(''), { command: '', args: [] });
  assert.equal(commandLine({ acp: { command: '/opt/My Agent/agent', args: ['acp', '--x=1'] } as never }), '"/opt/My Agent/agent" acp --x=1');
  assert.equal(commandLine({}), '');
  assert.deepEqual(parseEnvLines('# keys\nAPI_KEY=abc=def\n\nKEEP=\n'), { entries: [{ name: 'API_KEY', value: 'abc=def' }, { name: 'KEEP', value: '' }], error: '' });
  assert.match(parseEnvLines('NOT VALID=1').error, /Line 1/);
});

test('an ACP conversation is named by its agent, not as Claude', () => {
  const session = { id: 's', profileId: 'acp-gemini', provider: 'acp:gemini', model: '' } as never;
  assert.equal(conversationDetails(session, [{ id: 'acp-gemini', provider: 'acp:gemini', label: 'Gemini CLI', isolation: 'inherited' } as never], false), 'Gemini CLI');
  const ctx = { provider: 'acp:gemini', agentName: 'Gemini CLI', live: true, busy: false, coding: true, workspace: true, permission: 'ask', planWorkflow: false, livePermission: true, model: '', liveModel: true, resumable: true, models: [], agentMessage: true, shellBlock: false, compact: false, detail: 'smart', subscriptions: 1 } satisfies SlashContext;
  const commands = lazyboxCommands(ctx);
  assert.equal(commands.find(c => c.name === 'stop')?.description, 'Interrupt Gemini CLI');
  assert.match(commands.find(c => c.name === 'compact')?.disabled || '', /Gemini CLI has no \/compact/);
});

test('Ask Lazybox never needs or picks an ACP agent', () => {
  const r = report(status('claude', { subscriptions: [{ profileId: 'claude-default', label: '', state: 'available', resetAt: 0n, detail: '', managed: false, isolation: 'inherited' } as never] }), status('acp:gemini', { subscriptions: [{ profileId: 'acp-gemini', label: 'Gemini CLI', state: 'unverified', resetAt: 0n, detail: '', managed: false, isolation: 'inherited' } as never] }), status('acp:goose', { installed: false, state: 'not_installed' }));
  assert.deepEqual(providerNeeds(r).map(n => n.provider), ['claude']);
  assert.ok(helpChoices(r).every(c => c.provider === 'claude'));
});

test('Antigravity is grouped with the Gemini CLI key it shares, by name only', () => {
  const acp = (id: string, extra: Record<string, unknown> = {}) => ({ id, icon: 'agent-gemini', command: id, args: [], envNames: [], homepage: '', custom: false, launchProblem: '', sharesCredentialsWith: '', inheritedEnvNames: [], signInCommand: '', credentialsNote: '', ...extra }) as never;
  const gemini = status('acp:gemini', { title: 'Gemini CLI', installed: false, acp: acp('gemini', { envNames: ['GEMINI_API_KEY'] }) });
  const agy = status('acp:antigravity', { title: 'Google Antigravity', acp: acp('antigravity', { sharesCredentialsWith: 'gemini', inheritedEnvNames: ['GEMINI_API_KEY'] }) });
  const goose = status('acp:goose', { title: 'Goose', acp: acp('goose') });
  const groups = sharedCredentialGroups([gemini, agy, goose]);
  assert.deepEqual(groups.map(g => [g.title, g.agents.map(a => a.title)]), [['Google', ['Gemini CLI', 'Google Antigravity']]]);
  assert.equal(keySource(agy, gemini), 'Uses Gemini CLI’s GEMINI_API_KEY');
  assert.equal(keySource(gemini, gemini), 'Its own GEMINI_API_KEY');
  const own = status('acp:antigravity', { title: 'Google Antigravity', acp: acp('antigravity', { sharesCredentialsWith: 'gemini', envNames: ['GEMINI_API_KEY'] }) });
  assert.equal(keySource(own, gemini), 'Its own GEMINI_API_KEY');
  const none = status('acp:antigravity', { title: 'Google Antigravity', acp: acp('antigravity', { sharesCredentialsWith: 'gemini' }) });
  assert.match(keySource(none, gemini), /^No API key: set one on Gemini CLI/);
  // Neither on the Runner: nothing to show.
  assert.deepEqual(sharedCredentialGroups([gemini, { ...none, installed: false } as ProviderStatus]), []);
});
