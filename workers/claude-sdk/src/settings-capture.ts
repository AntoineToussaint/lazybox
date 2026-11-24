import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/**
 * Test support: Claude's settings_not_in_effect warning as the bundled Claude
 * Code prints it. Its informational message carries the same text Claude Code
 * prints on stderr when started outside the SDK, so each case starts the
 * bundled binary with bad settings (no sign-in, no network: it stops at "Not
 * logged in") and keeps its stderr. testdata/settings-not-in-effect.json is
 * that output; settings-effect.test.ts checks it is still what the binary
 * prints, and LAZYBOX_UPDATE_FIXTURES=1 rewrites it.
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

/** Where a captured path to the scratch workspace stands in the fixture. */
export const workspace = '<workspace>';
export const fixtureFile = new URL('./testdata/settings-not-in-effect.json', import.meta.url);

export type Fixture = { claudeCode: string; warnings: Record<string, string> };
export const loadFixture = (): Fixture => JSON.parse(readFileSync(fixtureFile, 'utf8'));

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
  return spawnSync(binary, ['--version'], { encoding: 'utf8', timeout: 60000, env: { PATH: process.env.PATH } }).stdout.trim();
}

/** What the binary prints for one case, with the scratch workspace written as <workspace>. */
export function capture(binary: string, c: SettingsCase): string {
  const home = mkdtempSync(join(tmpdir(), 'lazybox-settings-'));
  try {
    const repo = join(home, 'repo');
    mkdirSync(join(repo, '.claude'), { recursive: true });
    if (c.file) writeFileSync(join(repo, '.claude', c.file), c.content ?? '');
    const run = spawnSync(binary, ['-p', 'hi', '--settings', c.settings], { cwd: repo, encoding: 'utf8', timeout: 60000, env: { PATH: process.env.PATH, HOME: home, CLAUDE_CONFIG_DIR: join(home, 'config') } });
    if (run.error) throw run.error;
    return run.stderr.trim().split(realpathSync(repo)).join(workspace).split(repo).join(workspace);
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}
