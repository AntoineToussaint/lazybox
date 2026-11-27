import { mkdir } from 'node:fs/promises';
import { isAbsolute } from 'node:path';
import { chromium, type BrowserContext, type Page, type Request, type Route } from 'playwright-core';
import type { ActParams, CloseParams, CloseProfileParams, InputParams, OpenParams, Policy, ScreencastParams } from './protocol.js';
import { AgentBrowser, reason, shortURL, type Host } from './browser.js';
import { signInGuardScript, signInHeldBinding } from './sign-in-guard.js';
import { judgeURL, originOf } from './policy.js';
import { GateProxy } from './proxy.js';

/** The Runner, as the service sees it. */
export type Runner = {
  request(method: string, params: unknown): Promise<any>;
  emit(method: string, params: unknown): void;
};

const defaultViewport = { width: 1280, height: 800 };
/** The same refusal is logged once in this window, and a browser logs at most so many a minute. */
const blockedDedupeMs = 5000, blockedPerMinute = 20;
/** How long a popup's page and its first navigation wait to be paired. */
const popupPairingMs = 1500;

/** One workspace's profile: a persistent Chromium context, its gate proxy, and the browsers on it. */
type Profile = {
  dir: string; context: BrowserContext; proxy: GateProxy; policy: Policy; browsers: Set<AgentBrowser>; lastActive?: AgentBrowser; closing: boolean;
  /** Popup navigations that reached the gate before their page, and popup pages not yet paired with one. */
  strays: { url: string; at: number }[]; unclaimed: Page[];
};

/**
 * The browser service: Chromium for agents, isolated from the person's own
 * browser. Each profile directory (a workspace) is one persistent context of
 * its own, launched headless behind its own gate proxy; each browser (a
 * conversation) is one page in it. The Runner decides who may browse and which
 * sites; this service enforces the network rules it is given and asks the
 * Runner about everything else.
 */
export class BrowserService {
  private profiles = new Map<string, Profile>();
  private launching = new Map<string, Promise<Profile>>();
  private browsers = new Map<string, AgentBrowser>();
  private byPage = new WeakMap<Page, AgentBrowser>();
  /** Popups already moved into their opener's page. */
  private adopted = new WeakSet<Page>();
  /** Pages the service opened itself, marked before anything is awaited so none is taken for a popup. */
  private owned = new WeakSet<Page>();
  private recentBlocks = new Map<string, number>();
  private blockCounts = new Map<string, { minute: number; count: number }>();

  constructor(private runner: Runner, private launch: { executablePath?: string } = {}) {}

  /** Dispatches one Runner request. */
  handle(method: string, params: any): Promise<unknown> {
    switch (method) {
      case 'open': return this.open(params);
      case 'act': return this.act(params);
      case 'input': return this.input(params);
      case 'screencast': return this.screencast(params);
      case 'close': return this.close(params);
      case 'close_profile': return this.closeProfile(params);
      default: return Promise.reject(new Error(`Unsupported browser operation ${JSON.stringify(method)}`));
    }
  }

  private browser(id: unknown): AgentBrowser {
    const browser = typeof id === 'string' ? this.browsers.get(id) : undefined;
    if (!browser) throw new Error('This browser is not open.');
    return browser;
  }

  async open(p: OpenParams) {
    if (!p?.browser || typeof p.browser !== 'string') throw new Error('browser id required');
    if (!p.profile || !isAbsolute(p.profile)) throw new Error('profile must be an absolute directory');
    const policy = validPolicy(p.policy);
    const existing = this.browsers.get(p.browser);
    if (existing) {
      const profile = this.profileOf(existing);
      if (profile) profile.policy = policy;
      return existing.state();
    }
    const profile = await this.profile(p.profile, policy, p.viewport);
    profile.policy = policy;
    // The page a persistent context starts with is the first browser's.
    const blank = profile.browsers.size === 0 ? profile.context.pages().find(page => !this.byPage.has(page) && page.url() === 'about:blank') : undefined;
    const page = blank || await profile.context.newPage();
    this.owned.add(page);
    if (p.viewport) await page.setViewportSize(p.viewport);
    const cdp = await profile.context.newCDPSession(page);
    const browser = new AgentBrowser(p.browser, page, cdp, this.host(profile));
    this.browsers.set(p.browser, browser); this.byPage.set(page, browser); profile.browsers.add(browser); profile.lastActive = browser;
    await browser.start();
    page.on('crash', () => { this.forget(browser, 'The page crashed'); void page.close().catch(() => {}); });
    page.on('close', () => this.forget(browser, 'The page closed'));
    return browser.state();
  }

  /** What a browser of `profile` needs from the service. */
  private host(profile: Profile): Host {
    return {
      request: (method, params) => this.runner.request(method, params),
      emit: (method, params) => this.runner.emit(method, params),
      log: (browser, kind, summary, url) => this.log(browser, kind, summary, url),
      policy: () => profile.policy,
    };
  }

  private profileOf(browser: AgentBrowser): Profile | undefined {
    for (const profile of this.profiles.values()) if (profile.browsers.has(browser)) return profile;
    return undefined;
  }

  /** The profile's context, launched once even when several browsers open it at once. */
  private profile(dir: string, policy: Policy, viewport = defaultViewport): Promise<Profile> {
    const open = this.profiles.get(dir);
    if (open && !open.closing) return Promise.resolve(open);
    let launching = this.launching.get(dir);
    if (!launching) {
      launching = this.start(dir, policy, viewport).finally(() => this.launching.delete(dir));
      this.launching.set(dir, launching);
    }
    return launching;
  }

  private async start(dir: string, policy: Policy, viewport: { width: number; height: number }): Promise<Profile> {
    await mkdir(dir, { recursive: true, mode: 0o700 });
    const profile = { dir, policy, browsers: new Set<AgentBrowser>(), closing: false, strays: [], unclaimed: [] } as unknown as Profile;
    profile.proxy = new GateProxy(() => profile.policy, refusal => {
      if (profile.lastActive) this.log(profile.lastActive, 'blocked', `Blocked ${refusal.host}:${refusal.port}: ${refusal.reason}`);
    }, async port => {
      const browser = profile.lastActive;
      if (!browser) return false;
      const reply = await this.runner.request('preview_port', { browser: browser.id, port });
      return reply?.allow === true;
    });
    const server = await profile.proxy.listen();
    try {
      profile.context = await chromium.launchPersistentContext(dir, {
        headless: true, acceptDownloads: true, viewport, deviceScaleFactor: 1,
        // A service worker's requests escape page interception; the proxy would still gate them, but nothing here needs one.
        serviceWorkers: 'block',
        // Every connection, loopback included, goes through the gate proxy.
        proxy: { server, bypass: '<-loopback>' },
        ...(this.launch.executablePath ? { executablePath: this.launch.executablePath } : {}),
        args: ['--disable-extensions', '--disable-sync', '--no-first-run', '--no-default-browser-check', '--disable-background-networking'],
      });
    } catch (error) {
      await profile.proxy.close();
      throw new Error(launchProblem(error));
    }
    await profile.context.route('**/*', route => this.gate(profile, route));
    await profile.context.exposeBinding(signInHeldBinding, source => { const browser = this.byPage.get(source.page); if (browser) browser.signInHeld(source.frame); });
    await profile.context.addInitScript(signInGuardScript);
    profile.context.on('page', page => void this.opened(profile, page));
    profile.context.on('close', () => {
      for (const browser of profile.browsers) this.forget(browser, profile.closing ? 'The browser closed' : 'The browser stopped unexpectedly');
      if (this.profiles.get(dir) === profile) this.profiles.delete(dir);
      void profile.proxy.close();
    });
    this.profiles.set(dir, profile);
    return profile;
  }

  /**
   * Every request a page makes. Refuses what policy refuses (the proxy checks
   * again after DNS), folds popups into their opener's page, and asks the
   * Runner before the agent's page goes to a new origin.
   */
  private async gate(profile: Profile, route: Route) {
    const request = route.request(), url = request.url();
    const page = pageOf(request), browser = page ? this.byPage.get(page) : undefined;
    const mainFrame = !!browser && request.isNavigationRequest() && request.frame() === browser.page.mainFrame();
    // A refused navigation answers No Content, which leaves the page where it was instead of on an error page.
    const refuse = () => (mainFrame ? route.fulfill({ status: 204 }) : route.abort('blockedbyclient')).catch(() => {});
    const verdict = judgeURL(profile.policy, url);
    if (!verdict.allow) {
      const target = browser || profile.lastActive;
      if (target) this.log(target, 'blocked', `Blocked ${shortURL(url)}: ${verdict.reason}`, url);
      if (mainFrame) browser.blocked = verdict.reason;
      return refuse();
    }
    if (!browser) {
      // No page, or one the service does not manage: a popup, whose navigation moves into the opener's page.
      if (request.isNavigationRequest() && (!page || request.frame() === page.mainFrame())) {
        if (page) void this.adopt(profile, page, url); else this.stray(profile, { url });
      }
      return route.abort('blockedbyclient').catch(() => {});
    }
    if (mainFrame) {
      profile.lastActive = browser;
      // The agent's page asks before a new origin. The person's navigation never asks; whether it makes its
      // origin the agent's is decided when it commits (the person may not have started it).
      const origin = originOf(url);
      if (origin && origin !== browser.origin && browser.actor === 'agent' && !(await browser.allowSite(url, origin))) return refuse();
    }
    return route.continue().catch(() => {});
  }

  /**
   * A popup's first navigation is often issued before its page exists, so it
   * reaches the gate with no page. Such navigations and the pages no browser
   * owns are paired as they arrive: the popup is closed and its address opens
   * in the page that asked for it, through the same gate.
   */
  private stray(profile: Profile, entry: { url?: string; page?: Page }) {
    const now = Date.now();
    profile.strays = profile.strays.filter(s => now - s.at < popupPairingMs);
    if (entry.url) profile.strays.push({ url: entry.url, at: now });
    if (entry.page) profile.unclaimed.push(entry.page);
    while (profile.strays.length && profile.unclaimed.length) {
      const { url } = profile.strays.shift()!, popup = profile.unclaimed.shift()!;
      void this.adopt(profile, popup, url);
    }
  }

  private async adopt(profile: Profile, popup: Page, url: string) {
    if (this.adopted.has(popup)) return;
    this.adopted.add(popup);
    const opener = await popup.opener().catch(() => null);
    const browser = (opener && this.byPage.get(opener)) || profile.lastActive;
    await popup.close().catch(() => {});
    if (!browser) return;
    this.log(browser, 'popup', `Opened ${shortURL(url)} here instead of a new window`, url);
    void browser.page.goto(url, { waitUntil: 'domcontentloaded', timeout: 30_000 }).catch(() => {});
  }

  /** A new page: the service's own, or a popup, which is closed even if it never navigates. */
  private async opened(profile: Profile, page: Page) {
    // By the time the opener is known, a page the service opened itself is marked owned.
    await page.opener().catch(() => null);
    if (this.owned.has(page)) return;
    this.stray(profile, { page });
    setTimeout(() => {
      if (this.owned.has(page) || this.adopted.has(page)) return;
      profile.unclaimed = profile.unclaimed.filter(p => p !== page);
      void page.close().catch(() => {});
    }, popupPairingMs);
  }

  /** Reports something the Runner did not ask for. Blocked requests are deduplicated and rate-limited. */
  private log(browser: AgentBrowser, kind: 'blocked' | 'navigate' | 'download' | 'popup', summary: string, url?: string) {
    if (kind === 'blocked') {
      const now = Date.now(), key = `${browser.id}\n${summary}`;
      if (now - (this.recentBlocks.get(key) || 0) < blockedDedupeMs) return;
      this.recentBlocks.set(key, now);
      if (this.recentBlocks.size > 512) this.recentBlocks.clear();
      const minute = Math.floor(now / 60_000), counted = this.blockCounts.get(browser.id);
      const count = counted?.minute === minute ? counted.count + 1 : 1;
      this.blockCounts.set(browser.id, { minute, count });
      if (count > blockedPerMinute) return;
    }
    this.runner.emit('log', { browser: browser.id, kind, summary, ...(url ? { url } : {}) });
  }

  /** A browser that went away without the Runner closing it is reported once. */
  private forget(browser: AgentBrowser, why: string) {
    if (this.browsers.get(browser.id) !== browser) return;
    this.browsers.delete(browser.id);
    this.blockCounts.delete(browser.id);
    for (const profile of this.profiles.values()) {
      profile.browsers.delete(browser);
      if (profile.lastActive === browser) profile.lastActive = [...profile.browsers].at(-1);
    }
    if (!browser.closing) this.runner.emit('closed', { browser: browser.id, reason: why });
    browser.closing = true;
  }

  async act(p: ActParams) {
    const browser = this.browser(p?.browser);
    const profile = this.profileOf(browser);
    if (profile) {
      profile.lastActive = browser;
      if (p.previewPorts) profile.policy = validPolicy({ ...profile.policy, loopbackPorts: p.previewPorts });
    }
    return browser.act(p);
  }

  async input(p: InputParams) {
    return this.browser(p?.browser).input(p);
  }

  async screencast(p: ScreencastParams) {
    await this.browser(p?.browser).screencast(!!p.on);
    return {};
  }

  async close(p: CloseParams) {
    const browser = this.browsers.get(p?.browser);
    if (!browser) return {};
    const profile = this.profileOf(browser);
    browser.closing = true;
    this.forget(browser, 'Closed');
    await browser.page.close().catch(() => {});
    // A profile with no browser left closes its context: nothing runs for nobody.
    if (profile && profile.browsers.size === 0) await this.shut(profile);
    return {};
  }

  async closeProfile(p: CloseProfileParams) {
    const pending = this.launching.get(p?.profile);
    if (pending) await pending.catch(() => {});
    const profile = this.profiles.get(p?.profile);
    if (profile) {
      for (const browser of [...profile.browsers]) this.forget(browser, 'Browser data cleared');
      await this.shut(profile);
    }
    return {};
  }

  private async shut(profile: Profile) {
    profile.closing = true;
    this.profiles.delete(profile.dir);
    await profile.context.close().catch(() => {});
    await profile.proxy.close().catch(() => {});
  }

  /** Closes every context before the process exits. */
  async shutdown() {
    for (const browser of this.browsers.values()) browser.closing = true;
    await Promise.all([...this.launching.values()].map(p => p.catch(() => {})));
    await Promise.all([...this.profiles.values()].map(profile => this.shut(profile)));
  }
}

/** The page a request belongs to; none for a service worker's. */
function pageOf(request: Request): Page | undefined {
  try { return request.frame().page(); } catch { return undefined; }
}

function validPolicy(policy: Partial<Policy> | undefined): Policy {
  const ports = Array.isArray(policy?.loopbackPorts) ? policy.loopbackPorts.filter(port => Number.isInteger(port) && port > 0 && port < 65536) : [];
  const hosts = Array.isArray(policy?.blockedHosts) ? policy.blockedHosts.filter(host => typeof host === 'string' && host.includes(':')) : [];
  return { loopbackPorts: ports, blockedHosts: hosts };
}

/** A launch failure in words the person can act on. */
export function launchProblem(error: unknown): string {
  const text = error instanceof Error ? error.message : String(error);
  if (/Executable doesn't exist|browserType\.launch.*ENOENT|Looks like Playwright/i.test(text)) return 'Chromium for the agent browser is not installed. Run `make browser` to install it.';
  if (/ProcessSingleton|profile.*in use|SingletonLock/i.test(text)) return 'Another browser is using this workspace\'s agent browser profile.';
  return `The agent browser could not start: ${reason(error)}`;
}
