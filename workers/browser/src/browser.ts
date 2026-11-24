import type { CDPSession, Download, Frame, Locator, Page } from 'playwright-core';
import type { ActParams, ActResult, Box, InputParams, PageState, Policy } from './protocol.js';
import { judgeNavigation, judgeURL, originOf } from './policy.js';
import { NavigationWatch, navigationWait } from './navigation-watch.js';
import { capSnapshot, filledTextboxes, hiddenValue, pageLine, quote, refLabels } from './snapshot.js';
import { armExpression, dropExpression, pendingExpression, releaseExpression, secretFieldSelector } from './sign-in-guard.js';

/** What a browser needs from the service that owns it. */
export type Host = {
  /** Asks the Runner (site, download, sign_in) and waits for its answer. */
  request(method: string, params: unknown): Promise<any>;
  emit(method: string, params: unknown): void;
  log(browser: AgentBrowser, kind: 'blocked' | 'navigate' | 'download' | 'popup', summary: string, url?: string): void;
  policy(): Policy;
};

/** How long an action waits for its element to be actionable. */
const elementTimeout = 5000;
const navigationTimeout = 30_000;
/** A full-page screenshot stops here; the rest is a scroll away. */
const maxScreenshotHeight = 4000;
/** The parts of CDP's Network.requestWillBeSent and Fetch.requestPaused the browser reads. */
type DocumentRequest = { type?: string; frameId?: string; loaderId: string; hasUserGesture?: boolean; redirectResponse?: unknown };
type PausedResponse = { requestId: string; frameId: string; request: { url: string }; responseStatusCode?: number; responseHeaders?: { name: string; value: string }[] };

/** CDP mouse button bits, for drags during Take over. */
const buttonBits: Record<string, number> = { left: 1, right: 2, middle: 4 };
/** The person's input that gives the page a user activation (Chromium's list): a press, not a move or a wheel. */
const activatingInputs = new Set(['mouse_down', 'key_down', 'text']);

/** Whether `el` holds a password: a password field, or one whose autocomplete says so (a shown password). */
function isSecretField(el: Element): boolean {
  return el instanceof HTMLInputElement && el.matches('input[type=password i], input[autocomplete*=password i]');
}

/**
 * Whether activating `el` would submit a form holding a filled password
 * field: a submit button, or Enter in a field of that form. Runs in the page,
 * so it declares no inner named functions (a bundler would wrap them in
 * helpers the page does not have).
 */
function submitsPassword(el: Element | null, enter: boolean): boolean {
  if (!el) return false;
  const submitButton = 'button:not([type]), button[type=submit i], input[type=submit i], input[type=image i]';
  const submitter = enter ? el.tagName === 'INPUT' || el.matches(submitButton) : el.matches(submitButton) || !!el.closest(submitButton);
  if (!submitter) return false;
  const form = (el as HTMLInputElement).form || el.closest('form');
  if (form) return Array.from(form.querySelectorAll<HTMLInputElement>('input[type=password i], input[autocomplete*=password i]')).some(input => input.value !== '');
  return el instanceof HTMLInputElement && el.matches('input[type=password i], input[autocomplete*=password i]') && el.value !== '';
}

/** The first line of a Playwright error, in words an agent can act on. */
export function reason(error: unknown): string {
  const text = (error instanceof Error ? error.message : String(error)).split('\n')[0].replace(/^[\w.]+: /, '');
  const timeout = /^Timeout (\d+)ms exceeded/.exec(text);
  return timeout ? `Timed out after ${Number(timeout[1]) / 1000}s` : text;
}

/**
 * One conversation's browser: a page in its workspace's profile. It runs the
 * agent's actions one at a time and the person's input during Take over, and
 * holds the agent-only rules: no typing into password fields, and asking
 * before a sign-in form is submitted.
 */
export class AgentBrowser {
  /** Who acts now; navigations the person makes never ask. */
  actor: 'agent' | 'person' = 'agent';
  /**
   * The origin the agent may be on without asking: one the person allowed, or
   * went to themselves. A navigation of the agent's elsewhere asks first, and
   * an origin is taken only once allowed, never while the question is open.
   */
  origin = '';
  loading = false;
  /** The origin a navigation waits on the person for, while it waits. */
  pendingSite = '';
  /** The person's latest refusal of a site, to explain a blocked navigation. */
  denial?: { origin: string; reason: string };
  /** Why the gate refused the page's latest navigation, if it did. */
  blocked = '';
  closing = false;
  private refs = new Map<string, string>();
  private queue: Promise<unknown> = Promise.resolve();
  private inputs: Promise<unknown> = Promise.resolve();
  private casting = false;
  private buttons = 0;
  private stateTimer?: NodeJS.Timeout;
  private mainFrameId?: string;
  /** What the running action does to the page (beginAction() to settle()). */
  private watch?: NavigationWatch;
  /** The person is navigating from the panel (address, back, forward, reload). */
  private personNavigating = false;
  /** The person pressed something since the agent last acted: a user activation the page has is theirs. */
  private personActivated = false;
  /** Main-frame document loads (by loader id) and whether the person started each. */
  private started = new Map<string, boolean>();
  /** Frames where the sign-in guard is armed for the running action. */
  private armed: Frame[] = [];
  /** The person's answer about submitting a sign-in form during the running action, once asked. */
  private signInAnswer?: Promise<boolean>;
  /** Held submissions being released or dropped, by the answer. */
  private signInWork: Promise<unknown> = Promise.resolve();

  constructor(readonly id: string, readonly page: Page, private cdp: CDPSession, private host: Host) {
    const main = () => page.mainFrame();
    // Page events (a requested navigation, NavigationWatch) reach this session only once its Page domain is on.
    void this.readMainFrame();
    page.on('request', request => { if (request.isNavigationRequest() && request.frame() === main()) { this.loading = true; this.changed(); } });
    page.on('requestfailed', request => { if (request.isNavigationRequest() && request.frame() === main()) { this.loading = false; this.changed(); } });
    page.on('load', () => { this.loading = false; this.changed(); });
    page.on('domcontentloaded', () => this.changed());
    page.on('framenavigated', frame => { if (frame === main()) this.changed(); });
    // A new document only: an address changed in place (pushState, a hash link) keeps its origin.
    cdp.on('Page.frameNavigated', event => { if (!event.frame.parentId) void this.committed(event.frame.url, event.frame.loaderId); });
    cdp.on('Network.requestWillBeSent', event => this.requested(event));
    cdp.on('Fetch.requestPaused', event => void this.responded(event));
    page.on('download', download => void this.download(download));
    page.on('dialog', dialog => {
      // A dialog would stall the agent; it is answered at once and logged.
      const kind = dialog.type(), message = dialog.message();
      void (kind === 'beforeunload' ? dialog.accept() : dialog.dismiss()).catch(() => {});
      this.host.log(this, 'blocked', `Dismissed the page's ${kind}${message ? ': ' + quote(message) : ''}`, page.url());
    });
  }

  /** Runs `work` after every earlier action of this browser. */
  private serial<T>(chain: 'queue' | 'inputs', work: () => Promise<T>): Promise<T> {
    const run = this[chain].then(work, work);
    this[chain] = run.catch(() => {});
    return run;
  }

  /** Tells the Runner the page changed, at most every 100ms. */
  changed() {
    if (this.stateTimer || this.closing) return;
    this.stateTimer = setTimeout(() => {
      this.stateTimer = undefined;
      if (this.closing) return;
      void this.state().then(state => this.host.emit('state', { browser: this.id, state }), () => {});
    }, 100);
  }

  /**
   * Starts what the browser watches beyond Playwright: document requests (who
   * started each) and document responses (a redirect is gated before it is
   * followed). Before the first action.
   */
  async start() {
    await this.readMainFrame();
    await this.cdp.send('Network.enable');
    await this.cdp.send('Fetch.enable', { patterns: [{ urlPattern: '*', resourceType: 'Document', requestStage: 'Response' }] });
  }

  async state(): Promise<PageState> {
    let canGoBack = false, canGoForward = false, title = '';
    try {
      const history = await this.cdp.send('Page.getNavigationHistory');
      const at = history.currentIndex, entries = history.entries;
      // The blank page every browser starts on is not somewhere to go back to.
      canGoBack = at > 0 && entries[at - 1].url !== 'about:blank';
      canGoForward = at < entries.length - 1;
      title = entries[at]?.title || '';
    } catch { /* a closing page has no history */ }
    const viewport = this.page.viewportSize() || { width: 0, height: 0 };
    return { url: this.page.url(), title, loading: this.loading, canGoBack, canGoForward, viewportWidth: viewport.width, viewportHeight: viewport.height, origin: this.origin };
  }

  /**
   * The page's title, read without running anything in the page: an
   * evaluation would give the page a user activation, and a navigation it then
   * made would pass for the person's.
   */
  private async title(): Promise<string> {
    try {
      const history = await this.cdp.send('Page.getNavigationHistory');
      return history.entries[history.currentIndex]?.title || '';
    } catch { return ''; }
  }

  /**
   * Asks the person (through the Runner) whether the agent's page may go to
   * `origin`, and takes it only once they allow it.
   */
  async allowSite(url: string, origin: string): Promise<boolean> {
    this.pendingSite = origin; this.askingPerson(); this.changed();
    let answer: { allow?: boolean; reason?: string } | undefined;
    try { answer = await this.host.request('site', { browser: this.id, url, origin }); } catch { answer = undefined; }
    finally { this.pendingSite = ''; this.changed(); }
    if (answer?.allow) { this.origin = origin; return true; }
    this.denial = { origin, reason: answer?.reason || '' };
    return false;
  }

  /** A main-frame document request leaves: whether the person started it is noted for its commit. */
  private requested(event: DocumentRequest) {
    if (event.type !== 'Document' || event.redirectResponse || (this.mainFrameId && event.frameId !== this.mainFrameId)) return;
    // Their own navigation from the panel, or one a press of theirs in the page started (it carries their user
    // activation). A page that moves itself (a timer, a meta refresh) has no activation of theirs.
    // The panel's navigation is the first document request after the person asked for it, not what its page does next.
    const panel = this.personNavigating;
    this.personNavigating = false;
    const person = this.actor === 'person' && (panel || (this.personActivated && !!event.hasUserGesture));
    if (this.started.size > 64) this.started.clear();
    this.started.set(event.loaderId, person);
  }

  /**
   * A document's response, before the browser acts on it. A redirect of the
   * main frame is judged before it is followed, so neither a blocked address
   * nor a site the person has not allowed for the agent receives a request
   * (request interception does not see redirect hops). A refused redirect is
   * answered with No Content, which leaves the page where it was.
   */
  private async responded(event: PausedResponse) {
    const go = () => this.cdp.send('Fetch.continueRequest', { requestId: event.requestId }).catch(() => {});
    const refuse = () => this.cdp.send('Fetch.fulfillRequest', { requestId: event.requestId, responseCode: 204, responseHeaders: [{ name: 'content-length', value: '0' }], body: '' }).catch(() => {});
    try {
      const status = event.responseStatusCode || 0;
      const location = event.responseHeaders?.find(header => header.name.toLowerCase() === 'location')?.value;
      const main = !this.mainFrameId || event.frameId === this.mainFrameId;
      if (!main || status < 300 || status >= 400 || !location) return await go();
      let target: string;
      try { target = new URL(location, event.request.url).href; } catch { return await go(); }
      const verdict = judgeURL(this.host.policy(), target);
      if (!verdict.allow) {
        this.blocked = verdict.reason;
        this.host.log(this, 'blocked', `Blocked ${shortURL(target)}: ${verdict.reason}`, target);
        return await refuse();
      }
      const origin = originOf(target);
      if (this.actor === 'agent' && origin && origin !== this.origin && !(await this.allowSite(target, origin))) return await refuse();
      return await go();
    } catch { return go(); }
  }

  /**
   * A main-frame document committed. The person's own navigation makes its
   * origin the agent's to stay on; one the page made by itself while they held
   * the browser does not. An agent's page that reached an origin no gate asked
   * about (a page restored from history) asks now, keeps the origin it had
   * while the person decides, and leaves the page if they say no.
   */
  private async committed(url: string, loaderId: string) {
    this.changed();
    const person = this.started.get(loaderId);
    this.started.delete(loaderId);
    const origin = originOf(url);
    if (!origin || origin === this.origin) return;
    if (this.actor === 'person') {
      if (person ?? this.personNavigating) { this.origin = origin; this.changed(); }
      return;
    }
    if (await this.allowSite(url, origin)) return;
    if (originOf(this.page.url()) === origin) await this.page.goto('about:blank').catch(() => {});
  }

  private async download(download: Download) {
    const url = download.url(), fileName = download.suggestedFilename();
    let answer: { allow?: boolean; path?: string } | undefined;
    try { answer = await this.host.request('download', { browser: this.id, url, fileName }); } catch { answer = undefined; }
    if (answer?.allow && answer.path) {
      try {
        await download.saveAs(answer.path);
        this.host.log(this, 'download', `Saved the download ${quote(fileName)}`, url);
      } catch (error) { this.host.log(this, 'download', `The download ${quote(fileName)} failed: ${reason(error)}`, url); }
      return;
    }
    await download.cancel().catch(() => {});
    this.host.log(this, 'download', `Refused the download ${quote(fileName)}`, url);
  }

  async screencast(on: boolean) {
    if (on === this.casting) return;
    this.casting = on;
    if (!on) { await this.page.screencast.stop().catch(() => {}); return; }
    const size = this.page.viewportSize() || { width: 1280, height: 800 };
    await this.page.screencast.start({ size, quality: 60, onFrame: frame => this.host.emit('frame', { browser: this.id, data: frame.data.toString('base64'), width: frame.viewportWidth, height: frame.viewportHeight }) });
  }

  act(p: ActParams): Promise<ActResult> {
    return this.serial('queue', async () => {
      this.actor = p.actor === 'person' ? 'person' : 'agent';
      this.denial = undefined; this.blocked = '';
      if (this.actor === 'agent') { this.personActivated = false; this.signInAnswer = undefined; }
      else if (['navigate', 'back', 'forward', 'reload'].includes(p.action)) this.personNavigating = true;
      else if (['click', 'type', 'select', 'press'].includes(p.action)) this.personActivated = true;
      try { return await this.run(p); }
      finally {
        // An action that failed before it settled stops watching the page all the same.
        this.watch?.dispose(); this.watch = undefined;
        this.personNavigating = false;
        await this.disarm();
      }
    });
  }

  private async run(p: ActParams): Promise<ActResult> {
    switch (p.action) {
      case 'navigate': return this.navigate(p);
      case 'snapshot': {
        const snapshot = await this.snapshot(p.maxChars);
        return this.result(p, `Page: ${await this.where()}\n\n${snapshot}`, 'Read the page', undefined, false);
      }
      case 'click': return this.click(p);
      case 'type': return this.type(p);
      case 'select': return this.select(p);
      case 'press': return this.press(p);
      case 'scroll': return this.scroll(p);
      case 'screenshot': return this.screenshot(p);
      case 'back': case 'forward': case 'reload': return this.history(p);
      case 'wait_for': return this.waitFor(p);
      default: throw new Error(`Unknown browser action ${JSON.stringify(p.action)}`);
    }
  }

  /** The person's mouse and keyboard during Take over, in page CSS pixels. */
  input(p: InputParams): Promise<PageState> {
    return this.serial('inputs', async () => {
      this.actor = 'person';
      if (activatingInputs.has(p.type)) this.personActivated = true;
      const modifiers = p.modifiers || 0, x = p.x || 0, y = p.y || 0;
      switch (p.type) {
        case 'mouse_move': case 'mouse_down': case 'mouse_up': {
          const button = p.button || 'left';
          if (p.type === 'mouse_down') this.buttons |= buttonBits[button] || 0;
          const type = p.type === 'mouse_move' ? 'mouseMoved' : p.type === 'mouse_down' ? 'mousePressed' : 'mouseReleased';
          await this.cdp.send('Input.dispatchMouseEvent', { type, x, y, modifiers, buttons: this.buttons, button: p.type === 'mouse_move' ? (this.buttons ? button : 'none') : button, clickCount: p.type === 'mouse_move' ? 0 : p.clickCount || 1 });
          if (p.type === 'mouse_up') this.buttons &= ~(buttonBits[button] || 0);
          break;
        }
        case 'wheel': await this.cdp.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x, y, modifiers, deltaX: p.deltaX || 0, deltaY: p.deltaY || 0 }); break;
        // Playwright tracks held modifier keys itself, from the key events the panel forwards.
        case 'key_down': if (p.key) await this.page.keyboard.down(p.key).catch(() => {}); break;
        case 'key_up': if (p.key) await this.page.keyboard.up(p.key).catch(() => {}); break;
        case 'text': if (p.text) await this.page.keyboard.insertText(p.text); break;
        default: throw new Error(`Unknown input ${JSON.stringify(p.type)}`);
      }
      return this.state();
    });
  }

  private async where(): Promise<string> {
    return pageLine(await this.title(), this.page.url());
  }

  private async snapshot(maxChars?: number): Promise<string> {
    const raw = await this.page.ariaSnapshot({ mode: 'ai', timeout: 10_000 });
    this.refs = refLabels(raw);
    return capSnapshot(await this.hidePasswords(raw), maxChars);
  }

  /**
   * The snapshot shows a field's value; the agent never sees a password the
   * person typed. A filled field that holds one (a password field, or a shown
   * password, by its autocomplete) is shown without its value.
   */
  private async hidePasswords(snapshot: string): Promise<string> {
    const lines = snapshot.split('\n');
    for (const field of filledTextboxes(snapshot)) {
      // A field that cannot be checked (it went away) is hidden too.
      const secret = await this.page.getByRef(field.ref).evaluate(isSecretField).catch(() => true);
      if (secret) lines[field.line] = hiddenValue(lines[field.line]);
    }
    return lines.join('\n');
  }

  /** The result every action returns: its text, the page, and a snapshot when asked. */
  private async result(p: ActParams, text: string, summary: string, box?: Box, snapshot = true, image?: ActResult['image']): Promise<ActResult> {
    if (snapshot && p.withSnapshot) text += `\n\n${await this.snapshot(p.maxChars).catch(error => `(No snapshot: ${reason(error)})`)}`;
    return { text, summary, state: await this.state(), ...(box ? { box } : {}), ...(image ? { image } : {}) };
  }

  /** An element of the latest snapshot, or why the agent needs a new one. */
  private async locate(ref: string | undefined): Promise<{ locator: Locator; label: string; box?: Box }> {
    if (!ref) throw new Error('This action needs a ref from browser_snapshot, such as e12.');
    const label = this.refs.get(ref);
    if (!label) throw new Error(`There is no element ${ref} in the latest snapshot. Take a fresh browser_snapshot and use a ref from it.`);
    const locator = this.page.getByRef(ref);
    const count = await locator.count().catch(() => 0);
    if (count === 0) throw new Error(`Element ${ref} (${label}) is no longer on the page; it changed since the snapshot. Take a fresh browser_snapshot.`);
    const box = await locator.boundingBox({ timeout: 2000 }).catch(() => null);
    return { locator, label, box: box || undefined };
  }

  /**
   * Asks the person before the agent submits a sign-in form, when the action
   * plainly would (a submit button, Enter in its field), then arms the guard in
   * the page for any other way the action submits one (sign-in-guard.ts).
   */
  private async guardSignIn(check: () => Promise<boolean>) {
    if (this.actor !== 'agent') return;
    let approved = false;
    if (await check().catch(() => false)) {
      this.signInAnswer = this.askSignIn();
      if (!(await this.signInAnswer)) throw this.signInRefused();
      approved = true;
    }
    await this.arm(approved);
  }

  private async askSignIn(): Promise<boolean> {
    const url = this.page.url(), origin = originOf(url);
    try { return !!(await this.host.request('sign_in', { browser: this.id, url, origin }))?.allow; } catch { return false; }
  }

  private signInRefused(): Error {
    const url = this.page.url();
    return new Error(`The person did not allow submitting the sign-in form on ${originOf(url) || url}.`);
  }

  /** Arms the in-page guard in every frame for the running agent action. */
  private async arm(approved: boolean) {
    const frames = this.page.frames();
    await Promise.all(frames.map(frame => frame.evaluate(armExpression(true, approved)).catch(() => {})));
    this.armed = frames;
  }

  private async disarm() {
    const frames = this.armed.splice(0);
    await Promise.all(frames.map(frame => frame.evaluate(armExpression(false, false)).catch(() => {})));
  }

  /**
   * The guard in `frame` held a sign-in form the agent's action submitted:
   * the person is asked (once an action), and the submission goes on or is
   * dropped by their answer.
   */
  signInHeld(frame: Frame) {
    const answer = this.signInAnswer ??= this.askSignIn();
    this.signInWork = Promise.all([this.signInWork, answer]).then(([, allow]) => frame.evaluate(allow ? releaseExpression : dropExpression).catch(() => {}));
  }

  /** Waits for a held sign-in form's answer; the action fails if the person refused. */
  private async heldSignIn() {
    // What the action submitted as it was handled is held by now, whether or not its binding call has arrived.
    await Promise.all(this.armed.map(async frame => { if (await frame.evaluate(pendingExpression).catch(() => 0)) this.signInHeld(frame); }));
    const answer = this.signInAnswer;
    if (!answer) return;
    const allow = await answer;
    await this.signInWork;
    if (!allow) throw this.signInRefused();
  }

  /** Where the page is as an action begins; what it does to the page from now on is watched (NavigationWatch). */
  private beginAction(): string {
    this.watch?.dispose();
    if (!this.mainFrameId) void this.readMainFrame();
    this.watch = new NavigationWatch(this.page, this.cdp, this.mainFrameId, () => !!this.pendingSite);
    return this.page.url();
  }

  /** The gate began asking the person about a site: an action waiting on its navigation says so instead. */
  askingPerson() { this.watch?.wake(); }

  private async readMainFrame() {
    try { await this.cdp.send('Page.enable'); this.mainFrameId = (await this.cdp.send('Page.getFrameTree')).frameTree.frame.id; } catch { /* read again at the next action */ }
  }

  /** After an action that may navigate: wait for where it went, and say where the agent is. */
  private async settle(before: string): Promise<string> {
    // A page's own script may navigate a moment after the action, without Chromium seeing it requested by it.
    await this.page.waitForTimeout(150);
    // A sign-in form the action submitted waits on the person; it goes on (and is followed) only if they allow it.
    await this.heldSignIn();
    const watch = this.watch;
    const committed = watch ? await watch.settle() : false;
    watch?.dispose();
    if (this.watch === watch) this.watch = undefined;
    if (committed || this.page.url() !== before) await this.page.waitForLoadState('domcontentloaded', { timeout: navigationWait }).catch(() => {});
    const notes: string[] = [];
    if (this.page.url() !== before) notes.push(`Now on ${await this.where()}.`);
    if (this.pendingSite) notes.push(`Waiting for the person to allow ${this.pendingSite}; the page changes once they answer.`);
    if (this.blocked) { notes.push(`Lazybox blocked that page: ${this.blocked}.`); this.blocked = ''; }
    if (this.denial) { notes.push(`The person did not allow ${this.denial.origin}${this.denial.reason ? ': ' + this.denial.reason : ''}.`); this.denial = undefined; }
    return notes.length ? ' ' + notes.join(' ') : '';
  }

  private async navigate(p: ActParams): Promise<ActResult> {
    const url = p.url || '';
    const verdict = judgeNavigation(this.host.policy(), url);
    if (!verdict.allow) throw new Error(`Not opened: ${verdict.reason}.`);
    this.denial = undefined; this.blocked = '';
    const refused = () => {
      const denial = this.denial as { origin: string; reason: string } | undefined;
      if (!denial) return;
      this.denial = undefined;
      throw new Error(`The person did not allow ${denial.origin}${denial.reason ? ': ' + denial.reason : ''}.`);
    };
    let response;
    try {
      response = await this.page.goto(url, { waitUntil: 'domcontentloaded', timeout: navigationTimeout });
    } catch (error) {
      refused();
      if (this.blocked) { const why = this.blocked; this.blocked = ''; throw new Error(`Lazybox blocked ${url}: ${why}.`); }
      const why = reason(error);
      if (/Download is starting/i.test(why)) return this.result(p, `${url} is a download, not a page. The person decides whether to keep it.`, `Opened a download link`, undefined, false);
      if (/ERR_BLOCKED_BY_CLIENT|ERR_TUNNEL_CONNECTION_FAILED/.test(why)) throw new Error(`Lazybox blocked ${url}: agent browsers do not reach this computer's or a private network's addresses.`);
      throw new Error(`Could not open ${url}: ${why}`);
    }
    // A refused navigation is answered with No Content, which goto may report as a response.
    refused();
    if (this.blocked) { const why = this.blocked; this.blocked = ''; throw new Error(`Lazybox blocked ${url}: ${why}.`); }
    // The gateway proxy refuses an address a host name resolved to with a page saying so.
    if (response?.status() === 403 && (await response.text().catch(() => '')).startsWith('Blocked by Lazybox')) {
      throw new Error(`Lazybox blocked ${url}: ${(await response.text()).replace(/^Blocked by Lazybox: /, '')}.`);
    }
    return this.result(p, `Opened ${await this.where()}.`, `Opened ${shortURL(this.page.url())}`);
  }

  private async click(p: ActParams): Promise<ActResult> {
    const { locator, label, box } = await this.locate(p.ref);
    await this.guardSignIn(() => locator.evaluate(submitsPassword, false));
    const before = this.beginAction();
    // A navigation the click starts may be refused or wait on the person; settle() waits for it instead.
    await locator.click({ timeout: elementTimeout, noWaitAfter: true }).catch(error => { throw new Error(`Could not click ${label}: ${reason(error)}`); });
    return this.result(p, `Clicked ${label}.${await this.settle(before)}`, `Clicked ${label}`, box);
  }

  private async type(p: ActParams): Promise<ActResult> {
    const { locator, label, box } = await this.locate(p.ref);
    const text = p.text ?? '';
    if (this.actor === 'agent' && await locator.evaluate(isSecretField).catch(() => false)) {
      throw new Error(`${label} is a password field. Agents never type passwords: ask the person to use Take over in the browser panel and type it themselves.`);
    }
    // Typing may submit by script (a code field that signs in once full): the guard holds it.
    await this.guardSignIn(async () => false);
    await locator.fill(text, { timeout: elementTimeout }).catch(error => { throw new Error(`Could not type into ${label}: ${reason(error)}`); });
    let after = '';
    if (p.submit) {
      await this.guardSignIn(() => locator.evaluate(submitsPassword, true));
      const before = this.beginAction();
      await locator.press('Enter', { timeout: elementTimeout, noWaitAfter: true });
      after = await this.settle(before);
    } else await this.heldSignIn();
    const summary = `Typed ${quote(text)} into ${label}${p.submit ? ' and submitted' : ''}`;
    return this.result(p, `${summary}.${after}`, summary, box);
  }

  private async select(p: ActParams): Promise<ActResult> {
    const { locator, label, box } = await this.locate(p.ref);
    const values = p.values || [];
    await this.guardSignIn(async () => false);
    // A choice may submit its form (onchange): settle() follows it like a click's.
    const before = this.beginAction();
    const chosen = await locator.selectOption(values, { timeout: elementTimeout, noWaitAfter: true }).catch(error => { throw new Error(`Could not select in ${label}: ${reason(error)}`); });
    const summary = `Selected ${(chosen.length ? chosen : values).map(v => quote(v)).join(', ')} in ${label}`;
    return this.result(p, `${summary}.${await this.settle(before)}`, summary, box);
  }

  private async press(p: ActParams): Promise<ActResult> {
    const key = p.key || '';
    if (!key) throw new Error('press needs a key, such as Enter, Escape or ArrowDown.');
    // Enter submits from a field or a button, Space activates a focused button; the guard in the page holds any other way.
    const submitKey = /(^|\+)(Numpad)?Enter$/.test(key) ? 'enter' : /(^|\+)(Space| )$/.test(key) ? 'space' : '';
    await this.guardSignIn(async () => {
      if (!submitKey) return false;
      const active = await this.page.evaluateHandle(() => document.activeElement);
      try { return await active.evaluate(submitsPassword, submitKey === 'enter'); } finally { await active.dispose(); }
    });
    const before = this.beginAction();
    await this.page.keyboard.press(key).catch(error => { throw new Error(`Could not press ${key}: ${reason(error)}`); });
    return this.result(p, `Pressed ${key}.${await this.settle(before)}`, `Pressed ${key}`);
  }

  private async scroll(p: ActParams): Promise<ActResult> {
    if (p.ref) {
      const { locator, label } = await this.locate(p.ref);
      await locator.scrollIntoViewIfNeeded({ timeout: elementTimeout });
      const box = await locator.boundingBox().catch(() => null);
      return this.result(p, `Scrolled to ${label}.`, `Scrolled to ${label}`, box || undefined);
    }
    const viewport = this.page.viewportSize() || { width: 1280, height: 800 };
    const direction = p.direction || 'down';
    const amount = p.amount && p.amount > 0 ? p.amount : Math.round((direction === 'left' || direction === 'right' ? viewport.width : viewport.height) * 0.8);
    const dx = direction === 'left' ? -amount : direction === 'right' ? amount : 0, dy = direction === 'up' ? -amount : direction === 'down' ? amount : 0;
    await this.page.mouse.move(viewport.width / 2, viewport.height / 2);
    await this.page.mouse.wheel(dx, dy);
    await this.page.waitForTimeout(250);
    const at = await this.page.evaluate(() => ({ y: Math.round(scrollY), height: document.documentElement.scrollHeight, view: innerHeight }));
    return this.result(p, `Scrolled ${direction}. Showing ${at.y}–${at.y + at.view} of ${at.height} px.`, `Scrolled ${direction}`);
  }

  private async screenshot(p: ActParams): Promise<ActResult> {
    const viewport = this.page.viewportSize() || { width: 1280, height: 800 };
    let height = viewport.height, capped = false;
    if (p.fullPage) {
      const full = await this.page.evaluate(() => document.documentElement.scrollHeight);
      capped = full > maxScreenshotHeight;
      height = Math.min(full, maxScreenshotHeight);
    }
    // A password the person typed is masked, shown or not, in every frame.
    const mask = this.page.frames().map(frame => frame.locator(secretFieldSelector));
    const data = await this.page.screenshot({ type: 'jpeg', quality: 70, timeout: 15_000, mask, ...(p.fullPage ? { fullPage: true, clip: { x: 0, y: 0, width: viewport.width, height } } : {}) });
    const text = `Screenshot of ${await this.where()} (${viewport.width}×${height}${capped ? `, the top ${maxScreenshotHeight} px of the page` : ''}).`;
    return this.result(p, text, p.fullPage ? 'Took a full-page screenshot' : 'Took a screenshot', undefined, false, { mediaType: 'image/jpeg', data: data.toString('base64') });
  }

  private async history(p: ActParams): Promise<ActResult> {
    const options = { waitUntil: 'domcontentloaded' as const, timeout: navigationTimeout };
    const before = this.page.url();
    const words = { back: ['Went back', 'There is no previous page.'], forward: ['Went forward', 'There is no next page.'], reload: ['Reloaded the page', ''] }[p.action as 'back' | 'forward' | 'reload'];
    const response = p.action === 'back' ? await this.page.goBack(options) : p.action === 'forward' ? await this.page.goForward(options) : await this.page.reload(options);
    if (!response && p.action !== 'reload' && this.page.url() === before) return this.result(p, words[1], words[0], undefined, false);
    return this.result(p, `${words[0]}: ${await this.where()}.`, words[0]);
  }

  private async waitFor(p: ActParams): Promise<ActResult> {
    const timeout = Math.min(Math.max(p.timeoutMs ?? 10_000, 100), 60_000);
    const seconds = timeout / 1000;
    try {
      if (p.text) {
        await this.page.getByText(p.text).first().waitFor({ state: 'visible', timeout });
        return this.result(p, `${quote(p.text)} is on the page.`, `Waited for ${quote(p.text)}`);
      }
      if (p.textGone) {
        await this.page.getByText(p.textGone).first().waitFor({ state: 'hidden', timeout });
        return this.result(p, `${quote(p.textGone)} is gone.`, `Waited for ${quote(p.textGone)} to go`);
      }
      if (p.ref) {
        const { locator, label } = await this.locate(p.ref);
        await locator.waitFor({ state: 'visible', timeout });
        return this.result(p, `${label} is visible.`, `Waited for ${label}`);
      }
    } catch (error) {
      if (/Timeout/.test(String(error))) throw new Error(`Timed out after ${seconds}s waiting for ${p.text ? quote(p.text) : p.textGone ? `${quote(p.textGone)} to go` : p.ref}.`);
      throw error;
    }
    throw new Error('wait_for needs text, textGone or ref.');
  }
}

/** An address as a log line shows it: host and path, no scheme. */
export function shortURL(url: string): string {
  try {
    const u = new URL(url);
    const path = u.pathname === '/' ? '' : u.pathname;
    const text = u.host + path + u.search;
    return text.length > 80 ? text.slice(0, 79) + '…' : text;
  } catch { return url; }
}
