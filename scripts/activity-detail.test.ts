import test from 'node:test';
import assert from 'node:assert/strict';
import { applyToolPayload, briefItems, categoryOf, describeToolBrief, shortCommand, summarizeRun, summarizeTurn, turnActivity, unfolded, type ToolEntry, type TurnCache } from '../packages/surfaces/src/tool-lines.ts';
import { plainLazybox } from '../packages/surfaces/src/lazybox-cli.ts';

const ROOT = '/Users/me/ws/alpha';
let clock = 0, ids = 0;
/** A finished call: `ms` long, started after the previous one. */
const call = (name: string, input: unknown, output: unknown = 'ok', { ms = 300, failed = false, gap = 1000 } = {}): ToolEntry => {
  const id = `${name}-${++ids}`, start = (clock += gap);
  clock += ms;
  const started = applyToolPayload(undefined, { type: 'tool_call', id, name, status: 'running', input }, start)!;
  return applyToolPayload(started, { type: 'tool_call', id, status: failed ? 'failed' : 'completed', output }, start + ms)!;
};
const bash = (command: string, extra: Record<string, unknown> = {}, options = {}) => call('Bash', { command, ...extra }, 'ok', options);
const running = (name: string, input: unknown): ToolEntry => applyToolPayload(undefined, { type: 'tool_call', id: `${name}-${++ids}`, name, status: 'running', input }, (clock += 1000))!;
const thinking = (ms: number): ToolEntry => { const start = (clock += 100); clock += ms; return { id: `thinking-${++ids}`, name: 'Thinking', status: 'completed', startedAt: start, endedAt: start + ms }; };

test('a command reads as its program and what it acts on, never a script', () => {
  const cases: [string, string][] = [
    [`sed -n '1,200p' /Users/me/ws/alpha/services/runner/code/internal/runtime/runtime_access.go`, 'sed …/runtime_access.go'],
    [`cd /Users/me/ws/alpha && sed -n '40,80p' internal/x.go`, 'sed internal/x.go'],
    ['npm run test:client -- --watch', 'npm run test:client'],
    ['cd v2 && make -C v2 deps', 'make deps'],
    ['git -C /Users/me/ws/alpha status --short', 'git status'],
    ['go test ./... -run TestLogin -count 50', 'go test ./...'],
    ['rg -n "parent_tool_use_id" workers/claude-sdk/src', 'rg parent_tool_use_id'],
    ['grep -rn -A 3 createCache src | head -20', 'grep createCache'],
    [`/bin/zsh -lc "sed -n '1,120p' packages/surfaces/src/tool-lines.ts"`, 'sed …/tool-lines.ts'],
    ['FOO=1 BAR=2 timeout 30 npx playwright test e2e/chat.spec.ts', 'playwright e2e/chat.spec.ts'],
    [`python3 - <<'EOF'\nimport json\nprint(json.load(open("a.json")))\nEOF`, 'python3'],
    [`cat > /tmp/notes/very/long/folder/plan.md <<'EOF'\n# Plan\nEOF`, 'cat > …/plan.md'],
    ['# look around\nls -la src\necho done', 'ls src'],
    [`cd ${ROOT}\nsed -n '1,200p' services/runner/code/internal/runtime/grants.go\necho ---\nrg -n "resolveAccess" services`, 'sed …/grants.go'],
    ['go test ./internal/runtime/... -count 1', 'go test ./internal/runtime/...'],
    ['echo "---" && cat package.json', 'cat package.json'],
    ['gh pr view 1963 --json title', 'gh pr view'],
    ['cargo test -p lazybox-core 2>&1 | tail -30', 'cargo test'],
    ['node --import tsx --test scripts/activity-detail.test.ts', 'node …/activity-detail.test.ts'],
    // `lazybox run` reads as what it runs, past its `--`.
    ['lazybox run -- go test ./...', 'lazybox run go test ./...'],
    ['lazybox run --raw -- npm run test:client -- --watch', 'lazybox run npm run test:client'],
    ['lazybox run go test ./ledger/...', 'lazybox run go test ./ledger/...'],
  ];
  for (const [command, short] of cases) assert.equal(shortCommand(command), short, command);
  assert.ok(shortCommand('x'.repeat(200)).length <= 60, 'bounded');
});

test('the Lazybox CLI by its full path or $LAZYBOX_V2_CLI reads as lazybox', () => {
  // The briefing's fallback when another lazybox (V1's) comes first on the agent's PATH.
  const forms = ['/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox', '"$LAZYBOX_V2_CLI"', '$LAZYBOX_V2_CLI', '"${LAZYBOX_V2_CLI}"', `'/Users/me/Library/Application Support/Lazybox/bin/lazybox'`];
  for (const rest of ['run -- go test ./...', 'run --raw -- git diff', 'artifact put plan.md --title Plan', 'workspace request-repo api']) {
    const plain = `lazybox ${rest}`;
    for (const cli of forms) {
      const command = `${cli} ${rest}`;
      assert.equal(plainLazybox(command), plain, command);
      assert.equal(shortCommand(command), shortCommand(plain), command);
      assert.equal(categoryOf(bash(command)), categoryOf(bash(plain)), command);
      // As Codex reports it, wrapped in its shell (single-quoted, as shlex joins it).
      if (!command.includes("'")) assert.equal(shortCommand(`/bin/zsh -lc '${command}'`), shortCommand(plain), `zsh -lc ${command}`);
    }
  }
  assert.equal(categoryOf(bash('"$LAZYBOX_V2_CLI" run -- go test ./...')), 'test');
  // Nothing else is rewritten: another program, or lazybox as a word in an argument.
  for (const other of ['/usr/bin/notlazybox run', 'echo lazybox', 'cat /tmp/lazybox.log', 'ls /opt/lazybox/bin', 'echo $LAZYBOX_V2_CLI_DIR'])
    assert.equal(plainLazybox(other), other, other);
});

test('what a call was for names the run: explore, tests, checks, installs, git', () => {
  const kinds = (commands: string[]) => commands.map(command => categoryOf(bash(command)));
  assert.deepEqual(kinds(['sed -n 1,5p src/test_utils.py', 'rg foo', 'git log --oneline -5', 'ls']), ['explore', 'explore', 'explore', 'explore']);
  assert.deepEqual(kinds(['npm run test:client', 'go test ./...', 'cargo nextest run', 'node scripts/test-chat.mjs', 'pytest -q']), ['test', 'test', 'test', 'test', 'test']);
  assert.deepEqual(kinds(['npm ci', 'pip install -r requirements.txt', 'make deps', 'npm run typecheck', 'cargo clippy', 'npm run build', 'make', 'git commit -m x', 'curl https://x.dev']), ['install', 'install', 'install', 'check', 'check', 'build', 'build', 'git', 'command']);
  assert.equal(categoryOf(call('Read', { file_path: '/w/a' })), 'explore');
  assert.equal(categoryOf(call('WebFetch', { url: 'https://x.dev' })), 'web');
});

test('Smart lines speak in the agent’s words, without exit 0 or sub-second durations', () => {
  const described = bash(`cd ${ROOT} && sed -n '1,200p' internal/runtime/runtime_access.go`, { description: 'Read the access check.' });
  assert.deepEqual([describeToolBrief(described, ROOT).verb, describeToolBrief(described, ROOT).target, describeToolBrief(described, ROOT).meta, describeToolBrief(described, ROOT).aside], ['Read the access check', 'sed …/runtime_access.go', [], true]);
  const slow = bash('go test ./...', {}, { ms: 4200 });
  assert.deepEqual([describeToolBrief(slow).verb, describeToolBrief(slow).target, describeToolBrief(slow).meta], ['Ran', 'go test ./...', ['4.2s']]);
  const failing = call('Bash', { command: 'go test ./...' }, [{ type: 'text', text: 'Exit code 1\n--- FAIL: TestX' }], { failed: true, ms: 200 });
  assert.deepEqual([describeToolBrief(failing).meta, describeToolBrief(failing).problem], [['exit 1'], '--- FAIL: TestX']);
});

/** The user's transcript: a long run of shell reads between two sentences. */
const longRun = () => [
  thinking(1400),
  ...['runtime_access.go', 'policy.go', 'owner.go', 'session.go'].map(file => bash(`cd ${ROOT} && sed -n '1,200p' services/runner/code/internal/runtime/${file}`, {}, { ms: 100 })),
  thinking(1400), bash('rg -n "resolveAccess" services', {}, { ms: 100 }), thinking(1200),
  call('Read', { file_path: `${ROOT}/a.go` }), call('Read', { file_path: `${ROOT}/b.go` }), call('Read', { file_path: `${ROOT}/a.go` }),
  bash('git log --oneline -5', {}, { ms: 100 }), bash('ls services', {}, { ms: 100 }), bash('wc -l x.go', {}, { ms: 100 }),
];

test('Smart folds a run into one summary; failures, edits and the live plan stay', () => {
  const run = longRun();
  const items = briefItems(run);
  assert.equal(items.length, 1);
  assert.equal(items[0].kind, 'run');
  const summary = summarizeRun((items[0] as any).entries);
  assert.equal(summary.text, 'Explored the codebase');
  assert.equal(summary.meta[0], '8 commands, 2 files read');
  assert.match(summary.meta.at(-1)!, /^\d+s$/, 'the run’s wall time');
  assert.ok(!summary.meta.some(m => m.startsWith('thought')), 'short thinking says nothing');
  // Opened: each call, no short thinking.
  assert.equal(unfolded((items[0] as any).entries).length, 11);

  const failed = call('Bash', { command: 'npm test' }, 'Exit code 1\nFAIL', { failed: true });
  const edit = call('Edit', { file_path: `${ROOT}/a.go`, old_string: 'a', new_string: 'b' });
  const edit2 = call('Write', { file_path: `${ROOT}/c.go`, content: 'x' });
  const plan = call('TodoWrite', { todos: [{ content: 'x', status: 'in_progress' }] });
  const mixed = [bash('ls'), edit, edit2, bash('cat a'), failed, bash('npm test'), plan, thinking(14000)];
  const brief = briefItems(mixed, plan.id);
  assert.deepEqual(brief.map(i => i.kind === 'call' ? i.entry.name : i.kind), ['run', 'edits', 'Bash', 'TodoWrite']);
  const s = summarizeRun((brief[0] as any).entries);
  assert.equal(s.text, 'Ran tests and explored the codebase');
  assert.ok(s.meta.includes('thought for 14s'), s.meta.join(' | '));
  // An earlier plan folds into the run; a run of one call is that call.
  assert.deepEqual(briefItems([bash('ls'), plan], 'other').map(i => i.kind), ['run']);
  assert.deepEqual(briefItems([thinking(500), bash('ls')]).map(i => i.kind === 'call' ? i.entry.name : i.kind), ['Bash']);
  assert.deepEqual(briefItems([thinking(500)]), [], 'short thinking alone is nothing');
  assert.deepEqual(briefItems([thinking(12000)]).map(i => i.kind), ['call'], 'long thinking is a line');
});

test('while the agent works, the summary says what it does now', () => {
  const run = [bash('ls'), bash('cat a'), running('Bash', { command: 'npm run test:client' })];
  const summary = summarizeRun(run);
  assert.deepEqual([summary.text, summary.running], ['Running tests and exploring the codebase', true]);
  assert.ok(!summary.meta.some(m => /^\d+(\.\d)?s$/.test(m)), 'no total while it runs');
  const live: ToolEntry = { id: 'think', name: 'Thinking', status: 'running', startedAt: clock };
  assert.deepEqual(unfolded([bash('ls'), live]).map(e => e.name), ['Bash', 'Thinking'], 'running thinking shows at the end');
});

test('Quiet: one line per turn, edits kept, failures counted', () => {
  const failed = call('Bash', { command: 'npm test' }, 'Exit code 1', { failed: true });
  const edit = call('Edit', { file_path: `${ROOT}/a.go`, old_string: 'a', new_string: 'b' });
  const first = [bash('ls'), failed], second = [edit, bash('npm test', {}, { ms: 60_000 })];
  const turn = summarizeTurn([...first, ...second]);
  assert.match(turn.text, /^Worked for 1m \d+s$/);
  assert.deepEqual(turn.meta, ['4 steps', '1 failed']);
  assert.deepEqual(briefItems(second, '', true).map(i => i.kind === 'call' ? i.entry.name : i.kind), ['Edit']);
  assert.deepEqual(briefItems(first, '', true), [], 'a failure is counted on the turn line');
  const now = running('Bash', { command: 'go test ./...' });
  assert.deepEqual(briefItems([bash('ls'), now], '', true).map(i => i.kind === 'call' && i.entry.id), [now.id], 'the step running now shows where the agent is');
  assert.equal(summarizeTurn([bash('ls'), now]).text, 'Working');
  // The turn's calls are found on its first run, and reused while unchanged.
  const messages = [
    { id: '1', type: 'operation_accepted', turnId: 't1' }, { id: '2', type: 'tools', turnId: 't1', tools: first },
    { id: '3', type: 'message_delta', turnId: 't1' }, { id: '4', type: 'tools', turnId: 't1', tools: second },
    { id: '5', type: 'tools', turnId: 't2', tools: [bash('ls')] },
  ];
  const cache: TurnCache = new Map();
  const turns = turnActivity(messages, cache);
  assert.deepEqual([...turns.keys()], ['2', '5']);
  assert.equal(turns.get('2')!.length, 4);
  assert.equal(turnActivity([...messages], cache).get('2'), turns.get('2'), 'the same array while the turn is unchanged');
  assert.deepEqual([...turnActivity(messages, cache, 3).keys()], ['4', '5'], 'the line moves to the first run on screen');
});
