import { strict as assert } from 'node:assert';
import { after, before, describe, it } from 'node:test';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { BrowserService, type Runner } from './service.js';
import { refIn, startSites, type Sites } from './fixture.js';
import type { ActParams, Policy } from './protocol.js';

/** The Runner's side of the protocol, answering as each test sets it to. */
class FakeRunner implements Runner {
  asks: { method: string; params: any }[] = [];
  events: { method: string; params: any }[] = [];
  answers: Record<string, (params: any) => unknown> = { site: () => ({ allow: true }), download: () => ({ allow: false }), sign_in: () => ({ allow: false }) };
  request(method: string, params: unknown) { this.asks.push({ method, params }); return Promise.resolve(this.answers[method]?.(params)); }
  emit(method: string, params: unknown) { this.events.push({ method, params }); }
  asked(method: string) { return this.asks.filter(a => a.method === method).map(a => a.params); }
  logged(kind: string) { return this.events.filter(e => e.method === 'log' && e.params.kind === kind).map(e => e.params.summary as string); }
  async until(check: () => boolean, ms = 5000) {
    const end = Date.now() + ms;
    while (!check()) { if (Date.now() > end) throw new Error('timed out waiting'); await new Promise(r => setTimeout(r, 25)); }
  }
}

describe('browser service', () => {
  let sites: Sites, runner: FakeRunner, service: BrowserService, profile: string, downloads: string, policy: Policy;
  const act = (p: Omit<ActParams, 'browser' | 'actor'> & { actor?: 'agent' | 'person' }) => service.act({ browser: 'c1', actor: 'agent', ...p });
  const snapshot = async () => (await act({ action: 'snapshot' })).text;

  before(async () => {
    sites = await startSites();
    profile = await mkdtemp(join(tmpdir(), 'lazybox-browser-profile-'));
    downloads = await mkdtemp(join(tmpdir(), 'lazybox-browser-downloads-'));
    runner = new FakeRunner();
    service = new BrowserService(runner);
    policy = { loopbackPorts: [sites.ports.home, sites.ports.other], blockedHosts: [] };
    const state = await service.open({ browser: 'c1', profile, policy });
    assert.equal(state.url, 'about:blank');
    assert.equal(state.viewportWidth, 1280);
  });
  after(async () => {
    await service.shutdown();
    await sites.close();
    await rm(profile, { recursive: true, force: true });
    await rm(downloads, { recursive: true, force: true });
  });

  it('navigates after asking for the new site, then reads the page as a snapshot with refs', async () => {
    const result = await act({ action: 'navigate', url: `${sites.home}/`, withSnapshot: true });
    assert.match(result.text, /^Opened "Home" — http:\/\/127\.0\.0\.1:\d+\/\./);
    assert.match(result.text, /link "Form" \[ref=e\d+\]/);
    assert.equal(result.summary, `Opened 127.0.0.1:${sites.ports.home}`);
    assert.deepEqual(runner.asked('site').map(p => p.origin), [sites.home]);
    const text = await snapshot();
    assert.match(text, /^Page: "Home" — /);
    assert.match(text, /heading "Fixture home"/);
  });

  it('clicks by ref, types, selects and submits on the same site without asking again', async () => {
    const asked = runner.asked('site').length;
    const click = await act({ action: 'click', ref: refIn(await snapshot(), 'link "Form"'), withSnapshot: true });
    assert.match(click.text, /^Clicked link "Form"\. Now on "Form"/);
    assert.ok(click.box && click.box.width > 0, 'the click says where it happened');
    const page = click.text;
    const typed = await act({ action: 'type', ref: refIn(page, 'textbox "Name"'), text: 'Ada' });
    assert.equal(typed.summary, 'Typed "Ada" into textbox "Name"');
    const selected = await act({ action: 'select', ref: refIn(page, 'combobox "Color"'), values: ['blue'] });
    assert.equal(selected.summary, 'Selected "blue" in combobox "Color"');
    const sent = await act({ action: 'click', ref: refIn(page, 'button "Send"') });
    assert.match(sent.text, /Now on "Done"/);
    assert.equal(new URL(sent.state.url).search, '?name=Ada&color=blue');
    const waited = await act({ action: 'wait_for', text: 'Thanks Ada, blue' });
    assert.equal(waited.summary, 'Waited for "Thanks Ada, blue"');
    assert.equal(runner.asked('site').length, asked);
  });

  it('goes back and forward, and says when there is nowhere to go', async () => {
    const back = await act({ action: 'back' });
    assert.match(back.text, /^Went back: "Form"/);
    assert.equal(back.state.canGoForward, true);
    const forward = await act({ action: 'forward' });
    assert.match(forward.text, /^Went forward: "Done"/);
    assert.equal((await act({ action: 'forward' })).text, 'There is no next page.');
  });

  it('refuses a ref that is not in the latest snapshot', async () => {
    await assert.rejects(act({ action: 'click', ref: 'e999' }), /Take a fresh browser_snapshot/);
    await assert.rejects(act({ action: 'click' }), /needs a ref/);
  });

  it('takes JPEG screenshots of the viewport and of the page', async () => {
    const shot = await act({ action: 'screenshot' });
    assert.equal(shot.image?.mediaType, 'image/jpeg');
    assert.ok(Buffer.from(shot.image!.data, 'base64').subarray(0, 2).equals(Buffer.from([0xff, 0xd8])), 'a JPEG');
    assert.match(shot.text, /1280×800/);
    await act({ action: 'navigate', url: `${sites.home}/long` });
    const full = await act({ action: 'screenshot', fullPage: true });
    assert.match(full.text, /1280×3\d{3}/);
    const scrolled = await act({ action: 'scroll', direction: 'down' });
    assert.match(scrolled.text, /^Scrolled down\. Showing 640–1440 of 3\d{3} px\.$/);
  });

  it('waits for text and times out clearly', async () => {
    await act({ action: 'navigate', url: `${sites.home}/slow` });
    assert.equal((await act({ action: 'wait_for', text: 'Ready now' })).text, '"Ready now" is on the page.');
    await assert.rejects(act({ action: 'wait_for', textGone: 'Ready now', timeoutMs: 300 }), /Timed out after 0\.3s waiting for "Ready now" to go/);
  });

  it('asks before a new origin, and leaves the page where it is when the person says no', async () => {
    await act({ action: 'navigate', url: `${sites.home}/` });
    runner.answers.site = () => ({ allow: false, reason: 'not now' });
    const ref = refIn(await snapshot(), 'link "Other site"');
    const started = Date.now();
    const click = await act({ action: 'click', ref });
    assert.ok(Date.now() - started < 2500, `a declined site answers at once, took ${Date.now() - started}ms`);
    assert.match(click.text, new RegExp(`The person did not allow ${sites.other}: not now\\.`));
    assert.equal(new URL(click.state.url).origin, sites.home);
    await assert.rejects(act({ action: 'navigate', url: `${sites.other}/other` }), new RegExp(`The person did not allow ${sites.other}: not now`));
    runner.answers.site = () => ({ allow: true });
    const allowed = await act({ action: 'navigate', url: `${sites.other}/other` });
    assert.match(allowed.text, /Opened "Other site"/);
    assert.deepEqual(runner.asked('site').slice(-3).map(p => p.origin), [sites.other, sites.other, sites.other]);
  });

  it("never asks about the person's own navigation", async () => {
    runner.answers.site = () => ({ allow: false });
    const asked = runner.asked('site').length;
    const result = await act({ action: 'navigate', url: `${sites.home}/form`, actor: 'person' });
    assert.match(result.text, /Opened "Form"/);
    assert.equal(runner.asked('site').length, asked);
    runner.answers.site = () => ({ allow: true });
  });

  it('refuses file:, other loopback ports, Lazybox hosts and redirects or fetches to them', async () => {
    await assert.rejects(act({ action: 'navigate', url: 'file:///etc/passwd' }), /only http and https/);
    await assert.rejects(act({ action: 'navigate', url: `${sites.forbidden}/` }), /not one of the workspace's previews/);
    await assert.rejects(act({ action: 'navigate', url: `http://localhost:${sites.ports.forbidden}/` }), /not one of the workspace's previews/);
    await assert.rejects(act({ action: 'navigate', url: 'http://192.168.1.1/' }), /private or local network/);
    // A redirect is not seen by request interception: the gate proxy refuses it after the fact.
    await assert.rejects(act({ action: 'navigate', url: `${sites.home}/redirect-forbidden` }), /Lazybox blocked/);
    await act({ action: 'navigate', url: `${sites.home}/fetch-forbidden` });
    await act({ action: 'wait_for', text: 'refused' });
    assert.ok(runner.logged('blocked').some(s => s.includes(`127.0.0.1:${sites.ports.forbidden}`)), 'the refused fetch is logged');
    // The Runner's own address is never reachable, even on an allowed port.
    await service.open({ browser: 'c1', profile, policy: { ...policy, blockedHosts: [`127.0.0.1:${sites.ports.other}`] } });
    await assert.rejects(act({ action: 'navigate', url: `${sites.other}/other` }), /belongs to Lazybox/);
    await service.open({ browser: 'c1', profile, policy });
  });

  it('refuses a download the person declines, and saves one they allow', async () => {
    await act({ action: 'navigate', url: `${sites.home}/download` });
    const link = refIn(await snapshot(), 'link "Get the report"');
    await act({ action: 'click', ref: link });
    await runner.until(() => runner.logged('download').length === 1);
    assert.deepEqual(runner.logged('download'), ['Refused the download "report.txt"']);
    assert.equal(runner.asked('download')[0].fileName, 'report.txt');
    const path = join(downloads, 'report.txt');
    runner.answers.download = () => ({ allow: true, path });
    await act({ action: 'click', ref: link });
    await runner.until(() => runner.logged('download').length === 2);
    assert.equal(await readFile(path, 'utf8'), 'report body\n');
    runner.answers.download = () => ({ allow: false });
  });

  it('never types a password, and asks before submitting a sign-in form the person filled', async () => {
    await act({ action: 'navigate', url: `${sites.home}/signin` });
    let page = await snapshot();
    await assert.rejects(act({ action: 'type', ref: refIn(page, 'textbox "Password"'), text: 'hunter2' }), /password field.*Take over/);
    await act({ action: 'type', ref: refIn(page, 'textbox "Email"'), text: 'ada@example.com' });
    // The person types the password during Take over.
    const field = await act({ action: 'click', ref: refIn(page, 'textbox "Password"'), actor: 'person' });
    assert.ok(field.box);
    await service.input({ browser: 'c1', type: 'text', text: 'hunter2' });
    page = await snapshot();
    await assert.rejects(act({ action: 'click', ref: refIn(page, 'button "Sign in"') }), /did not allow submitting the sign-in form/);
    assert.equal(runner.asked('sign_in').length, 1);
    runner.answers.sign_in = () => ({ allow: true });
    const signedIn = await act({ action: 'click', ref: refIn(page, 'button "Sign in"') });
    assert.match(signedIn.text, /Now on "Welcome"/);
    runner.answers.sign_in = () => ({ allow: false });
  });

  // Regression: under load a submitted form's request left after settle()'s 150 ms look, and the
  // agent was told only "Clicked …" while the page went on to Welcome.
  it('waits for a navigation the click requested, however slow its page is to leave', async () => {
    await act({ action: 'navigate', url: `${sites.home}/signin-slow` });
    const submitted = await act({ action: 'click', ref: refIn(await snapshot(), 'button "Sign in"') });
    assert.match(submitted.text, /Now on "Welcome"/);
  });

  // A requested navigation that never changes this tab's address returns promptly, not after a timeout:
  // a download, a form back to the same address, a link answered with no content.
  it('returns promptly from a click whose navigation keeps the address', async () => {
    const timed = async (path: string, button: string) => {
      await act({ action: 'navigate', url: `${sites.home}${path}` });
      const ref = refIn(await snapshot(), button), started = Date.now();
      await act({ action: 'click', ref });
      return Date.now() - started;
    };
    // A link into a new tab is timed where popups are tested: it is folded into this page.
    for (const [path, button] of [['/download', 'link "Get the report"'], ['/same', 'button "Search again"'], ['/stay', 'link "Check again"']]) {
      const took = await timed(path, button);
      assert.ok(took < 2500, `${path}: the click took ${took} ms`);
    }
  });

  it('folds a popup into the page and dismisses dialogs', async () => {
    await act({ action: 'navigate', url: `${sites.home}/popup` });
    const link = refIn(await snapshot(), 'link "Open in a new window"'), started = Date.now();
    await act({ action: 'click', ref: link });
    // A link into a new tab is no navigation of this one: the click returns promptly.
    assert.ok(Date.now() - started < 2500, `the click took ${Date.now() - started} ms`);
    await runner.until(() => runner.logged('popup').length === 1);
    await act({ action: 'wait_for', text: 'Name' });
    assert.match(runner.logged('popup')[0], /^Opened 127\.0\.0\.1:\d+\/form here instead of a new window$/);
    await act({ action: 'navigate', url: `${sites.home}/alert` });
    await runner.until(() => runner.logged('blocked').some(s => s === 'Dismissed the page\'s alert: "hello"'));
  });

  it('streams screencast frames while on, and takes the person\'s mouse and keys', async () => {
    await act({ action: 'navigate', url: `${sites.home}/form` });
    await service.screencast({ browser: 'c1', on: true });
    await runner.until(() => runner.events.some(e => e.method === 'frame'));
    const frame = runner.events.find(e => e.method === 'frame')!.params;
    assert.equal(frame.browser, 'c1');
    assert.equal(frame.width, 1280);
    assert.ok(Buffer.from(frame.data, 'base64').subarray(0, 2).equals(Buffer.from([0xff, 0xd8])));
    await service.screencast({ browser: 'c1', on: false });
    const box = (await act({ action: 'scroll', ref: refIn(await snapshot(), 'textbox "Name"') })).box!;
    const x = box.x + box.width / 2, y = box.y + box.height / 2;
    await service.input({ browser: 'c1', type: 'mouse_down', x, y, button: 'left' });
    await service.input({ browser: 'c1', type: 'mouse_up', x, y, button: 'left' });
    for (const key of ['G', 'o']) { await service.input({ browser: 'c1', type: 'key_down', key }); await service.input({ browser: 'c1', type: 'key_up', key }); }
    await service.input({ browser: 'c1', type: 'key_down', key: 'Enter' });
    await act({ action: 'wait_for', text: 'Thanks Go, red', actor: 'person' });
  });

  it('reports state changes and closes', async () => {
    assert.ok(runner.events.some(e => e.method === 'state' && e.params.browser === 'c1' && e.params.state.title), 'state events carry the page');
    await service.close({ browser: 'c1' });
    await assert.rejects(act({ action: 'snapshot' }), /not open/);
    assert.ok(!runner.events.some(e => e.method === 'closed'), 'a close the Runner asked for is not reported back');
  });
});
