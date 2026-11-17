// A deterministic, realistic long agent conversation for transcript
// performance checks. It mirrors what the Claude worker journals: assistant
// prose streamed as many small text deltas, tool calls with large outputs,
// usage reports, code blocks in several languages and a few Mermaid diagrams.
// Tool outputs stop at 32 KB, where the worker truncates its tool previews.
const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');

function* chunks(text, size) { for (let i = 0; i < text.length; i += size) yield text.slice(i, i + size); }

const ts = `export async function retry<T>(work: () => Promise<T>, attempts = 3): Promise<T> {
  let last: unknown;
  for (let i = 0; i < attempts; i++) {
    try { return await work(); } catch (error) { last = error; await new Promise(r => setTimeout(r, 2 ** i * 100)); }
  }
  throw last;
}`;
const go = `func (s *Service) Watch(ctx context.Context, id string) error {
\tfor {
\t\trecord, err := s.journal.Next(ctx, s.cursor)
\t\tif err != nil {
\t\t\treturn fmt.Errorf("watch %s: %w", id, err)
\t\t}
\t\ts.cursor = record.Sequence
\t}
}`;
const diagram = n => `flowchart TD
  Client${n}[Browser client] -->|WatchEvents| Runner${n}
  Runner${n} --> Journal${n}[(Journal)]
  Runner${n} --> Engine${n}{Engine}
  Engine${n} -->|deltas| Worker${n}[Claude worker]
  Worker${n} -->|tool calls| Tools${n}[Tools]
  Tools${n} --> Engine${n}
  Engine${n} --> Journal${n}`;

function answer(turn) {
  const parts = [`## Step ${turn}: what changed\n\nI looked at the transcript reader and the journal. The replay path folds each event, then publishes. Here is the reasoning for turn ${turn}, written the way a long agent answer reads in practice, with enough prose to wrap across several lines in the chat pane.`];
  parts.push(`- The reader keeps a cursor per view.\n- Each delta extends the last message.\n- Tool output stays in the activity panel.`);
  if (turn % 2 === 0) parts.push('```ts\n' + ts + '\n```');
  if (turn % 3 === 0) parts.push('```go\n' + go + '\n```');
  if (turn % 5 === 0) parts.push('| File | Lines | State |\n|---|---|---|\n| transcript-model.ts | 78 | changed |\n| RichContent.tsx | 69 | changed |\n| journal.go | 140 | read |');
  if (turn % 50 === 25) parts.push('```mermaid\n' + diagram(turn) + '\n```');
  parts.push(`That completes turn ${turn}. ANSWER_END_${turn}`);
  return parts.join('\n\n');
}

/** Returns Runner journal events (without cursors) for a conversation of `turns` turns. */
export function longConversation({ turns = 200, deltaSize = 12, toolOutputBytes = 8000, model = 'claude-fixture-model' } = {}) {
  const events = [{ type: 'session_ready', providerDataJson: b64({ model }) }];
  for (let turn = 0; turn < turns; turn++) {
    const turnId = `turn-${turn}`;
    events.push({ type: 'operation_accepted', text: `Question ${turn}: please continue with the next step of the refactor and explain it.`, turnId });
    events.push({ type: 'turn_started', turnId });
    events.push({ type: 'activity', status: 'thinking', turnId });
    for (let t = 0; t < 3; t++) {
      const id = `tool-${turn}-${t}`;
      events.push({ type: 'activity', status: 'tool', name: 'Bash', turnId });
      events.push({ type: 'tool_activity', turnId, providerDataJson: b64({ type: 'tool_call', id, name: 'Bash', status: 'running', input: { command: `rg -n "appendTranscript" packages/ # ${turn}.${t}`, description: `Search the code base (${turn}.${t})` } }) });
      const line = `packages/surfaces/src/transcript-model.ts:${t}: export function appendTranscript(state, record) { /* ${turn} */ }\n`;
      events.push({ type: 'tool_activity', turnId, providerDataJson: b64({ type: 'tool_call', id, status: 'completed', output: line.repeat(Math.ceil((t === 2 ? 32000 : toolOutputBytes) / line.length)) }) });
    }
    events.push({ type: 'activity', status: 'responding', turnId });
    for (const text of chunks(answer(turn), deltaSize)) events.push({ type: 'message_delta', text, turnId });
    events.push({ type: 'usage', turnId, providerDataJson: b64({ model, input_tokens: 1200 + turn, output_tokens: 400 }) });
    events.push({ type: 'turn_ended', status: 'completed', turnId });
  }
  return events;
}

const tool = (id, command, output) => [
  { type: 'activity', status: 'tool', name: 'Bash' },
  { type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, name: 'Bash', status: 'running', input: { command, description: command } }) },
  { type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, status: 'completed', output }) },
  { type: 'activity', status: 'working' },
  { type: 'activity', status: 'requesting' },
];

/**
 * What a busy Claude conversation journals after a Runner restart (no turn
 * IDs), shaped after a real one whose view stopped at the restart: the next
 * Runner's `runner_reattached`, a message that starts a turn, a message sent
 * mid-turn (queued, journaled when accepted), Send now (an interrupt, then the
 * queued message is delivered: its receipt), another message queued behind it,
 * and the delivered message's turn, still working. `receipt` events carry their
 * receipt for the wire.
 */
export function deliveredQueuedTurn() {
  const accepted = (operationId, text) => ({ type: 'operation_accepted', operationId, text, providerDataJson: b64({ files: [] }) });
  const receipt = (operationId, delivery) => ({ type: 'receipt', receipt: { operationId, delivery } });
  const events = [
    { type: 'runner_reattached', status: 'native_process_adopted' },
    accepted('op-roles', 'Make the fixture global: two roles, admin and user.'),
    receipt('op-roles', 'submitted'),
    { type: 'session_ready', providerDataJson: b64({ model: 'claude-fixture-model' }) },
    { type: 'turn_started' },
    { type: 'activity', status: 'thinking' },
    { type: 'activity', status: 'responding' },
    { type: 'message_delta', text: 'You want one admin team and one user team. ROLES_ANSWER' },
    ...tool('toolu_roles', 'grep -rn TeamFor libraries/', 'teams.go:18: func ConventionalTeamName'),
    accepted('op-teams', 'QUEUED_DELIVERED: only two teams and two users in every solution.'),
    ...tool('toolu_fixture', 'sed -n 1,90p module/fixtures/dev-admin.yaml', '# dev-admin fixture'),
    accepted('op-teams:now', ''), // Send now: the interrupt it applies (no text)
    receipt('op-teams:now', 'acknowledged'),
    { type: 'turn_ended', status: 'interrupted' },
    accepted('op-continue', 'QUEUED_BEHIND: ok so continue'),
    receipt('op-teams', 'submitted'),
    { type: 'session_ready', providerDataJson: b64({ model: 'claude-fixture-model' }) },
    { type: 'turn_started' },
    { type: 'activity', status: 'thinking' },
    ...tool('toolu_plan', 'lazybox artifact put plan.md', 'Published “Authority fixture: two teams, two people” LATEST_TOOL_OUTPUT'),
    ...tool('toolu_baseline', 'go test ./libraries/authority-core/...', 'ok  authority-core  LATEST_TURN_STILL_WORKING'),
  ];
  return { events, operations: { delivered: 'op-teams', queued: 'op-continue' } };
}
