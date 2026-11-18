import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { artifactNames, latestJson, renderCask, signingPlan, tagOf, tauriReleaseConfig, downloadURL, bundleVersion, parseArgs, workerPackages, LINUX_ARCHES, linuxArtifactNames, sumsText } from './build.mjs';
import { productionPackages, isClaudeCodePackage } from './packages.mjs';
import { renderNotices, needsAttention, NOTICES_FILE } from './notices.mjs';
import { mkdtempSync, mkdirSync, writeFileSync, symlinkSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { parseChangelog, renderReleaseNotes, releaseBody, updaterNotes, updaterReleases, changelogPath, releaseNotesPath, installGuide, installGuidePath, unfoldedAbove, versionPattern } from './changelog.mjs';
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

// CLAUDE_COMPLIANCE.md: a release can leave Anthropic's Claude Code program
// out; the SDK itself and everything else still ship.
test('a release built --without-claude-code ships no Claude Code program', () => {
  const resolved = ['@anthropic-ai/claude-agent-sdk', '@anthropic-ai/claude-agent-sdk-darwin-arm64', '@anthropic-ai/claude-agent-sdk-linux-x64-musl', 'zod'].map(name => ({ name, version: '1' }));
  assert.deepEqual(workerPackages(resolved).map(p => p.name), resolved.map(p => p.name));
  assert.deepEqual(workerPackages(resolved, { claudeCode: false }).map(p => p.name), ['@anthropic-ai/claude-agent-sdk', 'zod']);
  assert.equal(isClaudeCodePackage('@anthropic-ai/claude-agent-sdk'), false);
  assert.equal(parseArgs(['--version', '1.0.0'], {}).claudeCode, true);
  assert.equal(parseArgs(['--version', '1.0.0', '--without-claude-code'], {}).claudeCode, false);
  assert.equal(parseArgs(['--version', '1.0.0'], { LAZYBOX_RELEASE_CLAUDE_CODE: 'omit' }).claudeCode, false);
  assert.equal(parseArgs(['--version', '1.0.0', '--with-claude-code'], { LAZYBOX_RELEASE_CLAUDE_CODE: 'omit' }).claudeCode, true);
});

test('a V2 tag never looks like a V1 tag', () => {
  assert.equal(tagOf('0.1.0'), 'v2-v0.1.0');
  assert.equal(downloadURL('0.1.0', 'x.dmg'), 'https://github.com/AntoineToussaint/lazybox/releases/download/v2-v0.1.0/x.dmg');
});

test('the update feed names the archive on this release and its signature', () => {
  const feed = latestJson({ version: '0.2.0', notes: 'Notes', signature: 'c2lnbmF0dXJl\n', date: new Date('2026-11-17T10:00:00.123Z') });
  assert.deepEqual(feed, {
    version: '0.2.0', notes: 'Notes', pub_date: '2026-11-17T10:00:00Z',
    platforms: { 'darwin-aarch64': { signature: 'c2lnbmF0dXJl', url: `https://github.com/AntoineToussaint/lazybox/releases/download/v2-v0.2.0/${artifactNames('0.2.0').updater}` } },
  });
  assert.throws(() => latestJson({ version: 'latest', notes: '', signature: 'x' }));
  // The changelog's sections ride along for the app's What's new before installing.
  const releases = [{ id: '0.2.0', date: '2026-12-01', title: 'Second', items: [{ title: 'New', body: 'It works.' }] }];
  assert.deepEqual(latestJson({ version: '0.2.0', notes: 'Notes', signature: 's', releases }).lazybox, { releases });
  assert.throws(() => latestJson({ version: '0.2.0', notes: '', signature: ' ' }), /signature/);
});

test('the cask is the template with the version and the disk image checksum', async () => {
  const template = await readFile(resolve(root, 'packaging/homebrew/lazybox-v2.rb.tmpl'), 'utf8');
  const sha = 'a'.repeat(64);
  const cask = renderCask(template, '0.2.0', sha);
  assert.match(cask, /cask "lazybox-v2" do/);
  assert.match(cask, /version "0\.2\.0"/);
  assert.match(cask, new RegExp(`sha256 "${sha}"`));
  assert.ok(!cask.includes('__'), 'a placeholder was left');
  // The download URL the cask builds is the one the release uploads.
  assert.ok(cask.includes('releases/download/v2-v#{version}/Lazybox_#{version}_aarch64.dmg'));
  assert.equal(artifactNames('0.2.0').dmg, 'Lazybox_0.2.0_aarch64.dmg');
  assert.throws(() => renderCask(template, '0.2.0', 'not-a-sum'));
  // The bundled Node needs macOS 13.5; Ventura is as close as a cask can say.
  assert.match(cask, /depends_on macos: ">= :ventura"/);
  // One command, `lazybox`, which the app links on first launch: the cask
  // links no second name, and no `lazybox` that the older formula's
  // bin/lazybox would make fail to install.
  assert.ok(!/^\s*binary /m.test(cask), 'the cask links a binary');
  assert.ok(!cask.includes('target: "lazybox-v2"'));
  // Caveats: the older formula and its fix, and how to open an unsigned
  // build; no product generations.
  const caveats = /caveats <<~EOS\n([^]*?)\n {2}EOS/.exec(cask)?.[1] || '';
  for (const words of ['`lazybox` in your terminal runs it', 'brew uninstall lazybox', 'lazybox doctor', 'Privacy & Security', 'Open Anyway', 'xattr -dr com.apple.quarantine /Applications/Lazybox.app']) assert.ok(caveats.includes(words), `the caveats lack ${words}`);
  for (const words of ['lazybox-v2', 'V1', 'V2']) assert.ok(!caveats.includes(words), `the caveats say ${words}`);
  // Homebrew's conflicts_with takes only cask:, and the tap has no
  // lazybox-desktop cask; V1's site is not this app's home.
  assert.ok(!/^\s*conflicts_with/m.test(cask));
  assert.match(cask, /homepage "https:\/\/github\.com\/AntoineToussaint\/lazybox"/);
});

test('signing follows the secrets present, never a flag', () => {
  assert.deepEqual(signingPlan({}), { identity: '-', signed: false, notarize: undefined, notarizeWith: '' });
  const id = 'Developer ID Application: Someone (ABCDE12345)';
  assert.equal(signingPlan({ APPLE_SIGNING_IDENTITY: id }).signed, true);
  assert.equal(signingPlan({ APPLE_SIGNING_IDENTITY: id }).notarize, undefined);
  const viaId = signingPlan({ APPLE_SIGNING_IDENTITY: id, APPLE_ID: 'a@b.c', APPLE_APP_PASSWORD: 'pw', APPLE_TEAM_ID: 'ABCDE12345' });
  assert.equal(viaId.notarizeWith, 'apple-id');
  assert.equal(viaId.notarize.APPLE_PASSWORD, 'pw', 'APPLE_APP_PASSWORD is accepted for V1\'s APPLE_PASSWORD');
  assert.equal(signingPlan({ APPLE_SIGNING_IDENTITY: id, APPLE_API_KEY: 'K', APPLE_API_ISSUER: 'I', APPLE_API_KEY_PATH: '/k.p8' }).notarizeWith, 'api-key');
  // Notarization credentials without a certificate cannot notarize anything.
  assert.equal(signingPlan({ APPLE_ID: 'a@b.c', APPLE_PASSWORD: 'pw', APPLE_TEAM_ID: 'T' }).notarize, undefined);
});

test('the release configuration adds the version, runtime and public key, and drops comments', async () => {
  const base = JSON.parse(await readFile(resolve(root, 'packaging/release.tauri.json'), 'utf8'));
  const config = tauriReleaseConfig(base, { version: '0.2.0', runtimeDir: '/stage/runtime', identity: '-', pubkey: 'PUB' });
  assert.equal(config.$comment, undefined);
  assert.equal(config.version, '0.2.0');
  assert.equal(config.identifier, 'ai.lazybox.app');
  assert.equal(config.productName, 'Lazybox');
  assert.deepEqual(config.bundle.resources, { '/stage/runtime/': 'runtime/' });
  assert.equal(config.bundle.macOS.signingIdentity, '-');
  assert.equal(config.bundle.macOS.hardenedRuntime, true);
  assert.equal(config.bundle.macOS.minimumSystemVersion, '13.5', 'the bundled Node needs macOS 13.5');
  // The hardened runtime denies the microphone (voice dictation) without this entitlement.
  assert.equal(config.bundle.macOS.entitlements, 'Entitlements.plist');
  assert.match(await readFile(resolve(root, 'apps/desktop/src-tauri', config.bundle.macOS.entitlements), 'utf8'), /<key>com\.apple\.security\.device\.audio-input<\/key>\s*<true\/>/);
  assert.equal(config.bundle.createUpdaterArtifacts, true);
  assert.equal(config.plugins.updater.pubkey, 'PUB');
  assert.ok(config.plugins.updater.endpoints.every(u => u.startsWith('https://')));
  assert.equal(config.plugins.updater.requireSignedVersion, true, 'a feed must not pair a new version with an older signed archive');
  assert.equal(base.plugins.updater.pubkey, undefined, 'the committed overlay carries no key');
  // A digits-only CFBundleVersion, and the notices beside the runtime.
  assert.equal(config.bundle.macOS.bundleVersion, '0.2.999');
  const withNotices = tauriReleaseConfig(base, { version: '0.1.0-pre.1', runtimeDir: '/stage/runtime', notices: '/stage/THIRD_PARTY_NOTICES.txt', identity: '-', pubkey: 'PUB' });
  assert.deepEqual(withNotices.bundle.resources, { '/stage/runtime/': 'runtime/', '/stage/THIRD_PARTY_NOTICES.txt': NOTICES_FILE });
  assert.equal(withNotices.bundle.macOS.bundleVersion, '0.1.1');
});

test('CFBundleVersion is one to three integers that grow with every release', () => {
  const order = ['0.1.0-pre.1', '0.1.0-pre.2', '0.1.0-rc.3', '0.1.0', '0.1.1-pre.1', '0.1.1', '0.2.0-pre.1', '1.0.0'];
  const numbers = order.map(bundleVersion);
  assert.deepEqual(numbers.slice(0, 4), ['0.1.1', '0.1.2', '0.1.3', '0.1.999']);
  for (const n of numbers) assert.match(n, /^\d+(\.\d+){0,2}$/);
  const compare = (a, b) => { const x = a.split('.').map(Number), y = b.split('.').map(Number); return x[0] - y[0] || x[1] - y[1] || x[2] - y[2]; };
  for (let i = 1; i < numbers.length; i++) assert.ok(compare(numbers[i - 1], numbers[i]) < 0, `${order[i - 1]} → ${numbers[i - 1]} is not below ${order[i]} → ${numbers[i]}`);
  assert.equal(bundleVersion('0.0.0-test'), '0.0.0');
  assert.throws(() => bundleVersion('0.1.0-pre.999'), /998/);
  assert.throws(() => bundleVersion('latest'));
});

test('the worker\'s packages come from package.json and node_modules, whatever the checkout path looks like', () => {
  // npm ls prints a UUID-looking path segment as ***; this resolves on disk instead.
  const base = realpathSync(mkdtempSync(join(tmpdir(), 'lazybox-packages-')));
  const root = join(base, '3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c', 'v2');
  const write = (path, json) => { mkdirSync(join(root, path), { recursive: true }); writeFileSync(join(root, path, 'package.json'), JSON.stringify(json)); };
  write('.', { name: 'repo', workspaces: ['workers/*'] });
  write('workers/w', { name: '@x/w', dependencies: { a: '1', '@x/shared': '0' }, optionalDependencies: { 'only-linux': '1', 'only-darwin': '1' } });
  write('packages/shared', { name: '@x/shared', dependencies: { c: '1' } });
  mkdirSync(join(root, 'node_modules/@x'), { recursive: true });
  symlinkSync(join(root, 'packages/shared'), join(root, 'node_modules/@x/shared'));
  symlinkSync(join(root, 'workers/w'), join(root, 'node_modules/@x/w'));
  write('node_modules/a', { name: 'a', version: '1.0.0', license: 'MIT', dependencies: { b: '2' }, peerDependencies: { 'not-installed': '1' } });
  write('node_modules/a/node_modules/b', { name: 'b', version: '2.0.0', license: 'ISC' });
  write('node_modules/b', { name: 'b', version: '1.0.0', license: 'ISC' });
  write('node_modules/c', { name: 'c', version: '1.0.0', license: 'Apache-2.0' });
  write('node_modules/only-darwin', { name: 'only-darwin', version: '1.0.0' });
  write('node_modules/dev-only', { name: 'dev-only', version: '1.0.0' });
  const found = productionPackages(root, 'workers/w');
  assert.deepEqual(found.map(p => `${p.path}@${p.version}`), ['a@1.0.0', 'a/node_modules/b@2.0.0', 'c@1.0.0', 'only-darwin@1.0.0']);
  assert.equal(found.find(p => p.name === 'c').license, 'Apache-2.0');
  write('workers/w', { name: '@x/w', dependencies: { missing: '1' } });
  assert.throws(() => productionPackages(root, 'workers/w'), /missing, a dependency of @x\/w, is not installed/);
});

test('the notices list every component, print each licence text once, and flag what is not open source', () => {
  const dir = mkdtempSync(join(tmpdir(), 'lazybox-notices-'));
  const file = (name, text) => { writeFileSync(join(dir, name), text); return join(dir, name); };
  const mit = file('LICENSE', 'MIT License\n\nPermission is hereby granted, free of charge…');
  const ofl = file('OFL.txt', 'SIL OPEN FONT LICENSE Version 1.1');
  const closed = file('LICENSE.md', '© Someone. All rights reserved.');
  const text = renderNotices({
    version: '0.1.0-pre.1',
    components: [
      { kind: 'npm', name: '@fontsource-variable/inter', version: '5.3.0', license: 'OFL-1.1', files: [ofl], in: ['the desktop app'] },
      { kind: 'npm', name: 'left-pad', version: '1.0.0', license: 'MIT', files: [mit], in: ['the desktop app'] },
      { kind: 'npm', name: 'closed-binary', version: '2.0.0', license: 'SEE LICENSE IN LICENSE.md', files: [closed], in: ['the Claude worker'] },
      { kind: 'go', name: 'example.com/mod', version: 'v1.0.0', license: 'MIT', files: [mit] },
      { kind: 'rust', name: 'serde', version: '1.0.0', license: 'MIT OR Apache-2.0', files: [] },
    ],
    node: { kind: 'node', name: 'Node.js', version: 'v24.0.0', license: 'MIT', files: [mit] },
  });
  assert.match(text, /^Third-party notices for Lazybox 0\.1\.0-pre\.1/);
  for (const heading of ['Fonts', 'npm packages', 'Go modules', 'Rust crates', 'Node.js', 'Licence and notice texts']) assert.match(text, new RegExp(`^${heading}`, 'm'));
  assert.match(text, /Inter: @fontsource-variable\/inter 5\.3\.0 {2}— {2}OFL-1\.1 {2}\[[0-9a-f]{12}\]/);
  assert.equal(text.split('Permission is hereby granted').length - 1, 1, 'one MIT text, printed once');
  assert.match(text, /Covers: left-pad 1\.0\.0, example\.com\/mod v1\.0\.0, Node\.js v24\.0\.0/);
  assert.match(text, /SIL OPEN FONT LICENSE Version 1\.1/);
  assert.match(text, /not an open-source licence[^]*closed-binary 2\.0\.0[^]*\nFonts/);
  assert.match(text, /serde 1\.0\.0 {2}— {2}MIT OR Apache-2\.0 {2}\(no licence file in the package/);
  assert.ok(needsAttention('SEE LICENSE IN README.md') && needsAttention('') && needsAttention('proprietary (see its licence text)'));
  assert.ok(!needsAttention('MIT') && !needsAttention('Apache-2.0 OR MIT'));
});

test('the changelog is the one source of release notes', async () => {
  const releases = parseChangelog(await readFile(changelogPath, 'utf8'));
  assert.equal(await readFile(releaseNotesPath, 'utf8'), renderReleaseNotes(releases), 'run node scripts/release/changelog.mjs --write');
  assert.ok(releases.length > 0 && releases.every(r => r.items.length > 0));
});

test('a release body collects the dated notes since the previous version', () => {
  const releases = parseChangelog([
    '# Changelog', '', 'Intro.', '',
    '## [0.2.0] - 2026-12-01 · Second', '', '- **New thing** It works.', '',
    '## [2026-11-20.1] - 2026-11-20 · Between', '', '- **Dev only** First shipped to Dev,', '  then to everyone.', '',
    '## [0.1.0] - 2026-11-17 · First', '', '- **Old thing** Already released.',
  ].join('\n'));
  const body = releaseBody(releases, '0.2.0');
  assert.match(body, /## Second/);
  assert.match(body, /### Between/);
  assert.match(body, /- \*\*Dev only\.\*\* First shipped to Dev, then to everyone\./);
  assert.ok(!body.includes('Old thing'));
  assert.ok(!body.includes('Unsigned build'));
  assert.match(releaseBody(releases, '0.2.0', { unsigned: true }), /Unsigned build/);
  assert.throws(() => releaseBody(releases, '9.9.9'), /no section for 9\.9\.9/);
  assert.equal(updaterNotes(releases, '0.2.0'), 'Second\n• New thing: It works.');
  // The feed carries this version's section and the ones before it, so an app on any of them sees what changed since.
  assert.deepEqual(updaterReleases(releases, '0.2.0').map(r => r.id), ['0.2.0', '2026-11-20.1', '0.1.0']);
  assert.deepEqual(updaterReleases(releases, '2026-11-20.1').map(r => r.id), ['2026-11-20.1', '0.1.0']);
  assert.equal(updaterReleases(releases, '0.2.0', 1).length, 1);
  assert.deepEqual(updaterReleases(releases, '9.9.9'), []);
  assert.deepEqual(updaterReleases(releases, '2026-11-20.1')[0].items, [{ title: 'Dev only', body: 'First shipped to Dev, then to everyone.' }]);
  assert.throws(() => parseChangelog('## [x] - 2026-01-01 · T\n\nnot an item'), /only items/);
  assert.throws(() => parseChangelog('## 0.1.0\n'), /heading/);
});

test('a big section groups its items, and the notes show the groups as headings', () => {
  const releases = parseChangelog([
    '# Changelog', '',
    '## [0.2.0-pre.1] - 2026-12-01 · Big', '',
    '### Chat', '', '- **Send now** It sends.', '- **Boxes** Attachments.', '',
    '### Review', '', '- **Review** From the chat.', '',
    '## [2026-11-20.1] - 2026-11-20 · Between', '', '- **Plain** No groups.',
  ].join('\n'));
  assert.deepEqual(releases[0].items.map(i => i.group), ['Chat', 'Chat', 'Review']);
  assert.equal(releases[1].items[0].group, undefined);
  const body = releaseBody(releases, '0.2.0-pre.1');
  assert.match(body, /## Big\n\n### Chat\n\n- \*\*Send now\.\*\* It sends\.\n- \*\*Boxes\.\*\* Attachments\.\n\n### Review\n/);
  assert.match(body, /\n### Between\n\n- \*\*Plain\.\*\* No groups\.\n/);
  assert.equal(updaterNotes(releases, '0.2.0-pre.1'), 'Big\nChat\n• Send now: It sends.\n• Boxes: Attachments.\nReview\n• Review: From the chat.');
  assert.deepEqual(updaterReleases(releases, '0.2.0-pre.1')[0].items[2], { group: 'Review', title: 'Review', body: 'From the chat.' });
  assert.match(renderReleaseNotes(releases), /\{ group: 'Chat', title: 'Send now', body: 'It sends\.' \}/);
  assert.throws(() => parseChangelog('## [x] - 2026-01-01 · T\n\n- **A** a\n\n### G\n\n- **B** b'), /belong in a group/);
  assert.throws(() => parseChangelog('## [x] - 2026-01-01 · T\n\n### G\n\n- **A** a\n\n### G\n\n- **B** b'), /appears twice/);
});

test('the release notes of the current version are the whole of its section', async () => {
  const releases = parseChangelog(await readFile(changelogPath, 'utf8'));
  const [newest] = releases;
  const body = releaseBody(releases, newest.id);
  for (const item of newest.items) assert.ok(body.includes(`**${item.title}.** ${item.body}`), `${item.title} is missing from the notes`);
});

test('the release notes link to the install guide where the repository has it', async () => {
  // From the repository root: docs/install.md in the monorepo; the export
  // drops the v2/ prefix along with the folder.
  // The link names the guide by its path from the repository root: docs/install.md in the
  // monorepo, docs/install.md in the standalone repository (and in a staged copy of this folder).
  assert.match(installGuidePath, /^(v2\/)?docs\/install\.md$/);
  assert.ok(existsSync(resolve(root, 'docs/install.md')), 'docs/install.md is not in this repository');
  assert.match(await readFile(resolve(root, 'docs/install.md'), 'utf8'), /^### Unsigned builds$/m, 'the #unsigned-builds anchor the notes link to');
  const notes = releaseBody(parseChangelog('## [1.0.0] - 2026-12-01 · T\n\n- **A** a'), '1.0.0', { unsigned: true });
  assert.ok(notes.includes(`(${installGuide}#unsigned-builds)`));
  assert.match(notes, /Privacy & Security → Open Anyway/);
  // The cask is published for releases without a prerelease suffix only: the notes do not promise it.
  assert.ok(!notes.includes('brew install'));
});

test('dated notes merged after a version\'s section are collected by the next version, or must be folded in', async () => {
  const dated = [
    '## [2026-11-18.2] - 2026-11-18 · Later', '', '- **Later** After the section.', '',
    '## [2026-11-18.1] - 2026-11-18 · Also later', '', '- **Also** After it too.', '',
  ];
  const first = ['## [0.1.0-pre.1] - 2026-11-17 · First', '', '### Chat', '', '- **Send now** It sends.', ''];
  // Releasing 0.1.0-pre.1 now would leave them out: the build refuses until they are folded in.
  const releases = parseChangelog([...dated, ...first].join('\n'));
  assert.deepEqual(unfoldedAbove(releases, '0.1.0-pre.1'), ['2026-11-18.2', '2026-11-18.1']);
  assert.ok(!releaseBody(releases, '0.1.0-pre.1').includes('After the section'));
  // A later version's section on top collects them, and leaves pre.1's own items out.
  const next = parseChangelog(['## [0.1.0-pre.2] - 2026-11-19 · Second', '', '- **New** Thing.', '', ...dated, ...first].join('\n'));
  assert.deepEqual(unfoldedAbove(next, '0.1.0-pre.2'), []);
  const body = releaseBody(next, '0.1.0-pre.2');
  assert.ok(body.includes('After the section') && body.includes('After it too') && !body.includes('It sends'));
  // The changelog's newest version is releasable as it stands.
  const current = parseChangelog(await readFile(changelogPath, 'utf8'));
  assert.deepEqual(unfoldedAbove(current, current.find(r => versionPattern.test(r.id)).id), []);
});

// REMOTE_RUNNERS.md §5.2: the Linux Runner ships for x64 and arm64, each
// tarball checksummed, with a pinned, checksummed Node for its platform.
test('the Linux Runner ships for x64 and arm64 with a pinned Node each', async () => {
  assert.deepEqual(LINUX_ARCHES.map(a => a.name), ['x64', 'arm64']);
  assert.deepEqual(linuxArtifactNames('0.1.0-pre.1', 'arm64'), {
    runner: 'lazybox-runner-0.1.0-pre.1-linux-arm64.tar.gz', runnerDir: 'lazybox-runner-0.1.0-pre.1-linux-arm64',
    cli: 'lazybox-cli-0.1.0-pre.1-linux-arm64.tar.gz', cliDir: 'lazybox-cli-0.1.0-pre.1-linux-arm64',
  });
  assert.throws(() => linuxArtifactNames('0.1.0', 'riscv64'));
  assert.throws(() => linuxArtifactNames('../x', 'x64'));
  const pin = JSON.parse(await readFile(join(root, 'scripts/release/node.json'), 'utf8'));
  for (const arch of LINUX_ARCHES) {
    const archive = pin.archives[arch.node];
    assert.ok(archive, `node.json pins no Node for ${arch.node}`);
    assert.equal(archive.file, `node-${pin.version}-${arch.node}.tar.gz`);
    assert.match(archive.sha256, /^[0-9a-f]{64}$/);
  }
  const sum = 'a'.repeat(64);
  assert.equal(sumsText([['bin/lazybox', sum], ['VERSION', sum]]), `${sum}  bin/lazybox\n${sum}  VERSION\n`);
  assert.throws(() => sumsText([['x', 'not-a-sum']]));
  assert.equal(parseArgs(['--version', '1.0.0', '--linux-only'], {}).linuxOnly, true);
  assert.throws(() => parseArgs(['--from-source', '--linux-only'], {}));
});
