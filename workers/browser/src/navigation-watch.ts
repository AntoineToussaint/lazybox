import type { CDPSession, Page, Request } from 'playwright-core';

/** How long a requested navigation may take to send its request, and then to commit. */
export const signWait = 3000, navigationWait = 10_000;
/** How long after the action's script calls answer a navigation they lead to may take to be requested. */
const afterCalls = 100;

/**
 * What one action did to this tab's page: whether it asked to go somewhere,
 * and where that went. It starts when the action begins and listens only
 * until it settles, so neither an earlier page's own loading nor a later
 * navigation counts as the action's.
 *
 * Chromium reports a requested navigation (Page.frameRequestedNavigation)
 * while the click is still being handled, before any request leaves: a page
 * slow to leave (its beforeunload, a busy machine) cannot hide it. The watch
 * then follows that navigation's own request, through its redirects, to its
 * outcome: committed (to the same address too), failed, finished without a
 * commit (a 204, a refused site), or a download.
 *
 * A page's script may decide to navigate only once a call it made answers (a
 * sign-in that posts with fetch, then goes to the dashboard): the watch waits
 * for the calls the action started, bounded, and then for that navigation.
 */
export class NavigationWatch {
  private requested = false;
  private request?: Request;
  private outcome?: 'committed' | 'ended';
  /** The page's script calls (fetch, XHR) started since the action began, until they answer. */
  private readonly calls = new Set<Request>();
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
      if (request.frame() !== main()) return;
      if (!request.isNavigationRequest()) {
        if (request.resourceType() === 'fetch' || request.resourceType() === 'xhr') this.calls.add(request);
        return;
      }
      // Its first request, or the next one of its redirects.
      if (!this.request || request.redirectedFrom() === this.request) { this.request = request; this.wake(); }
    });
    // A new document only: a pushState or a hash link changes the address in place, and is no commit to wait for.
    listen<{ frame: { parentId?: string } }>(cdp as never, 'Page.frameNavigated', event => { if (!event.frame.parentId) this.finish('committed'); });
    const answered = (request: Request) => { if (this.calls.delete(request)) this.wake(); };
    listen<Request>(page as never, 'requestfailed', request => { answered(request); if (request === this.request) this.finish('ended'); });
    // Finished without a commit (a 204, a site refused): a navigation that commits does so first.
    // A redirect finishes its request too, just before the next one starts: that one is followed instead.
    listen<Request>(page as never, 'requestfinished', request => {
      answered(request);
      if (request === this.request) setTimeout(() => { if (request === this.request) this.finish('ended'); }, 50);
    });
    // The action's own navigation turned into a download, not any download the page had going.
    listen<unknown>(page as never, 'download', () => { if (this.request) this.finish('ended'); });
  }

  /** Something changed that settle() may be waiting on (the gate asking the person). */
  wake() { const waiting = this.waiting; this.waiting = undefined; waiting?.(); }

  private finish(outcome: 'committed' | 'ended') { if (!this.outcome) this.outcome = outcome; this.wake(); }

  private next(ms: number): Promise<void> {
    return new Promise<void>(resolve => { const timer = setTimeout(resolve, Math.max(0, ms)); this.waiting = () => { clearTimeout(timer); resolve(); }; });
  }

  /** Not waiting any more: the navigation is known, or the person is being asked. */
  private known() { return !!this.outcome || this.askingPerson(); }

  /** Waits for where the action's navigation went; true when this page committed one. */
  async settle(): Promise<boolean> {
    // Script calls the action started, and no navigation yet: wait for their answers, bounded, then a moment
    // for the navigation the script decides on.
    if (this.calls.size && !this.requested && !this.request) {
      for (const end = Date.now() + signWait; this.calls.size && !this.requested && !this.request && !this.known() && Date.now() < end;) await this.next(end - Date.now());
      if (!this.requested && !this.request && !this.known()) await this.next(afterCalls);
    }
    // Asked to go somewhere and its request has not left yet (a page slow to leave): wait for it, bounded.
    for (const end = Date.now() + signWait; this.requested && !this.request && !this.known() && Date.now() < end;) await this.next(end - Date.now());
    // On its way: wait for its commit, its failure, a download or the person being asked, not the old page's load.
    for (const end = Date.now() + navigationWait; this.request && !this.known() && Date.now() < end;) await this.next(end - Date.now());
    return this.outcome === 'committed';
  }

  /** Stops listening: the action is over. */
  dispose() { for (const off of this.off.splice(0)) off(); this.wake(); }
}
