/**
 * The Runner's lines, read one at a time. A line the worker cannot take never
 * ends it: a request too large is refused under its own id (the Runner sees
 * that one request fail), a reply too large fails the tool call it answers,
 * and a line that is not a JSON object is set aside and said on stderr.
 */
export type Request = { id?: string; method?: string; params?: any; result?: any; error?: any };

/**
 * The longest line taken, in UTF-16 code units (never more than its UTF-8
 * bytes). It equals `WorkerLineBytes` in the Runner's Claude adapter
 * (services/runner/code/internal/runner/adapters/claude), which refuses to
 * write a longer message and whose test shows the largest message the
 * Runner accepts (its attachments and images as base64) fits.
 */
export const LINE_LIMIT = 64 << 20;

export type Sinks = {
  /** A request, or a reply to one of the worker's tool calls. */
  handle(request: Request): void;
  /** The pending tool call `id`, if there is one, fails with `message`. */
  failTool(id: string, message: string): boolean;
  /** Answers the Runner's request `id` with an error. */
  refuse(id: string | number, message: string): void;
  /** Says why a line was set aside. */
  discard(reason: string): void;
};

/** The id of a line too long to parse: the Runner writes it first (`{"id":"lazybox-7",…`). */
export function leadingId(line: string): string | number | undefined {
  const match = /^\s*\{\s*"id"\s*:\s*("(?:[^"\\]|\\.){1,256}"|-?\d{1,15})\s*[,}]/.exec(line.slice(0, 600));
  if (!match) return undefined;
  try { return JSON.parse(match[1]) as string | number; } catch { return undefined; }
}

export function route(line: string, sinks: Sinks, limit = LINE_LIMIT): void {
  if (line.length > limit) {
    const id = leadingId(line);
    const message = `The message is too large for Claude: ${Math.ceil(line.length / (1 << 20))} MiB encoded, at most ${limit >> 20} MiB`;
    if (id === undefined) sinks.discard(`a line of ${line.length} characters with no id`);
    else if (!sinks.failTool(String(id), message)) sinks.refuse(id, message);
    return;
  }
  let value: unknown;
  try { value = JSON.parse(line); } catch { sinks.discard('a line that is not JSON'); return; }
  if (!value || typeof value !== 'object' || Array.isArray(value)) { sinks.discard('a line that is not a JSON object'); return; }
  sinks.handle(value as Request);
}
