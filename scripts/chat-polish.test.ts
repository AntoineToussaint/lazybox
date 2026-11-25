import { create } from '@bufbuild/protobuf';
import { CollectionSchema, JournalEventSchema, SessionSchema, type JournalEvent, type Session } from '@lazybox/protocol';
import { test } from 'node:test';
import { Code, ConnectError } from '@lazybox/client-core';
import { humanError, isCancellation, isTransient, refreshFailure, REFRESH_FAILURE_GRACE_MS, streamLabel } from '../packages/surfaces/src/human-error.ts';
import assert from 'node:assert/strict';
import { elapsedLabel, liveStatus, phaseFromEvents } from '../packages/surfaces/src/live-status.ts';
import { awaitingFirstMessage, sessionStatus } from '../packages/surfaces/src/agent-status.ts';
import { deriveAttention } from '../packages/surfaces/src/attention.ts';
import { caretOnFirstLine, caretOnLastLine, parsePromptHistory, recallOrder, recordPrompt } from '../packages/surfaces/src/prompt-history.ts';
import { parseSnippetUsage, recordSnippetUse, searchSnippets, sortSnippets } from '../packages/surfaces/src/snippet-usage.ts';
import { branchForName, generatedName, repositoryGroups, suggestWorkspaceName } from '../packages/surfaces/src/repository-model.ts';
import { branchSummary, conversationDetails, conversationTitle, titleFromMessage } from '../packages/surfaces/src/conversation-model.ts';

type Agent = { id?: string; kind?: string; status?: string; phase?: string; tool?: string; since?: number; started?: number; ended?: number; lastTurnStatus?: string };
const session = (init: Omit<Partial<Session>, 'agents'> & { agents?: Agent[] } = {}): Session => {
  const { agents = [], ...rest } = init;
  return create(SessionSchema, { id: 's', status: 'ready', ...rest, agents: agents.map(a => ({ id: a.id ?? 'root', kind: a.kind ?? '', status: a.status ?? 'running', phase: a.phase ?? '', tool: a.tool ?? '', phaseSinceUnixMillis: BigInt(a.since ?? 0), turnStartedAtUnixMillis: BigInt(a.started ?? 0), turnEndedAtUnixMillis: BigInt(a.ended ?? 0), lastTurnStatus: a.lastTurnStatus ?? '' })) });
};
const event = (type: string, at: number, extra: Record<string, string> = {}): JournalEvent => create(JournalEventSchema, { capturedAt: new Date(at).toISOString(), event: { type, ...extra } });

test('elapsed time reads short at every scale', () => {
  assert.equal(elapsedLabel(12_400), '12s');
  assert.equal(elapsedLabel(184_000), '3m 04s');
  assert.equal(elapsedLabel(3_720_000), '1h 02m');
  assert.equal(elapsedLabel(-5), '0s');
});

test('a working agent says what it does and for how long, from the Runner phase', () => {
  const s = session({ busy: true, status: 'running', agents: [{ phase: 'tool', tool: 'Bash', since: 10_000 }] });
  const live = liveStatus(s, 22_000);
  assert.equal(live.tone, 'busy');
  assert.equal(live.text, 'Running a command · 12s');
  assert.equal(live.label, 'Running a command', 'the announced label does not tick');
  assert.equal(liveStatus(session({ busy: true, status: 'running', agents: [{ phase: 'thinking' }] }), 1).text, 'Thinking…');
});

test('the journal refines the step: the earliest consecutive report is when it began', () => {
  const events = [event('turn_started', 1_000), event('activity', 2_000, { status: 'tool', name: 'Read' }), event('activity', 3_000, { status: 'tool', name: 'Bash' }), event('activity', 4_000, { status: 'tool', name: 'Bash' })];
  assert.deepEqual(phaseFromEvents(events), { phase: 'tool', tool: 'Bash', at: 3_000 });
  assert.equal(phaseFromEvents([...events, event('turn_ended', 5_000)]), undefined, 'an ended turn has no step');
  const s = session({ busy: true, status: 'running', agents: [{ phase: 'working', since: 500 }] });
  assert.equal(liveStatus(s, 9_000, events).text, 'Running a command · 6s');
  assert.equal(liveStatus(s, 9_000, [...events, event('message_delta', 8_000, { text: 'ok' })]).text, 'Writing · 1s');
  const stale = liveStatus(session({ busy: true, status: 'running', agents: [{ status: 'running' }] }), 9_000, [...events, event('turn_ended', 8_000, { status: 'completed' })]);
  assert.equal(stale.tone, 'idle', 'a turn the journal saw end stops moving before the next poll');
});

test('waiting, failed, saved and sub-agent states are distinct and still', () => {
  assert.deepEqual(liveStatus(session({ status: 'waiting_approval' }), 0).tone, 'waiting');
  assert.equal(liveStatus(session({ status: 'waiting_input' }), 0).text, 'Waiting for your answer');
  assert.equal(liveStatus(session({ fault: 'provider_failure', status: 'failed' }), 0).tone, 'failed');
  assert.equal(liveStatus(session({ closed: true }), 0).text, 'Saved');
  const children = session({ status: 'running', agents: [{ status: 'done', lastTurnStatus: 'completed' }, { id: 'child', status: 'running' }] });
  assert.equal(liveStatus(children, 0).text, 'Running · 1 sub-agent');
  const background = session({ status: 'running', agents: [{ status: 'done', lastTurnStatus: 'completed' }, { id: 'claude-tool:a', status: 'running' }, { id: 'claude-tool:b', status: 'running', kind: 'local_bash' }, { id: 'claude-tool:c', status: 'running', kind: 'local_bash' }] });
  assert.equal(liveStatus(background, 0).text, 'Running · 1 sub-agent · 2 background commands', 'a background command is not a sub-agent');
  // The Agents list nests the sub-agents under the row: its line does not count them again.
  assert.equal(liveStatus(children, 0, undefined, { subagentsShown: true }).text, 'Running');
  assert.equal(liveStatus(background, 0, undefined, { subagentsShown: true }).text, 'Running · 2 background commands');
});

test('an opened session that never ran a turn is Ready, not stuck Starting', () => {
  const fresh = session({ status: 'starting', agents: [{ status: 'starting' }] });
  assert.equal(awaitingFirstMessage(fresh), true);
  assert.equal(sessionStatus(fresh), 'Ready');
  assert.equal(liveStatus(fresh, 0).text, 'Ready');
  assert.equal(liveStatus(fresh, 0).tone, 'idle');
  assert.equal(deriveAttention(fresh).state, 'idle', 'the inbox does not list it as working');
  const sent = session({ status: 'starting', busy: true, agents: [{ status: 'starting' }] });
  assert.equal(awaitingFirstMessage(sent), false, 'a first message makes it work');
  assert.equal(liveStatus(sent, 0).tone, 'busy');
  const after = session({ status: 'ready', agents: [{ status: 'ready', lastTurnStatus: 'completed', ended: 5 }] });
  assert.equal(awaitingFirstMessage(after), false);
  assert.equal(awaitingFirstMessage(session({ status: 'starting', fault: 'delivery_failed' })), false, 'a fault is never Ready');
});

test('prompt history recalls this conversation first, then every other, newest first', () => {
  let history = recordPrompt([], 'global one', 'other');
  history = recordPrompt(history, 'mine one', 'c1');
  history = recordPrompt(history, 'global two', 'other');
  history = recordPrompt(history, 'mine one', 'c1');
  history = recordPrompt(history, '   ', 'c1');
  assert.equal(history.length, 3, 'repeats move to the top and blanks are not kept');
  assert.deepEqual(recallOrder(history, 'c1', ['from transcript', 'mine one']), ['mine one', 'from transcript', 'global two', 'global one']);
  assert.deepEqual(parsePromptHistory('{oops'), []);
  assert.deepEqual(parsePromptHistory(JSON.stringify([{ text: 'a', conversation: 'b' }, { bad: true }])), [{ text: 'a', conversation: 'b' }]);
  assert.equal(caretOnFirstLine('a\nb', 1, 1), true);
  assert.equal(caretOnFirstLine('a\nb', 2, 2), false);
  assert.equal(caretOnLastLine('a\nb', 2, 3), false, 'a selection is not a caret');
  assert.equal(caretOnLastLine('a\nb', 3, 3), true);
});

test('snippet usage counts uses, shows the last used first, and sorts frequent', () => {
  const rows = [{ key: 'alpha', category: 'A' }, { key: 'beta', category: 'A' }, { key: 'gamma', category: 'B' }, { key: 'delta', category: 'B' }];
  let usage = recordSnippetUse({}, 'gamma', 100);
  usage = recordSnippetUse(usage, 'gamma', 200);
  usage = recordSnippetUse(usage, 'gamma', 300);
  usage = recordSnippetUse(usage, 'beta', 400);
  assert.deepEqual(usage.gamma, { count: 3, last: 300 });
  assert.deepEqual(sortSnippets(rows, usage, ['delta'], 'suggested').map(r => r.key), ['beta', 'delta', 'gamma', 'alpha'], 'last used, then favorites, then recency');
  assert.deepEqual(sortSnippets(rows, usage, ['delta'], 'frequent').map(r => r.key), ['gamma', 'beta', 'alpha', 'delta']);
  assert.deepEqual(parseSnippetUsage(null, JSON.stringify(['b', 'a'])), { b: { count: 1, last: 2 }, a: { count: 1, last: 1 } }, 'the old recent list carries over');
  assert.deepEqual(parseSnippetUsage('{"x":{"count":"no"}}'), {});
});

test('workspace names and branches read well', () => {
  assert.equal(suggestWorkspaceName(['codefly-dev/authority']), 'authority');
  assert.equal(suggestWorkspaceName(['team/api', 'team/web']), 'api + web');
  assert.equal(suggestWorkspaceName(['codefly-dev/authority', 'codefly-dev/cli', 'codefly-dev/core', 'codefly-dev/module-datasource', 'codefly-dev/web']), 'authority +4');
  assert.equal(suggestWorkspaceName([]), 'New workspace');
  assert.equal(branchForName('Fix the login test!'), 'feature/fix-the-login-test');
  assert.equal(branchForName('Café .. déjà.lock'), 'feature/cafe-.-deja');
  assert.equal(branchForName('  ***  '), '');
  assert.equal(generatedName('gentle-copper-otter'), true);
  assert.equal(generatedName('Login fix'), false);
  assert.deepEqual(repositoryGroups(['a/x', 'a/y']), { owner: 'a', repositories: [{ full: 'a/x', short: 'x' }, { full: 'a/y', short: 'y' }] });
  assert.equal(repositoryGroups(['a/x', 'b/y']).owner, '', 'mixed owners keep full names');
});

test('a conversation titled after its folder has no title; the branch leads instead', () => {
  assert.equal(conversationTitle(session({ title: 'strongly-tight-collie', workingDirectory: '/w/strongly-tight-collie' })), '');
  assert.equal(conversationTitle(session({ title: 'Fix login', workingDirectory: '/w/strongly-tight-collie' })), 'Fix login');
  // The words that only open a request go, as the Runner's fallback title drops them.
  assert.equal(titleFromMessage('## Can you fix the flaky login test?\nMore'), 'Fix the flaky login test?');
  assert.equal(titleFromMessage('word '.repeat(30)).endsWith('…'), true);
  const one = create(CollectionSchema, { members: [{ spec: { name: 'api', branch: 'feature/x' } }, { spec: { name: 'web', branch: 'feature/x' } }] });
  assert.deepEqual(branchSummary(one), { name: 'feature/x', detail: '2 repositories', title: 'api: feature/x\nweb: feature/x' });
  const two = create(CollectionSchema, { members: [{ spec: { name: 'api', branch: 'feature/x' } }, { spec: { name: 'web', branch: 'feature/y' } }] });
  assert.equal(branchSummary(two)?.detail, '+1 branch');
  assert.equal(branchSummary(undefined), undefined);
  assert.equal(conversationDetails(session({ provider: 'claude' }), [], false), 'Claude', 'an unknown model is left out');
});

test('tools read as words, MCP tools included', async () => {
  const { toolLabel } = await import('../packages/surfaces/src/live-status.ts');
  assert.equal(toolLabel('mcp__lazybox__command'), 'Running a command');
  assert.equal(toolLabel('Read'), 'Reading files');
  assert.equal(toolLabel('Task'), 'Running a sub-agent');
  assert.equal(toolLabel('mcp__github__create_issue'), 'Using github · create issue');
  assert.equal(toolLabel('custom_tool'), 'Using custom tool');
});

// Raw Connect errors (`ConnectError: [failed_precondition] no active turn`) were shown as they came.
test('errors read as sentences: the Runner\'s words without the Connect wrapping, plain words for the developer ones', () => {
  assert.equal(humanError(new ConnectError('the review is already running', Code.FailedPrecondition)), 'the review is already running');
  assert.equal(humanError(new ConnectError('no active turn', Code.FailedPrecondition)), 'The agent is not working on anything: there is nothing to stop.');
  assert.match(humanError('Replay gap: retained cursor is not contiguous'), /history is missing/);
  assert.match(humanError(new ConnectError('Runner unreachable: Failed to fetch', Code.Unavailable)), /Runner is not answering/);
  assert.equal(humanError(new ConnectError('Test delivery failure', Code.Unavailable)), 'Test delivery failure', 'the Runner\'s own words stand');
  assert.equal(humanError(new Error('Runner did not return the resumed conversation')), 'Runner did not return the resumed conversation');
  assert.doesNotMatch(humanError(new ConnectError('boom', Code.Internal)), /ConnectError|\[internal\]/);
  assert.equal(streamLabel('Disconnected: ConnectError: [unavailable] Runner unreachable'), 'Reconnecting…');
  assert.equal(streamLabel('ConnectError: [not_found] execution is no longer available'), 'Not connected');
});

test('snippet search ranks names above metadata, and only searches bodies on request', () => {
  const rows = [
    { key: 'clarify', category: 'Review', description: 'Explain briefly', body: 'Use deepclarify for more.' },
    { key: 'inspect', category: 'Review', description: 'Run a deep review', body: '' },
    { key: 'deep-review', category: 'Code', description: 'Inspect a diff', body: '' },
    { key: 'deep', category: 'Code', description: 'Explore a problem', body: '' },
  ];
  const find = (query: string) => searchSnippets(rows, query).map(row => row.key);
  assert.deepEqual(find(' Deep '), ['deep', 'deep-review', 'inspect']);
  assert.deepEqual(find('review deep'), ['deep-review', 'inspect']);
  assert.deepEqual(find('deep review'), ['deep-review', 'inspect']);
  assert.deepEqual(find('clar'), ['clarify']);
  assert.deepEqual(find('arif'), ['clarify']);
  assert.deepEqual(find('rify inspect'), []);
  assert.deepEqual(find('body: deepclarify'), ['clarify']);
});

test('a Claude child keeps the conversation spinner after the root turn ends', () => {
  const root = { id: 'root', status: 'done', ended: 3000 };
  const child = { id: 'reviewer', status: 'running' };
  const active = session({ provider: 'claude', status: 'ready', agents: [root, child] });
  assert.equal(sessionStatus(active), 'Running in the background');
  assert.equal(deriveAttention(active).state, 'working', 'an unread review dot would replace the spinner');
  assert.equal(liveStatus(active, 4000).tone, 'busy');
  assert.equal(liveStatus(session({ provider: 'claude', agents: [child] }), 4000).tone, 'busy');
  const finished = session({ status: 'ready', agents: [root, { ...child, status: 'done' }] });
  assert.equal(deriveAttention(finished).state, 'review');
  assert.equal(liveStatus(finished, 5000).tone, 'idle');
});

test('the Runner\'s own Go context errors read in words, never raw', () => {
  assert.equal(humanError(new ConnectError('context deadline exceeded', Code.Unknown)), 'The Runner did not answer in time. Try again.');
  assert.equal(humanError(new ConnectError('get session: context deadline exceeded', Code.Internal)), 'The Runner did not answer in time. Try again.');
  assert.ok(isCancellation(new ConnectError('context canceled', Code.Unknown)));
  assert.ok(isCancellation(new ConnectError('the operation was aborted', Code.Canceled)));
  assert.ok(!isCancellation(new ConnectError('no such session', Code.NotFound)));
  assert.doesNotMatch(humanError(new ConnectError('context canceled', Code.Unknown)), /context/);
});

test('a refresh that fails only briefly shows nothing; one that keeps failing says so', () => {
  const slow = new ConnectError('context deadline exceeded', Code.Unknown);
  // The clock starts when the first failing refresh was sent (10 s before it timed out).
  let state = refreshFailure(undefined, 1_000, 11_000, slow);
  assert.equal(state.shown, '', 'one slow answer under load is no outage');
  state = refreshFailure(state.failingSince, 12_500, 1_000 + REFRESH_FAILURE_GRACE_MS - 1, slow);
  assert.equal(state.shown, '', 'still within the grace period');
  state = refreshFailure(state.failingSince, 14_000, 1_000 + REFRESH_FAILURE_GRACE_MS, slow);
  assert.equal(state.shown, 'The Runner did not answer in time. Try again.');
  // A cancellation (a Runner restarting cancels its handlers) leaves what is shown alone and keeps the clock.
  assert.deepEqual(refreshFailure(state.failingSince, 20_000, 20_000, new ConnectError('context canceled', Code.Canceled)), { failingSince: state.failingSince, shown: undefined });
  assert.deepEqual(refreshFailure(undefined, 5_000, 5_000, new ConnectError('context canceled', Code.Unknown)), { failingSince: undefined, shown: undefined });
});

test('a lasting failure shows at once: no grace for a deleted session or a refused credential', () => {
  for (const code of [Code.NotFound, Code.PermissionDenied, Code.Unauthenticated, Code.FailedPrecondition]) {
    const state = refreshFailure(undefined, 1_000, 1_100, new ConnectError('no such session', code));
    assert.ok(state.shown, `code ${Code[code]} shows at once`);
  }
  assert.ok(isTransient(new ConnectError('slow', Code.Unavailable)) && !isTransient(new ConnectError('get: boom', Code.Internal)));
  assert.equal(humanError(new ConnectError('context canceled', Code.Canceled)), 'The request was cancelled. Try again.');
});
