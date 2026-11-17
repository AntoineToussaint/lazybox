/** Why the SDK stream ended, as the failure the worker reports before it exits. */
export type StreamEnd = { type: 'failure'; status: 'provider_process_exit' | 'sdk_error'; text: string };

/**
 * Reads the SDK's message stream until it ends. In streaming-input mode it
 * ends only when the Claude Code process behind it is gone, or the SDK failed:
 * either way nothing will ever read another message.
 */
export async function readStream<T>(stream: AsyncIterable<T>, each: (message: T) => void): Promise<StreamEnd> {
  try {
    for await (const message of stream) each(message);
    return { type: 'failure', status: 'provider_process_exit', text: 'SDK stream ended' };
  } catch (error) {
    return { type: 'failure', status: 'sdk_error', text: String(error) };
  }
}

/**
 * Reports why the stream ended, then exits once that line is written. A
 * worker that stayed up would accept the next message into a generator nobody
 * consumes and be busy forever; exiting makes the Runner see the agent
 * process end, which it handles like any agent process that ended.
 */
export function exitAfter(end: StreamEnd, write: (line: string, written: () => void) => void, exit: (code: number) => void): void {
  write(JSON.stringify({ method: 'event', params: end }) + '\n', () => exit(1));
}
