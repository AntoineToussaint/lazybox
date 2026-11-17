// Builds an installable Lazybox release for macOS (RELEASING.md):
//
//   make release VERSION=0.1.0                  # what CI runs on a v2-v0.1.0 tag
//   node scripts/release/build.mjs --version 0.1.0 [--gates] [--skip-smoke] [--publishable] [--without-claude-code]
//   node scripts/release/build.mjs --from-source [--version 0.1.0] [--skip-smoke] [--without-claude-code]
//
// Output, in dist/<version>/ beside the Makefile:
//   Lazybox_<version>_aarch64.dmg               the disk image (drag to Applications)
//   Lazybox_<version>_aarch64.app.tar.gz(.sig)  the updater's archive and its signature
//   latest.json                                 the update feed entry
//   lazybox-v2.rb                               the Homebrew cask
//   RELEASE_NOTES.md, SHA256SUMS, release.json  notes, checksums, what was signed how
//   THIRD_PARTY_NOTICES.txt                     every bundled component's licence (also in the app and the DMG)
//
// Signing follows the secrets present, never a flag: APPLE_SIGNING_IDENTITY
// signs with a Developer ID (hardened runtime), APPLE_ID/APPLE_PASSWORD/
// APPLE_TEAM_ID (or an App Store Connect API key) also notarize and staple.
// Without them the build is ad-hoc signed and says so everywhere: in
// release.json, in the release notes and in the GitHub release title.
// The updater key: TAURI_SIGNING_PRIVATE_KEY with packaging/updater.pub, or
// (never with --publishable) a throwaway key generated under .cache for a
// local build, which no installed app trusts.
//
// --from-source is what `make install` builds (scripts/source-install.mjs):
// the app alone, ad-hoc signed whatever secrets are in the environment, with
// no updater at all (no key, no feed: it updates with `make update`), and a
// runtime/SOURCE.json naming the commit and checkout it was built from. No
// disk image, feed, cask or checksums. The version defaults to the newest
// one CHANGELOG.md names.
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { cp, mkdir, readFile, rm, writeFile, readdir, stat, chmod, rename, access } from 'node:fs/promises';
import { resolve, dirname, join, relative, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { readChangelog, releaseBody, updaterNotes, updaterReleases, versionPattern, renderReleaseNotes, releaseNotesPath, unfoldedAbove } from './changelog.mjs';
import { productionPackages, isClaudeCodePackage } from './packages.mjs';
import { writeNotices, NOTICES_FILE } from './notices.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const REPOSITORY = 'AntoineToussaint/lazybox';
/** The release tag of a version: its own namespace, so V1's cargo-dist tags (v0.1.21) never collide. */
export const tagOf = version => `v2-v${version}`;
/** The rolling release whose latest.json every installed app polls. */
export const UPDATES_TAG = 'v2-updates';
export const ARCH = { tauri: 'darwin-aarch64', file: 'aarch64', go: 'arm64', node: 'darwin-arm64' };

export function artifactNames(version) {
  const base = `Lazybox_${version}_${ARCH.file}`;
  return { dmg: `${base}.dmg`, updater: `${base}.app.tar.gz`, signature: `${base}.app.tar.gz.sig`, latest: 'latest.json', cask: 'lazybox-v2.rb', notes: 'RELEASE_NOTES.md', notices3p: NOTICES_FILE, sums: 'SHA256SUMS', manifest: 'release.json' };
}

export function downloadURL(version, file) {
  return `https://github.com/${REPOSITORY}/releases/download/${tagOf(version)}/${file}`;
}

/**
 * The update feed entry the Tauri updater reads (plugins.updater.endpoints).
 * `lazybox.releases` carries the changelog's sections from this version
 * down, so an installed app shows everything since its own version before
 * the person installs (apps/desktop/src-tauri/src/updates.rs). The updater
 * ignores the field; only the signature decides what installs.
 */
export function latestJson({ version, notes, signature, releases = [], date = new Date() }) {
  if (!versionPattern.test(version)) throw new Error(`${version} is not a version`);
  if (!signature?.trim()) throw new Error('the updater archive has no signature');
  return {
    version,
    notes,
    pub_date: date.toISOString().replace(/\.\d{3}Z$/, 'Z'),
    platforms: { [ARCH.tauri]: { signature: signature.trim(), url: downloadURL(version, artifactNames(version).updater) } },
    ...(releases.length ? { lazybox: { releases } } : {}),
  };
}

/** The cask, rendered from packaging/homebrew/lazybox-v2.rb.tmpl like V1's lazybox-desktop cask. */
export function renderCask(template, version, sha256) {
  if (!/^[0-9a-f]{64}$/.test(sha256)) throw new Error('invalid SHA-256');
  return template.replaceAll('__VERSION__', version).replaceAll('__SHA256__', sha256);
}

/** How this build signs, from the environment alone. */
export function signingPlan(env) {
  const identity = env.APPLE_SIGNING_IDENTITY?.trim() || '';
  const password = env.APPLE_PASSWORD || env.APPLE_APP_PASSWORD || '';
  const appleId = identity && env.APPLE_ID && password && env.APPLE_TEAM_ID ? { APPLE_ID: env.APPLE_ID, APPLE_PASSWORD: password, APPLE_TEAM_ID: env.APPLE_TEAM_ID } : undefined;
  const apiKey = identity && env.APPLE_API_KEY && env.APPLE_API_ISSUER && env.APPLE_API_KEY_PATH ? { APPLE_API_KEY: env.APPLE_API_KEY, APPLE_API_ISSUER: env.APPLE_API_ISSUER, APPLE_API_KEY_PATH: env.APPLE_API_KEY_PATH } : undefined;
  return { identity: identity || '-', signed: !!identity, notarize: appleId || apiKey, notarizeWith: appleId ? 'apple-id' : apiKey ? 'api-key' : '' };
}

/**
 * CFBundleVersion for a release version. macOS wants one to three
 * period-separated integers that grow with every build (0.1.0-pre.1 is not
 * one, and neither is 0.1.0.1, which has four), while the version people see
 * stays CFBundleShortVersionString. So x.y.z-<label>.<n> is x.y.(z*1000+n)
 * and x.y.z is x.y.(z*1000+999), above all of its prereleases:
 * 0.1.0-pre.1 → 0.1.1, 0.1.0-pre.2 → 0.1.2, 0.1.0 → 0.1.999, 0.1.1-rc.1 →
 * 0.1.1001. A prerelease without a trailing number counts as 0; one series
 * of numbered prereleases per version keeps the order (pre.2 and rc.1 of one
 * version are told apart by their numbers only).
 */
export function bundleVersion(version) {
  const m = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/.exec(version);
  if (!m) throw new Error(`${version} is not a version`);
  const [, major, minor, patch, pre] = m;
  const n = pre === undefined ? 999 : Number(/(\d+)$/.exec(pre)?.[1] ?? 0);
  if (n > 999 || (pre !== undefined && n > 998)) throw new Error(`${version}: a prerelease number above 998 leaves no room in CFBundleVersion`);
  return `${major}.${minor}.${Number(patch) * 1000 + n}`;
}

/**
 * The Tauri configuration merged over tauri.conf.json for this release. A
 * build from source (`source`) has no updater: no feed, no key, no updater
 * archive, so the app never offers an update it could not install.
 */
export function tauriReleaseConfig(base, { version, runtimeDir, notices, identity, pubkey, source = false }) {
  const { $comment, ...config } = structuredClone(base);
  config.version = version;
  const resources = { [runtimeDir.replace(/\/?$/, '/')]: 'runtime/', ...(notices ? { [notices]: NOTICES_FILE } : {}) };
  config.bundle = { ...config.bundle, resources, macOS: { ...config.bundle?.macOS, bundleVersion: bundleVersion(version), signingIdentity: identity } };
  if (source) {
    config.bundle.createUpdaterArtifacts = false;
    const { updater, ...plugins } = config.plugins || {};
    config.plugins = plugins;
  } else {
    config.plugins = { ...config.plugins, updater: { ...config.plugins?.updater, pubkey } };
  }
  return config;
}

/** The version a build from source carries: the newest released version CHANGELOG.md names. */
export function sourceVersion(releases) {
  const found = releases.find(r => versionPattern.test(r.id))?.id;
  if (!found) throw new Error('CHANGELOG.md names no version (## [x.y.z] - date · title)');
  return found;
}

/** runtime/SOURCE.json: what a build from source was built from, for About and `make update`. */
export function sourceManifest({ commit, dirty, checkout, date = new Date() }) {
  return { commit: commit || '', dirty: !!dirty, checkout, builtAt: date.toISOString().replace(/\.\d{3}Z$/, 'Z') };
}

// ---- the pipeline ----

const cache = join(root, '.cache', 'release');
/** Where the Tauri build leaves the app (make install copies it from here). */
export const builtApp = join(cache, 'target', 'release', 'bundle', 'macos', 'Lazybox.app');
const buildEnv = { ...process.env, CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS || '4', GOMAXPROCS: process.env.GOMAXPROCS || '4', GOFLAGS: process.env.GOFLAGS || '-p=4' };

function step(title) { console.log(`\n▸ ${title}`); }

async function run(command, args, { cwd = root, env = buildEnv, capture = false } = {}) {
  const child = spawn(command, args, { cwd, env, stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit' });
  let out = '';
  child.stdout?.on('data', chunk => { out += chunk; });
  const [code, signal] = await once(child, 'exit');
  if (code !== 0) throw new Error(`${command} ${args.join(' ')} failed (${signal || code})`);
  return out;
}

async function sha256(path) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
}

async function exists(path) { try { await access(path); return true; } catch { return false; } }

export function parseArgs(argv, env = process.env) {
  // claudeCode: ship the SDK's copy of Claude Code. --without-claude-code (or
  // LAZYBOX_RELEASE_CLAUDE_CODE=omit) leaves it out (CLAUDE_COMPLIANCE.md).
  const options = { version: env.VERSION || '', gates: false, smoke: true, publishable: false, source: false, launchd: env.LAZYBOX_V2_RELEASE_LAUNCHD === '1', claudeCode: env.LAZYBOX_RELEASE_CLAUDE_CODE !== 'omit' };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--version') options.version = argv[++i] || '';
    else if (a === '--gates') options.gates = true;
    else if (a === '--skip-smoke') options.smoke = false;
    else if (a === '--publishable') options.publishable = true;
    else if (a === '--launchd') options.launchd = true;
    else if (a === '--from-source') options.source = true;
    else if (a === '--without-claude-code') options.claudeCode = false;
    else if (a === '--with-claude-code') options.claudeCode = true;
    else throw new Error(`Unknown option ${a}`);
  }
  if (options.source && options.publishable) throw new Error('--from-source builds are never published');
  if (options.source && !options.version) return options;
  if (!versionPattern.test(options.version)) throw new Error('Pass the release version: make release VERSION=x.y.z');
  return options;
}

async function updaterKey(publishable) {
  const pubPath = join(root, 'packaging', 'updater.pub');
  if (process.env.TAURI_SIGNING_PRIVATE_KEY) {
    if (!await exists(pubPath)) throw new Error('TAURI_SIGNING_PRIVATE_KEY is set but packaging/updater.pub is missing: commit the public key of that pair (RELEASING.md)');
    return { pubkey: (await readFile(pubPath, 'utf8')).trim(), env: {}, kind: 'release' };
  }
  if (publishable) throw new Error('A publishable release needs the updater signing key: set TAURI_SIGNING_PRIVATE_KEY (and _PASSWORD) and commit packaging/updater.pub (RELEASING.md)');
  // A local build only: a throwaway pair under .cache, which no installed app trusts.
  const key = join(cache, 'updater-test.key');
  if (!await exists(key)) {
    await mkdir(cache, { recursive: true });
    await run(join(root, 'node_modules/.bin/tauri'), ['signer', 'generate', '--ci', '--force', '-p', '', '-w', key], { capture: true });
  }
  console.log('  Updater: a local test key (.cache/release/updater-test.key). Installed releases will not accept this build as an update.');
  return { pubkey: (await readFile(key + '.pub', 'utf8')).trim(), env: { TAURI_SIGNING_PRIVATE_KEY: await readFile(key, 'utf8'), TAURI_SIGNING_PRIVATE_KEY_PASSWORD: '' }, kind: 'test' };
}

async function buildGo(runtime, version) {
  const ldflags = `-s -w -X github.com/AntoineToussaint/lazybox/services/runner/code/internal/version.Version=${version}`;
  const env = { ...buildEnv, GOOS: 'darwin', GOARCH: ARCH.go, CGO_ENABLED: '0' };
  const code = join(root, 'services/runner/code');
  await run('go', ['build', '-trimpath', '-ldflags', ldflags, '-o', join(runtime, 'bin', 'lazybox-runner'), '.'], { cwd: code, env });
  await run('go', ['build', '-trimpath', '-ldflags', ldflags, '-o', join(runtime, 'bin', 'lazybox'), './cmd/lazybox'], { cwd: code, env });
}

async function fetchNode(runtime) {
  const pin = JSON.parse(await readFile(join(root, 'scripts/release/node.json'), 'utf8'));
  const archive = pin.archives[ARCH.node];
  const downloads = join(cache, 'downloads');
  await mkdir(downloads, { recursive: true });
  const file = join(downloads, archive.file);
  if (!await exists(file) || await sha256(file) !== archive.sha256) {
    const url = `https://nodejs.org/dist/${pin.version}/${archive.file}`;
    console.log(`  Downloading ${url}`);
    const response = await fetch(url);
    if (!response.ok) throw new Error(`${url}: ${response.status}`);
    await writeFile(file + '.part', Buffer.from(await response.arrayBuffer()));
    await rename(file + '.part', file);
  }
  const digest = await sha256(file);
  if (digest !== archive.sha256) throw new Error(`${archive.file} has SHA-256 ${digest}, pinned ${archive.sha256}`);
  const unpacked = join(cache, 'node');
  await rm(unpacked, { recursive: true, force: true });
  await mkdir(unpacked, { recursive: true });
  await run('tar', ['-xzf', file, '-C', unpacked, '--strip-components', '1']);
  await mkdir(join(runtime, 'node', 'bin'), { recursive: true });
  await cp(join(unpacked, 'bin', 'node'), join(runtime, 'node', 'bin', 'node'));
  await cp(join(unpacked, 'LICENSE'), join(runtime, 'node', 'LICENSE'));
  return pin.version;
}

const WORKER = 'workers/claude-sdk';

/** The worker's packages that ship: all of them, or all but Claude Code's. */
export function workerPackages(packages, { claudeCode = true } = {}) {
  return claudeCode ? packages : packages.filter(p => !isClaudeCodePackage(p.name));
}

/** The compiled worker and exactly the production packages the lockfile resolved for it. */
async function bundleWorker(runtime, { claudeCode = true } = {}) {
  await run('npm', ['run', 'build:worker']);
  const worker = join(runtime, 'worker');
  await cp(join(root, 'workers/claude-sdk/dist'), join(worker, 'dist'), { recursive: true, filter: source => !source.endsWith('.test.js') && !source.endsWith('.d.ts') });
  await cp(join(root, 'workers/claude-sdk/package.json'), join(worker, 'package.json'));
  // Resolved from package.json and node_modules (packages.mjs), not from
  // `npm ls` text, which hides UUID-looking path segments as ***.
  const packages = workerPackages(productionPackages(root, WORKER), { claudeCode });
  const placed = new Map();
  for (const p of packages) {
    if (placed.has(p.path)) throw new Error(`${p.name}@${p.version} and ${placed.get(p.path)} both belong at worker/node_modules/${p.path}`);
    placed.set(p.path, `${p.name}@${p.version}`);
    // Each package without its own node_modules: the ones in it that the
    // worker needs are in the list, at their own paths.
    await cp(p.dir, join(worker, 'node_modules', p.path), { recursive: true, filter: source => basename(source) !== '.bin' && !relative(p.dir, source).split('/').includes('node_modules') });
  }
  if (claudeCode && !packages.some(p => p.name === `@anthropic-ai/claude-agent-sdk-${ARCH.node}`)) throw new Error(`The Claude Agent SDK for ${ARCH.node} is not installed; run npm ci on this platform`);
  return packages.length;
}

/** Mach-O files in the runtime: ours are signed here, a vendor's must carry a valid Developer ID signature. */
async function machOFiles(dir) {
  const found = [];
  const walk = async path => {
    for (const entry of await readdir(path, { withFileTypes: true })) {
      const full = join(path, entry.name);
      if (entry.isDirectory()) await walk(full);
      else if (entry.isFile() && (await stat(full)).size > 4096) {
        const handle = await readFile(full).then(b => b.subarray(0, 4));
        const magic = handle.readUInt32BE(0);
        if ([0xcffaedfe, 0xcefaedfe, 0xfeedfacf, 0xfeedface, 0xcafebabe].includes(magic)) found.push(full);
      }
    }
  };
  await walk(dir);
  return found;
}

async function signRuntime(runtime, plan) {
  const ours = new Set([join(runtime, 'bin', 'lazybox'), join(runtime, 'bin', 'lazybox-runner')]);
  const report = [];
  for (const file of await machOFiles(runtime)) {
    const rel = relative(runtime, file);
    if (ours.has(file)) {
      const args = plan.signed ? ['--force', '--options', 'runtime', '--timestamp', '--sign', plan.identity, file] : ['--force', '--sign', '-', file];
      await run('codesign', args);
      report.push({ file: rel, signedBy: plan.signed ? plan.identity : 'ad-hoc' });
      continue;
    }
    // Node and Claude Code keep their publishers' signatures: re-signing would
    // only replace a notarized Developer ID with ours.
    await run('codesign', ['--verify', '--strict', file]);
    const details = await new Promise(done => {
      const child = spawn('codesign', ['-dvv', file]);
      let text = '';
      child.stderr.on('data', c => { text += c; });
      child.on('exit', () => done(text));
    });
    const authority = /Authority=(Developer ID Application: [^\n]+)/.exec(details)?.[1];
    if (!authority) throw new Error(`${rel} is not signed with a Developer ID; it cannot ship in a notarized app`);
    if (!/flags=0x10000\(runtime\)/.test(details)) throw new Error(`${rel} lacks the hardened runtime`);
    report.push({ file: rel, signedBy: authority });
  }
  return report;
}

async function dmgbuild() {
  const venv = join(cache, 'dmgbuild-venv');
  const marker = join(venv, '.requirements');
  const requirements = await readFile(join(root, 'packaging/dmg/requirements.txt'), 'utf8');
  if (!await exists(marker) || await readFile(marker, 'utf8') !== requirements) {
    await rm(venv, { recursive: true, force: true });
    await run('python3', ['-m', 'venv', venv]);
    await run(join(venv, 'bin', 'pip'), ['install', '--quiet', '--require-hashes', '--only-binary', ':all:', '-r', join(root, 'packaging/dmg/requirements.txt')]);
    await writeFile(marker, requirements);
  }
  return join(venv, 'bin', 'dmgbuild');
}

async function notarize(path, plan) {
  const credentials = plan.notarizeWith === 'apple-id'
    ? ['--apple-id', plan.notarize.APPLE_ID, '--password', plan.notarize.APPLE_PASSWORD, '--team-id', plan.notarize.APPLE_TEAM_ID]
    : ['--key', plan.notarize.APPLE_API_KEY_PATH, '--key-id', plan.notarize.APPLE_API_KEY, '--issuer', plan.notarize.APPLE_API_ISSUER];
  await run('xcrun', ['notarytool', 'submit', path, '--wait', '--timeout', '30m', ...credentials]);
  await run('xcrun', ['stapler', 'staple', path]);
}

/** A git answer about this checkout, or '' outside one. */
function gitText(args) {
  const result = spawnSync('git', ['-C', root, ...args], { encoding: 'utf8' });
  return result.status === 0 ? result.stdout.trim() : '';
}

async function main() {
  if (process.platform !== 'darwin') throw new Error('Releases are built on macOS');
  const options = parseArgs(process.argv.slice(2));
  const releases = await readChangelog();
  const version = options.version || sourceVersion(releases);
  // A build from source never signs with what happens to be in the environment.
  const plan = signingPlan(options.source ? {} : process.env);
  const names = artifactNames(version);
  const out = join(root, 'dist', version);
  const stage = join(cache, 'stage', version);
  const runtime = join(stage, 'runtime');
  console.log(`Lazybox ${version} for macOS (${ARCH.file})${options.source ? ', built from source' : ''}`);
  console.log(`  Signing: ${plan.signed ? `Developer ID (${plan.identity})` : options.source ? 'ad-hoc, for this Mac' : 'ad-hoc (no APPLE_SIGNING_IDENTITY): an unsigned build'}`);
  if (!options.source) console.log(`  Notarization: ${plan.notarize ? plan.notarizeWith : 'none'}`);
  if (options.publishable && !plan.signed) console.log('  This release will be published as UNSIGNED: macOS asks people to confirm before opening it.');

  step('Release notes');
  if (await readFile(releaseNotesPath, 'utf8') !== renderReleaseNotes(releases)) throw new Error('packages/surfaces/src/release-notes.ts does not match CHANGELOG.md: run node scripts/release/changelog.mjs --write and commit');
  const prerelease = version.includes('-');
  const noteVersion = releases.some(r => r.id === version) ? version : prerelease ? releases.find(r => versionPattern.test(r.id))?.id : undefined;
  if (!noteVersion) throw new Error(`CHANGELOG.md has no section for ${version}; add "## [${version}] - YYYY-MM-DD · Title" at the top, then run node scripts/release/changelog.mjs --write`);
  if (noteVersion !== version) console.log(`  ${version} is a test version: using the notes of ${noteVersion}.`);
  const unfolded = noteVersion === version ? unfoldedAbove(releases, version) : [];
  if (unfolded.length) throw new Error(`CHANGELOG.md has dated sections above ${version}'s (${unfolded.join(', ')}), which its release notes would leave out: fold them into its section, then node scripts/release/changelog.mjs --write`);

  const key = options.source ? { pubkey: undefined, env: {}, kind: 'none' } : await updaterKey(options.publishable);

  if (options.gates) {
    step('Release gates (scripts/release-gates.json)');
    await run(process.execPath, ['scripts/release-gates.mjs', 'run', 'release', '--report', join(cache, `gates-${version}.json`)]);
  }

  step('Runtime: Runner, CLI, Node, Claude worker, phone app');
  await rm(stage, { recursive: true, force: true });
  await mkdir(join(runtime, 'bin'), { recursive: true });
  await run('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund']);
  await buildGo(runtime, version);
  const nodeVersion = await fetchNode(runtime);
  const packages = await bundleWorker(runtime, { claudeCode: options.claudeCode });
  await run('npm', ['run', 'build:web']);
  await cp(join(root, 'apps/web/dist'), join(runtime, 'web'), { recursive: true });
  // The documentation Ask Lazybox reads (the app bundles its own copy for
  // Help → Documentation): Markdown only, never docs/internal.
  const docs = join(root, 'docs');
  await cp(docs, join(runtime, 'docs'), { recursive: true, filter: source => { const rel = relative(docs, source); return !rel.startsWith('internal') && !rel.startsWith('images') && (!rel || rel.endsWith('.md') || !rel.includes('.')); } });
  await writeFile(join(runtime, 'VERSION'), version + '\n');
  if (options.source) {
    const commit = gitText(['rev-parse', 'HEAD']);
    const dirty = !!gitText(['status', '--porcelain', '--untracked-files=no']);
    await writeFile(join(runtime, 'SOURCE.json'), JSON.stringify(sourceManifest({ commit, dirty, checkout: root }), null, 2) + '\n');
  }
  for (const exe of ['bin/lazybox', 'bin/lazybox-runner', 'node/bin/node']) await chmod(join(runtime, exe), 0o755);
  console.log(`  Node ${nodeVersion}, ${packages} worker packages${options.claudeCode ? ', Claude Code included' : ', without Claude Code (the person installs it)'}`);
  const signatures = await signRuntime(runtime, plan);

  step('Third-party notices');
  const notices = join(stage, NOTICES_FILE);
  const noticeReport = writeNotices({ out: notices, version, nodeLicense: join(runtime, 'node', 'LICENSE'), nodeVersion, omit: options.claudeCode ? undefined : isClaudeCodePackage });
  console.log(`  ${noticeReport.components} components in ${NOTICES_FILE}`);
  // Shipped, and listed at the top of the notices; whether their terms allow
  // redistributing them is the publisher's call (RELEASING.md).
  for (const a of noticeReport.attention) console.log(`  ! ${a.name} ${a.version}: ${a.license || 'no licence stated'}`);

  step('Desktop app (Tauri)');
  await run('npm', ['run', 'build:ui', '-w', '@lazybox/desktop']);
  const base = JSON.parse(await readFile(join(root, 'packaging/release.tauri.json'), 'utf8'));
  const configPath = join(stage, 'tauri.release.json');
  await writeFile(configPath, JSON.stringify(tauriReleaseConfig(base, { version, runtimeDir: runtime, notices, identity: plan.identity, pubkey: key.pubkey, source: options.source }), null, 2));
  const target = join(cache, 'target');
  const tauriEnv = { ...buildEnv, ...key.env, CARGO_TARGET_DIR: target };
  for (const name of ['APPLE_SIGNING_IDENTITY', 'APPLE_ID', 'APPLE_PASSWORD', 'APPLE_APP_PASSWORD', 'APPLE_TEAM_ID', 'APPLE_API_KEY', 'APPLE_API_ISSUER', 'APPLE_API_KEY_PATH', 'APPLE_CERTIFICATE', 'APPLE_CERTIFICATE_PASSWORD']) delete tauriEnv[name];
  if (plan.signed) tauriEnv.APPLE_SIGNING_IDENTITY = plan.identity;
  if (plan.notarize) Object.assign(tauriEnv, plan.notarize);
  const bundles = join(target, 'release', 'bundle', 'macos');
  await rm(bundles, { recursive: true, force: true });
  await run(join(root, 'node_modules/.bin/tauri'), ['build', '--bundles', 'app', '--config', configPath], { cwd: join(root, 'apps/desktop'), env: tauriEnv });
  const app = join(bundles, 'Lazybox.app');
  await run('codesign', ['--verify', '--deep', '--strict', '--verbose=2', app]);
  if (!await exists(join(app, 'Contents', 'Resources', NOTICES_FILE))) throw new Error(`${NOTICES_FILE} is not in the app bundle`);
  if (plan.notarize) {
    await run('xcrun', ['stapler', 'validate', app]);
    await run('spctl', ['--assess', '--type', 'exec', '-vv', app]);
  }

  if (options.source) {
    if (options.smoke) await smoke(app, options);
    console.log(`\n✓ Lazybox ${version} built from source: ${app}`);
    return;
  }

  step('Disk image');
  await rm(out, { recursive: true, force: true });
  await mkdir(out, { recursive: true });
  const background = join(stage, 'background.tiff');
  await run('tiffutil', ['-cathidpicheck', join(root, 'packaging/dmg/background.png'), join(root, 'packaging/dmg/background@2x.png'), '-out', background]);
  const dmg = join(out, names.dmg);
  await run(await dmgbuild(), ['-s', join(root, 'packaging/dmg/dmg-settings.py'), '-D', `app=${app}`, '-D', `notices=${notices}`, '-D', `background=${background}`, '-D', `icon=${join(root, 'apps/desktop/src-tauri/icons/icon.icns')}`, 'Lazybox', dmg]);
  if (plan.signed) await run('codesign', ['--force', '--timestamp', '--sign', plan.identity, dmg]);
  if (plan.notarize) {
    await notarize(dmg, plan);
    await run('spctl', ['--assess', '--type', 'open', '--context', 'context:primary-signature', '-vv', dmg]);
  }

  step('Update feed, cask, notes, checksums');
  await cp(join(bundles, 'Lazybox.app.tar.gz'), join(out, names.updater));
  await cp(join(bundles, 'Lazybox.app.tar.gz.sig'), join(out, names.signature));
  await cp(notices, join(out, names.notices3p));
  const signature = await readFile(join(out, names.signature), 'utf8');
  await writeFile(join(out, names.latest), JSON.stringify(latestJson({ version, notes: updaterNotes(releases, noteVersion), releases: updaterReleases(releases, noteVersion), signature }), null, 2) + '\n');
  const dmgSum = await sha256(dmg);
  await writeFile(join(out, names.cask), renderCask(await readFile(join(root, 'packaging/homebrew/lazybox-v2.rb.tmpl'), 'utf8'), version, dmgSum));
  await writeFile(join(out, names.notes), releaseBody(releases, noteVersion, { unsigned: !plan.notarize }));
  const sums = [];
  for (const file of [names.dmg, names.updater, names.signature, names.latest, names.cask, names.notices3p]) sums.push(`${await sha256(join(out, file))}  ${file}`);
  await writeFile(join(out, names.sums), sums.join('\n') + '\n');
  const manifest = {
    version, tag: tagOf(version), arch: ARCH.file, builtAt: new Date().toISOString(),
    signing: plan.signed ? 'developer-id' : 'ad-hoc', identity: plan.signed ? plan.identity : undefined,
    notarized: !!plan.notarize, updaterKey: key.kind, node: nodeVersion, runtimeSignatures: signatures,
    bundleVersion: bundleVersion(version), notices: { file: NOTICES_FILE, components: noticeReport.components, attention: noticeReport.attention },
    artifacts: Object.fromEntries(sums.map(line => line.split('  ').reverse())),
  };
  await writeFile(join(out, names.manifest), JSON.stringify(manifest, null, 2) + '\n');

  if (options.smoke) await smoke(app, options);

  console.log(`\n✓ Lazybox ${version} is in ${relative(process.cwd(), out) || out}`);
  console.log(`  ${plan.notarize ? 'Signed and notarized' : plan.signed ? 'Signed, NOT notarized' : 'Ad-hoc signed: an UNSIGNED build (RELEASING.md)'}; updater key: ${key.kind}.`);
  console.log(`  App: ${app}`);
}

async function smoke(app, options) {
  step('Smoke test: the built Runner and CLI');
  const smokeEnv = { ...buildEnv, LAZYBOX_V2_RELEASE_RUNTIME: join(app, 'Contents', 'Resources', 'runtime'), ...(options.launchd ? { LAZYBOX_V2_RELEASE_LAUNCHD: '1' } : {}) };
  await run('go', ['test', '-count=1', '-timeout', '5m', '-v', '-run', '^TestRelease', './cmd/lazybox'], { cwd: join(root, 'services/runner/code'), env: smokeEnv });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(`\n✗ ${error.message}`); process.exitCode = 1; });
