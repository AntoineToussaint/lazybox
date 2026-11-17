import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { isPrivateAddress, judgeNavigation, judgeURL, originOf } from './policy.js';
import { GateProxy, splitAuthority, type Refusal } from './proxy.js';
import { capSnapshot, refLabels } from './snapshot.js';

const policy = { loopbackPorts: [5173], blockedHosts: ['127.0.0.1:7000', 'runner.tailnet.ts.net:443'] };

test('the web is allowed; local and private addresses are not, except the workspace previews', () => {
  assert.equal(judgeURL(policy, 'https://example.com/docs').allow, true);
  assert.equal(judgeURL(policy, 'http://127.0.0.1:5173/').allow, true);
  assert.equal(judgeURL(policy, 'http://localhost:5173/app').allow, true);
  assert.equal(judgeURL(policy, 'data:text/plain,hi').allow, true);
  const refused = (url: string, why: RegExp) => { const v = judgeURL(policy, url); assert.equal(v.allow, false, url); assert.match((v as { reason: string }).reason, why, url); };
  refused('http://127.0.0.1:8080/', /port 8080 is not one of the workspace's previews/);
  refused('http://app.localhost:8080/', /port 8080/);
  refused('http://[::1]:3000/', /port 3000/);
  refused('http://127.0.0.1:7000/', /belongs to Lazybox/);
  refused('https://runner.tailnet.ts.net/', /belongs to Lazybox/);
  refused('http://10.1.2.3/', /private or local network/);
  refused('http://169.254.169.254/latest/meta-data', /private or local network/);
  refused('http://100.100.1.1/', /private or local network/);
  refused('http://[fd00::1]/', /private or local network/);
  // A URL rewrites an embedded IPv4 address in hex: ::ffff:c0a8:1.
  refused('http://[::ffff:192.168.0.1]/', /private or local network/);
  refused('http://[::ffff:127.0.0.1]:8080/', /port 8080/);
  refused('http://[64:ff9b::10.0.0.1]/', /private or local network/);
  assert.equal(judgeURL(policy, 'http://[::ffff:127.0.0.1]:5173/').allow, true);
  refused('file:///etc/passwd', /do not open file:/);
  refused('chrome://settings', /do not open chrome:/);
  refused('https://user:secret@example.com/', /user name or password/);
});

test('navigation is http and https only, and an origin is scheme, host and port', () => {
  assert.match((judgeNavigation(policy, 'ws://example.com') as { reason: string }).reason, /only http and https/);
  assert.match((judgeNavigation(policy, 'javascript:alert(1)') as { reason: string }).reason, /only http and https/);
  assert.equal(originOf('https://Example.com:443/a?b'), 'https://example.com');
  assert.equal(originOf('http://127.0.0.1:5173/x'), 'http://127.0.0.1:5173');
  assert.equal(originOf('about:blank'), '');
});

test('private address ranges', () => {
  for (const a of ['0.0.0.0', '10.0.0.1', '172.16.0.1', '172.31.255.255', '192.168.1.1', '169.254.1.1', '100.64.0.1', '198.18.0.1', '224.0.0.1', '::', 'fe80::1', 'fc00::1', 'ff02::1']) assert.equal(isPrivateAddress(a), true, a);
  for (const a of ['8.8.8.8', '172.32.0.1', '100.128.0.1', '2606:4700::1111']) assert.equal(isPrivateAddress(a), false, a);
});

test('the gate proxy vets names by what they resolve to, and reports refusals', async () => {
  const refusals: Refusal[] = [];
  const proxy = new GateProxy(() => policy, refusal => refusals.push(refusal));
  assert.deepEqual(await proxy.vet('localhost', 5173), { ok: true, address: '127.0.0.1' });
  assert.equal((await proxy.vet('localhost', 22)).ok, false);
  assert.equal((await proxy.vet('127.0.0.1', 7000)).ok, false);
  assert.equal((await proxy.vet('no-such-host.invalid', 80)).ok, false);
  assert.deepEqual(refusals.map(r => `${r.host}:${r.port}`), ['localhost:22', '127.0.0.1:7000', 'no-such-host.invalid:80']);
  assert.deepEqual(splitAuthority('[::1]:8080'), { host: '::1', port: 8080 });
  assert.deepEqual(splitAuthority('example.com:443'), { host: 'example.com', port: 443 });
  assert.deepEqual(splitAuthority('example.com'), { host: '', port: 0 });
});

test('snapshots are capped at a line, and refs keep their role and name', () => {
  const snapshot = '- heading "Docs" [level=1] [ref=e1]\n- button "Sign in" [ref=e2] [cursor=pointer]\n- generic [active] [ref=e3]:\n  - textbox "Email" [ref=e4]';
  assert.deepEqual([...refLabels(snapshot)], [['e1', 'heading "Docs"'], ['e2', 'button "Sign in"'], ['e3', 'generic'], ['e4', 'textbox "Email"']]);
  const long = Array.from({ length: 400 }, (_, i) => `- listitem "Item ${i}" [ref=e${i}]`).join('\n');
  const capped = capSnapshot(long, 1000);
  assert.ok(capped.length < 1200);
  assert.match(capped, /\[ref=e\d+\]\n\(truncated — \d+ more characters; scroll or use browser_snapshot with a larger maxChars\)$/);
  assert.equal(capSnapshot(snapshot), snapshot);
});
