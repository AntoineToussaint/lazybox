import { createClient, ConnectError, Code } from '@connectrpc/connect';
import { createConnectTransport } from '@connectrpc/connect-web';
export { ConnectError, Code } from '@connectrpc/connect';
import { RunnerService } from '@lazybox/protocol';

/** Where a Runner serves and the owner bearer it accepts. A restart may change both. */
export type RunnerAuthority = { baseUrl: string; token: string };
export type RunnerLinkOptions = {
  fetch?: typeof globalThis.fetch;
  /** Reread the Runner's locator (the desktop's endpoint file). Absent where nothing can be reread, as in a browser tab. */
  renew?: () => Promise<RunnerAuthority | undefined>;
};

// A failed renew is not repeated for this long: every poll and stream of a
// window that lost its Runner would otherwise reread the locator at once.
const RENEW_SETTLE_MS = 250;

/**
 * One window's connection to its Runner. Every client it makes reads the
 * current authority on every request, so a Runner restarted with a new bearer
 * or on a new port is adopted by all of them at once, including clients held
 * by caches and long-lived streams. A request refused as unauthenticated, or
 * that cannot reach the Runner, rereads the locator; a refused one is retried
 * once when the locator names a different authority. An empty bearer is never
 * sent.
 */
export class RunnerLink {
  private current: RunnerAuthority;
  private listeners = new Set<() => void>();
  private renewing?: Promise<boolean>;
  private settled = 0;
  /** Bumped whenever the authority changes. */
  version = 0;
  /** Why the Runner last refused this window's bearer; cleared by the next accepted request. */
  authFailure = '';
  constructor(authority: RunnerAuthority, private options: RunnerLinkOptions = {}) { this.current = { baseUrl: authority.baseUrl.replace(/\/+$/, ''), token: authority.token }; }
  get authority(): RunnerAuthority { return this.current; }
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  private notify() { for (const listener of this.listeners) listener(); }
  /** Make `next` current; returns whether anything changed. An empty bearer is never adopted over a resolved one. */
  adopt(next: RunnerAuthority): boolean {
    if (!next.token && this.current.token) return false;
    const baseUrl = next.baseUrl.replace(/\/+$/, '');
    if (baseUrl === this.current.baseUrl && next.token === this.current.token) return false;
    this.current = { baseUrl, token: next.token }; this.version++; this.authFailure = '';
    this.notify();
    return true;
  }
  /** Reread the locator. Resolves true when it named a different authority, now adopted. */
  renew(): Promise<boolean> {
    const read = this.options.renew;
    if (!read) return Promise.resolve(false);
    if (this.renewing) return this.renewing;
    if (Date.now() - this.settled < RENEW_SETTLE_MS) return Promise.resolve(false);
    const before = this.version;
    this.renewing = read().then(next => { if (next) this.adopt(next); return this.version !== before; }, () => false)
      .finally(() => { this.renewing = undefined; this.settled = Date.now(); });
    return this.renewing;
  }
  private accepted() { if (this.authFailure) { this.authFailure = ''; this.notify(); } }
  private refused(error: unknown) {
    const message = ConnectError.from(error).rawMessage || String(error);
    if (this.authFailure !== message) { this.authFailure = message; this.notify(); }
  }
  /** A client bound to this link. Each call makes a distinct client object over the same authority. */
  client(): RunnerClient {
    const base = this.current.baseUrl || 'http://runner.invalid';
    const link = this;
    const transport = createConnectTransport({ baseUrl: base, fetch: this.options.fetch, interceptors: [next => async request => {
      // A server-streaming request carries one message; keep it so a retry can send it again.
      const messages: unknown[] = [];
      if (request.stream) for await (const message of request.message) messages.push(message);
      let sentWith = link.version;
      const attempt = async () => {
        let authority = link.current;
        if (!authority.token) { await link.renew(); authority = link.current; }
        if (!authority.token) throw new ConnectError('This window has no Runner bearer yet; it reconnects once the Runner publishes its endpoint', Code.Unauthenticated);
        sentWith = link.version;
        request.header.set('Authorization', `Bearer ${authority.token}`);
        const url = authority.baseUrl + request.url.slice(base.length);
        // The transport sends the request it is given: same header, current origin.
        const sent = request.stream ? { ...request, url, message: replay(messages) } : { ...request, url };
        return next(sent as typeof request);
      };
      try { const response = await attempt(); link.accepted(); return response; }
      catch (error) {
        const code = failureCode(error);
        if (request.signal.aborted || (code !== Code.Unauthenticated && code !== Code.Unavailable)) throw error;
        // Moved already when another request adopted the successor while this one was out.
        const moved = link.version !== sentWith || await link.renew();
        // Only a refused bearer is resent: the Runner rejected it before running
        // anything. A request that could not complete may have run before its
        // Runner went away, so it is not repeated; the next one goes to the successor.
        if (!moved || code !== Code.Unauthenticated || request.signal.aborted) { if (code === Code.Unauthenticated) link.refused(error); throw unreachable(error); }
        try { const response = await attempt(); link.accepted(); return response; }
        catch (retried) { if (failureCode(retried) === Code.Unauthenticated) link.refused(retried); throw unreachable(retried); }
      }
    }] });
    const client = createClient(RunnerService, transport);
    links.set(client, this);
    return client;
  }
}
async function* replay<T>(messages: T[]) { yield* messages; }
// A request that never reached a Runner (connection refused, reset) is a fetch
// failure rather than a Connect error: the Runner is unavailable.
function failureCode(error: unknown): Code {
  if (error instanceof ConnectError) return error.code;
  if (error instanceof Error && error.name === 'AbortError') return Code.Canceled;
  return Code.Unavailable;
}
// Say what a failed fetch means for a Runner client: the Runner is unavailable.
function unreachable(error: unknown): unknown {
  return error instanceof ConnectError || failureCode(error) !== Code.Unavailable ? error : new ConnectError(`Runner unreachable: ${error instanceof Error ? error.message : String(error)}`, Code.Unavailable, undefined, undefined, error);
}
const links = new WeakMap<object, RunnerLink>();
/** The link a client was made from, so caches outlive a rotated bearer. */
export function runnerLink(client: object): RunnerLink | undefined { return links.get(client); }

export function createRunnerClient(baseUrl: string, token: string, fetch?: typeof globalThis.fetch) {
  return new RunnerLink({ baseUrl, token }, { fetch }).client();
}
export type RunnerClient = ReturnType<typeof createClient<typeof RunnerService>>;

// Each view owns its own cursor. Rendering speed never participates in the
// producer's delivery and disconnecting a view only aborts its subscription.
// A heartbeat (WatchEvents with `heartbeats`) is no record: it says the stream
// is open and caught up, so an idle one reads Live instead of Connecting or
// Reconnecting forever. A Runner that sends none leaves that to the next record.
export async function follow<T extends { cursor: bigint; heartbeat?: boolean }>(
  open: (after: bigint, signal: AbortSignal) => AsyncIterable<T>,
  signal: AbortSignal, receive: (event: T) => void, status: (value: string) => void,
  failed?: (error: ConnectError) => void,
  initialCursor = 0n,
  { delay = reconnectDelay }: { delay?: (failures: number) => number } = {},
) {
  let after = initialCursor;
  let firstAttempt = true;
  // Failures in a row: the wait grows with them, and the first event resets them.
  let failures = 0;
  while (!signal.aborted) {
    try {
      status(firstAttempt ? 'Connecting' : 'Reconnecting'); firstAttempt=false;
      for await (const event of open(after, signal)) {
        failures = 0;
        if (event.heartbeat) { if (signal.aborted) return; status('Live'); continue; }
        if (event.cursor <= after) continue;
        if (event.cursor !== after + 1n) throw new ConnectError('Replay gap: retained cursor is not contiguous', Code.OutOfRange);
        if (signal.aborted) return;
        receive(event); after = event.cursor; status('Live');
      }
      if (!signal.aborted) status('Complete'); return;
    } catch (error) {
      if (signal.aborted) return;
      const code = ConnectError.from(error).code;
      // A refused bearer is not final: a restarted Runner publishes a new one,
      // which the client adopts, so keep reconnecting from the same cursor.
      if ([Code.OutOfRange, Code.PermissionDenied, Code.FailedPrecondition, Code.ResourceExhausted, Code.NotFound].includes(code)) { status(String(error)); failed?.(ConnectError.from(error)); return; }
      status(`Disconnected: ${String(error)}`);
      const wait = delay(++failures);
      await new Promise<void>(resolve => {
        const done = () => { clearTimeout(timer); signal.removeEventListener('abort', done); resolve(); };
        const timer = setTimeout(done, wait); signal.addEventListener('abort', done, { once: true });
      });
    }
  }
}

/**
 * How long a live stream waits before its next reconnection, after `failures`
 * failures in a row: about 1 s, doubling to at most 30 s, each wait between half
 * and all of that (jitter), so the windows of one machine do not all knock on a
 * restarting Runner in the same instant.
 */
export function reconnectDelay(failures: number, random: () => number = Math.random): number {
  const ceiling = Math.min(30_000, 1000 * 2 ** Math.max(0, failures - 1));
  return Math.round(ceiling / 2 + random() * ceiling / 2);
}

export function providerData(data: Uint8Array): any {
  if (!data.length) return {};
  try { return JSON.parse(new TextDecoder().decode(data)); } catch { return { error: 'Invalid provider data' }; }
}
export * from './device';
