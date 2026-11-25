import { create } from '@bufbuild/protobuf';
import { CommandStatusSchema, JournalEventSchema, type JournalEvent } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { commandParts, mayNeverEnd, offersBackground, outputLines, QUIET_MS, rootStep, runningCommand, stepDetail } from '../packages/surfaces/src/live-step.ts';

let cursor = 0n;
const record = (type: string, data?: unknown, extra: Record<string, string> = {}): JournalEvent => create(JournalEventSchema, {
  cursor: ++cursor, capturedAt: new Date(1_000_000 + Number(cursor)).toISOString(),
  event: { type, ...extra, providerDataJson: data === undefined ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(data)) },
});
const call = (id: string, name: string, status: string, input?: unknown) => record('tool_activity', { type: 'tool_call', id, name, status, input });

test('the step is the main agent\'s call in progress, not a finished one, a sub-agent\'s, or a spawn', () => {
  const events = [record('turn_started'), call('a', 'Read', 'running', { file_path: 'x' }), call('a', 'Read', 'completed'), call('b', 'Bash', 'running', { command: 'cd services/front-door && pnpm run check' }),
    call('t', 'Task', 'running', { description: 'Review' }), record('tool_activity', { type: 'tool_call', id: 'c', name: 'Grep', status: 'running' }, { agentId: 'claude-tool:t' })];
  const step = rootStep(events);
  assert.equal(step?.id, 'b');
  assert.deepEqual(stepDetail(step!), { code: 'pnpm run check', where: 'services/front-door', full: 'cd services/front-door && pnpm run check' });
  assert.equal(rootStep([...events, call('b', 'Bash', 'completed')]), undefined, 'a recorded result ends the step');
  assert.equal(rootStep([...events, record('activity', undefined, { status: 'thinking' })]), undefined, 'thinking is not a step');
  assert.equal(rootStep([...events, record('turn_ended')]), undefined);
});

test('a step says its command, or what it does in words', () => {
  assert.deepEqual(stepDetail({ id: 'x', name: 'mcp__lazybox__command', input: { action: 'start', argv: ['go', 'test', './...'] } }), { code: 'go test ./...', full: 'go test ./...', where: undefined });
  assert.deepEqual(stepDetail({ id: 'x', name: 'mcp__lazybox__command', input: { action: 'wait', id: 'c1' } }), { words: 'Waiting for its command' });
  assert.deepEqual(stepDetail({ id: 'x', name: 'Read', input: { file_path: 'src/app.ts' } }), { words: 'Reading src/app.ts' });
  assert.deepEqual(commandParts('export A=1\nmake test'), { code: 'export A=1 …', where: undefined });
  assert.deepEqual(commandParts('cd "my dir"; npm test'), { code: 'npm test', where: 'my dir' });
});

test('the running managed command is this conversation\'s own, newest first', () => {
  const command = (id: string, done: boolean, sessionId = 's', origin = '') => create(CommandStatusSchema, { id, done, sessionId, origin });
  assert.equal(runningCommand([command('a', false), command('b', true), command('c', false)], 's')?.id, 'c');
  const newer = { ...command('a', false), startedAtUnixMillis: 20n };
  const older = { ...command('c', false), startedAtUnixMillis: 10n };
  assert.equal(runningCommand([newer, older], 's')?.id, 'a');
  assert.equal(runningCommand([older, newer], 's')?.id, 'a');
  assert.equal(runningCommand([command('a', false, 'other'), command('w', false, 's', 'action')], 's'), undefined, 'workspace runs and other conversations are not this turn\'s');
  // A server `lazybox run` moved to the background is not the turn's step any more.
  assert.equal(runningCommand([command('a', false), create(CommandStatusSchema, { id: 'bg', done: false, sessionId: 's', background: true })], 's')?.id, 'a');
});

test('Move to background: a following lazybox run that listens, looks like a server, or has gone quiet', () => {
  const run = (fields: Record<string, unknown>) => create(CommandStatusSchema, { id: 'r', sessionId: 's', mode: 'pipe', canBackground: true, title: 'go test ./...', ...fields });
  assert.equal(offersBackground(run({}), 0), false, 'a test run that prints is waited for');
  assert.equal(offersBackground(run({ ports: [3000] }), 0), true, 'a listening port says server');
  assert.equal(offersBackground(run({ title: 'npm run dev' }), 0), true, 'so does a dev server');
  assert.equal(offersBackground(run({}), QUIET_MS), true, 'and output quiet for a while');
  assert.equal(offersBackground(run({ canBackground: false, ports: [3000] }), QUIET_MS), false, 'only a run whose CLI follows the Runner can move');
  assert.equal(offersBackground(run({ background: true, ports: [3000] }), QUIET_MS), false);
  // The agent's own shell step: a server may never end; a lazybox run has its own bar.
  assert.equal(mayNeverEnd('npm run dev'), true);
  assert.equal(mayNeverEnd('go test ./...'), false);
  const wrapped = "/bin/zsh -lc 'lazybox run --wait -- /tmp/heavy/slot.sh env FLAG=true node --test suite'";
  assert.deepEqual(commandParts(wrapped), { code: '/tmp/heavy/slot.sh env FLAG=true node --test suite', where: undefined });
  assert.equal(mayNeverEnd(wrapped), false);
  assert.equal(mayNeverEnd('lazybox run -- npm run dev'), false);
  // However the agent calls the CLI: by its full path, or through $LAZYBOX_V2_CLI when another lazybox shadows it.
  for (const cli of ['/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox', '"$LAZYBOX_V2_CLI"', '${LAZYBOX_V2_CLI}', '"/Users/me/Library/Application Support/Lazybox/bin/lazybox"'])
    assert.equal(mayNeverEnd(`${cli} run -- npm run dev`), false, cli);
  assert.equal(commandParts('/usr/local/bin/notlazybox run -- npm run dev').code, '/usr/local/bin/notlazybox run -- npm run dev');
  assert.equal(commandParts(['/bin/zsh', '-lc', 'lazybox run --title "test suite" -- node --test']).code, 'node --test');
});

test('output reads as lines: colour codes gone, redraws resolved', () => {
  assert.deepEqual(outputLines('\x1b[32mok\x1b[0m one\nprogress 10%\rprogress 100%\nlast\n\n', 2), ['progress 100%', 'last']);
});
