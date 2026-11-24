import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { spawn } from 'node:child_process';
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { loadFixture } from './settings-fixture.js';

/**
 * The worker as the Runner runs it (main.ts over its pipe), with a stand-in
 * for Claude Code (`binary`): it answers the SDK's initialize and then prints
 * the lines FAKE_SAY lists, as Claude Code writes them, and records on
 * FAKE_LOG how it ended. Nothing here is a real Claude Code or model; it pins
 * main.ts's own wiring.
 */
const fakeClaude = `#!/usr/bin/env node
const fs = require('fs');
const log = line => fs.appendFileSync(process.env.FAKE_LOG, line + '\\n');
const say = JSON.parse(process.env.FAKE_SAY || '[]');
let buffer = '';
process.stdin.on('data', chunk => {
  buffer += chunk;
  let i;
  while ((i = buffer.indexOf('\\n')) >= 0) {
    const line = buffer.slice(0, i); buffer = buffer.slice(i + 1);
    let m; try { m = JSON.parse(line); } catch { continue; }
    if (m.type === 'control_request' && m.request && m.request.subtype === 'initialize') {
      process.stdout.write(JSON.stringify({ type: 'control_response', response: { subtype: 'success', request_id: m.request_id, response: {} } }) + '\\n');
      for (const s of say) process.stdout.write(JSON.stringify(s) + '\\n');
    }
  }
});
process.stdin.on('end', () => { log('stdin closed'); process.exit(0); });
process.on('SIGTERM', () => { log('terminated'); process.exit(0); });
`;

type Line = { id?: string; result?: unknown; method?: string; params?: { type?: string; status?: string; text?: string; data?: unknown } };

/** Starts the worker, opens it with `open`, and collects what it writes until it exits or `until` holds. */
async function drive(open: Record<string, unknown>, say: unknown[], until: (lines: Line[]) => boolean): Promise<{ lines: Line[]; code: number | null; log: string }> {
  const dir = mkdtempSync(join(tmpdir(), 'lazybox-worker-'));
  try {
    const binary = join(dir, 'claude');
    writeFileSync(binary, fakeClaude);
    chmodSync(binary, 0o755);
    const logFile = join(dir, 'log');
    writeFileSync(logFile, '');
    const main = fileURLToPath(new URL('./main.ts', import.meta.url));
    const child = spawn(process.execPath, ['--import', 'tsx', main], { stdio: ['pipe', 'pipe', 'inherit'], env: { ...process.env, FAKE_LOG: logFile, FAKE_SAY: JSON.stringify(say) } });
    const lines: Line[] = [];
    const exited = new Promise<number | null>(resolve => child.on('exit', code => resolve(code)));
    const done = new Promise<void>(resolve => {
      createInterface({ input: child.stdout }).on('line', line => {
        lines.push(JSON.parse(line));
        if (until(lines)) resolve();
      });
      void exited.then(() => resolve());
    });
    child.stdin.write(JSON.stringify({ id: 'open-1', method: 'open', params: { cwd: dir, binary, ...open } }) + '\n');
    const timer = setTimeout(() => child.kill(), 20000);
    await done;
    const stoppedItself = child.exitCode !== null || child.signalCode !== null;
    let code: number | null = child.exitCode;
    if (!stoppedItself) {
      child.stdin.end();
      code = await exited;
    }
    clearTimeout(timer);
    // A worker that stopped by itself let the stand-in go: it records how, soon after.
    for (let i = 0; stoppedItself && i < 50 && !readFileSync(logFile, 'utf8'); i++) await new Promise(r => setTimeout(r, 50));
    return { lines, code, log: readFileSync(logFile, 'utf8') };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

const warning = (content: string) => ({ type: 'system', subtype: 'informational', tag: 'settings_not_in_effect', level: 'warning', content, uuid: '00000000-0000-4000-8000-000000000001', session_id: 'native' });
// What the bundled Claude Code prints, as committed (settings-fixture.ts; npm run test:claude-settings checks it still does).
const real = loadFixture().warnings;
const events = (lines: Line[]) => lines.filter(l => l.method === 'event').map(l => l.params!);

test('an ask session whose own permission settings Claude rejected closes Claude, says why last, and exits', async () => {
  for (const name of ['inline did not load (permissions)', 'inline entry skipped (permissions)']) {
    const { lines, code, log } = await drive({ permission: 'ask' }, [warning(real[name])], () => false);
    assert.equal(lines[0]?.id, 'open-1', `${name}: the open reply comes first`);
    const said = events(lines);
    const last = said.at(-1)!;
    assert.equal(last.type, 'failure', name);
    assert.equal(last.status, 'sdk_error', `${name}: the stop is the last line, not the stream end that follows the close`);
    assert.match(last.text!, /stopped rather than run a command without asking you/, name);
    assert.ok(last.text!.includes('--settings (inline JSON) (permissions.ask)'), name);
    assert.equal(said.filter(e => e.type === 'failure').length, 1, `${name}: one failure: the close adds no provider_process_exit`);
    assert.equal(code, 1, `${name}: the worker exits, so the Runner sees the agent process end`);
    assert.match(log, /stdin closed|terminated/, `${name}: Claude Code was let go (q.close())`);
  }
});

test('an ask session whose only rejected settings are the person’s own shows a notice and keeps running', async () => {
  for (const [name, file] of [['project file did not load', 'settings.json'], ['local file entry skipped (permissions)', 'settings.local.json']]) {
    const { lines, code } = await drive({ permission: 'ask' }, [warning(real[name])], ls => events(ls).some(e => e.type === 'provider_notice'));
    const notice = events(lines).find(e => e.type === 'provider_notice')!;
    assert.equal(notice?.status, 'settings_not_in_effect', name);
    assert.ok(notice.text!.includes(`<workspace>/.claude/${file}`), name);
    assert.equal(events(lines).filter(e => e.type === 'failure').length, 0, `${name}: nothing faults the conversation`);
    // It was still running: it ended only when its pipe closed, as a worker does.
    assert.notEqual(code, 1, name);
  }
});

test('a grant the worker was opened with and does not hold is reported right after the open reply', async () => {
  const kept = { id: 'g-exact', label: '`make test`', tool: 'Bash', exact: 'make test' };
  const { lines } = await drive({ permission: 'ask', grants: [kept, { id: 'g-wide', label: 'every Monitor request', tool: 'Monitor', all: true }] }, [], ls => ls.length >= 2);
  assert.equal(lines[0]?.id, 'open-1');
  assert.deepEqual(lines[1]?.params && { type: lines[1].params.type, data: lines[1].params.data }, { type: 'permission_grant_dropped', data: { id: 'g-wide' } });
  assert.equal(events(lines).filter(e => e.type === 'permission_grant_dropped').length, 1, 'the grant it holds is not reported');
});
