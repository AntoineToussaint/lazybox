/**
 * One clear connection state for the whole app, said once, quietly: the
 * Runner's polls (`Info`) and, inside a conversation, its live stream
 * (client-core `follow`). Pure.
 */
export type ConnectionTone = 'live' | 'connecting' | 'reconnecting' | 'unreachable' | 'ended';
export type ConnectionView = { tone: ConnectionTone; label: string; detail: string; /** A banner is shown: something is not right. */ banner: boolean };

export function connectionView({ hasInfo, error, stream, foreground = true }: {
  /** The Runner answered at least once since the app opened. */
  hasInfo: boolean;
  /** The last poll's failure, or '' when it answered. */
  error: string;
  /** A conversation's stream status, when one is open. */
  stream?: string;
  foreground?: boolean;
}): ConnectionView {
  if (!foreground) return { tone: 'connecting', label: 'Paused', detail: 'Lazybox reconnects when you come back.', banner: false };
  if (error && !hasInfo) return { tone: 'connecting', label: 'Connecting to your computer…', detail: error, banner: true };
  if (error) return { tone: 'unreachable', label: 'Can’t reach your computer · retrying', detail: 'What you see was loaded earlier. Check that it is awake and on your tailnet.', banner: true };
  if (stream === undefined || stream === 'Live') return { tone: 'live', label: 'Live', detail: '', banner: false };
  if (stream === 'Connecting') return { tone: 'connecting', label: 'Connecting…', detail: '', banner: false };
  if (stream === 'Complete') return { tone: 'ended', label: 'Ended', detail: '', banner: false };
  if (stream === 'Reconnecting' || stream.startsWith('Disconnected')) return { tone: 'reconnecting', label: 'Reconnecting…', detail: '', banner: false };
  // A failure the stream does not retry (client-core `follow`): said as it is.
  return { tone: 'unreachable', label: 'Not live', detail: stream, banner: true };
}
