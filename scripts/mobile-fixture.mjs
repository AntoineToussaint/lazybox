// A fake Runner for the iPhone app (apps/mobile) on the iOS Simulator: the
// fixture Runner's device gateway (pairing, sign-in, the device allowlist)
// with a phone-sized story in it, and a pairing offer ready to scan or paste.
// The simulator shares the Mac's network, so the app reaches the gateway on
// 127.0.0.1. No agent runs: this script plays the agent's side (streaming
// text, a turn that ends, approvals and messages answered) so the app has a
// live conversation to follow.
//
//   node scripts/mobile-fixture.mjs [--state <file>] [--grants read,respond]
//
// Prints one JSON line {gateway, link, token} and writes it to --state. Logs
// each WatchEvents stream the phone opens and closes (with its cursor), which
// is the evidence for the streaming spike (MOBILE_CLOUD_HUB.md, S1).
import { writeFile } from 'node:fs/promises';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');
const ms = value => String(value);
const MIN = 60_000;
const log = (...parts) => console.error(new Date().toISOString().slice(11, 23), ...parts);

/** A conversation's journal, recorded the way the Runner records it (scripts/docs-shots.mjs). */
function journal(f, sessionId, start) {
  const channel = f.openEvents(sessionId);
  let clock = start;
  const push = (event, seconds = 1) => channel.push(event, new Date(clock = Math.max(clock + seconds * 1000, start)).toISOString());
  const now = event => { clock = Date.now(); return channel.push(event, new Date(clock).toISOString()); };
  return {
    channel, push, now,
    prompt: (text, operationId = `op-${channel.events.length + 1}`) => push({ type: 'operation_accepted', text, operationId }),
    say: text => push({ type: 'message_delta', text }),
    tool: (id, fields, seconds = 2) => push({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id, ...fields }) }, seconds),
    start: () => push({ type: 'turn_started' }),
    end: () => push({ type: 'turn_ended' }),
  };
}

export async function startPhoneFixture({ grants = ['read', 'respond'] } = {}) {
  const f = await startRunnerFixture();
  const now = Date.now();
  const root = (status, updated, extra = {}) => [{ id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra }];
  const base = { profileId: 'claude-default', provider: 'claude', model: 'claude-opus-5-5', modelReported: true, capabilities: { files: true, steer: true }, pending: [], queuedMessages: [] };
  f.collections[0].name = 'Response cache'; f.collections[1].name = 'Release 2.4';

  // What needs the person: an approval, a flagged approval, a question.
  const approval = { ...base, id: 'deps', collectionId: 'beta', title: 'Bump dependencies', status: 'waiting_approval', createdAtUnixMillis: ms(now - 50 * MIN), agents: root('waiting_approval', now - 2 * MIN),
    pending: [{ type: 'approval', requestId: 'a-deps', text: 'Bash', providerDataJson: b64({ input: { command: 'npm update --save', description: 'Update dependencies within their ranges' } }) }] };
  const flagged = { ...base, id: 'clean', collectionId: 'alpha', title: 'Clean the build cache', status: 'waiting_approval', createdAtUnixMillis: ms(now - 30 * MIN), agents: root('waiting_approval', now - 4 * MIN),
    pending: [{ type: 'approval', requestId: 'a-clean', text: 'Bash', providerDataJson: b64({ input: { command: 'rm -rf dist .cache', description: 'Remove stale build output' } }) }] };
  const question = { ...base, id: 'notes', collectionId: 'beta', title: 'Release notes', provider: 'codex', model: 'gpt-5.4', status: 'waiting_input', createdAtUnixMillis: ms(now - 90 * MIN), agents: root('waiting_input', now - 6 * MIN),
    pending: [{ type: 'question', requestId: 'q-notes', providerDataJson: b64({ questions: [{ question: 'Which version should the notes cover?', header: 'Version', options: [{ label: '2.4.0' }, { label: '2.4.1' }, { label: '3.0.0' }] }] }) }] };
  // Working, streaming live; failed; done; pinned.
  const live = { ...base, id: 'index', collectionId: 'alpha', title: 'Index the API routes', status: 'running', busy: true, createdAtUnixMillis: ms(now - 20 * MIN), agents: root('running', now - 5_000, { phase: 'tool', tool: 'Bash' }) };
  const failed = { ...base, id: 'deploy', collectionId: 'beta', title: 'Deploy the preview', status: 'failed', fault: 'provider delivery failed; reconcile before continuing', createdAtUnixMillis: ms(now - 120 * MIN), agents: root('failed', now - 25 * MIN) };
  const done = [
    { ...base, id: 'parser', collectionId: 'alpha', title: 'Refactor the parser', status: 'done', createdAtUnixMillis: ms(now - 300 * MIN), lastMessage: 'Split the parser into three files. All 42 tests pass.', agents: root('done', now - 140 * MIN, { turnEndedAtUnixMillis: ms(now - 140 * MIN), lastTurnStatus: 'completed' }), pinnedAtUnixMillis: ms(now - 100 * MIN) },
    { ...base, id: 'flaky', collectionId: 'beta', title: 'Fix the flaky upload test', status: 'done', createdAtUnixMillis: ms(now - 600 * MIN), lastMessage: 'The race was in the temp directory cleanup; fixed and ran it 200 times.', agents: root('done', now - 400 * MIN, { turnEndedAtUnixMillis: ms(now - 400 * MIN), lastTurnStatus: 'completed' }) },
  ];
  f.sessions.splice(0, f.sessions.length, approval, flagged, question, live, failed, ...done);

  const deps = journal(f, 'deps', now - 50 * MIN);
  deps.prompt('Update our dependencies and make sure the tests still pass.');
  deps.start();
  deps.tool('t1', { name: 'Read', status: 'completed', input: { file_path: 'package.json' }, output: 'ok' });
  deps.tool('t2', { name: 'Bash', status: 'completed', input: { command: 'npm outdated' }, output: '14 packages outdated' });
  deps.say('Fourteen packages are behind, all within their semver ranges. Nothing major: **vite**, **typescript** and a few test helpers.\n\nI’ll update them in place and then run the whole suite.');
  deps.push({ type: 'approval', requestId: 'a-deps', text: 'Bash', providerDataJson: approval.pending[0].providerDataJson });

  const clean = journal(f, 'clean', now - 30 * MIN);
  clean.prompt('The build keeps picking up stale chunks. Clear whatever cache is causing it.');
  clean.start();
  clean.tool('c1', { name: 'Bash', status: 'completed', input: { command: 'du -sh dist .cache' }, output: '412M dist\n1.1G .cache' });
  clean.say('The stale chunks come from `.cache` and an old `dist`. Removing both is safe: the next build recreates them.');
  clean.push({ type: 'approval', requestId: 'a-clean', text: 'Bash', providerDataJson: flagged.pending[0].providerDataJson });

  const notes = journal(f, 'notes', now - 90 * MIN);
  notes.prompt('Draft the release notes from the merged pull requests.');
  notes.start();
  notes.tool('n1', { name: 'Bash', status: 'completed', input: { command: 'gh pr list --state merged --limit 40' }, output: '31 pull requests' });
  notes.say('I grouped the 31 merged pull requests into features, fixes and internal changes.');
  notes.push({ type: 'question', requestId: 'q-notes', providerDataJson: question.pending[0].providerDataJson });

  const parser = journal(f, 'parser', now - 300 * MIN);
  parser.prompt('The parser file is 2,000 lines. Split it into sensible modules without changing behaviour.');
  parser.start(); parser.tool('p1', { name: 'Edit', status: 'completed', input: { file_path: 'src/parser/index.ts' }, output: 'ok' });
  parser.say('Split the parser into three files:\n\n- `lexer.ts`: tokens and positions\n- `grammar.ts`: the rules\n- `index.ts`: the public API, unchanged\n\nAll 42 tests pass.'); parser.end();
  journal(f, 'flaky', now - 600 * MIN).say(done[1].lastMessage);
  const deploy = journal(f, 'deploy', now - 120 * MIN);
  deploy.prompt('Deploy this branch to the preview environment.'); deploy.start();
  deploy.push({ type: 'failure', text: 'A message could not be delivered to the agent', status: 'delivery_failed' });

  // The live conversation: a turn that keeps writing while the phone watches.
  const index = journal(f, 'index', now - 20 * MIN);
  index.prompt('Index every API route with its handler, method and auth requirement.');
  index.start();
  index.tool('i1', { name: 'Grep', status: 'completed', input: { pattern: 'router\\.(get|post|put|delete)' }, output: '38 matches' });
  index.tool('i2', { name: 'Read', status: 'completed', input: { file_path: 'src/routes.ts' }, output: 'ok' });
  const streamed = 'Found 38 routes across 6 files. So far:\n\n- `GET /health`: public\n- `GET /users/:id`: session required\n- `POST /uploads`: session required, 25 MB limit\n\nReading the admin routes next; they use a separate middleware stack, so I’m checking each one by hand.';
  let streaming;
  const streamTurn = (text, onDone) => {
    clearInterval(streaming);
    const words = text.split(/(?<=\s)/);
    let i = 0;
    Object.assign(live, { status: 'running', busy: true, agents: root('running', Date.now(), { phase: 'writing' }) });
    streaming = setInterval(() => {
      if (i >= words.length) { clearInterval(streaming); onDone?.(); return; }
      const chunk = words.slice(i, i + 3).join(''); i += 3;
      index.now({ type: 'message_delta', text: chunk });
      live.agents = root('running', Date.now(), { phase: 'writing' });
    }, 350);
  };
  streamTurn(streamed);

  // The agent's side: an answer resumes the turn; a message starts one.
  f.remote.resolveOnAnswer = true;
  const finishTurn = (session, j, text) => {
    j.now({ type: 'message_delta', text });
    j.now({ type: 'turn_ended' });
    const at = Date.now();
    Object.assign(session, { status: 'done', busy: false, lastMessage: text, agents: root('done', at, { turnEndedAtUnixMillis: ms(at), lastTurnStatus: 'completed' }) });
  };
  const journals = { deps, clean, notes, index, parser, deploy };
  let seen = 0;
  const answer = setInterval(() => {
    for (const m of f.mutations.slice(seen)) {
      seen++;
      if (m.method !== 'Apply' || !m.device) continue;
      const session = f.sessions.find(s => s.id === m.body.sessionId);
      const j = journals[m.body.sessionId] || (journals[m.body.sessionId] = journal(f, m.body.sessionId, Date.now()));
      if (!session) continue;
      if (m.body.approval) {
        const accept = m.body.approval.decision === 'DECISION_ACCEPT';
        log('phone', accept ? 'allowed' : 'denied', m.body.approval.requestId);
        j.now({ type: 'operation_accepted', requestId: m.body.approval.requestId, providerDataJson: b64({ response: { kind: 'approve', decision: accept ? 'accept' : 'decline', reason: m.body.approval.reason || '' } }) });
        if (accept) {
          j.now({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id: `run-${seen}`, name: 'Bash', status: 'completed', input: JSON.parse(Buffer.from(session.pending?.[0]?.providerDataJson || b64({}), 'base64').toString()).input || {}, output: 'ok' }) });
          setTimeout(() => finishTurn(session, j, session.id === 'deps' ? 'Updated 14 packages. All 212 tests pass, and the lockfile changed only for them.' : 'Done.'), 1500);
        } else setTimeout(() => finishTurn(session, j, 'Understood, I left it as it was.'), 800);
      } else if (m.body.answer) {
        log('phone', 'answered', JSON.stringify(m.body.answer.answers));
        j.now({ type: 'operation_accepted', requestId: m.body.answer.requestId, providerDataJson: b64({ response: { kind: 'answer', answers: Object.fromEntries(Object.entries(m.body.answer.answers).map(([k, v]) => [k, v.values])) } }) });
        setTimeout(() => finishTurn(session, j, 'Drafted the notes for that version: 9 features, 14 fixes, 8 internal changes.'), 1500);
      } else if (m.body.message) {
        log('phone', 'sent', JSON.stringify(m.body.message.text), session.busy ? '(queued behind the turn)' : '');
        if (session.busy) continue; // The fixture queued it (session.queuedMessages); Send now delivers it.
        j.now({ type: 'operation_accepted', text: m.body.message.text, operationId: m.body.operationId });
        j.now({ type: 'turn_started' });
        if (session.id === 'index') streamTurn('On it. The admin routes all require the `admin` role; I’ll add that column to the index.', () => finishTurn(session, j, '\n\nThe index is in `docs/routes.md`.'));
        else { Object.assign(session, { status: 'running', busy: true }); setTimeout(() => finishTurn(session, j, 'Got it. I’ll do that next.'), 1500); }
      } else if (m.body.deliverQueued) {
        log('phone', 'send now', m.body.deliverQueued.how, m.body.deliverQueued.operationId);
        // The fixture interrupted the turn and journaled the stop; the queued message is the next turn.
        clearInterval(streaming);
        const queued = m.body.deliverQueued.operationId;
        setTimeout(() => { j.now({ type: 'turn_started' }); streamTurn('Stopped and switched to your message. Here is the short version first.', () => finishTurn(session, j, '')); }, 600);
        log('phone', 'delivered now', queued);
      } else if (m.body.interrupt) {
        log('phone', 'interrupt');
        clearInterval(streaming); j.now({ type: 'turn_ended', status: 'interrupted' });
        Object.assign(session, { status: 'interrupted', busy: false, agents: root('interrupted', Date.now(), { turnEndedAtUnixMillis: ms(Date.now()), lastTurnStatus: 'interrupted' }) });
      }
    }
  }, 100);

  // The streaming spike's evidence: every WatchEvents the phone opens, from which cursor, and how many stay open.
  const streams = [];
  let calls = 0; const open = new Map();
  const watch = setInterval(() => {
    for (const call of f.calls.slice(calls)) {
      calls++;
      if (call.method !== 'WatchEvents' || !call.device) continue;
      streams.push({ at: Date.now(), session: call.body.sessionId, kind: 'open', after: BigInt(call.body.after || 0) });
      log('stream open', call.body.sessionId, 'after', call.body.after || '0');
    }
    for (const [id, channel] of Object.entries(journals)) {
      const count = channel.channel.clients.size;
      if ((open.get(id) || 0) !== count) { streams.push({ at: Date.now(), session: id, kind: 'count', count }); log('streams', id, (open.get(id) || 0), '→', count); open.set(id, count); }
    }
  }, 200);
  // Like the Runner (internal/transport/bench.go, watchHeartbeat): an idle stream that asked for
  // heartbeats hears one every 15 s, which keeps it inside iOS's 60 s idle timeout for a request.
  const heartbeat = (channel) => { const data = Buffer.from(JSON.stringify({ cursor: String(channel.events.length), heartbeat: true, retained: String(channel.events.length) })); const head = Buffer.alloc(5); head.writeUInt32BE(data.length, 1); return Buffer.concat([head, data]); };
  const beats = setInterval(() => { for (const j of Object.values(journals)) for (const res of j.channel.clients) res.write(heartbeat(j.channel)); }, 15_000);

  f.remote.tailscale = { ...f.remote.tailscale, enabled: true, state: 'serving' };
  f.remote.phoneUrl = f.url;
  const offer = await fetch(`${f.url}/lazybox.bench.v1.RunnerService/BeginDevicePairing`, { method: 'POST', headers: { authorization: `Bearer ${f.token}`, 'content-type': 'application/json' }, body: JSON.stringify({ grants }) }).then(r => r.json());
  return {
    f, gateway: f.url, link: offer.url, live, streams,
    /** The live conversation's agent says more, as the Runner would journal it. */
    say: text => index.now({ type: 'message_delta', text }),
    async close() { clearInterval(streaming); clearInterval(answer); clearInterval(watch); clearInterval(beats); await f.close(); },
  };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const arg = name => { const i = process.argv.indexOf(name); return i > 0 ? process.argv[i + 1] : undefined; };
  const phone = await startPhoneFixture({ grants: (arg('--grants') || 'read,respond').split(',') });
  const info = { gateway: phone.gateway, link: phone.link, token: phone.f.token };
  console.log(JSON.stringify(info));
  if (arg('--state')) await writeFile(arg('--state'), JSON.stringify(info));
  const stop = async () => { await phone.close(); process.exit(0); };
  process.on('SIGINT', stop); process.on('SIGTERM', stop);
}
