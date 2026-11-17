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
  if (failure.code === Code.DeadlineExceeded) return 'The Runner did not answer in time. Try again.';
  return raw || 'Something went wrong.';
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
