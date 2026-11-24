import assert from 'node:assert/strict';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { LINE_LIMIT, leadingId, route, type Request, type Sinks } from './wire.ts';

function sinks(tools: string[] = []) {
  const seen = { handled: [] as Request[], failed: [] as [string, string][], refused: [] as [string | number, string][], discarded: [] as string[] };
  const out: Sinks = {
    handle: request => seen.handled.push(request),
    failTool: (id, message) => { if (!tools.includes(id)) return false; seen.failed.push([id, message]); return true; },
    refuse: (id, message) => seen.refused.push([id, message]),
    discard: reason => seen.discarded.push(reason),
  };
  return { out, seen };
}

test('a JSON object is a request; anything else is set aside, never the end of the worker', () => {
  const { out, seen } = sinks();
  route('{"id":"lazybox-1","method":"message","params":{"text":"hi"}}', out);
  for (const line of ['null', '7', '"text"', '[1,2]', 'not json', '{"id":']) route(line, out);
  assert.deepEqual(seen.handled, [{ id: 'lazybox-1', method: 'message', params: { text: 'hi' } }]);
  assert.equal(seen.discarded.length, 6);
  assert.deepEqual(seen.refused, []);
});

test('a request too large is refused under its own id; a reply too large fails its tool call', () => {
  const big = (head: string) => head + '"' + 'A'.repeat(200) + '"}}';
  const { out, seen } = sinks(['tool-3']);
  route(big('{"id":"lazybox-9","method":"message","params":{"data":'), out, 100);
  route(big('{"id":"tool-3","result":{"data":'), out, 100);
  route(big('{"method":"event","params":{"data":'), out, 100);
  assert.equal(seen.refused.length, 1);
  assert.equal(seen.refused[0][0], 'lazybox-9');
  assert.match(seen.refused[0][1], /too large for Claude/);
  assert.deepEqual(seen.failed.map(([id]) => id), ['tool-3']);
  assert.equal(seen.discarded.length, 1, 'no id: nothing to answer');
  assert.deepEqual(seen.handled, []);
});

test('the id of a line too long to parse is read from its start', () => {
  assert.equal(leadingId('{"id":"lazybox-12","method":"message"'), 'lazybox-12');
  assert.equal(leadingId(' { "id" : 42 , "result":'), 42);
  assert.equal(leadingId('{"id":"a\\"b","x":1}'), 'a"b');
  assert.equal(leadingId('{"method":"message","id":"late"}'), undefined, 'only a leading id');
  assert.equal(leadingId('{"id":{"nested":1},'), undefined);
  assert.equal(leadingId('[1]'), undefined);
});

test('the limit takes the largest message the Runner accepts, as base64', () => {
  // Kept equal to WorkerLineBytes in the Runner's Claude adapter, whose test
  // encodes a message at the Runner's own payload limit (34 MiB) under it.
  assert.equal(LINE_LIMIT, 64 << 20);
});

test('the worker answers after lines it cannot take instead of exiting', async () => {
  const main = fileURLToPath(new URL('./main.ts', import.meta.url));
  const worker = spawn(process.execPath, ['--import', 'tsx', main], { stdio: ['pipe', 'pipe', 'pipe'] });
  let stderr = '';
  worker.stderr.on('data', chunk => { stderr += chunk; });
  const lines = createInterface({ input: worker.stdout });
  const exited = once(worker, 'exit');
  try {
    worker.stdin.write('null\nnot json\n[1]\n{"id":"lazybox-1","method":"title"}\n');
    const [line] = await Promise.race([
      once(lines, 'line') as Promise<[string]>,
      exited.then(([code]) => { throw new Error(`the worker exited (${code}): ${stderr}`); }),
    ]);
    assert.deepEqual(JSON.parse(line), { id: 'lazybox-1', error: { message: 'session not opened' } });
    assert.equal(worker.exitCode, null, 'still running');
    assert.match(stderr, /ignored a line that is not a JSON object/);
  } finally {
    worker.stdin.end();
    await exited;
  }
});
