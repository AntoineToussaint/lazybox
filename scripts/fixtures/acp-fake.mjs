#!/usr/bin/env node
// An explicitly fake ACP agent (Agent Client Protocol v1 over stdio) for the
// Runner's ACP adapter tests and the ACP UI suite. It runs no model: what it
// does in a turn is chosen by the last words of the prompt.
//
//   hello            two message chunks, then end_turn
//   tour             thought, plan, a file read, an edit behind a permission
//                    request (written through fs/write_text_file), a command
//                    in a client terminal, usage with cost, a closing message
//   ask              one permission request; says which option was chosen
//   abandon          one permission request, then ends the turn without
//                    waiting for its answer
//   wait             runs until session/cancel
//   slow             a chunk, a pause, a chunk (Runner restart tests)
//   fail | refuse | auth | limit   ends the turn with that error/stop reason
//   read:<path> | write:<path> | run:<command line>
//                    one client request; says what came back
//   context?         says whether the conversation's context arrived
//
// Environment: ACP_FAKE_AUTH=1 makes session/new require sign-in;
// ACP_FAKE_VERSION overrides the protocol version it answers with;
// ACP_FAKE_NO_RESUME=1 offers neither session/load nor session/resume;
// ACP_FAKE_LOAD_ONLY=1 offers session/load (which replays history) only;
// ACP_FAKE_SLOW_MS sets the pause of `slow`;
// ACP_FAKE_LOG=<file> appends every line it receives, for assertions;
// ACP_FAKE_LOG_ENV=<names> also appends, at start, `env NAME=value` for each
// comma-separated name (the agent's environment, for launch assertions).
import { createInterface } from 'node:readline';
import { appendFileSync } from 'node:fs';
import { join } from 'node:path';

const send = message => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...message }) + '\n');
const log = line => { if (process.env.ACP_FAKE_LOG) appendFileSync(process.env.ACP_FAKE_LOG, line + '\n'); };
for (const name of (process.env.ACP_FAKE_LOG_ENV || '').split(',').filter(Boolean)) log(`env ${name}=${process.env[name] ?? '<unset>'}`);
let nextId = 1;
const waiting = new Map();
/** A request to the client; resolves with { result } or { error }. */
const call = (method, params) => new Promise(resolve => { const id = nextId++; waiting.set(id, resolve); send({ id, method, params }); });
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

let sessionId = '';
let cwd = '';
let mode = 'default';
let model = 'fake-pro';
let turn = null; // { id, cancelled, cancel() }
let contextSeen = false;
let systemPrompt = '';
const update = u => send({ method: 'session/update', params: { sessionId, update: u } });
const say = text => update({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text } });

const configOptions = () => [{ id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: model, options: [{ value: 'fake-pro', name: 'Fake Pro', description: 'The careful one' }, { value: 'fake-flash', name: 'Fake Flash' }] }];
const modes = () => ({ currentModeId: mode, availableModes: [{ id: 'default', name: 'Default' }, { id: 'autoEdit', name: 'Auto edit' }, { id: 'yolo', name: 'YOLO' }, { id: 'plan', name: 'Plan' }] });
const commands = () => update({ sessionUpdate: 'available_commands_update', availableCommands: [{ name: 'compress', description: 'Compress the conversation context' }, { name: 'memory', description: 'Manage memory', input: { hint: 'add <text>' } }] });

async function permission(toolCall, extra = []) {
  const options = [{ optionId: 'allow-once', name: 'Allow', kind: 'allow_once' }, { optionId: 'allow-always', name: 'Always allow', kind: 'allow_always' }, ...extra, { optionId: 'reject-once', name: 'Reject', kind: 'reject_once' }];
  const answer = await call('session/request_permission', { sessionId, toolCall, options });
  return answer.result?.outcome || { outcome: 'error' };
}

async function tour() {
  update({ sessionUpdate: 'session_info_update', title: 'Fake tour' });
  update({ sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: 'private reasoning' } });
  const plan = statuses => update({ sessionUpdate: 'plan', entries: [['Read the README', statuses[0]], ['Write the notes', statuses[1]], ['Run the check', statuses[2]]].map(([content, status]) => ({ content, priority: 'medium', status })) });
  plan(['in_progress', 'pending', 'pending']);
  const readme = join(cwd, 'README.md');
  update({ sessionUpdate: 'tool_call', toolCallId: 'call-read', title: 'Read README.md', kind: 'read', status: 'in_progress', locations: [{ path: readme }] });
  const read = await call('fs/read_text_file', { sessionId, path: readme });
  update({ sessionUpdate: 'tool_call_update', toolCallId: 'call-read', status: read.error ? 'failed' : 'completed', content: [{ type: 'content', content: { type: 'text', text: read.result?.content ?? read.error?.message ?? '' } }] });
  plan(['completed', 'in_progress', 'pending']);
  const notes = join(cwd, 'notes.txt');
  const edit = { toolCallId: 'call-edit', title: 'Write notes.txt', kind: 'edit', status: 'pending', locations: [{ path: notes }], content: [{ type: 'diff', path: notes, oldText: null, newText: 'first line\nsecond line\n' }] };
  update({ sessionUpdate: 'tool_call', ...edit });
  const outcome = await permission(edit);
  if (turn?.cancelled) return;
  if (outcome.outcome === 'selected' && outcome.optionId.startsWith('allow')) {
    const written = await call('fs/write_text_file', { sessionId, path: notes, content: 'first line\nsecond line\n' });
    update({ sessionUpdate: 'tool_call_update', toolCallId: 'call-edit', status: written.error ? 'failed' : 'completed' });
  } else {
    update({ sessionUpdate: 'tool_call_update', toolCallId: 'call-edit', status: 'failed', content: [{ type: 'content', content: { type: 'text', text: 'The user declined the edit.' } }] });
  }
  plan(['completed', 'completed', 'in_progress']);
  update({ sessionUpdate: 'tool_call', toolCallId: 'call-run', title: 'Run the check', kind: 'execute', status: 'pending', rawInput: { command: 'echo fake-tour-check' } });
  const created = await call('terminal/create', { sessionId, command: 'echo', args: ['fake-tour-check'], cwd, outputByteLimit: 4096 });
  if (created.result?.terminalId) {
    const terminalId = created.result.terminalId;
    update({ sessionUpdate: 'tool_call_update', toolCallId: 'call-run', status: 'in_progress', content: [{ type: 'terminal', terminalId }] });
    await call('terminal/wait_for_exit', { sessionId, terminalId });
    const output = await call('terminal/output', { sessionId, terminalId });
    await call('terminal/release', { sessionId, terminalId });
    update({ sessionUpdate: 'tool_call_update', toolCallId: 'call-run', status: output.result?.exitStatus?.exitCode === 0 ? 'completed' : 'failed' });
  } else {
    update({ sessionUpdate: 'tool_call_update', toolCallId: 'call-run', status: 'failed', content: [{ type: 'content', content: { type: 'text', text: created.error?.message || 'no terminal' } }] });
  }
  plan(['completed', 'completed', 'completed']);
  update({ sessionUpdate: 'usage_update', used: 1200, size: 200000, cost: { amount: 0.0123, currency: 'USD' } });
  say('Done: read the README, wrote notes.txt and ran the check.');
}

async function prompt(id, params) {
  const texts = (params.prompt || []).filter(b => b.type === 'text').map(b => b.text);
  // The person's words: the last text block's first line (attachments follow it).
  const last = (texts.at(-1) || '').trim().split('\n')[0];
  if (texts.some(t => t.includes('LAZYBOX-CONTEXT-MARKER'))) contextSeen = true;
  let cancelled;
  const done = new Promise(resolve => { cancelled = resolve; });
  turn = { id, cancelled: false, cancel() { this.cancelled = true; cancelled(); } };
  const finish = (result, error) => { if (turn?.id === id) turn = null; send(error ? { id, error } : { id, result }); };
  try {
    if (/^hello\b/i.test(last)) { say('Hello from '); say('the fake ACP agent.'); }
    else if (/^tour\b/i.test(last)) { await tour(); }
    else if (/^ask\b/i.test(last)) {
      const outcome = await permission({ toolCallId: 'call-ask', title: 'Run make deploy', kind: 'execute', status: 'pending', rawInput: { command: 'make deploy' } });
      say(`decision=${outcome.outcome === 'selected' ? outcome.optionId : outcome.outcome}`);
    }
    else if (/^abandon\b/i.test(last)) { permission({ toolCallId: 'call-abandon', title: 'Run make clean', kind: 'execute', status: 'pending', rawInput: { command: 'make clean' } }); say('Gave up waiting.'); }
    else if (/^wait\b/i.test(last)) { say('Waiting until cancelled.'); await done; }
    else if (/^slow\b/i.test(last)) { say('slow part one. '); await sleep(Number(process.env.ACP_FAKE_SLOW_MS || 1500)); say('slow part two.'); }
    // The agent process dies mid-turn.
    else if (/^die\b/i.test(last)) process.exit(4);
    else if (/^fail\b/i.test(last)) return finish(undefined, { code: -32603, message: 'The fake model is overloaded' });
    else if (/^auth\b/i.test(last)) return finish(undefined, { code: -32000, message: 'Authentication required' });
    else if (/^refuse\b/i.test(last)) return finish({ stopReason: 'refusal' });
    else if (/^limit\b/i.test(last)) return finish({ stopReason: 'max_turn_requests' });
    else if (/^read:/i.test(last)) { const r = await call('fs/read_text_file', { sessionId, path: last.slice(5) }); say(r.error ? `error ${r.error.code}: ${r.error.message}` : `content=${r.result.content}`); }
    else if (/^write:/i.test(last)) { const r = await call('fs/write_text_file', { sessionId, path: last.slice(6), content: 'written by the fake agent\n' }); say(r.error ? `error ${r.error.code}: ${r.error.message}` : 'wrote'); }
    else if (/^run:/i.test(last)) {
      const r = await call('terminal/create', { sessionId, command: last.slice(4) });
      if (r.error) say(`error ${r.error.code}: ${r.error.message}`);
      else { const exit = await call('terminal/wait_for_exit', { sessionId, terminalId: r.result.terminalId }); const out = await call('terminal/output', { sessionId, terminalId: r.result.terminalId }); say(`exit=${exit.result?.exitCode} output=${out.result?.output}`); }
    }
    else if (/^context\?/i.test(last)) say(`context=${contextSeen || systemPrompt.includes('LAZYBOX-CONTEXT-MARKER') ? 'yes' : 'no'} mode=${mode} model=${model}`);
    else say(`echo: ${last}`);
  } catch (e) { return finish(undefined, { code: -32603, message: String(e) }); }
  finish({ stopReason: turn?.cancelled ? 'cancelled' : 'end_turn' });
}

createInterface({ input: process.stdin }).on('line', line => {
  log(line);
  let m;
  try { m = JSON.parse(line); } catch { process.stderr.write('not JSON: ' + line + '\n'); process.exit(2); }
  if (m.jsonrpc !== '2.0') { process.stderr.write('missing jsonrpc 2.0: ' + line + '\n'); process.exit(3); }
  if (!m.method) { const resolve = waiting.get(m.id); waiting.delete(m.id); resolve?.(m); return; }
  const { id, method, params = {} } = m;
  switch (method) {
    case 'initialize': {
      const load = !process.env.ACP_FAKE_NO_RESUME, resume = load && !process.env.ACP_FAKE_LOAD_ONLY;
      send({ id, result: { protocolVersion: Number(process.env.ACP_FAKE_VERSION || 1), agentCapabilities: { loadSession: load, promptCapabilities: { image: true, embeddedContext: false }, sessionCapabilities: resume ? { resume: {} } : {} }, authMethods: [{ id: 'fake-login', name: 'Fake sign-in', description: 'Signs in the fake agent' }], agentInfo: { name: 'acp-fake', title: 'Fake ACP agent', version: '1.0.0' } } });
      break;
    }
    case 'session/new':
      if (process.env.ACP_FAKE_AUTH) { send({ id, error: { code: -32000, message: 'Authentication required' } }); break; }
      sessionId = 'fake-session-1'; cwd = params.cwd; systemPrompt = JSON.stringify(params.systemPrompt || '');
      send({ id, result: { sessionId, modes: modes(), configOptions: configOptions() } });
      commands();
      break;
    case 'session/resume':
      sessionId = params.sessionId; cwd = params.cwd;
      send({ id, result: { modes: modes(), configOptions: configOptions() } });
      commands();
      break;
    case 'session/load':
      sessionId = params.sessionId; cwd = params.cwd;
      update({ sessionUpdate: 'user_message_chunk', content: { type: 'text', text: 'an old question' } });
      say('an old answer that the journal already has');
      send({ id, result: { modes: modes(), configOptions: configOptions() } });
      break;
    case 'session/set_mode':
      mode = params.modeId; send({ id, result: {} }); update({ sessionUpdate: 'current_mode_update', currentModeId: mode });
      break;
    case 'session/set_config_option':
      if (params.configId === 'model') model = params.value;
      send({ id, result: { configOptions: configOptions() } });
      break;
    case 'session/prompt':
      void prompt(id, params);
      break;
    case 'session/cancel':
      turn?.cancel();
      break;
    default:
      if (id !== undefined) send({ id, error: { code: -32601, message: `method not found: ${method}` } });
  }
});
