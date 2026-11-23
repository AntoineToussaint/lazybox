import assert from 'node:assert/strict';
import test from 'node:test';
import { exitAfter, readStream } from './stream.ts';

async function* messages(items: string[], failure?: Error) {
  for (const item of items) yield item;
  if (failure) throw failure;
}

test('a stream that ends is reported as the provider process ending, after every message', async () => {
  const seen: string[] = [];
  const end = await readStream(messages(['a', 'b']), message => seen.push(message));
  assert.deepEqual(seen, ['a', 'b']);
  assert.deepEqual(end, { type: 'failure', status: 'provider_process_exit', text: 'SDK stream ended' });
});

test('a stream that fails is reported as an SDK error', async () => {
  const end = await readStream(messages(['a'], new Error('control channel closed')), () => {});
  assert.equal(end.status, 'sdk_error');
  // The error's message, as the person reads it: not JavaScript's "Error: control channel closed".
  assert.equal(end.text, 'control channel closed');
});

test('the worker exits once it has said why the stream ended, never before', () => {
  const lines: string[] = [];
  let pending: (() => void) | undefined;
  const exits: number[] = [];
  exitAfter({ type: 'failure', status: 'provider_process_exit', text: 'SDK stream ended' }, (line, written) => { lines.push(line); pending = written; }, code => exits.push(code));
  assert.deepEqual(lines.map(line => JSON.parse(line)), [{ method: 'event', params: { type: 'failure', status: 'provider_process_exit', text: 'SDK stream ended' } }]);
  assert.ok(lines[0].endsWith('\n'));
  assert.deepEqual(exits, [], 'exited before the line was written');
  pending!();
  assert.deepEqual(exits, [1]);
});
