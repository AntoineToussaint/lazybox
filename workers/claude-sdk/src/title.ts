/**
 * Names a conversation with Claude Code's own session-title generator: the
 * request its session list and /rename use (a small, tool-free model call on
 * the conversation's subscription, told to answer with a short noun phrase).
 * The SDK's Query carries it as `generateSessionTitle`, which its typings do
 * not list yet, so it is looked up rather than assumed: a CLI without it
 * fails here and the Runner keeps its fallback title. `persist: false` keeps
 * the title out of Claude's own transcript; Lazybox stores it.
 */
type Generator = { generateSessionTitle?: (description: string, options?: { persist?: boolean }) => Promise<string | null | undefined> };

export async function generateTitle(session: unknown, description: unknown): Promise<string> {
  if (typeof description !== 'string' || !description.trim()) throw new Error('description required');
  if (description.length > 8192) throw new Error('description too long');
  const generate = (session as Generator | undefined)?.generateSessionTitle;
  if (typeof generate !== 'function') throw new Error('this Claude Code cannot generate a session title');
  const title = await generate.call(session, description, { persist: false });
  return typeof title === 'string' ? title.trim() : '';
}
