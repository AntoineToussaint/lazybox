import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { TerminalStatus } from '../packages/protocol/src/index';
import { HOOKLESS_SETTLE_MS, MARKED_TIMEOUT_MS, SETTLE_MS, UNMARKED_TIMEOUT_MS, reusableTerminal, runsAtOnce, shellInput, shellReadiness, startedIn, type ShellSample } from '../packages/surfaces/src/shell-input';

const at = (ms: number, values: Partial<ShellSample> = {}): ShellSample => ({ at: ms, offset: 0n, promptsReady: 0n, commandEnds: 0n, promptMarks: true, done: false, ...values });

test('a zsh with the integration is ready when it says so, not when its precmd runs', () => {
  // The precmd mark comes first, then a theme draws the prompt: still not ready.
  const starting = [at(0), at(500, { offset: 40n, commandEnds: 1n }), at(900, { offset: 900n, commandEnds: 1n })];
  assert.equal(shellReadiness(starting, 0, 1000), 'wait');
  assert.equal(shellReadiness([...starting, at(1100, { offset: 960n, commandEnds: 1n, promptsReady: 1n })], 0, 1100), 'ready');
  // A slow .zshrc prints nothing for seconds: no settling stands for the mark before the precmd ran.
  assert.equal(shellReadiness([at(0), at(5000)], 0, 5000), 'wait');
  assert.equal(shellReadiness([at(0, { offset: 10n }), at(5000, { offset: 10n })], 0, 5000), 'wait');
});

test('a zsh whose line-init hook was replaced is ready once its prompt settles after the precmd mark', () => {
  const samples = [at(0, { offset: 900n, commandEnds: 1n }), at(HOOKLESS_SETTLE_MS - 1, { offset: 900n, commandEnds: 1n })];
  assert.equal(shellReadiness(samples, 0, HOOKLESS_SETTLE_MS - 1), 'wait');
  assert.equal(shellReadiness([...samples, at(HOOKLESS_SETTLE_MS, { offset: 900n, commandEnds: 1n })], 0, HOOKLESS_SETTLE_MS), 'settled');
});

test('bash and fish, without marks, are ready once their output settles', () => {
  const bash = (ms: number, offset: bigint) => at(ms, { offset, promptMarks: false });
  assert.equal(shellReadiness([bash(0, 0n), bash(2000, 0n)], 0, 2000), 'wait', 'nothing printed yet');
  assert.equal(shellReadiness([bash(0, 30n), bash(300, 80n), bash(600, 80n)], 0, 600), 'wait', 'still printing');
  assert.equal(shellReadiness([bash(0, 30n), bash(300, 80n), bash(300 + SETTLE_MS, 80n)], 0, 300 + SETTLE_MS), 'settled');
});

test('the wait ends: the command is typed after a timeout rather than never, and a shell that exited is gone', () => {
  assert.equal(shellReadiness([at(0)], 0, MARKED_TIMEOUT_MS), 'timeout');
  assert.equal(shellReadiness([at(0, { promptMarks: false })], 0, UNMARKED_TIMEOUT_MS), 'timeout');
  assert.equal(shellReadiness([], 0, UNMARKED_TIMEOUT_MS), 'timeout');
  assert.equal(shellReadiness([at(0), at(100, { done: true })], 0, 100), 'gone');
});

test('Run runs one line with nothing to fill in; a script or a placeholder is typed for review', () => {
  assert.equal(runsAtOnce('gcloud auth login someone@example.com --force --no-launch-browser'), true);
  assert.equal(runsAtOnce('env -u ACME_TOKEN ./bin/acme login staging'), true);
  assert.equal(runsAtOnce('make seed\nmake test'), false);
  assert.equal(runsAtOnce('gcloud config set project <your-project>'), false);
  assert.equal(runsAtOnce('export API_KEY=YOUR_API_KEY'), false);
  assert.equal(shellInput('make seed', true, false), 'make seed\r');
  assert.equal(shellInput('make seed', false, false), 'make seed');
  assert.equal(shellInput('make seed\nmake test', false, false), '\x1b[200~make seed\nmake test\x1b[201~');
  assert.equal(shellInput('make seed', true, true), '\x15make seed\r', 'a reused prompt is cleared first');
});

const terminal = (id: string, values: Partial<TerminalStatus>) => ({ id, place: '', directory: '', repositoryId: '', idle: true, done: false, ...values }) as TerminalStatus;

test('an idle shell started in the same place is reused; a busy one, another place or one being typed into is not', () => {
  const home = { repositoryId: 'backend', label: 'api home' }, root = { directory: '', label: 'the workspace' };
  const shells = [
    terminal('root', {}),
    terminal('api-busy', { place: 'repository-home', repositoryId: 'backend', idle: false }),
    terminal('api-old', { place: 'repository-home', repositoryId: 'backend' }),
    terminal('api', { place: 'repository-home', repositoryId: 'backend' }),
    terminal('web', { place: 'repository-home', repositoryId: 'frontend' }),
    terminal('api-exited', { place: 'repository-home', repositoryId: 'backend', done: true }),
    terminal('home', { place: 'user-home' }),
  ];
  assert.equal(reusableTerminal(shells, home, new Set())?.id, 'api', 'the most recent idle one');
  assert.equal(reusableTerminal(shells, home, new Set(['api']))?.id, 'api-old', 'one being typed into is taken');
  assert.equal(reusableTerminal(shells, home, new Set(['api', 'api-old'])), undefined, 'busy: a new shell');
  assert.equal(reusableTerminal(shells, root, new Set())?.id, 'root');
  assert.equal(reusableTerminal(shells, { directory: 'api', label: 'api' }, new Set()), undefined);
  assert.equal(reusableTerminal(shells, { userHome: true, label: 'your home folder' }, new Set())?.id, 'home');
  assert.equal(startedIn(shells[0], home), false);
});
