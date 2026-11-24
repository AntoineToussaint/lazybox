import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fixtureFile, loadFixture, workspace, type Fixture } from './settings-fixture.js';

/**
 * The check that testdata/settings-not-in-effect.json is still what the
 * bundled Claude Code prints: `npm run test:claude-settings`, a gate of its own
 * in scripts/release-gates.json with its own deadline, run in CI's typescript
 * job and by the release catalog. It starts the real binary, so how long it
 * takes depends on the machine; it is never part of a unit suite, whose tests
 * read only the committed fixture.
 *
 * Claude's informational message carries the same text Claude Code prints on
 * stderr when started outside the SDK, so each case starts the bundled binary
 * with bad settings (no sign-in, no network: it stops at "Not logged in") and
 * keeps its stderr. Drift, a capture that times out or cannot start, and a
 * platform with no bundled binary all fail the check. `--write` rewrites the
 * fixture from the binary.
 */
export type SettingsCase = { name: string; settings: string; file?: string; content?: string };

export const settingsCases: SettingsCase[] = [
  { name: 'inline did not load (permissions)', settings: '{"permissions":{"ask":"Bash"}}' },
  { name: 'inline did not load (other setting)', settings: '{"disableClaudeAiConnectors":"yes"}' },
  { name: 'inline entry skipped (permissions)', settings: '{"permissions":{"ask":["Bash",3]}}' },
  { name: 'inline entry skipped (hooks)', settings: '{"hooks":{"Nope":[]}}' },
  { name: 'project file did not load', settings: '{"permissions":{"ask":["Bash"]}}', file: 'settings.json', content: '{oops' },
  { name: 'local file entry skipped (permissions)', settings: '{"permissions":{"ask":["Bash"]}}', file: 'settings.local.json', content: '{"permissions":{"allow":["Bash",3]}}' },
];

/**
 * How long one start of the binary may take. Generous, because the check
 * runs on loaded machines (a shared box, a CI runner); the gate's deadline
 * bounds the whole check above the sum of these.
 */
export const CAPTURE_TIMEOUT_MS = 60_000;

/** The bundled Claude Code for this platform, or undefined where none is installed. */
export function bundledClaude(): string | undefined {
  const require = createRequire(import.meta.url);
  for (const suffix of ['', '-musl']) {
    try {
      return require.resolve(`@anthropic-ai/claude-agent-sdk-${process.platform}-${process.arch}${suffix}/claude${process.platform === 'win32' ? '.exe' : ''}`);
    } catch { /* the next candidate */ }
  }
  return undefined;
}

/** Claude Code's version, as it reports it. */
export function claudeVersion(binary: string): string {
  const run = spawnSync(binary, ['--version'], { encoding: 'utf8', timeout: CAPTURE_TIMEOUT_MS, env: { PATH: process.env.PATH } });
  if (run.error) throw new Error(`asking ${binary} its version: ${run.error.message}`);
  const version = run.stdout.trim();
  if (run.status !== 0 || !version) throw new Error(`asking ${binary} its version: exit ${run.status ?? run.signal}, said ${JSON.stringify((run.stderr || run.stdout).trim().slice(0, 200))}`);
  return version;
}

/** The source a case's warning must name: Lazybox's inline settings, or the workspace file the case broke. */
export const expectedSource = (c: SettingsCase) => c.file ? `${workspace}/.claude/${c.file}` : '--settings (inline JSON)';

/**
 * Why a capture is not the settings warning (a crash, a sign-in prompt, an
 * empty line), or undefined when it is: it starts with Claude's heading
 * ("These settings …") and names its case's source. `--write` saves nothing
 * while any capture fails this.
 */
export function invalidCapture(c: SettingsCase, text: string): string | undefined {
  if (!text.startsWith('These settings')) return `"${c.name}" is not the settings warning: ${JSON.stringify(text.slice(0, 200))}`;
  if (!text.includes(expectedSource(c))) return `"${c.name}" does not name ${expectedSource(c)}: ${JSON.stringify(text.slice(0, 200))}`;
  return undefined;
}

/** What the binary prints for one case, with the scratch workspace written as <workspace>. */
export function capture(binary: string, c: SettingsCase): string {
  const home = mkdtempSync(join(tmpdir(), 'lazybox-settings-'));
  try {
    const repo = join(home, 'repo');
    mkdirSync(join(repo, '.claude'), { recursive: true });
    if (c.file) writeFileSync(join(repo, '.claude', c.file), c.content ?? '');
    const run = spawnSync(binary, ['-p', 'hi', '--settings', c.settings], { cwd: repo, encoding: 'utf8', timeout: CAPTURE_TIMEOUT_MS, env: { PATH: process.env.PATH, HOME: home, CLAUDE_CONFIG_DIR: join(home, 'config') } });
    if (run.error) throw new Error(`capturing "${c.name}" from ${binary}: ${run.error.message}`);
    if (run.signal) throw new Error(`capturing "${c.name}" from ${binary}: ended by ${run.signal}`);
    return run.stderr.trim().split(realpathSync(repo)).join(workspace).split(repo).join(workspace);
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}

/** The cases whose captured text differs from the fixture, in words; empty when none does. */
export function drift(fixture: Fixture, now: Record<string, string>): string[] {
  const names = [...new Set([...Object.keys(fixture.warnings), ...Object.keys(now)])];
  return names.filter(name => fixture.warnings[name] !== now[name]).map(name =>
    `${name}:\n  fixture: ${JSON.stringify(fixture.warnings[name] ?? null)}\n  binary:  ${JSON.stringify(now[name] ?? null)}`);
}

function main(write: boolean): number {
  const binary = bundledClaude();
  if (!binary) {
    console.error(`no bundled Claude Code for ${process.platform}-${process.arch}: the settings warning cannot be checked here`);
    return 1;
  }
  const now = Object.fromEntries(settingsCases.map(c => [c.name, capture(binary, c)]));
  if (write) {
    // Only the warning itself becomes the fixture: never a crash or a prompt.
    const bad = settingsCases.map(c => invalidCapture(c, now[c.name])).filter((why): why is string => !!why);
    if (bad.length) {
      console.error(`not writing ${fileURLToPath(fixtureFile)}:\n  ${bad.join('\n  ')}`);
      return 1;
    }
    writeFileSync(fixtureFile, JSON.stringify({ claudeCode: claudeVersion(binary), warnings: now }, null, 2) + '\n');
    console.log(`wrote ${fileURLToPath(fixtureFile)}`);
    return 0;
  }
  const differ = drift(loadFixture(), now);
  if (differ.length) {
    console.error(`Claude Code (${claudeVersion(binary)}) no longer prints the settings warning the worker's tests read:\n${differ.join('\n')}\nRun npm run test:claude-settings -- --write, then check settings-effect.ts still reads it.`);
    return 1;
  }
  console.log(`the settings warning fixture matches ${binary} (${settingsCases.length} cases)`);
  return 0;
}

if (process.argv[1] && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))) {
  try {
    process.exitCode = main(process.argv.includes('--write'));
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
