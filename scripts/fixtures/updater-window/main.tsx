// The desktop's main window without the native shell, for the Updates and
// feedback tests: the Workspace with an app host whose updater reads a fake
// feed (served by the test at /fixture-feed/latest.json) and whose "native"
// side records what it was asked to do (/fixture-feed/*, /fixture-feedback).
// Everything the window decides (offer, skip, remind, warn, redact) is the
// real code; only the Tauri updater, the file system and the screenshot are
// stand-ins. The Runner is the fake Runner, reached through the test's proxy.
import React from 'react';
import { createRoot } from 'react-dom/client';
import { Workspace } from '@lazybox/surfaces';
import type { AppHost, UpdateErrorKind, WorkspaceHost } from '@lazybox/platform';
import { isNewer } from '../../../packages/surfaces/src/update-model';
// Fake credentials for the redaction tests, assembled so no source line looks like a real one (export-repo secret scan).
const FAKE_GITHUB_TOKEN = ['ghp', '0123456789abcdefghijABCDEFGHIJ012345'].join('_');

const params = new URLSearchParams(location.search);
const version = params.get('version') || '0.1.0';
const token = params.get('token') || '';
const viewState = { key: 'fixture:main', scope: 'fixture', root: 'fixture', getItem: (key: string) => localStorage.getItem(key), setItem: (key: string, value: string) => localStorage.setItem(key, value) };
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
const fail = (kind: UpdateErrorKind, message: string) => Object.assign(new Error(message), { kind });
type Feed = { version: string; notes: string; pub_date: string; platforms: Record<string, { signature: string }>; lazybox?: { releases: [] } };
let pending: Feed | undefined;
declare global { interface Window { fixtureOpened: string[]; fixtureRevealed: string[] } }
window.fixtureOpened = []; window.fixtureRevealed = [];

// ?source=1: a build from source (make install), which has no updater: the
// native side refuses a check without asking the network, as updates.rs does.
const source = params.get('source') === '1';

const app: AppHost = {
  info: async () => source
    ? { version, builtAtUnixSeconds: 1794909600, channel: 'release', updater: false, cli: '/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox', source: { commit: '0123456789abcdef0123456789abcdef01234567', dirty: false, checkout: '/Users/tester/lazybox' } }
    : { version, channel: 'release', updater: true, cli: '/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox' },
  async checkForUpdate() {
    if (source) throw fail('unavailable', 'This build of Lazybox does not update itself');
    let response: Response;
    try { response = await fetch('/fixture-feed/latest.json', { cache: 'no-store' }); } catch (error) { throw fail('offline', String(error)); }
    // The test answers 599 for "no network": what the updater reports as a connection error.
    if (response.status === 599) throw fail('offline', 'error sending request for url');
    if (!response.ok) throw fail('feed', 'Could not fetch a valid release JSON from the remote');
    const feed = await response.json() as Feed;
    if (!isNewer(feed.version, version)) { pending = undefined; return undefined; }
    pending = feed;
    return { version: feed.version, current: version, notes: feed.notes, date: feed.pub_date, releases: feed.lazybox?.releases };
  },
  async installUpdate(onProgress) {
    if (!pending) throw fail('install', 'No update to install; check for updates first');
    const total = 3_000_000;
    for (let downloaded = 0; downloaded <= total; downloaded += 750_000) { onProgress?.({ phase: 'download', downloaded, total }); await sleep(150); }
    onProgress?.({ phase: 'verify', downloaded: 0, total: 0 }); await sleep(200);
    // The real updater verifies the minisign signature against the app's key; the feed's says whether it would pass.
    if (pending.platforms['darwin-aarch64']?.signature !== 'valid') throw fail('signature', 'signature verification failed');
    onProgress?.({ phase: 'install', downloaded: 0, total: 0 }); await sleep(200);
    onProgress?.({ phase: 'runner', downloaded: 0, total: 0 }); await fetch('/fixture-feed/installed', { method: 'POST', body: pending.version }); await sleep(200);
    onProgress?.({ phase: 'restart', downloaded: 0, total: 0 }); await sleep(200);
    const next = new URL(location.href); next.searchParams.set('version', pending.version); location.replace(next);
    await new Promise(() => {});
  },
  linkCli: async () => ({ link: '/usr/local/bin/lazybox', hints: ['Open a new terminal to pick up the change.'] }),
  moveRunner: async () => { const response = await fetch('/fixture-feed/move-runner', { method: 'POST', body: version }); if (!response.ok) throw new Error(await response.text()); },
  feedbackDiagnostics: async () => ({
    os: '15.1', arch: 'aarch64', home: '/Users/tester',
    runnerLog: { path: '/Users/tester/Library/Logs/Lazybox/runner.log', tail: [
      '2026/12/02 09:00:01 runner: serving on 127.0.0.1:43210',
      '2026/12/02 09:00:02 runner: state /Users/tester/Library/Application Support/Lazybox/state',
      '2026/12/02 09:00:03 gateway: upstream Authorization: Bearer sk-ant-api03-SECRETSECRETSECRETSECRET',
      `2026/12/02 09:00:04 github: token ${FAKE_GITHUB_TOKEN} refreshed for tester@example.com`,
      '2026/12/02 09:00:05 agent: turn ended input_tokens=1200',
    ].join('\n') },
  }),
  async saveFeedback({ markdown, screenshot }) {
    await fetch('/fixture-feedback', { method: 'POST', body: JSON.stringify({ markdown, screenshot: screenshot?.length || 0 }) });
    return '/Users/tester/Downloads/Lazybox feedback 2026-12-02 09.00.00';
  },
  revealPath: async path => { window.fixtureRevealed.push(path); },
};

// A small PNG drawn on a canvas stands in for the window's own snapshot (capture.rs).
async function capture(): Promise<Uint8Array> {
  const canvas = document.createElement('canvas'); canvas.width = 320; canvas.height = 200;
  const g = canvas.getContext('2d')!;
  g.fillStyle = getComputedStyle(document.body).backgroundColor || '#111'; g.fillRect(0, 0, 320, 200);
  g.fillStyle = '#6aa2ff'; g.fillRect(0, 0, 320, 28); g.fillStyle = '#888'; for (let y = 44; y < 190; y += 18) g.fillRect(16, y, 120 + (y % 90), 8);
  const blob = await new Promise<Blob>(resolve => canvas.toBlob(b => resolve(b!), 'image/png'));
  return new Uint8Array(await blob.arrayBuffer());
}

const host: WorkspaceHost = {
  app,
  openLink: async url => { window.fixtureOpened.push(url); },
  captureWindow: capture,
  focusWindow: async () => {},
  openChat: async () => {},
  dockChat: async () => {},
};

createRoot(document.getElementById('root')!).render(<React.StrictMode>
  <Workspace fixedWorkspaceId={params.get('fixedWorkspace') || undefined} connection={{ baseUrl: location.origin, token }} viewState={viewState} host={host} initialTarget={{ collectionId: params.get('collection') || '', sessionId: params.get('session') || '', commandId: '' }}/>
</React.StrictMode>);
