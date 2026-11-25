import { Code, ConnectError } from '@lazybox/client-core';

/**
 * A failure as people read it: what the Runner said, without the Connect
 * wrapping (`ConnectError: [failed_precondition] …`), and in plain words for
 * the few that only make sense to a developer.
 */
const PLAIN: readonly [RegExp, string][] = [
  // client-core `follow` and the transcript's pages (transcript-cache.ts).
  [/^Replay gap\b/i, 'Part of this conversation’s history is missing, so it cannot be shown in order.'],
  // An interrupt, or Send now, with no turn running (the Runner's engine).
  [/^no active turn$/i, 'The agent is not working on anything: there is nothing to stop.'],
];

export function humanError(error: unknown): string {
  const failure = ConnectError.from(error);
  const raw = (failure.rawMessage || (error instanceof Error ? error.message : String(error))).trim();
  for (const [pattern, text] of PLAIN) if (pattern.test(raw)) return text;
  // The window could not reach the Runner at all (client-core says "Runner unreachable: …"):
  // what the browser said is no help. When the Runner itself answers Unavailable, its words stand.
  if (failure.code === Code.Unavailable && /^Runner unreachable\b|failed to fetch|load failed|networkerror|fetch failed/i.test(raw)) return 'The Runner is not answering (it may be restarting). Try again in a moment.';
  // The Runner's own Go context ran out of time ("context deadline exceeded", whatever code carries it).
  if (failure.code === Code.DeadlineExceeded || /\bcontext deadline exceeded\b/i.test(raw)) return 'The Runner did not answer in time. Try again.';
  if (isCancellation(error)) return 'The request was cancelled. Try again.';
  return raw || 'Something went wrong.';
}

/**
 * A request that ended because it was cancelled (a window reconnecting, a view
 * moving on, the Runner's own "context canceled"), not because anything failed:
 * a refresh that ends this way is no error to show.
 */
export function isCancellation(error: unknown): boolean {
  const failure = ConnectError.from(error);
  const raw = (failure.rawMessage || (error instanceof Error ? error.message : String(error))).trim();
  return failure.code === Code.Canceled || (error instanceof Error && error.name === 'AbortError') || /\bcontext canceled\b/i.test(raw);
}

/** How long refreshes may keep failing before the window says so: one slow answer under load is not an outage. */
export const REFRESH_FAILURE_GRACE_MS = 15_000;

/** A failure the next attempt may not hit: the Runner slow, restarting or briefly out of reach. */
export function isTransient(error: unknown): boolean {
  const failure = ConnectError.from(error);
  const raw = (failure.rawMessage || (error instanceof Error ? error.message : String(error))).trim();
  switch (failure.code) {
    case Code.Unavailable: case Code.DeadlineExceeded: case Code.ResourceExhausted: case Code.Aborted: return true;
    case Code.Unknown: case Code.Internal: return /\bcontext deadline exceeded\b/i.test(raw);
    default: return false;
  }
}

/**
 * What a polling view shows after a failed refresh. `failingSince` is when
 * the first failing refresh in a row was sent (undefined after a success),
 * `sentAt` when this one was. A cancellation changes nothing (`shown` is
 * undefined: keep what is shown); a lasting failure (a deleted session, a
 * refused credential) shows at once; a transient one only once refreshes have
 * kept failing for the grace period.
 */
export function refreshFailure(failingSince: number | undefined, sentAt: number, now: number, error: unknown): { failingSince: number | undefined; shown: string | undefined } {
  if (isCancellation(error)) return { failingSince, shown: undefined };
  const since = failingSince ?? sentAt;
  if (!isTransient(error)) return { failingSince: since, shown: humanError(error) };
  return { failingSince: since, shown: now - since >= REFRESH_FAILURE_GRACE_MS ? humanError(error) : '' };
}

/**
 * The state of a conversation's live stream (client-core `follow`), as the
 * session menu says it: never the raw error a reconnecting stream carries.
 */
export function streamLabel(stream: string): string {
  if (stream === 'Live') return 'Live';
  if (stream === 'Connecting') return 'Connecting…';
  if (stream === 'Reconnecting' || stream.startsWith('Disconnected')) return 'Reconnecting…';
  if (stream === 'Complete') return 'Ended';
  return 'Not connected';
}
