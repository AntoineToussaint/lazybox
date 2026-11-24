import type { CDPSession, Page, Request } from 'playwright-core';

/** How long a requested navigation may take to send its request, and then to commit. */
export const signWait = 3000, navigationWait = 10_000;

/**
 * What one action did to this tab's page: whether it asked to go somewhere,
 * and where that went. It starts when the action begins and listens only
 * until it settles, so neither an earlier page's own loading nor a later
 * navigation counts as the action's.
 *
 * Chromium reports a requested navigation (Page.frameRequestedNavigation)
 * while the click is still being handled, before any request leaves: a page
 * slow to leave (its beforeunload, a busy machine) cannot hide it. The watch
 * then follows that navigation's own request to its outcome: committed (to
 * the same address too), failed, finished without a commit (a 204, a refused
 * site), or a download.
 */
export class NavigationWatch {
  private requested = false;
  private request?: Request;
  private outcome?: 'committed' | 'ended';
  private waiting?: () => void;
  private readonly off: Array<() => void> = [];

  /**
   * `mainFrameId` is the page's main frame in CDP's terms; unknown (not read
   * yet), any frame's request counts, which can cost a bounded wait, never a
   * missed navigation. `askingPerson` is whether the gate waits on the person
   * for the site: the action then says so instead of waiting.
   */
  constructor(page: Page, cdp: CDPSession, mainFrameId: string | undefined, private askingPerson: () => boolean) {
    const main = () => page.mainFrame();
    const listen = <T>(target: { on(event: string, handler: (value: T) => void): unknown; off(event: string, handler: (value: T) => void): unknown }, event: string, handler: (value: T) => void) => {
      target.on(event, handler);
      this.off.push(() => { target.off(event, handler); });
    };
    // This tab's page only: not a frame inside it, nor a new tab, window or download link.
    listen<{ frameId: string; disposition: string }>(cdp as never, 'Page.frameRequestedNavigation', event => {
      if ((!mainFrameId || event.frameId === mainFrameId) && event.disposition === 'currentTab') { this.requested = true; this.wake(); }
    });
    listen<Request>(page as never, 'request', request => {
      if (!request.isNavigationRequest() || request.frame() !== main()) return;
      // Its first request, or the next one of its redirects.
      if (!this.request || request.redirectedFrom() === this.request) { this.request = request; this.wake(); }
    });
    listen<{ url(): string }>(page as never, 'framenavigated', frame => { if (frame === main()) this.finish('committed'); });
    listen<Request>(page as never, 'requestfailed', request => { if (request === this.request) this.finish('ended'); });
    // Finished without a commit (a 204, a site refused): a navigation that commits does so first.
    listen<Request>(page as never, 'requestfinished', request => { if (request === this.request) setTimeout(() => this.finish('ended'), 50); });
    listen<unknown>(page as never, 'download', () => this.finish('ended'));
  }

  /** Something changed that settle() may be waiting on (the gate asking the person). */
  wake() { const waiting = this.waiting; this.waiting = undefined; waiting?.(); }

  private finish(outcome: 'committed' | 'ended') { if (!this.outcome) this.outcome = outcome; this.wake(); }

  private next(ms: number): Promise<void> {
    return new Promise<void>(resolve => { const timer = setTimeout(resolve, Math.max(0, ms)); this.waiting = () => { clearTimeout(timer); resolve(); }; });
  }

  /** Waits for where the action's navigation went; true when this page committed one. */
  async settle(): Promise<boolean> {
    // Asked to go somewhere and its request has not left yet (a page slow to leave): wait for it, bounded.
    if (this.requested && !this.request && !this.outcome && !this.askingPerson()) await this.next(signWait);
    // On its way: wait for its commit, its failure, a download or the person being asked, not the old page's load.
    for (const end = Date.now() + navigationWait; this.request && !this.outcome && !this.askingPerson() && Date.now() < end;) await this.next(end - Date.now());
    return this.outcome === 'committed';
  }

  /** Stops listening: the action is over. */
  dispose() { for (const off of this.off.splice(0)) off(); this.wake(); }
}
