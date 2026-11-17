// Explicit live qualification: installs only a unique, disposable Codefly service.
// No providers are started, and the user's Working pointer is never written.
import assert from 'node:assert/strict';
import { access, cp, mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { environmentPaths, promote, readValidatedRelease } from './environments.mjs';
import { controlService, serviceLabel, serviceStatus } from './runner-service.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(check, description, timeout = 180_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const result = await check();
    if (result) return result;
    await delay(250);
  }
  throw new Error(`Timed out: ${description}`);
}
async function info(endpoint, token = endpoint.token) {
  return fetch(new URL('/lazybox.bench.v1.RunnerService/Info', endpoint.url), {
    method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', 'Connect-Protocol-Version': '1' },
    body: '{}', signal: AbortSignal.timeout(1000), redirect: 'error',
  });
}

async function main() {
  if (process.platform !== 'darwin' || process.argv.length !== 3) throw new Error('Use qualify-runner-service.mjs <validated-macOS-release-directory>');
  const release = resolve(process.argv[2]), id = basename(release);
  await readValidatedRelease(dirname(dirname(release)), id);
  const base = join(root, '.cache/service-qualification');
  await mkdir(base, { recursive: true });
  const environment = await mkdtemp(join(base, 'run-'));
  const paths = environmentPaths(environment, 'working');
  const report = { version: 1, label: serviceLabel(environment), release: id, startedAt: new Date().toISOString(), checks: [], status: 'running' };
  const record = name => { report.checks.push(name); console.log(`PASS ${name}`); };
  let installed = false;
  console.log(`Isolated qualification: ${environment}`);
  try {
    assert.equal((await serviceStatus(environment)).installed, false);
    await mkdir(join(environment, 'releases'), { recursive: true });
    await cp(release, join(environment, 'releases', id), { recursive: true, force: false, errorOnExist: true });
    await promote(environment, id);
    await controlService(environment, 'install');
    installed = true;
    assert.equal((await serviceStatus(environment)).running, false);
    record('install does not start the service');
    const marker = join(paths.state, 'qualification-preserve.txt');
    await writeFile(marker, 'preserved', { mode: 0o600 });
    let previous;
    for (let cycle = 1; cycle <= 2; cycle++) {
      await controlService(environment, 'start');
      const endpoint = await until(async () => {
        try {
          const current = JSON.parse(await readFile(paths.endpoint, 'utf8'));
          if (previous?.token === current.token) return false;
          const response = await info(current);
          if (!response.ok) return false;
          await response.json();
          return current;
        } catch { return false; }
      }, 'authenticated Runner readiness');
      assert.equal((await serviceStatus(environment)).running, true);
      record(`cycle ${cycle}: native supervisor and authenticated Runner are ready`);
      const refused = await info(endpoint, 'invalid-qualification-credential');
      assert.equal(refused.ok, false); await refused.arrayBuffer();
      const viewers = await Promise.all([info(endpoint), info(endpoint)]);
      for (const response of viewers) { assert.equal(response.ok, true); await response.json(); }
      // Completed/disconnected independent readers must not own service lifetime.
      const retained = await info(endpoint); assert.equal(retained.ok, true); await retained.json();
      record(`cycle ${cycle}: authentication and independent reader lifetimes`);
      await controlService(environment, 'stop');
      await until(async () => !(await serviceStatus(environment)).running, 'native service stopped', 30_000);
      await until(async () => { try { const response = await info(endpoint); await response.arrayBuffer(); return false; } catch { return true; } }, 'Runner listener released', 30_000);
      assert.equal(await readFile(marker, 'utf8'), 'preserved');
      record(`cycle ${cycle}: stop releases listener and preserves state`);
      previous = endpoint;
    }
    report.status = 'passed';
  } catch (error) {
    report.status = 'failed'; report.error = error.message;
  } finally {
    try {
      // Also reconcile a partial install that failed before returning.
      const status = await serviceStatus(environment);
      installed ||= status.installed;
      if (installed) {
        await controlService(environment, 'stop');
        await until(async () => !(await serviceStatus(environment)).running, 'cleanup stop', 30_000);
        await controlService(environment, 'uninstall');
      }
      assert.equal((await serviceStatus(environment)).installed, false);
      if (installed) await access(paths.state);
      record('qualification service removed; product state retained');
    } catch (error) { report.status = 'failed'; report.cleanupError = error.message; }
    report.finishedAt = new Date().toISOString();
    const receipt = join(environment, 'qualification.json');
    await writeFile(receipt, JSON.stringify(report, null, 2), { mode: 0o600 });
    console.log(`${report.status}: ${receipt}`);
    if (report.error) console.error(report.error);
    if (report.cleanupError) console.error(`Cleanup needs attention for ${report.label}: ${report.cleanupError}`);
    if (report.status !== 'passed') process.exitCode = 1;
  }
}
main().catch(error => { console.error(error.message); process.exitCode = 1; });
