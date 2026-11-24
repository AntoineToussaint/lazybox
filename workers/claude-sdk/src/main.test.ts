import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { spawn } from 'node:child_process';
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

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
const inlineRejected = 'These settings did not load, so nothing in them is in effect, including hooks, permission rules and sandbox settings, though environment variables they already set can stay:\n  SDK inline settings (permissions.ask): Invalid value\nFix each problem listed, then start Claude Code again.';
const userRejected = 'These settings did not load, so nothing in them is in effect, including hooks, permission rules and sandbox settings, though environment variables they already set can stay:\n  /work/repo/.claude/settings.json: Invalid JSON\nFix each problem listed, then start Claude Code again.';
const events = (lines: Line[]) => lines.filter(l => l.method === 'event').map(l => l.params!);

test('an ask session whose own settings Claude rejected closes Claude, says why last, and exits', async () => {
  const { lines, code, log } = await drive({ permission: 'ask' }, [warning(inlineRejected)], () => false);
  assert.equal(lines[0]?.id, 'open-1', 'the open reply comes first');
  const said = events(lines);
  const last = said.at(-1)!;
  assert.equal(last.type, 'failure');
  assert.equal(last.status, 'sdk_error', 'the stop is the last line, not the stream end that follows the close');
  assert.match(last.text!, /stopped rather than run a command without asking you/);
  assert.ok(last.text!.includes('SDK inline settings (permissions.ask)'));
  assert.equal(said.filter(e => e.type === 'failure').length, 1, 'one failure: the close adds no provider_process_exit');
  assert.equal(code, 1, 'the worker exits, so the Runner sees the agent process end');
  assert.match(log, /stdin closed|terminated/, 'Claude Code was let go (q.close())');
});

test('an ask session whose only rejected settings are the person’s own shows a notice and keeps running', async () => {
  const { lines, code } = await drive({ permission: 'ask' }, [warning(userRejected)], ls => events(ls).some(e => e.type === 'provider_notice'));
  const notice = events(lines).find(e => e.type === 'provider_notice')!;
  assert.equal(notice.status, 'settings_not_in_effect');
  assert.ok(notice.text!.includes('/work/repo/.claude/settings.json'));
  assert.equal(events(lines).filter(e => e.type === 'failure').length, 0, 'nothing faults the conversation');
  // It was still running: it ended only when its pipe closed, as a worker does.
  assert.notEqual(code, 1);
});

test('a grant the worker was opened with and does not hold is reported right after the open reply', async () => {
  const kept = { id: 'g-exact', label: '`make test`', tool: 'Bash', exact: 'make test' };
  const { lines } = await drive({ permission: 'ask', grants: [kept, { id: 'g-wide', label: 'every Monitor request', tool: 'Monitor', all: true }] }, [], ls => ls.length >= 2);
  assert.equal(lines[0]?.id, 'open-1');
  assert.deepEqual(lines[1]?.params && { type: lines[1].params.type, data: lines[1].params.data }, { type: 'permission_grant_dropped', data: { id: 'g-wide' } });
  assert.equal(events(lines).filter(e => e.type === 'permission_grant_dropped').length, 1, 'the grant it holds is not reported');
});
