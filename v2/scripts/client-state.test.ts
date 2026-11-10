import test from 'node:test';
import assert from 'node:assert/strict';
import { Code, ConnectError } from '@connectrpc/connect';
import { follow } from '../packages/client-core/src/index.ts';
import { readDraft, writeDraft, readLaunchChoices, writeLaunchChoices, readConsoleState, writeConsoleState } from '../packages/surfaces/src/view-state.ts';
import type { ViewStateStore } from '../packages/platform/src/index.ts';

function storage(key: string, items = new Map<string, string>()): ViewStateStore {
  return { key, getItem: name => items.get(name) ?? null, setItem: (name, value) => { items.set(name, value); } };
}

test('drafts and console navigation are independent by window, execution, and command', () => {
  const items = new Map<string, string>();
  const first = storage('authority:main', items), second = storage('authority:console', items);
  const draft = { text: 'unsent\nfollow-up', files: [{ name: 'notes.txt', size: 123, lastModified: 42 }], operationId: 'uncertain-send' };
  writeDraft(first, 'session-a', draft);
  assert.deepEqual(readDraft(first, 'session-a'), draft);
  assert.equal(readDraft(second, 'session-a').text, '');
  assert.equal(readDraft(first, 'session-b').text, '');
  writeConsoleState(second, 'command-a', { search: 'warning', follow: false, scrollTop: 420 });
  assert.deepEqual(readConsoleState(second, 'command-a'), { search: 'warning', follow: false, scrollTop: 420 });
  assert.deepEqual(readConsoleState(first, 'command-a'), { search: '', follow: true, scrollTop: 0 });
  assert.equal(readConsoleState(second, 'command-b').search, '');
  assert.equal([...items.values()].some(value => value.includes('base64') || value.includes('file bytes')), false);
});

test('corrupt state is bounded and a failed save is visible to the caller', () => {
  const store = storage('view');
  store.setItem('view:draft:session', 'broken json');
  assert.equal(readDraft(store, 'session').text, '');
  store.setItem('view:draft:session', JSON.stringify({ text: 'invalid file', files: [{ name: 'x', size: -1, lastModified: 1 }] }));
  assert.equal(readDraft(store, 'session').files.length, 0);
  store.setItem('view:console:command', JSON.stringify({ scrollTop: -50, follow: false }));
  assert.equal(readConsoleState(store, 'command').scrollTop, 0);
  store.setItem('view:draft:session', 'x'.repeat((1 << 20) + 1));
  assert.equal(readDraft(store, 'session').text, '');
  store.setItem = () => { throw new Error('storage unavailable'); };
  assert.throws(() => writeDraft(store, 'session', { text: 'keep in memory', files: [], operationId: '' }), /storage unavailable/);
});

test('a dropped stream resumes after its own last cursor without duplicate output', async () => {
  const opened: bigint[] = [], received: bigint[] = [], statuses: string[] = [];
  const control = new AbortController();
  let calls = 0;
  await follow(async function* (after) {
    opened.push(after);
    if (++calls === 1) { yield { cursor: 1n }; yield { cursor: 2n }; throw new ConnectError('partition', Code.Unavailable); }
    yield { cursor: 2n }; yield { cursor: 3n };
  }, control.signal, event => received.push(event.cursor), status => statuses.push(status));
  assert.deepEqual(opened, [0n, 2n]);
  assert.deepEqual(received, [1n, 2n, 3n]);
  assert(statuses.some(status => status.startsWith('Disconnected:')));
  assert.equal(statuses.at(-1), 'Complete');
});

test('gaps and stale execution identities are explicit terminal outcomes', async () => {
  const received: bigint[] = [], statuses: string[] = [];
  await follow(async function* () { yield { cursor: 2n }; }, new AbortController().signal, event => received.push(event.cursor), status => statuses.push(status));
  assert.deepEqual(received, []);
  assert.match(statuses.at(-1)!, /Replay gap/);
  let attempts = 0;
  await follow(async function* () { attempts++; throw new ConnectError('execution is no longer available', Code.NotFound); }, new AbortController().signal, () => assert.fail('unexpected event'), status => statuses.push(status));
  assert.equal(attempts, 1);
  assert.match(statuses.at(-1)!, /no longer available/);
});

test('canceling a reader ends retry wait without reopening a subscription', async () => {
  const control = new AbortController(); let attempts = 0;
  await follow(async function* () { attempts++; throw new ConnectError('offline', Code.Unavailable); }, control.signal, () => {}, status => {
    if (status.startsWith('Disconnected:')) queueMicrotask(() => control.abort());
  });
  assert.equal(attempts, 1);
});


test('launch choices follow workspace scope across windows without carrying permissions', () => {
  const items = new Map<string,string>();
  const first = {...storage('main',items),scope:'runner:workspace-a'};
  const second = {...storage('detached',items),scope:'runner:workspace-a'};
  const choices = {provider:'codex',profile:'saved',model:'model',gateway:'metered'};
  writeLaunchChoices(first,choices);
  assert.deepEqual(readLaunchChoices(second),choices);
  assert.deepEqual(readLaunchChoices({...second,scope:'runner:workspace-b'}),{});
  first.setItem('runner:workspace-a:launch-choices',JSON.stringify({...choices,permission:'yolo',instructions:'old-context'}));
  assert.deepEqual(readLaunchChoices(first),choices);
  first.setItem('runner:workspace-a:launch-choices','broken');assert.deepEqual(readLaunchChoices(first),{});
});


test('attaching at a cached cursor is not reported as a connection failure',async()=>{
 const statuses:string[]=[];
 await follow(async function* (after){assert.equal(after,7n);yield{cursor:8n};},new AbortController().signal,()=>{},status=>statuses.push(status),undefined,7n);
 assert.deepEqual(statuses,['Connecting','Live','Complete']);
});
