import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { startSites } from './fixture.js';

/** The service as the Runner runs it: a child process speaking JSON lines on stdin and stdout. */
test('the JSON-lines protocol: open, ask the Runner, act, stream frames, refuse, exit on EOF', async () => {
  const sites = await startSites();
  const profile = await mkdtemp(join(tmpdir(), 'lazybox-browser-protocol-'));
  const root = fileURLToPath(new URL('../../..', import.meta.url));
  const child = spawn(process.execPath, ['--import', 'tsx', fileURLToPath(new URL('./main.ts', import.meta.url))], { cwd: root, stdio: ['pipe', 'pipe', 'inherit'] });
  const replies = new Map<string, (line: any) => void>();
  const asks: any[] = [], events: any[] = [];
  const write = (line: unknown) => child.stdin.write(JSON.stringify(line) + '\n');
  createInterface({ input: child.stdout }).on('line', text => {
    const line = JSON.parse(text);
    if (line.method && line.id) {
      // The service asks the Runner: allow the fixture site.
      asks.push(line);
      write({ id: line.id, result: { allow: line.params.origin === sites.home } });
    } else if (line.method) events.push(line);
    else replies.get(line.id)?.(line);
  });
  let next = 0;
  const call = (method: string, params: unknown) => new Promise<any>(resolve => { const id = `r${++next}`; replies.set(id, resolve); write({ id, method, params }); });
  try {
    const opened = await call('open', { browser: 'conv', profile, policy: { loopbackPorts: [sites.ports.home], blockedHosts: [] } });
    assert.equal(opened.result.url, 'about:blank');
    const navigated = await call('act', { browser: 'conv', actor: 'agent', action: 'navigate', url: `${sites.home}/form`, withSnapshot: true });
    assert.match(navigated.result.text, /^Opened "Form"/);
    assert.match(navigated.result.text, /button "Send" \[ref=e\d+\]/);
    assert.deepEqual(asks.map(a => [a.method, a.params.browser, a.params.origin, a.id]), [['site', 'conv', sites.home, 'browser-1']]);
    await call('screencast', { browser: 'conv', on: true });
    for (let i = 0; i < 100 && !events.some(e => e.method === 'frame'); i++) await new Promise(r => setTimeout(r, 50));
    assert.ok(events.some(e => e.method === 'frame' && e.params.browser === 'conv' && e.params.data.startsWith('/9j/')), 'a JPEG frame');
    const refused = await call('act', { browser: 'conv', actor: 'agent', action: 'navigate', url: `${sites.forbidden}/` });
    assert.match(refused.error.message, /not one of the workspace's previews/);
    const unknown = await call('teleport', {});
    assert.match(unknown.error.message, /Unsupported browser operation/);
    const exited = new Promise<number | null>(resolve => child.on('exit', code => resolve(code)));
    child.stdin.end();
    assert.equal(await exited, 0);
  } finally {
    child.kill();
    await sites.close();
    await rm(profile, { recursive: true, force: true });
  }
});
