import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { agentRun, backgroundReport, fixRequestMessage, inlineCommand, isShellLanguage, matchingAction, normalCommand, outputTail, requestKind, serverLike, serverLikeArgv, START_REQUEST, pathProgramsIn, placeholdersIn, programAt, requestedScript, runArgv, runRequestMessage, runsScript, runTitle, scriptOf, shellBlocksIn, shellScript, standaloneCommand } from '../packages/surfaces/src/code-run.ts';

test('shell blocks are the shell languages, transcripts included', () => {
  for (const language of ['bash', 'sh', 'shell', 'zsh', 'console', 'shell-session', 'BASH']) assert.ok(isShellLanguage(language), language);
  for (const language of ['', 'text', 'json', 'go', 'typescript', 'diff', 'powershell']) assert.ok(!isShellLanguage(language), language);
});

test('a block runs as written; a transcript keeps its prompted lines without prompts or output', () => {
  assert.equal(shellScript('acme login --profile dev\nclaude mcp add acme -- acme mcp\n', 'bash'), 'acme login --profile dev\nclaude mcp add acme -- acme mcp');
  // Prompts in a bash block make it a transcript too.
  assert.equal(shellScript('$ make build\nok\n$ make test', 'bash'), 'make build\nmake test');
  assert.equal(shellScript('% npm test\n\n> test\n> vitest\n PASS 3 tests', 'zsh'), 'npm test');
  // A console block: output lines dropped, continuations kept.
  assert.equal(shellScript('$ docker run \\\n    --rm alpine \\\n    echo hi\nhi\n$ echo done\ndone', 'console'), 'docker run \\\n    --rm alpine \\\n    echo hi\necho done');
  assert.equal(shellScript('just output\nno prompt', 'console'), '');
  // A `$` inside a line is not a prompt.
  assert.equal(shellScript('echo $HOME', 'sh'), 'echo $HOME');
});

test('placeholders: angle names, ellipses and YOUR_ names; never redirections, heredocs or ./...', () => {
  assert.deepEqual(placeholdersIn('acme login --profile <your-profile> --token <TOKEN>'), ['<your-profile>', '<TOKEN>']);
  assert.deepEqual(placeholdersIn('claude mcp add …'), ['…']);
  assert.deepEqual(placeholdersIn('curl -H "Authorization: Bearer YOUR_API_KEY" https://x'), ['YOUR_API_KEY']);
  assert.deepEqual(placeholdersIn('gh pr create ...'), ['...']);
  for (const clean of ['go test ./...', 'cat < input.txt > out.txt 2>&1', 'cat <<EOF\nhi\nEOF', 'diff <(ls a) <(ls b)', 'echo "a" >> log', 'make up --solutions none'])
    assert.deepEqual(placeholdersIn(clean), [], clean);
});

test('fenced shell blocks of a message, indented ones included, as the renderer shows them', () => {
  const message = 'First:\n\n```bash\nacme login --profile dev\n```\n\nThen:\n\n1. Add it:\n   ```sh\n   claude mcp add acme\n   ```\n\n```json\n{"a":1}\n```\n\n```console\n$ make test\nok\n```';
  assert.deepEqual(shellBlocksIn(message).map(({language, script}) => ({language, script})), [{ language: 'bash', script: 'acme login --profile dev' }, { language: 'sh', script: 'claude mcp add acme' }, { language: 'console', script: 'make test' }]);
});

test('inline commands: whole commands, and conservative bare subcommands of a path program', () => {
  for (const command of ['bin/devbox check', 'make', 'make lint', 'npm test', 'go test ./...', './run.sh', './scripts/setup.sh --fast', 'git status', 'claude mcp add acme'])
    assert.ok(standaloneCommand(command), command);
  for (const prose of ['build', 'src/main.go', 'bin/devbox', 'npm', 'go', 'useState', 'README.md', 'a\nb', '// comment', 'main'])
    assert.ok(!standaloneCommand(prose), prose);
  assert.equal(inlineCommand('bin/devbox check'), 'bin/devbox check');
  // Next to `bin/devbox …`, a bare subcommand is that program's.
  assert.equal(inlineCommand('build', 'bin/devbox'), 'bin/devbox build');
  assert.equal(inlineCommand('up --solutions none', 'bin/devbox'), 'bin/devbox up --solutions none');
  // Without such a program, or for words that are never subcommands, nothing.
  assert.equal(inlineCommand('build'), '');
  for (const word of ['main', 'true', 'null', 'MyType', 'foo.bar()', 'a b c d e f', 'npm']) assert.equal(inlineCommand(word, 'bin/devbox'), '', word);
  const message = 'Run `bin/devbox check`, then `build`, then `up --solutions none`. Also `./tools/lint fix` then `again`.';
  const programs = pathProgramsIn(message);
  assert.deepEqual(programs.map(p => p.program), ['bin/devbox', './tools/lint']);
  assert.equal(programAt(programs, message.indexOf('`build`')), 'bin/devbox');
  assert.equal(programAt(programs, message.indexOf('`again`')), './tools/lint');
  // A program in a shell block counts too.
  assert.deepEqual(pathProgramsIn('```bash\n$ bin/devbox check\n```\nthen `build`').map(p => p.program), ['bin/devbox']);
});

test('the Run request carries the exact script, fenced, and is recognized back', () => {
  const script = 'acme login --profile dev\nclaude mcp add acme -- acme mcp';
  const text = runRequestMessage(script, 'bash');
  assert.equal(text, 'Run this and tell me the result:\n```bash\nacme login --profile dev\nclaude mcp add acme -- acme mcp\n```');
  assert.equal(requestedScript(text), script);
  // A transcript goes as the script it runs; a fence inside gets a longer fence.
  assert.equal(runRequestMessage('make test', 'console'), 'Run this and tell me the result:\n```sh\nmake test\n```');
  const fenced = runRequestMessage('cat <<EOF\n```\nEOF');
  assert.match(fenced, /^Run this and tell me the result:\n````sh\n/);
  assert.equal(requestedScript(fenced), 'cat <<EOF\n```\nEOF');
  assert.equal(requestedScript('Please run this'), undefined);
});

test("the agent's run of a block is its matching tool call after the request", () => {
  const script = 'acme login --profile dev';
  const tool = (id: string, command: string, status: 'running' | 'completed' | 'failed', exitCode?: number) => ({ id, name: 'Bash', status, input: { command }, exitCode });
  const messages = [
    { id: '1', type: 'message_delta', text: 'Do this:\n```bash\nacme login --profile dev\n```' },
    { id: '2', type: 'operation_accepted', text: runRequestMessage(script, 'bash'), operationId: 'op-1' },
    { id: '3', type: 'tools', text: '', tools: [tool('a', 'git status', 'completed', 0)] },
  ];
  assert.deepEqual(agentRun(messages, '1', script), { operationId: 'op-1', kind: 'run', outcome: undefined });
  messages.push({ id: '4', type: 'tools', text: '', tools: [tool('b', 'cd /w && lazybox run -- acme login --profile dev', 'running')] });
  assert.deepEqual(agentRun(messages, '1', script)?.outcome, { state: 'running' });
  messages[3].tools![0] = tool('b', 'cd /w && lazybox run -- acme login --profile dev', 'completed', 2);
  assert.deepEqual(agentRun(messages, '1', script)?.outcome, { state: 'failed', exitCode: 2 });
  // The agent fixed it and ran it again: the latest call counts.
  messages.push({ id: '5', type: 'tools', text: '', tools: [tool('c', 'lazybox run -- acme login   --profile dev', 'completed', 0)] });
  assert.deepEqual(agentRun(messages, '1', script)?.outcome, { state: 'ok', exitCode: 0 });
  // No request: nothing to show. A later message repeating the block owns later requests.
  assert.equal(agentRun(messages, '3', 'make test'), undefined);
  const later = [...messages, { id: '6', type: 'message_delta', text: 'Again:\n```bash\nacme login --profile dev\n```' }, { id: '7', type: 'operation_accepted', text: runRequestMessage(script, 'bash'), operationId: 'op-2' }];
  assert.equal(agentRun(later, '1', script)?.operationId, 'op-1');
  assert.equal(agentRun(later, '6', script)?.operationId, 'op-2');
  assert.ok(runsScript({ name: 'Bash', input: { command: 'sh -c "make build && make test"' } }, 'make build\n# then\nmake test'));
  assert.ok(!runsScript({ name: 'Bash', input: { command: 'ls' } }, 'make build'));
});

test('running it yourself: one shell, a short title, and the end of the output for a fix', () => {
  assert.deepEqual(runArgv('make a\nmake b'), ['/bin/sh', '-lc', 'make a\nmake b']);
  assert.equal(scriptOf(runArgv('make a')), 'make a');
  assert.equal(scriptOf(['/bin/sh', '-c', 'make a']), '');
  assert.equal(runTitle('# set up\nacme login --profile dev\nclaude mcp add'), 'acme login --profile dev');
  assert.equal(runTitle('x'.repeat(80)).length, 60);
  const output = Array.from({ length: 100 }, (_, i) => `\u001b[31mline ${i}\u001b[0m`).join('\n') + '\n';
  const tail = outputTail(output);
  assert.equal(tail.omitted, 40);
  assert.equal(tail.text.split('\n')[0], 'line 40');
  const message = fixRequestMessage({ script: 'make test', folder: 'api', exitCode: 2, output, truncated: false });
  assert.match(message, /^I ran this myself in `api` and it failed \(exit 2\):\n```sh\nmake test\n```\nThe last 60 lines of its output:\n```text\nline 40\n/);
  assert.match(message, /line 99\n```\nFind out why and fix it\.$/);
  assert.match(fixRequestMessage({ script: 'sleep 9', folder: '', signal: 'SIGTERM', output: '', truncated: false }), /^I ran this myself and it was stopped \(SIGTERM\):[\s\S]*\(no output\)/);
});

// The Runner's own vectors (services/runner/code/internal/serverlike): Go and
// this mirror read the same file, so `lazybox run` and Run on a block agree.
test('servers and watchers: the same rules as the Runner, on its shared vectors', () => {
  const vectors = JSON.parse(readFileSync(new URL('../services/runner/code/internal/serverlike/testdata/commands.json', import.meta.url), 'utf8')) as { script: string; server: boolean }[];
  assert.ok(vectors.length >= 40);
  for (const { script, server } of vectors) assert.equal(serverLike(script).server, server, script);
  assert.equal(serverLike('cd web && npm run dev').reason, 'npm run dev');
  assert.equal(serverLikeArgv(['/bin/sh', '-lc', 'cd web && npm run dev']).server, true);
  assert.equal(serverLikeArgv(['docker', 'compose', 'up', '-d']).server, false);
});

test('a server is asked to start in the background, by its workspace action when there is one, and recognized back', () => {
  const start = runRequestMessage('npm run dev', 'bash', { server: true });
  assert.equal(start, `${START_REQUEST}\n\`\`\`bash\nnpm run dev\n\`\`\``);
  assert.equal(requestKind(start), 'start');
  assert.equal(requestedScript(start), 'npm run dev');
  const action = runRequestMessage('npm run dev', 'sh', { server: true, action: 'api/dev' });
  assert.match(action, /^Start the workspace action `api\/dev` \(it runs this\) and tell me when it is ready:\n```sh\nnpm run dev\n```$/);
  assert.equal(requestKind(action), 'start');
  assert.equal(requestedScript(action), 'npm run dev');
  assert.equal(requestKind(runRequestMessage('make test')), 'run');
});

test('a start the agent moved to the background reads as running there, with its port', () => {
  const script = 'npm run dev';
  const report = '[lazybox] npm run dev → running in the background as command ABC123 · listening on http://localhost:5173 · output: /tmp/x.log · stop: `lazybox stop ABC123`';
  assert.deepEqual(backgroundReport(report), { commandId: 'ABC123', port: 5173 });
  assert.equal(backgroundReport('[lazybox] make test → exit 0'), undefined);
  type Tool = { id: string; name: string; status: 'running' | 'completed' | 'failed'; input: Record<string, unknown>; output?: string; exitCode?: number };
  const messages: { id: string; type: string; text: string; operationId?: string; tools?: Tool[] }[] = [
    { id: '1', type: 'message_delta', text: '```bash\nnpm run dev\n```' },
    { id: '2', type: 'operation_accepted', text: runRequestMessage(script, 'bash', { server: true }), operationId: 'op-1' },
    { id: '3', type: 'tools', text: '', tools: [{ id: 'a', name: 'Bash', status: 'running', input: { command: 'lazybox run -- npm run dev' } }] },
  ];
  assert.deepEqual(agentRun(messages, '1', script)?.outcome, { state: 'running' });
  messages[2].tools![0] = { id: 'a', name: 'Bash', status: 'completed', input: { command: 'lazybox run -- npm run dev' }, output: report, exitCode: 0 };
  assert.deepEqual(agentRun(messages, '1', script), { operationId: 'op-1', kind: 'start', outcome: { state: 'background', port: 5173 } });
  // An action request is answered by the command tool's start_action.
  const byAction = [messages[0], { id: '2', type: 'operation_accepted', text: runRequestMessage(script, 'sh', { server: true, action: 'api/dev' }) },
    { id: '3', type: 'tools', text: '', tools: [{ id: 'b', name: 'mcp__lazybox__command', status: 'completed' as const, input: { action: 'start_action', name: 'api/dev' } }] }];
  assert.deepEqual(agentRun(byAction, '1', script)?.outcome, { state: 'background' });
});

test('a block stands for the workspace action with the same command', () => {
  assert.equal(normalCommand('npm run dev -- --port $LAZYBOX_PORT'), 'npm run dev');
  assert.equal(normalCommand('pnpm storybook -p ${LAZYBOX_PORT_1}'), 'pnpm storybook');
  assert.equal(normalCommand('cd api && npm run dev', 'api'), 'npm run dev');
  const actions = [{ key: 'api/dev', member: 'api', run: 'npm run dev -- --port $LAZYBOX_PORT', blocked: '' }, { key: 'web/storybook', member: 'web', run: 'pnpm storybook -p $LAZYBOX_PORT_1', blocked: 'Approve web’s scripts first' }];
  assert.equal(matchingAction(actions, 'npm run dev')?.key, 'api/dev');
  assert.equal(matchingAction(actions, 'cd api && npm  run dev')?.key, 'api/dev');
  assert.equal(matchingAction(actions, 'pnpm storybook'), undefined, 'a blocked action runs as a command');
  assert.equal(matchingAction(actions, 'npm run dev:api'), undefined);
  assert.equal(matchingAction(actions, 'npm ci\nnpm run dev'), undefined, 'a longer script is not the action');
});
