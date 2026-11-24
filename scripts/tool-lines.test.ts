import test from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { JournalEventSchema } from '@lazybox/protocol';
import { emptyTranscript, foldTranscript } from '../packages/surfaces/src/transcript-model.ts';
import { applyToolPayload, artifactOutcomes, describeGroup, describeTool, groupTools, kindOf, latestPlanId, lineChange, publishedArtifacts, retiredArtifacts, shortPath, type ToolEntry } from '../packages/surfaces/src/tool-lines.ts';
import { answerNote, approvalOf, decisionForKey, decisionRows, questionsOf, requestNote, risksOf } from '../packages/surfaces/src/request-model.ts';

const ROOT = '/Users/me/ws/alpha';
const call = (payload: Record<string, unknown>, previous?: ToolEntry, at?: number) => applyToolPayload(previous, payload, at)!;
const ran = (name: string, input: unknown, output?: unknown, extra: Record<string, unknown> = {}) => {
  const started = call({ type: 'tool_call', id: `${name}-${JSON.stringify(input).length}`, name, status: 'running', input }, undefined, 1000);
  return output === undefined ? started : call({ type: 'tool_call', id: started.id, status: extra.failed ? 'failed' : 'completed', output, ...extra }, started, 5200);
};

test('an agent\'s `lazybox artifact put` is recognized from what the CLI printed', () => {
  const said = (title: string, id: string, version: number, kind: string) => `Published “${title}” as artifact ${id}, version ${version} (${kind}). The user sees it in Lazybox → Artifacts. Publish a new version with --id ${id}.\n`;
  const first = ran('Bash', { command: 'lazybox artifact put PLAN.md --title "Migration plan" --kind plan', description: 'Publish the plan' }, said('Migration plan', 'ART123', 1, 'plan'));
  assert.deepEqual(publishedArtifacts(first), [{ id: 'ART123', title: 'Migration plan', version: 1, kind: 'plan' }]);
  // A new version, through the full path the briefing falls back to, in Claude's content blocks.
  const update = ran('Bash', { command: '"/Applications/Lazybox.app/Contents/MacOS/lazybox" artifact put - --id ART123 < PLAN.md' }, [{ type: 'text', text: said('Migration plan', 'ART123', 3, 'plan') }]);
  assert.deepEqual(publishedArtifacts(update), [{ id: 'ART123', title: 'Migration plan', version: 3, kind: 'plan' }]);
  // Two in one command; a chained step that failed afterwards does not undo them.
  const two = ran('Bash', { command: '$LAZYBOX_V2_CLI artifact put a.md --title A && lazybox artifact put b.md --title B && false' }, [{ type: 'text', text: 'Exit code 1\n' + said('A', 'ID_A', 1, 'other') + said('B', 'ID-B', 2, 'report') }], { failed: true });
  assert.deepEqual(publishedArtifacts(two).map(p => `${p.id}@${p.version}`), ['ID_A@1', 'ID-B@2']);
  // Codex's shell, the same.
  assert.equal(publishedArtifacts(ran('exec_command', { cmd: 'lazybox artifact put r.md --title R --kind report' }, said('R', 'R1', 1, 'report'))).length, 1);
  // Nothing published: an unchanged put, a read, the line quoted by another command, a call still running, another tool.
  assert.deepEqual(publishedArtifacts(ran('Bash', { command: 'lazybox artifact put - --id ART123' }, 'Artifact ART123 “Migration plan” unchanged at version 3 (same content).\n')), []);
  assert.deepEqual(publishedArtifacts(ran('Bash', { command: 'lazybox artifact get ART123' }, '# Plan\n')), []);
  assert.deepEqual(publishedArtifacts(ran('Bash', { command: 'grep Published /tmp/log' }, said('Old', 'OLD1', 1, 'plan'))), []);
  assert.deepEqual(publishedArtifacts(ran('Bash', { command: 'lazybox artifact put PLAN.md --title P' })), []);
  assert.deepEqual(publishedArtifacts(ran('Read', { file_path: '/tmp/out.txt' }, said('Old', 'OLD1', 1, 'plan'))), []);
});

test('an agent\'s merge (`put --supersedes`) and `retire` are recognized from what the CLI printed', () => {
  const said = (title: string, id: string, version: number) => `Published “${title}” as artifact ${id}, version ${version} (plan). The user sees it in Lazybox → Artifacts. Publish a new version with --id ${id}.\n`;
  const retiredLine = (title: string, id: string, by?: [string, string]) => `Retired “${title}” (artifact ${id})${by ? `, superseded by “${by[0]}” (artifact ${by[1]})` : ''}.`;
  const restorable = 'Retired artifacts keep every version; the person can restore them in Lazybox → Artifacts.\n';
  // Three overlapping artifacts merged into a new status page: one merge, nothing else.
  const merge = ran('Bash', { command: 'lazybox artifact put STATUS.md --title Status --supersedes A1,B2,C3' }, said('Status', 'S9', 1) + [retiredLine('Milestone plan', 'A1', ['Status', 'S9']), retiredLine('What\'s left', 'B2', ['Status', 'S9']), retiredLine('Limitations', 'C3', ['Status', 'S9'])].join('\n') + '\n' + restorable);
  assert.deepEqual(retiredArtifacts(merge).map(r => `${r.id}>${r.supersededBy?.id}`), ['A1>S9', 'B2>S9', 'C3>S9']);
  const outcome = artifactOutcomes(merge);
  assert.deepEqual([outcome.published, outcome.retired, outcome.merges.map(m => m.text)], [[], [], ['Merged 3 artifacts into “Status”']]);
  // Merged into one of them (a new version of it): it counts too.
  const into = artifactOutcomes(ran('Bash', { command: 'lazybox artifact put - --id A1 --supersedes B2,C3' }, said('Milestone plan', 'A1', 4) + retiredLine('What\'s left', 'B2', ['Milestone plan', 'A1']) + '\n' + retiredLine('Limitations', 'C3', ['Milestone plan', 'A1']) + '\n' + restorable));
  assert.deepEqual(into.merges.map(m => [m.text, m.retired.length]), [['Merged 3 artifacts into “Milestone plan”', 2]]);
  // One replaced by a new one.
  assert.deepEqual(artifactOutcomes(ran('Bash', { command: 'lazybox artifact put n.md --title New --supersedes O1' }, said('New', 'N1', 1) + retiredLine('Old', 'O1', ['New', 'N1']) + '\n')).merges.map(m => m.text), ['Replaced “Old” with “New”']);
  // `retire` alone, through the full path of the CLI.
  const alone = artifactOutcomes(ran('Bash', { command: '"$LAZYBOX_V2_CLI" artifact retire C3 --superseded-by S9' }, retiredLine('Limitations', 'C3', ['Status', 'S9']) + ' ' + restorable));
  assert.deepEqual([alone.published, alone.merges, alone.retired], [[], [], [{ id: 'C3', title: 'Limitations', supersededBy: { id: 'S9', title: 'Status' } }]]);
  // A line quoted by another command is not a retirement.
  assert.deepEqual(retiredArtifacts(ran('Bash', { command: 'cat /tmp/log' }, retiredLine('Old', 'O1') + '\n')), []);
});

test('a call reads as a person says it: verb, target, short facts', () => {
  const read = ran('Read', { file_path: `${ROOT}/src/front_door/mcp.py` }, 'a\nb\nc\n');
  assert.deepEqual(describeTool(read, ROOT), { verb: 'Read', target: 'src/front_door/mcp.py', code: true, file: `${ROOT}/src/front_door/mcp.py`, meta: ['3 lines'], problem: undefined });
  const check = ran('Bash', { command: 'pnpm run check', description: 'Run the checks' }, 'ok\n');
  assert.deepEqual([describeTool(check).verb, describeTool(check).target, describeTool(check).meta], ['Ran', 'pnpm run check', ['4.2s']]);
  const failing = ran('Bash', { command: 'pnpm test' }, [{ type: 'text', text: 'Exit code 2\nFAIL src/a.test.ts\n' }], { failed: true });
  const title = describeTool(failing);
  assert.deepEqual([title.meta, title.problem, failing.exitCode], [['exit 2', '4.2s'], 'FAIL src/a.test.ts', 2]);
  const running = call({ type: 'tool_call', id: 'r', name: 'Bash', status: 'running', input: { command: 'sleep 9' } });
  assert.equal(describeTool(running).verb, 'Running');
  assert.deepEqual(describeTool(ran('Grep', { pattern: 'appendTranscript', path: `${ROOT}/packages` }, 'x'), ROOT).target, 'appendTranscript in packages');
  assert.equal(describeTool(ran('WebSearch', { query: 'herdr' }, 'r')).target, '“herdr”');
  assert.equal(describeTool(ran('mcp__github__create_pr', { title: 'x' }, 'ok')).target, 'github · create pr');
  assert.equal(shortPath('/Users/me/elsewhere/x.ts', ROOT), '~/elsewhere/x.ts');
});

test('edits count lines added and removed, and carry a diff', () => {
  const edit = ran('Edit', { file_path: `${ROOT}/a.ts`, old_string: 'one\ntwo\nthree', new_string: 'one\n2\n2b\nthree' }, 'ok');
  assert.deepEqual(edit.diff, { added: 2, removed: 1, files: [`${ROOT}/a.ts`] });
  assert.match(edit.patch!, /^-two$/m); assert.match(edit.patch!, /^\+2b$/m);
  assert.deepEqual(describeTool(edit, ROOT).meta, ['+2 −1']);
  const write = ran('Write', { file_path: `${ROOT}/b.ts`, content: 'x\ny\n' }, 'ok');
  assert.deepEqual([describeTool(write, ROOT).verb, write.diff!.added], ['Wrote', 3]);
  // Codex: a list of { path, kind, diff }.
  const codex = ran('File changes', [{ path: '/w/a.go', kind: { type: 'update' }, diff: '@@\n-a\n+b\n+c\n' }, { path: '/w/b.go', kind: { type: 'add' }, diff: 'x\ny\n' }], 'done');
  assert.deepEqual(codex.diff, { added: 4, removed: 1, files: ['/w/a.go', '/w/b.go'] });
  assert.equal(describeTool(codex).target, '2 files');
  assert.deepEqual(lineChange('a\nb', 'a\nb'), { removed: [], added: [], context: ['a', 'b'] });
});

test('Codex commands keep their exit, duration and parsed reads', () => {
  const cat = call({ type: 'tool_call', id: 'c', name: 'Shell command', status: 'completed', input: 'cat a.go', output: 'package a', exitCode: 0, durationMs: 1200, actions: [{ type: 'read', name: 'a.go', path: '/w/a.go' }] });
  assert.equal(kindOf(cat), 'read');
  assert.deepEqual([describeTool(cat, '/w').target, cat.durationMs], ['a.go', 1200]);
  const test = call({ type: 'tool_call', id: 't', name: 'Shell command', status: 'failed', input: 'go test ./...', output: 'FAIL x', exitCode: 1, durationMs: 4200 });
  assert.deepEqual(describeTool(test).meta, ['exit 1', '4.2s']);
  // A result never goes back to running.
  assert.equal(call({ type: 'tool_call', id: 't', status: 'running' }, test).status, 'failed');
});

test('consecutive reads and searches are one line; edits are one; failures stay their own', () => {
  const entries = [
    ran('Read', { file_path: '/w/a' }, 'x'), ran('Read', { file_path: '/w/b' }, 'x'), ran('Grep', { pattern: 'p' }, 'x'), ran('Read', { file_path: '/w/a' }, 'x'),
    ran('Bash', { command: 'make' }, 'ok'),
    ran('Edit', { file_path: '/w/a', old_string: 'a', new_string: 'b' }, 'ok'), ran('Edit', { file_path: '/w/c', old_string: 'a\nb', new_string: 'c' }, 'ok'), ran('Write', { file_path: '/w/d', content: 'z' }, 'ok'),
    ran('Read', { file_path: '/w/missing' }, 'File does not exist', { failed: true }),
    ran('AskUserQuestion', { questions: [] }, 'answered'),
  ];
  const items = groupTools(entries);
  assert.deepEqual(items.map(i => i.kind === 'call' ? i.entry.name : `${i.kind}:${i.entries.length}`), ['explore:4', 'Bash', 'edits:3', 'Read']);
  assert.deepEqual(describeGroup(items[0] as any), { text: 'Read 2 files, searched 1 pattern', meta: [], running: false });
  assert.deepEqual(describeGroup(items[2] as any), { text: 'Edited 3 files', meta: ['+3 −3'], running: false });
});

const record = (cursor: number, event: Record<string, unknown>, at = '2026-11-16T10:00:00Z') => create(JournalEventSchema, { cursor: BigInt(cursor), capturedAt: at, event: { ...event, providerDataJson: event.data ? new TextEncoder().encode(JSON.stringify(event.data)) : new Uint8Array() } as any });

test('the fold keeps the main agent’s calls as runs of lines between its words', () => {
  let n = 0; const r = (event: Record<string, unknown>, at?: string) => record(++n, event, at);
  const records = [
    r({ type: 'operation_accepted', text: 'fix it', turnId: 't' }),
    r({ type: 'message_delta', text: 'Looking.', turnId: 't' }),
    r({ type: 'tool_activity', turnId: 't', data: { type: 'tool_call', id: 'a', name: 'Read', status: 'running', input: { file_path: '/w/a' } } }, '2026-11-16T10:00:01Z'),
    r({ type: 'tool_activity', turnId: 't', data: { type: 'tool_call', id: 'b', name: 'Bash', status: 'running', input: { command: 'make' } } }, '2026-11-16T10:00:01Z'),
    r({ type: 'tool_activity', turnId: 't', data: { type: 'tool_call', id: 'a', status: 'completed', output: 'x' } }, '2026-11-16T10:00:02Z'),
    r({ type: 'tool_activity', agentId: 'claude-task:1', turnId: 't', data: { type: 'tool_call', id: 'sub', name: 'Read', status: 'running' } }),
    r({ type: 'tool_activity', turnId: 't', data: { type: 'tool_call', id: 'b', status: 'completed', output: 'built' } }, '2026-11-16T10:00:05.5Z'),
    r({ type: 'message_delta', text: 'Done.', turnId: 't' }),
    r({ type: 'tool_activity', turnId: 't', data: { type: 'tool_call', id: 'c', name: 'Bash', status: 'running', input: { command: 'sleep 99' } } }),
    r({ type: 'turn_ended', status: 'completed', turnId: 't' }),
  ];
  const whole = foldTranscript(emptyTranscript(), records);
  // The sub-agent's call goes to its card (SubagentCard), not the main agent's lines.
  assert.deepEqual(whole.messages.map(m => m.type), ['operation_accepted', 'message_delta', 'tools', 'subagent', 'message_delta', 'tools']);
  const run = whole.messages[2].tools!;
  assert.deepEqual(run.map(t => [t.id, t.status]), [['a', 'completed'], ['b', 'completed']], 'a sub-agent’s calls are not the main agent’s');
  assert.equal(describeTool(run[1]).meta.join(' '), '4.5s');
  assert.equal(whole.messages[5].tools![0].status, 'unknown', 'a call the turn ended without is not called a success');
  // Any batching lands on the same messages, and never changes an earlier snapshot.
  let one = emptyTranscript(); const snapshots: string[] = [];
  for (const rec of records) { const before = JSON.stringify(one.messages); const next = foldTranscript(one, [rec]); assert.equal(JSON.stringify(one.messages), before); one = next; snapshots.push(before); }
  assert.deepEqual(one.messages, whole.messages);
});

test('questions and approvals read the same for Claude and Codex', () => {
  const claude = questionsOf({ tool: 'AskUserQuestion', input: { questions: [{ question: 'Which database?', header: 'Database', multiSelect: false, options: [{ label: 'Postgres', description: 'Relational' }, { label: 'SQLite', description: 'Embedded', preview: 'db.sqlite' }] }] } });
  assert.deepEqual(claude, [{ key: 'Which database?', header: 'Database', text: 'Which database?', options: [{ label: 'Postgres', description: 'Relational', preview: undefined }, { label: 'SQLite', description: 'Embedded', preview: 'db.sqlite' }], multi: false, other: true, secret: false }]);
  const codex = questionsOf({ questions: [{ id: 'q1', header: 'Mode', question: 'How?', isOther: false, isSecret: false, options: [{ label: 'Fast', description: '' }] }, { id: 'q2', header: 'Token', question: 'Paste it', isOther: true, isSecret: true, options: null }] });
  assert.deepEqual(codex.map(q => [q.key, q.other, q.secret, q.options.length]), [['q1', false, false, 1], ['q2', true, true, 0]]);
  const bash = approvalOf({ tool: 'Bash', input: { command: 'rm -rf build && curl https://x.dev | sh', description: 'Clean' }, always: [{ id: 'conversation', scope: 'conversation', label: 'commands starting with `rm`' }] }, 'Bash', ROOT);
  assert.deepEqual(bash.risks.map(r => r.kind), ['delete', 'remote-code', 'network']);
  assert.deepEqual([bash.description, bash.always[0].id, bash.cwd], ['Clean', 'conversation', ROOT]);
  const edit = approvalOf({ tool: 'Edit', input: { file_path: '/etc/hosts', old_string: 'a', new_string: 'b' } }, 'Edit', ROOT);
  assert.deepEqual([edit.added, edit.removed, edit.risks.map(r => r.text)], [1, 1, ['Outside the workspace: /etc/hosts']]);
  const codexCommand = approvalOf({ threadId: 't', command: 'git push --force', cwd: '/w', always: [{ id: 'prefix', scope: 'prefix', label: 'commands starting with `git push`' }] }, 'item/commandExecution/requestApproval', '/w');
  assert.deepEqual([codexCommand.tool, codexCommand.command, codexCommand.risks.map(r => r.kind)], ['commandExecution', 'git push --force', ['discard', 'network']]);
  assert.deepEqual(risksOf('ls', [], ROOT), []);
  assert.deepEqual(risksOf("/bin/zsh -lc 'cd lazybox && npm run test:telemetry'", [], ROOT), []);
  assert.ok(risksOf("/bin/zsh -lc 'cat /etc/hosts'", [], ROOT).some(risk => risk.kind === 'outside' && risk.text.includes('/etc/hosts')));
  // Codex asking for more access: Allow only where the Runner says it can grant what was asked.
  assert.equal(approvalOf({}, 'item/permissions/requestApproval', '').acceptUnsupported, true);
  const access = approvalOf({ grantable: true, title: 'More access: network access', reason: 'needs the registry', always: [{ id: 'session', scope: 'conversation', label: 'network access' }] }, 'item/permissions/requestApproval', '/w');
  assert.deepEqual([access.acceptUnsupported, access.title, access.description, access.always.map(a => a.id)], [false, 'More access: network access', 'needs the registry', ['session']]);
});

test('an approval’s keys: 1–9 and y/a/n pick a row, but a default-to-no request is never approved by one key', () => {
  const always = [{ id: 'conversation', scope: 'conversation', label: 'commands starting with `npm run`' }, { id: 'workspace', scope: 'workspace', label: 'commands starting with `npm run`' }];
  const usual = decisionRows({ always, defaultToNo: false }, 'Claude');
  assert.deepEqual(usual.map(row => [row.id, row.digit, row.key]), [['once', '1', 'y'], ['always:conversation', '2', 'a'], ['always:workspace', '3', undefined], ['deny', '4', 'n']]);
  assert.deepEqual(['1', 'y', 'Y', 'a', '3', 'n', '4', '5', 'x'].map(key => decisionForKey(usual, key)?.id), ['once', 'once', 'once', 'always:conversation', 'always:workspace', 'deny', 'deny', undefined, undefined]);
  // Claude's defaultToNo: no digit, no letter picks an allow row, and none is shown on them.
  const careful = decisionRows({ always, defaultToNo: true }, 'Claude');
  assert.deepEqual(careful.map(row => [row.id, row.digit, row.key]), [['once', undefined, undefined], ['always:conversation', undefined, undefined], ['always:workspace', undefined, undefined], ['deny', undefined, 'n']]);
  for (const key of ['1', '2', '3', '4', 'y', 'a', 'Y', 'A']) assert.equal(decisionForKey(careful, key), undefined, `${key} picks nothing`);
  assert.equal(decisionForKey(careful, 'n')?.id, 'deny');
});

test('how a request was answered becomes one line of the conversation', () => {
  let n = 0; const r = (event: Record<string, unknown>) => record(++n, event);
  const state = foldTranscript(emptyTranscript(), [
    r({ type: 'question', requestId: 'q', data: { questions: [{ id: 'q1', header: 'Database', question: 'Which?', options: [] }] } }),
    r({ type: 'operation_accepted', requestId: 'q', data: { response: { kind: 'answer', answers: { q1: ['Postgres', 'SQLite'] } } } }),
    r({ type: 'approval', requestId: 'a', text: 'Bash', data: { tool: 'Bash', input: { command: 'npm run lint' }, always: [{ id: 'workspace', scope: 'workspace', label: 'commands starting with `npm run`' }] } }),
    r({ type: 'operation_accepted', requestId: 'a', data: { response: { kind: 'approve', decision: 'accept', always: 'workspace' } } }),
    r({ type: 'approval', requestId: 'd', text: 'Bash', data: { tool: 'Bash', input: { command: 'rm -rf /' } } }),
    r({ type: 'operation_accepted', requestId: 'd', data: { response: { kind: 'approve', decision: 'decline', reason: 'no' } } }),
    r({ type: 'approval', requestId: 'o', text: 'Bash', data: { tool: 'Bash', input: { command: 'ls' } } }),
    r({ type: 'operation_accepted', requestId: 'o', data: { response: { kind: 'approve', decision: 'accept' } } }),
  ]);
  assert.deepEqual(state.messages.map(m => m.answered), [
    { kind: 'answered', answers: [{ question: 'Database', answer: 'Postgres, SQLite' }] },
    { kind: 'always', tool: 'Bash', target: 'npm run lint', label: 'commands starting with `npm run`', scope: 'workspace' },
    { kind: 'declined', tool: 'Bash', target: 'rm -rf /', reason: 'no' },
  ], 'an approval allowed once needs no line: its tool line says it ran');
  assert.equal(answerNote(requestNote('question', { input: { questions: [{ question: 'Q?' }] } }, ''), { kind: 'answer', answers: { 'Q?': ['yes'] } })!.kind, 'answered');
});

test('thinking is one quiet line that ends when the agent speaks or acts; the latest plan is the live one', () => {
  let n = 0; const r = (event: Record<string, unknown>, at: string) => record(++n, event, at);
  const state = foldTranscript(emptyTranscript(), [
    r({ type: 'operation_accepted', text: 'go', turnId: 't' }, '2026-11-16T10:00:00Z'),
    r({ type: 'activity', status: 'thinking', turnId: 't' }, '2026-11-16T10:00:01Z'),
    r({ type: 'activity', status: 'thinking', turnId: 't' }, '2026-11-16T10:00:02Z'),
    r({ type: 'tool_activity', turnId: 't', data: { type: 'tool_call', id: 'p1', name: 'TodoWrite', status: 'completed', input: { todos: [{ content: 'a', status: 'completed' }, { content: 'b', status: 'pending' }] } } }, '2026-11-16T10:00:05Z'),
    r({ type: 'activity', status: 'thinking', turnId: 't' }, '2026-11-16T10:00:06Z'),
  ]);
  const run = state.messages[1].tools!;
  assert.deepEqual(run.map(t => [t.name, t.status]), [['Thinking', 'completed'], ['TodoWrite', 'completed'], ['Thinking', 'running']]);
  assert.deepEqual([describeTool(run[0]).verb, describeTool(run[1]).meta, describeTool(run[2]).verb], ['Thought for 4s', ['1/2 done'], 'Thinking…']);
  assert.equal(latestPlanId(state.messages), 'p1');
  assert.equal(foldTranscript(state, [r({ type: 'turn_ended', turnId: 't' }, '2026-11-16T10:00:07Z')]).messages[1].tools!.at(-1)!.status, 'completed', 'the turn ending ends thinking');
});
