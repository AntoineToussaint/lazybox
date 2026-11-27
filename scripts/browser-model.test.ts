import test from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { AgentEventSchema, BrowserActionSchema, BrowserStateSchema, SessionSchema } from '@lazybox/protocol';
import { ago, applyAction, browserBadge, browserRequestHeading, browserRequestOf, browserRequestSummary, browserVerb, decisionRows, describeBrowserCall, fitRect, mergeHistory, modifiersOf, normalizeSite, pageOrigin, pointedAction, sizeText, toDrawn, toPage } from '../packages/surfaces/src/browser-model.ts';
import { applyToolPayload, categoryOf, describeTool, kindOf, summarizeRun } from '../packages/surfaces/src/tool-lines.ts';
import { deriveAttention } from '../packages/surfaces/src/attention.ts';
import { liveStatus } from '../packages/surfaces/src/live-status.ts';

const state = (value: Record<string, unknown>) => create(BrowserStateSchema, value);
const action = (seq: number, value: Record<string, unknown> = {}) => create(BrowserActionSchema, { seq: BigInt(seq), kind: 'click', summary: `step ${seq}`, status: 'done', ...value });

test('the badge says who has the browser and whether it waits', () => {
  assert.equal(browserBadge(undefined), undefined);
  assert.equal(browserBadge(state({ status: 'closed' })), undefined);
  assert.deepEqual(browserBadge(state({ status: 'open', controller: 'agent', agentName: 'Claude' })), { label: 'Claude is browsing', tone: 'live' });
  assert.equal(browserBadge(state({ status: 'open', controller: 'person', agentName: 'Codex' }))?.label, 'You’re browsing');
  assert.equal(browserBadge(state({ status: 'open', controller: 'agent', waiting: true }))?.tone, 'waiting');
  assert.equal(browserBadge(state({ status: 'starting' }), 'Claude')?.label, 'Claude is opening the browser');
  assert.equal(browserBadge(state({ status: 'stopped' }))?.tone, 'stopped');
  assert.equal(browserBadge(state({ status: 'failed', problem: 'Chromium is missing' }))?.detail, 'Chromium is missing');
});

test('the action log keeps order, updates a step in place and stays bounded', () => {
  let log = mergeHistory([action(1), action(2)]);
  log = applyAction(log, action(3, { status: 'running', box: { x: 10, y: 20, width: 100, height: 30 } }));
  assert.deepEqual(log.map(a => a.status), ['done', 'done', 'running']);
  log = applyAction(log, action(3, { status: 'done', box: { x: 10, y: 20, width: 100, height: 30 } }));
  assert.equal(log.length, 3);
  assert.equal(log[2].status, 'done');
  // A late step goes where its seq says.
  log = applyAction(applyAction(log, action(5)), action(4));
  assert.deepEqual(log.map(a => Number(a.seq)), [1, 2, 3, 4, 5]);
  assert.equal(pointedAction(log)?.seq, 3n);
  let long: ReturnType<typeof mergeHistory> = [];
  for (let i = 1; i <= 230; i++) long = applyAction(long, action(i));
  assert.equal(long.length, 200);
  assert.equal(long[0].seq, 31n);
});

test('page pixels map to the letterboxed drawing and back', () => {
  // A 1280×800 page in a 640×600 box: 640×400, centred vertically.
  const drawn = fitRect(640, 600, 1280, 800);
  assert.deepEqual(drawn, { x: 0, y: 100, width: 640, height: 400 });
  assert.deepEqual(toPage(320, 300, drawn, { width: 1280, height: 800 }), { x: 640, y: 400 });
  assert.equal(toPage(320, 50, drawn, { width: 1280, height: 800 }), undefined);
  assert.deepEqual(toDrawn({ x: 100, y: 200, width: 300, height: 40 } as any, drawn, { width: 1280, height: 800 }), { x: 50, y: 200, width: 150, height: 20 });
  assert.deepEqual(fitRect(0, 10, 10, 10), { x: 0, y: 0, width: 0, height: 0 });
});

test('modifiers are the bits the Runner takes', () => {
  assert.equal(modifiersOf({ altKey: true, ctrlKey: false, metaKey: true, shiftKey: true }), 13);
  assert.equal(modifiersOf({ altKey: false, ctrlKey: true, metaKey: false, shiftKey: false }), 2);
});

const event = (request: Record<string, unknown>) => create(AgentEventSchema, { type: 'browser_request', requestId: String(request.id), text: 'Open', providerDataJson: new TextEncoder().encode(JSON.stringify(request)) });

test('a browser request reads as what the agent wants, with the choices it offers', () => {
  const site = browserRequestOf(event({ id: 'b1', kind: 'site', url: 'https://docs.example.com/guide?x=1', origin: 'https://docs.example.com', reason: 'The API reference', permission: 'yolo', agentName: 'Claude' }));
  assert.equal(site.host, 'docs.example.com');
  assert.equal(browserRequestHeading(site, 'Claude'), 'Claude wants to open docs.example.com');
  assert.deepEqual(decisionRows(site, 'Claude').map(r => [r.decision, r.key]), [['once', 'y'], ['conversation', 'c'], ['workspace', 'a'], ['deny', 'n']]);
  const preview = browserRequestOf(event({ kind: 'site', url: 'http://localhost:50471/auth/sign-in', collectionId: 'ws-1' }));
  assert.equal(decisionRows(preview, 'Codex').find(row => row.key === 'a')?.decision, 'workspace_previews');
  const download = browserRequestOf(event({ id: 'b2', kind: 'download', url: 'https://example.com/r.pdf', origin: 'https://example.com', fileName: 'report.pdf' }));
  assert.equal(browserRequestHeading(download, 'Claude'), 'Claude wants to download report.pdf');
  assert.deepEqual(decisionRows(download, 'Claude').map(r => r.decision), ['once', 'deny']);
  assert.equal(browserRequestSummary(event({ id: 'b3', kind: 'sign_in', origin: 'https://app.example.com' })), 'Sign in on app.example.com');
  // Malformed data is still a request, never a crash.
  assert.equal(browserRequestOf(create(AgentEventSchema, { type: 'browser_request', requestId: 'x', providerDataJson: new TextEncoder().encode('{') })).kind, 'site');
});

test('a waiting browser request needs the person, in the inbox and the status', () => {
  const session = create(SessionSchema, { id: 's', status: 'running', pending: [event({ id: 'b1', kind: 'site', origin: 'https://example.com' })] });
  const attention = deriveAttention(session);
  assert.equal(attention.state, 'needs-input');
  assert.equal(attention.reason, 'Browser');
  assert.equal(attention.line, 'Open example.com');
  assert.equal(liveStatus(session, Date.now()).label, 'Waiting for you');
});

test('browser tool calls read as what the agent did on the page', () => {
  assert.equal(browserVerb('mcp__lazybox_browser__browser_click'), 'click');
  assert.equal(browserVerb('browser_wait_for'), 'wait_for');
  assert.equal(browserVerb('mcp__other__browser_click'), undefined);
  assert.deepEqual(describeBrowserCall('open', { url: 'https://example.com/' }, false), { verb: 'Opened', target: 'example.com' });
  assert.deepEqual(describeBrowserCall('click', { ref: 'e12', element: 'Sign in button' }, true), { verb: 'Clicking', target: '“Sign in button”' });
  assert.deepEqual(describeBrowserCall('click', { ref: 'e12' }, false), { verb: 'Clicked', target: 'e12' });
  // Typed text is a short preview, never the whole of it.
  assert.equal(describeBrowserCall('type', { ref: 'e3', text: 'a very long search query about many things', submit: true }, false).target, '“a very long search quer…” into e3 and submitted');
  assert.equal(describeBrowserCall('screenshot', {}, false).verb, 'Took a screenshot');
  assert.equal(describeBrowserCall('snapshot', {}, false).verb, 'Read the page');
  const entry = applyToolPayload(undefined, { type: 'tool_call', id: 't', name: 'mcp__lazybox_browser__browser_navigate', status: 'completed', input: { url: 'https://news.example.com/today' }, output: 'ok' }, 1)!;
  assert.equal(kindOf(entry), 'browser');
  assert.equal(categoryOf(entry), 'browser');
  assert.deepEqual([describeTool(entry).verb, describeTool(entry).target], ['Went to', 'news.example.com/today']);
  const codex = applyToolPayload(undefined, { type: 'tool_call', id: 'c', name: 'browser_snapshot', status: 'completed', input: {}, output: 'x' }, 1)!;
  const run = summarizeRun([entry, codex]);
  assert.equal(run.text, 'Used the browser');
  assert.deepEqual(run.meta, ['2 browser steps']);
});

test('sites for the allow-list are host names or wildcards', () => {
  assert.deepEqual(normalizeSite('https://Docs.Example.com/x'), { site: 'docs.example.com' });
  assert.deepEqual(normalizeSite('*.example.com'), { site: '*.example.com' });
  assert.deepEqual(normalizeSite('localhost:5173'), { site: 'localhost:5173' });
  assert.ok('error' in normalizeSite(''));
  assert.ok('error' in normalizeSite('file:///etc/passwd'));
  assert.ok('error' in normalizeSite('not a site'));
  assert.ok('error' in normalizeSite('*.com'));
  assert.ok('error' in normalizeSite('example.com:http'));
});

test('small words: ages, sizes, origins', () => {
  assert.equal(ago(1000, 3000), 'now');
  assert.equal(ago(0, 75_000), '1m');
  assert.equal(ago(0, 7_300_000), '2h');
  assert.equal(sizeText(3_600_000), '3.4 MB');
  assert.equal(sizeText(0), '0 B');
  assert.deepEqual(pageOrigin('http://127.0.0.1:5173/a?b'), { host: '127.0.0.1:5173', secure: false, local: true, path: '/a?b' });
  assert.equal(pageOrigin('file:///x'), undefined);
});
