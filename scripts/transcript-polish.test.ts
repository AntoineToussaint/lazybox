// What Lazybox says in a conversation, what runs in its background and what
// its tokens cost: the models behind the transcript's system events, the
// background work list and the usage breakdown.
import { create } from '@bufbuild/protobuf';
import { JournalEventSchema, UsageGroupSchema, type JournalEvent } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { emptyTranscript, foldTranscript, type Transcript } from '../packages/surfaces/src/transcript-model.ts';
import { connectionLine, isSystemEvent, legacyOrigin, messageOrigin, restartTitle, systemEventView } from '../packages/surfaces/src/system-events.ts';
import { backgroundCommands, newestFirstTree, recentWork, runningSummary, workCounts, workSummary, workTotals, type Subagent, type SubagentStatus } from '../packages/surfaces/src/subagent-model.ts';
import { dollars, unpricedHelp, unpricedLine, usageBreakdown } from '../packages/surfaces/src/usage-model.ts';

let cursor = 0n;
const T0 = Date.parse('2026-11-16T10:00:00Z');
type Init = { type: string; text?: string; turnId?: string; operationId?: string; agentId?: string; parentAgentId?: string; nativeId?: string; name?: string; agentKind?: string; status?: string; data?: unknown };
const record = (at: number, { data, ...event }: Init, receipt?: Record<string, string>): JournalEvent => create(JournalEventSchema, {
  cursor: ++cursor, capturedAt: new Date(T0 + at).toISOString(),
  event: { ...event, providerDataJson: data === undefined ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(data)) },
  ...(receipt ? { receipt } : {}),
});
const fold = (state: Transcript, ...records: JournalEvent[]) => foldTranscript(state, records);
const NOTE = '[Lazybox] You asked to add codefly-dev/authority to this workspace. The person added it:\n- authority at /Users/me/.cache/environments/dev/collections/heavily-heroic-firefly/authority (branch feature/revenge)\nContinue with what you needed it for.';
const ADDED = { kind: 'repository_added', summary: 'Added authority to this workspace', detail: 'feature/revenge', copy: '/Users/me/.cache/environments/dev/collections/heavily-heroic-firefly/authority' };

test('every kind of message Lazybox writes maps to an icon, a tone and one line', () => {
  const kinds = ['repository_added', 'repository_declined', 'review_brief', 'review_findings', 'review_round', 'agent_ask', 'agent_answer', 'mission_brief', 'continuation'];
  for (const kind of kinds) {
    const view = systemEventView({ kind, summary: '', detail: '', copy: '', from: '' }, 'Claude');
    assert.ok(view.icon && view.icon !== 'sparkle', `${kind} has its own icon`);
    assert.ok(view.line && !/Lazybox\]/.test(view.line), `${kind} says what happened without a summary: ${view.line}`);
    assert.ok(view.kind, `${kind} names its kind`);
  }
  const added = systemEventView({ ...ADDED, from: '' }, 'Claude');
  assert.deepEqual([added.icon, added.tone, added.line, added.detail, added.copy], ['repository', 'ok', 'Added authority to this workspace', 'feature/revenge', ADDED.copy]);
  assert.equal(added.sentBy, 'What Lazybox told Claude');
  const ask = systemEventView({ kind: 'agent_ask', summary: 'Codex asked Claude: why a mutex?', detail: '', copy: '', from: 'Codex' }, 'Claude');
  assert.equal(ask.line, 'Codex asked Claude: why a mutex?');
  assert.equal(ask.sentBy, 'Codex’s message, as Lazybox gave it to Claude');
  assert.equal(systemEventView({ kind: 'something_new', summary: 'A future event', detail: '', copy: '', from: '' }).icon, 'sparkle', 'an unknown kind still shows, plainly');
  assert.equal(messageOrigin({ kind: '' }), undefined);
  assert.equal(messageOrigin('review'), undefined);
  assert.deepEqual(messageOrigin({ kind: 'review_findings', summary: 'Codex review · 3 findings sent' }), { kind: 'review_findings', summary: 'Codex review · 3 findings sent', detail: '', copy: '', from: '' });
});

test('journals from before origins: the Runner’s own operation ids say which messages were Lazybox’s', () => {
  assert.equal(legacyOrigin('repository-note-rr-1')?.kind, 'repository_note');
  assert.equal(legacyOrigin('review-brief-1234')?.kind, 'review_brief');
  assert.equal(legacyOrigin('review-ab12-findings-2')?.kind, 'review_findings');
  assert.equal(legacyOrigin('review-ab12-round-3')?.kind, 'review_round');
  assert.equal(legacyOrigin('review-ask-9')?.kind, 'agent_ask');
  assert.equal(legacyOrigin('review-answer-9')?.kind, 'agent_answer');
  assert.equal(legacyOrigin('3f2a9c1e-7b1d-4c55-9a52-1f0e4f0b6d33'), undefined, 'the person’s messages are theirs');
  assert.equal(legacyOrigin(''), undefined);
});

test('a message Lazybox wrote is an event of the conversation, never "You", and the agent’s text is kept whole', () => {
  cursor = 0n;
  const t = fold(emptyTranscript(),
    record(0, { type: 'operation_accepted', text: 'add the auth repo', operationId: 'typed' }),
    record(10, { type: 'message_delta', text: 'Requesting it', turnId: 't1' }),
    record(20, { type: 'operation_accepted', text: NOTE, operationId: 'repository-note-rr-1', data: { files: [], origin: ADDED } }),
  );
  const [typed, , note] = t.messages;
  assert.equal(typed.origin, undefined, 'the person’s own message stays theirs');
  assert.deepEqual(note.origin, { ...ADDED, from: '' });
  assert.equal(note.text, NOTE, 'the event opens to exactly what the agent received');
  assert.equal(note.operationId, 'repository-note-rr-1', 'it still starts its turn, for checkpoints');
  assert.ok(isSystemEvent(note) && !isSystemEvent(typed));
});

test('what Lazybox told the agent of its own changes is one quiet event before the message it went with, never "You"', () => {
  cursor = 0n;
  const DELTA = '[Lazybox] Since this conversation started, Lazybox added or changed the following; everything else you were told still holds.\n- Start another conversation with any agent (Claude, Codex, …) when the person asks: `lazybox conversation new --provider P --title T --brief-artifact ID`.';
  const t = fold(emptyTranscript(),
    record(0, { type: 'operation_accepted', text: 'continue this with Codex', operationId: 'm1' }),
    // The Runner journals the update once the message is accepted, naming it.
    record(5, { type: 'briefing_updated', text: DELTA, status: 'features', operationId: 'm1', data: { sections: ['conversations'], versions: { conversations: '2' } } }),
    record(10, { type: 'message_delta', text: 'I will ask the person to start it.', turnId: 't1' }),
  );
  const [update, typed, reply] = t.messages;
  assert.equal(typed.text, 'continue this with Codex');
  assert.equal(typed.origin, undefined, 'the person’s message stays theirs');
  assert.equal(update.type, 'briefing', 'never a message the person sent');
  assert.equal(update.text, DELTA, 'it opens to exactly what the agent received');
  assert.ok(isSystemEvent(update));
  assert.equal(reply.text, 'I will ask the person to start it.');
  const view = systemEventView(update.origin!, 'Claude');
  assert.equal(view.line, 'Lazybox told Claude about new features');
  assert.equal(view.sentBy, 'What Lazybox told Claude');
  assert.equal(view.tone, 'muted');
  // Only the person's own instructions changed: said as such. Without its message in view, it ends the list.
  const settings = fold(t, record(20, { type: 'briefing_updated', text: '[Lazybox] …', status: 'instructions', operationId: 'gone' }));
  assert.equal(systemEventView(settings.messages.at(-1)!.origin!, 'Codex').line, 'Lazybox gave Codex its updated instructions');
});

test('a question between agents is one event, not the question twice', () => {
  cursor = 0n;
  const line = 'Codex asked Claude: why does Handle log instead of returning?';
  // In the asked conversation the question arrives as a message, then the review's line.
  let t = fold(emptyTranscript(),
    record(0, { type: 'operation_accepted', text: `[Lazybox] Codex (reviewing your work) asks you:\n\n> why…`, operationId: 'review-ask-1', data: { origin: { kind: 'agent_ask', summary: line, from: 'Codex' } } }),
    record(5, { type: 'review_exchange', text: line, data: { review: 'rv-1', workspace: 'w' } }),
  );
  assert.deepEqual(t.messages.map(m => m.origin?.kind || m.type), ['agent_ask']);
  // In the asker's conversation the answer's line comes first, the answer as a message later.
  const answer = 'Claude answered Codex: it predates the error type; fixing it.';
  t = fold(emptyTranscript(),
    record(10, { type: 'review_exchange', text: answer, data: { review: 'rv-1', workspace: 'w' } }),
    record(15, { type: 'message_delta', text: 'Waiting on the fix', turnId: 't' }),
    record(20, { type: 'operation_accepted', text: '[Lazybox] Claude answered your question…', operationId: 'review-answer-1', data: { origin: { kind: 'agent_answer', summary: answer, from: 'Claude' } } }),
  );
  assert.deepEqual(t.messages.map(m => m.origin?.kind || m.type), ['message_delta', 'agent_answer']);
});

test('restarts that interrupted nothing are one quiet mark per stretch the person did not speak', () => {
  cursor = 0n;
  let t = fold(emptyTranscript(), record(0, { type: 'operation_accepted', text: 'go', operationId: 'p1' }), record(1, { type: 'message_delta', text: 'Working', turnId: 't' }));
  for (let i = 0; i < 13; i++) t = fold(t, record(1000 * (i + 2), { type: 'runner_reattached', status: 'native_process_adopted' }), record(1000 * (i + 2) + 1, { type: 'message_delta', text: ` step ${i}`, turnId: `t${i}` }));
  const marks = t.messages.filter(m => m.type === 'reattached');
  assert.equal(marks.length, 1, 'never repeated per restart');
  assert.equal(marks[0].count, 13);
  assert.equal(marks[0].at, T0 + 14000, 'it knows when the last one was');
  assert.match(restartTitle(13, marks[0].at), /restarted 13 times here, last at .*nothing was interrupted/);
  // A Lazybox message does not end the stretch; the person speaking does.
  t = fold(t, record(20000, { type: 'operation_accepted', text: NOTE, operationId: 'repository-note-1', data: { origin: ADDED } }), record(20001, { type: 'runner_reattached' }));
  assert.equal(t.messages.filter(m => m.type === 'reattached').length, 1);
  t = fold(t, record(30000, { type: 'operation_accepted', text: 'next', operationId: 'p2' }), record(30001, { type: 'runner_reattached' }));
  assert.equal(t.messages.filter(m => m.type === 'reattached').length, 2);
  assert.equal(restartTitle(1), 'The Runner restarted once here. Each time it re-attached to this agent; nothing was interrupted.');
});

test('what a restart left uncertain is one line, said once however often it repeats', () => {
  cursor = 0n;
  const t = fold(emptyTranscript(),
    record(0, { type: 'receipt' }, { operationId: 'a', delivery: 'unknown_after_runner_restart' }),
    record(1, { type: 'receipt' }, { operationId: 'b', delivery: 'unknown_after_runner_restart' }),
    record(2, { type: 'request_resolved', status: 'outcome_unknown_after_restart' }),
  );
  assert.deepEqual(t.messages.map(m => [m.type, m.notice, m.count]), [['notice', 'delivery-unknown', 2], ['notice', 'outcome-unknown', undefined]]);
});

test('an agent lost to a reboot is a notice with the Runner\'s own sentence, not a quiet restart mark', () => {
  cursor = 0n;
  const said = 'Lazybox restarted and this agent was no longer running: the Mac restarted. The conversation is saved: send a message to resume the agent\'s session.';
  const t = fold(emptyTranscript(), record(0, { type: 'agent_lost', status: 'host_gone_after_restart', text: said }));
  assert.deepEqual(t.messages.map(m => [m.type, m.notice, m.text]), [['notice', 'agent-lost', said]]);
});

test('a lost connection the Runner got back is one line of history: lost at, reconnected at', () => {
  cursor = 0n;
  // The Dev journal: the Runner's own write failed, its supervisor ended, and
  // the next Runner re-adopted the agent, whose turn then finished.
  let t = fold(emptyTranscript(),
    record(0, { type: 'operation_accepted', text: 'measure the disk', operationId: 'p1' }),
    record(1, { type: 'turn_started' }),
    record(2, { type: 'process_exit', status: 'unknown', text: 'database or disk is full (13)' }),
    record(3, { type: 'process_exit', status: 'unknown', text: 'provider supervisor ended' }),
  );
  const lost = t.messages.filter(m => m.notice === 'connection-lost');
  assert.equal(lost.length, 1, 'the cause and the supervisor\'s end are one loss');
  assert.equal(lost[0].text, 'database or disk is full (13)', 'the cause is kept, for the line\'s hover');
  assert.equal(lost[0].reconnected, undefined);
  assert.match(connectionLine(lost[0].text, lost[0].at), /^Lazybox lost the connection to the agent at \d/);
  t = fold(t,
    record(2_600_000, { type: 'runner_reattached', status: 'native_process_adopted' }),
    record(2_600_001, { type: 'message_delta', text: 'The disk is still full.', turnId: 't' }),
    record(2_600_002, { type: 'turn_ended', status: 'completed' }),
  );
  const line = t.messages.find(m => m.notice === 'connection-lost')!;
  assert.deepEqual(line.reconnected, { at: T0 + 2_600_000 });
  assert.equal(t.messages.filter(m => m.type === 'reattached').length, 0, 'the reconnection is said on the loss\'s line, not as a restart mark');
  assert.match(connectionLine(line.text, line.at, line.reconnected?.at, true), /^Lazybox lost the connection to the agent at \d\d?.\d\d.* · reconnected at \d/);
  // An agent that really exited is said as such.
  assert.match(connectionLine('native process exited (code 1)', T0), /^The agent process stopped at /);
});

test('a Runner that could not save says so once, as its own notice', () => {
  cursor = 0n;
  const said = 'Lazybox couldn\'t save this conversation\'s usage — the disk is full. The agent keeps running and Lazybox keeps trying; free some space.';
  const t = fold(emptyTranscript(), record(0, { type: 'storage_error', status: 'disk_full', text: said }));
  assert.deepEqual(t.messages.map(m => [m.type, m.notice, m.text]), [['notice', 'storage', said]]);
});

test('a Codex outside the versions Lazybox is tested with says so in one plain line', () => {
  cursor = 0n;
  const said = "This Codex version (0.171.0) isn't tested with Lazybox yet; some features may not work. Lazybox is tested with Codex 0.164 to 0.169.";
  const t = fold(emptyTranscript(), record(0, { type: 'provider_notice', status: 'untested_version', text: said }));
  assert.deepEqual(t.messages.map(m => [m.type, m.notice, m.text]), [['notice', 'provider', said]]);
});

test('an "always allow" the agent reports is kept by the Runner, not drawn in the conversation', () => {
  cursor = 0n;
  const t = fold(emptyTranscript(), record(0, { type: 'permission_grant', providerDataJson: new TextEncoder().encode(JSON.stringify({ id: 'g1', label: '`npm test`', tool: 'Bash', exact: 'npm test' })) }));
  assert.deepEqual(t.messages, []);
});

// ── Background work.
const agent = (id: string, kind: 'agent' | 'command', status: SubagentStatus, extra: Partial<Subagent> = {}): Subagent => ({
  id, runnerId: id, parentId: 'root', depth: 0, nativeId: '', anchor: '', name: id, type: '', prompt: '', toolUseId: '', status, error: '', phase: '', phaseTool: '',
  toolCount: 0, reportedTools: 0, progress: '', result: '', summary: '', backgrounded: false, kind, command: '', output: '', outputFile: '', items: [], dropped: 0, children: [], runningTool: '', ...extra,
});

test('background work is counted by kind, never as "19 sub-agents and commands"', () => {
  const agents = [agent('a1', 'agent', 'done'), agent('a2', 'agent', 'failed')];
  const commands = Array.from({ length: 17 }, (_, i) => agent(`c${i}`, 'command', i === 0 ? 'failed' : 'done'));
  const counts = workCounts(agents, commands, a => a.status);
  assert.equal(workTotals(counts), '2 sub-agents · 17 commands');
  assert.equal(workSummary(counts), '2 sub-agents · 17 commands · 0 running · 2 failed');
  assert.equal(runningSummary(counts), 'Nothing running');
  const live = workCounts([agent('a', 'agent', 'running')], [agent('c', 'command', 'starting'), agent('d', 'command', 'waiting')], a => a.status);
  assert.equal(runningSummary(live), '1 sub-agent · 2 commands running');
  assert.equal(workSummary(live), '1 sub-agent · 2 commands · 3 running');
  assert.equal(workTotals(workCounts([], [agent('c', 'command', 'done')], a => a.status)), '1 command');
});

test('the list is newest first, and folds older finished items away but never what runs or failed', () => {
  const items = Array.from({ length: 17 }, (_, i) => ({ id: i, status: i === 15 ? 'failed' : i === 16 ? 'running' : 'done' }));
  const { shown, earlier } = recentWork(items, item => item.status !== 'done', 5);
  assert.deepEqual(shown.map(i => i.id), [0, 1, 2, 3, 4, 15, 16]);
  assert.equal(earlier, 10);
  assert.equal(recentWork(items.slice(0, 3), () => false, 5).earlier, 0);
  const byId: Record<string, Subagent> = { old: agent('old', 'agent', 'done', { children: ['kid'] }), kid: agent('kid', 'agent', 'done', { parentId: 'old', depth: 1 }), recent: agent('recent', 'agent', 'running') };
  assert.deepEqual(newestFirstTree({ byId, order: ['old', 'kid', 'recent'], spawns: {}, aliases: {}, truncated: false }).map(n => `${n.agent.id}:${n.depth}`), ['recent:0', 'old:0', 'kid:1'], 'a sub-agent stays under who started it');
});

test('a background command reads as what the agent said it does, never cut mid-phrase', () => {
  cursor = 0n;
  const bash = (id: string, input: Record<string, unknown>) => record(0, { type: 'tool_activity', data: { type: 'tool_call', id, name: 'Bash', status: 'running', input: { ...input, run_in_background: true } } });
  const started = (id: string, name: string, at = 10) => record(at, { type: 'agent_status', agentId: `claude-tool:${id}`, parentAgentId: 'root', nativeId: `bash_${id}`, status: 'running', name, agentKind: 'local_bash', data: { toolUseId: id, taskType: 'local_bash' } });
  let t = fold(emptyTranscript(),
    bash('t1', { command: 'buf generate && go generate ./...', description: 'Generate the protobuf and Go code' }), started('t1', 'Generate the protobuf and Go code'),
    bash('t2', { command: 'docker compose up -d postgres' }), started('t2', 'Add postgres to the dev stack'),
    bash('t3', { command: 'cd api && PYTHONPATH=src uv run pytest tests -x' }), started('t3', 'cd api && PYTHONPATH=src uv run pytest tests -x'),
  );
  const named = Object.fromEntries(backgroundCommands(t.subagents).map(c => [c.toolUseId, [c.name, c.description || '', c.command]]));
  assert.deepEqual(named.t1, ['Generate the protobuf and Go code', 'Generate the protobuf and Go code', 'buf generate && go generate ./...']);
  assert.deepEqual(named.t2, ['Add postgres to the dev stack', 'Add postgres to the dev stack', 'docker compose up -d postgres'], 'the task’s own description, whole');
  assert.deepEqual(named.t3, ['pytest tests', '', 'cd api && PYTHONPATH=src uv run pytest tests -x'], 'no description: named by what runs');
  // The task may be reported before its shell call.
  cursor = 0n;
  t = fold(emptyTranscript(), started('t4', 'Seed the database'), bash('t4', { command: 'make seed' }));
  const late = t.subagents.byId['claude-tool:t4'];
  assert.deepEqual([late.name, late.command], ['Seed the database', 'make seed']);
});

// ── Usage.
const group = (fields: Record<string, unknown>) => create(UsageGroupSchema, fields as never);

test('usage says what tokens are made of and what each part cost', () => {
  const usage = usageBreakdown([
    group({ model: 'claude-sonnet-4-6', agent: 'root', records: 2n, unpriced: 0n, incomplete: 0n, nanos: 3300000n,
      tokens: { input: 200n, output: 40n, cacheRead: 2000n, cacheWrite5m: 400n, cacheWrite1h: 0n },
      costs: { input: 600000n, output: 600000n, cacheRead: 600000n, cacheWrite5m: 1500000n, cacheWrite1h: 0n } }),
  ]);
  assert.deepEqual(usage.parts.map(p => [p.label, p.tokens, p.nanos]), [['Input', 200n, 600000n], ['Cache read', 2000n, 600000n], ['Cache write', 400n, 1500000n], ['Output', 40n, 600000n]]);
  assert.equal(usage.tokens, 2640n);
  assert.equal(usage.nanos, 3300000n);
  assert.equal(dollars(usage.nanos), '$0.0033');
  assert.ok(usage.partsKnown);
  assert.equal(usage.cached?.toFixed(3), (2000 / 2600).toFixed(3), 'how much input the cache served');
  assert.deepEqual(usage.unpriced, []);
});

test('unpriced usage says which model and why, instead of "+ unpriced"', () => {
  const usage = usageBreakdown([
    group({ model: 'claude-sonnet-4-6', agent: 'root', records: 3n, unpriced: 1n, incomplete: 0n, nanos: 1650000n,
      tokens: { input: 200n, output: 40n, cacheRead: 2000n, cacheWrite5m: 400n, cacheWrite1h: 0n },
      costs: { input: 300000n, output: 300000n, cacheRead: 300000n, cacheWrite5m: 750000n, cacheWrite1h: 0n },
      unpricedReasons: ['Additional tool fees are not priced'], unpricedTokens: { input: 100n, output: 20n, cacheRead: 1000n, cacheWrite5m: 200n, cacheWrite1h: 0n } }),
    group({ model: 'gpt-5.5', agent: 'root', records: 1n, unpriced: 1n, incomplete: 1n, tokens: { input: 900n, output: 100n, cacheRead: 9000n }, unpricedReasons: ['Provider cumulative usage; per-request model price unavailable'] }),
    group({ model: 'claude-fable-9', agent: 'reviewer', records: 1n, unpriced: 1n, incomplete: 0n, tokens: { input: 10n, output: 5n }, unpricedReasons: ['No exact model price'] }),
  ]);
  assert.deepEqual(usage.unpriced.map(u => [unpricedLine(u), u.tokens]), [
    ['claude-sonnet-4-6: Additional tool fees are not priced', 1320n],
    ['gpt-5.5: Provider cumulative usage; per-request model price unavailable', 10000n],
    ['claude-fable-9 (reviewer): No exact model price', 15n],
  ]);
  assert.equal(usage.nanos, 1650000n, 'the cost covers the priced usage only');
  assert.equal(usage.incomplete, 1);
  assert.equal(usage.parts[0].tokens, 1110n, 'tokens count everything reported');
  assert.match(unpricedHelp(['No exact model price']), /Settings → Pricing/);
  assert.match(unpricedHelp(['Provider cumulative usage; per-request model price unavailable']), /running total/);
  // A group priced before parts were recorded: its total stands, its parts are unknown.
  const legacy = usageBreakdown([group({ model: 'm', records: 1n, unpriced: 0n, incomplete: 0n, nanos: 999n, tokens: { input: 1n, output: 1n } })]);
  assert.equal(legacy.nanos, 999n);
  assert.equal(legacy.partsKnown, false);
  assert.ok(legacy.parts.every(p => p.nanos === undefined));
  assert.equal(dollars(undefined), '—');
  assert.equal(dollars(12_300_000_000n), '$12.30');
  assert.equal(dollars(10n), '<$0.0001');
});
