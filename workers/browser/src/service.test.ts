import { strict as assert } from 'node:assert';
import { after, before, describe, it } from 'node:test';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { chromium } from 'playwright-core';
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

/** A row of a JPEG's pixels, [r, g, b] each, decoded by Chromium itself. */
async function pixelRow(jpeg: string, x: number, y: number, width: number): Promise<number[][]> {
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    return await page.evaluate(async ({ jpeg, x, y, width }) => {
      const image = new Image();
      image.src = 'data:image/jpeg;base64,' + jpeg;
      await image.decode();
      const canvas = document.createElement('canvas');
      canvas.width = image.width; canvas.height = image.height;
      const context = canvas.getContext('2d')!;
      context.drawImage(image, 0, 0);
      const data = context.getImageData(x, y, width, 1).data, row: number[][] = [];
      for (let i = 0; i < data.length; i += 4) row.push([data[i], data[i + 1], data[i + 2]]);
      return row;
    }, { jpeg, x: Math.round(x), y: Math.round(y), width });
  } finally { await browser.close(); }
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

  // Regression: request interception does not see redirect hops, and the site was asked about only once the
  // redirect had landed: the other site had the request, and what it carried, before the person answered.
  it('asks before following a redirect to another site, which gets nothing if the person declines', async () => {
    await act({ action: 'navigate', url: `${sites.home}/` });
    const before = sites.otherRequests.length, pages = () => sites.otherRequests.slice(before).filter(path => path !== '/favicon.ico');
    let reachedWhenAsked: string[] | undefined;
    runner.answers.site = () => { reachedWhenAsked = pages(); return { allow: false, reason: 'not there' }; };
    await assert.rejects(act({ action: 'navigate', url: `${sites.home}/redirect-other` }), new RegExp(`The person did not allow ${sites.other}: not there`));
    assert.deepEqual(reachedWhenAsked, [], 'the other site was reached before the person answered');
    assert.deepEqual(pages(), []);
    assert.equal(runner.asked('site').at(-1).url, `${sites.other}/other?d=SECRET`);
    assert.equal(new URL((await service.open({ browser: 'c1', profile, policy })).url).origin, sites.home);
    runner.answers.site = () => { reachedWhenAsked = pages(); return { allow: true }; };
    const allowed = await act({ action: 'navigate', url: `${sites.home}/redirect-other` });
    assert.match(allowed.text, /Opened "Other site"/);
    assert.deepEqual(reachedWhenAsked, []);
    assert.deepEqual(pages(), ['/other?d=SECRET']);
  });

  // Regression: the person's browsing made every site the page reached the agent's, a page that moved itself
  // included: the agent then went on there without asking.
  it("takes as the agent's only the sites the person went to themselves", async () => {
    await act({ action: 'navigate', url: `${sites.home}/` });
    await act({ action: 'navigate', url: `${sites.home}/moves-itself`, actor: 'person' });
    await act({ action: 'wait_for', text: 'Other origin page', actor: 'person' });
    assert.equal((await service.open({ browser: 'c1', profile, policy })).origin, sites.home);
    const asked = runner.asked('site').length;
    runner.answers.site = () => ({ allow: false });
    await assert.rejects(act({ action: 'navigate', url: `${sites.other}/other?again` }), /did not allow/);
    assert.equal(runner.asked('site').length, asked + 1);
    runner.answers.site = () => ({ allow: true });
    // A link the person clicks is their choice.
    await act({ action: 'navigate', url: `${sites.home}/link-other`, actor: 'person' });
    await service.input({ browser: 'c1', type: 'mouse_down', x: 100, y: 50, button: 'left' });
    await service.input({ browser: 'c1', type: 'mouse_up', x: 100, y: 50, button: 'left' });
    await act({ action: 'wait_for', text: 'Other origin page', actor: 'person' });
    assert.equal((await service.open({ browser: 'c1', profile, policy })).origin, sites.other);
    await act({ action: 'navigate', url: `${sites.other}/other?again` });
    assert.equal(runner.asked('site').length, asked + 1);
  });

  // Regression (review of the above): the page that moved itself was not the agent's, but the agent could still
  // read it and act on it without being asked once the person gave the browser back.
  it('asks before the agent acts on a page that moved itself to another site', async () => {
    await act({ action: 'navigate', url: `${sites.home}/` });
    await act({ action: 'navigate', url: `${sites.home}/moves-itself`, actor: 'person' });
    await act({ action: 'wait_for', text: 'Other origin page', actor: 'person' });
    const asked = runner.asked('site').length;
    runner.answers.site = () => ({ allow: false, reason: 'not that one' });
    await assert.rejects(act({ action: 'snapshot' }), new RegExp(`The person did not allow ${sites.other}: not that one; the browser left that page`));
    assert.equal(runner.asked('site').length, asked + 1);
    assert.equal((await service.open({ browser: 'c1', profile, policy })).url, 'about:blank');
    // Allowed, the agent goes on there.
    await act({ action: 'navigate', url: `${sites.home}/moves-itself`, actor: 'person' });
    await act({ action: 'wait_for', text: 'Other origin page', actor: 'person' });
    runner.answers.site = () => ({ allow: true });
    assert.match((await act({ action: 'snapshot' })).text, /Other origin page/);
    assert.equal(runner.asked('site').length, asked + 2);
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

  // Regression: the snapshot shows a field's value, so a password the person typed during Take over reached
  // the agent as `textbox "Password": hunter2`.
  it('never shows the agent a password the person typed', async () => {
    await act({ action: 'navigate', url: `${sites.home}/signin` });
    let page = await snapshot();
    await act({ action: 'click', ref: refIn(page, 'textbox "Password"'), actor: 'person' });
    await service.input({ browser: 'c1', type: 'text', text: 'hunter2' });
    page = await snapshot();
    assert.ok(!page.includes('hunter2'), page);
    assert.match(page, /textbox "Password" .*\[ref=\w+\]: "\(password hidden\)"/);
    await act({ action: 'type', ref: refIn(page, 'textbox "Email"'), text: 'ada@example.com' });
    assert.match(await snapshot(), /textbox "Email" .*\[ref=\w+\]: ada@example.com/, 'other fields keep their value');
    await assert.rejects(act({ action: 'wait_for', text: 'hunter2', timeoutMs: 300 }), /Timed out/);
    // A shown password is plain text on the page: its autocomplete says what it holds.
    await act({ action: 'navigate', url: `${sites.home}/signin-shown` });
    page = await snapshot();
    const field = await act({ action: 'click', ref: refIn(page, 'textbox "Password"'), actor: 'person' });
    await service.input({ browser: 'c1', type: 'text', text: 'hunter2' });
    const shown = await act({ action: 'snapshot', withSnapshot: true });
    assert.ok(!shown.text.includes('hunter2'), shown.text);
    await assert.rejects(act({ action: 'type', ref: refIn(page, 'textbox "Password"'), text: 'x' }), /password field/);
    // The screenshot masks it: where the text is, every pixel is the mask's magenta.
    const shot = await act({ action: 'screenshot' }), box = field.box!;
    const row = await pixelRow(shot.image!.data, box.x + 10, box.y + box.height / 2, 150);
    assert.ok(row.every(([r, g, b]) => r > 200 && g < 80 && b > 200), `unmasked pixels: ${JSON.stringify(row.filter(([r, g, b]) => !(r > 200 && g < 80 && b > 200)).slice(0, 5))}`);
  });

  // Regression (review): a "show password" toggle turned the field into type=text, and its value reached the
  // agent's snapshot and screenshot, and the agent could type into it.
  it('keeps a password secret after the page shows it as text', async () => {
    await act({ action: 'navigate', url: `${sites.home}/signin-toggle` });
    const page = await snapshot();
    const field = await act({ action: 'click', ref: refIn(page, 'textbox "Password"'), actor: 'person' });
    await service.input({ browser: 'c1', type: 'text', text: 'hunter2' });
    await act({ action: 'click', ref: refIn(page, 'button "Show password"'), actor: 'person' });
    const shown = await snapshot();
    assert.ok(!shown.includes('hunter2'), shown);
    assert.match(shown, /textbox "Password" .*: "\(password hidden\)"/);
    await assert.rejects(act({ action: 'type', ref: refIn(shown, 'textbox "Password"'), text: 'x' }), /password field/);
    const shot = await act({ action: 'screenshot' }), box = field.box!;
    const row = await pixelRow(shot.image!.data, box.x + 10, box.y + box.height / 2, 150);
    assert.ok(row.every(([r, g, b]) => r > 200 && g < 80 && b > 200), `unmasked pixels: ${JSON.stringify(row.filter(([r, g, b]) => !(r > 200 && g < 80 && b > 200)).slice(0, 5))}`);
  });

  // Regression (review): the guard was disarmed when the action returned, so a script that submitted the form a
  // moment later (after a slow check) sent the password without asking.
  it('asks before a sign-in form the agent\'s action submits late', async () => {
    const fill = async () => {
      await act({ action: 'navigate', url: `${sites.home}/signin-late` });
      const page = await snapshot();
      await act({ action: 'click', ref: refIn(page, 'textbox "Password"'), actor: 'person' });
      await service.input({ browser: 'c1', type: 'text', text: 'hunter2' });
      return page;
    };
    let page = await fill(), asked = runner.asked('sign_in').length;
    const clicked = await act({ action: 'click', ref: refIn(page, 'button "Continue"') });
    assert.equal(new URL(clicked.state.url).pathname, '/signin-late');
    await runner.until(() => runner.asked('sign_in').length === asked + 1);
    // Declined: the form stays unsent; the next look at the page is still the sign-in.
    assert.equal(new URL((await act({ action: 'snapshot' })).state.url).pathname, '/signin-late');
    runner.answers.sign_in = () => ({ allow: true });
    page = await fill(); asked = runner.asked('sign_in').length;
    await act({ action: 'click', ref: refIn(page, 'button "Continue"') });
    await act({ action: 'wait_for', text: 'Signed in' });
    assert.equal(runner.asked('sign_in').length, asked + 1);
    runner.answers.sign_in = () => ({ allow: false });
  });

  // Regression: only Enter and real submit buttons were checked, so Space on the button, NumpadEnter, and a
  // script that submits the form sent the person's password without asking.
  it('asks before any way the agent submits a sign-in form the person filled', async () => {
    const fill = async (path: string) => {
      await act({ action: 'navigate', url: `${sites.home}${path}` });
      const page = await snapshot();
      await act({ action: 'click', ref: refIn(page, 'textbox "Password"'), actor: 'person' });
      await service.input({ browser: 'c1', type: 'text', text: 'hunter2' });
      return page;
    };
    const ways: [string, string, (page: string) => Promise<unknown>][] = [
      ['NumpadEnter in the password field', '/signin', () => act({ action: 'press', key: 'NumpadEnter' })],
      ['Space on the sign-in button', '/signin', async () => { await act({ action: 'press', key: 'Tab' }); return act({ action: 'press', key: 'Space' }); }],
      ['a plain button whose script calls form.submit()', '/signin-scripted', page => act({ action: 'click', ref: refIn(page, 'button "Continue"') })],
      ['a link whose script calls requestSubmit()', '/signin-scripted', page => act({ action: 'click', ref: refIn(page, 'link "Sign in with a link"') })],
    ];
    for (const [way, path, submit] of ways) {
      const page = await fill(path), asked = runner.asked('sign_in').length;
      await assert.rejects(submit(page), /did not allow submitting the sign-in form/, way);
      assert.equal(runner.asked('sign_in').length, asked + 1, way);
      assert.equal(new URL((await service.open({ browser: 'c1', profile, policy })).url).pathname, path, `${way}: the form was not sent`);
    }
    runner.answers.sign_in = () => ({ allow: true });
    const page = await fill('/signin-scripted'), asked = runner.asked('sign_in').length;
    const submitted = await act({ action: 'click', ref: refIn(page, 'button "Continue"') });
    assert.match(submitted.text, /Now on "Welcome"/);
    assert.equal(runner.asked('sign_in').length, asked + 1);
    runner.answers.sign_in = () => ({ allow: false });
  });

  // Regression: under load a submitted form's request left after settle()'s 150 ms look, and the
  // agent was told only "Clicked …" while the page went on to Welcome.
  it('waits for a navigation the click requested, however slow its page is to leave', async () => {
    await act({ action: 'navigate', url: `${sites.home}/signin-slow` });
    const submitted = await act({ action: 'click', ref: refIn(await snapshot(), 'button "Sign in"') });
    assert.match(submitted.text, /Now on "Welcome"/);
  });

  it('follows a sign-in through its redirect, and one a script makes once its call answers', async () => {
    for (const path of ['/signin-redirect', '/signin-script']) {
      await act({ action: 'navigate', url: `${sites.home}${path}` });
      const submitted = await act({ action: 'click', ref: refIn(await snapshot(), 'button "Sign in"') });
      assert.match(submitted.text, /Now on "Dashboard"/, path);
    }
  });

  it('follows a choice that submits its form', async () => {
    await act({ action: 'navigate', url: `${sites.home}/choose` });
    const chosen = await act({ action: 'select', ref: refIn(await snapshot(), 'combobox'), values: ['blue'] });
    assert.match(chosen.text, /Now on "Done"/);
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
