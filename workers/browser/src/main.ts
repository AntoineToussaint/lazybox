import { createInterface } from 'node:readline';
import { BrowserService, type Runner } from './service.js';

/**
 * The browser service's JSON-lines pipe to the Runner (protocol.ts). stdout
 * carries protocol lines only; anything else goes to stderr.
 */

/** Frames are dropped, not queued, while the Runner reads this far behind: a watcher wants the latest. */
const frameBacklog = 4 << 20;

type Line = { id?: string; method?: string; params?: unknown; result?: unknown; error?: { message?: string } };

const send = (value: unknown) => process.stdout.write(JSON.stringify(value) + '\n');
const waiting = new Map<string, { resolve(value: unknown): void; reject(error: Error): void }>();
let next = 0;

const runner: Runner = {
  request: (method, params) => new Promise((resolve, reject) => {
    const id = `browser-${++next}`;
    waiting.set(id, { resolve, reject });
    send({ id, method, params });
  }),
  emit: (method, params) => {
    if (method === 'frame' && process.stdout.writableLength > frameBacklog) return;
    send({ method, params });
  },
};

const service = new BrowserService(runner);

async function handle(line: Line) {
  if (!line.method && line.id && waiting.has(line.id)) {
    const pending = waiting.get(line.id)!; waiting.delete(line.id);
    if (line.error) pending.reject(new Error(line.error.message || 'Runner error')); else pending.resolve(line.result);
    return;
  }
  if (!line.method || !line.id) return;
  try { send({ id: line.id, result: (await service.handle(line.method, line.params)) ?? {} }); }
  catch (error) { send({ id: line.id, error: { message: error instanceof Error ? error.message : String(error) } }); }
}

let stopping = false;
async function stop() {
  if (stopping) return;
  stopping = true;
  for (const pending of waiting.values()) pending.reject(new Error('The Runner went away'));
  await service.shutdown().catch(error => process.stderr.write(`browser: shutdown: ${String(error)}\n`));
  process.exit(0);
}

const reader = createInterface({ input: process.stdin, crlfDelay: Infinity });
reader.on('line', text => {
  let line: unknown;
  try { line = JSON.parse(text); } catch { process.stderr.write('browser: ignored a line that is not JSON\n'); return; }
  // Every protocol line is an object; anything else (null, a number, a list) has no id to answer, so the
  // error goes back with none, as JSON-RPC does.
  if (typeof line !== 'object' || line === null || Array.isArray(line)) {
    process.stderr.write('browser: ignored a line that is not a JSON object\n');
    send({ id: null, error: { message: 'A protocol line must be a JSON object' } });
    return;
  }
  void handle(line as Line);
});
reader.on('close', () => void stop());
process.on('SIGTERM', () => void stop());
