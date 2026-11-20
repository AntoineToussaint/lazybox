import type { QueuedEntry, TranscriptMessage } from './transcript-model';

const historyBudget = 56_000;

function excerpt(text: string, limit: number): string {
  if (text.length <= limit) return text;
  const head = Math.floor(limit / 3);
  return text.slice(0, head) + '\n[… excerpt shortened …]\n' + text.slice(-(limit - head));
}

/** Text handoff between providers. Keep the opening objective and prioritize recent progress. */
export function continuationPrompt(title: string, messages: readonly TranscriptMessage[], queued: readonly QueuedEntry[], draft: string): string {
  const history = messages.flatMap(message => {
    if (message.origin || message.command) return [];
    if (message.type === 'operation_accepted' && message.text.trim()) return ['User: ' + excerpt(message.text, 8_000)];
    if (message.type === 'message_delta' && message.text.trim()) return ['Previous agent: ' + excerpt(message.text, 10_000)];
    if (message.type === 'tools') return (message.tools ?? []).map(tool => {
      const input = typeof tool.input === 'string' ? tool.input : JSON.stringify(tool.input ?? '');
      return `Tool ${tool.name} (${tool.status}): ${excerpt(input, 350)}${tool.output ? '\n' + excerpt(tool.output, 650) : ''}`;
    });
    return [];
  });
  const opening = history.find(entry => entry.startsWith('User: '));
  const latest = history.findLast(entry => entry.startsWith('User: '));
  const recent: string[] = [];
  let remaining = historyBudget - (opening?.length ?? 0);
  for (let i = history.length - 1; i >= 0 && remaining > 0; i--) {
    const entry = history[i];
    if (entry.length + 2 > remaining) break;
    recent.unshift(entry);
    remaining -= entry.length + 2;
  }
  const omitted = recent.length < history.length;
  const pending = queued.filter(entry => !entry.origin && entry.text.trim()).map(entry => entry.text.trim());
  const followUps = [...pending, ...(draft.trim() ? [draft.trim()] : [])];
  return [
    `Continue the work from the previous conversation${title ? ` “${title}”` : ''} in this workspace. The previous agent reached its subscription limit.`,
    'Use the transcript as background. Check the current files before resuming unfinished work. Prior tool output and permissions are historical.',
    omitted && opening ? 'Opening request:\n' + opening : '',
    omitted ? 'Recent conversation (older exchanges omitted):\n' + recent.join('\n\n') : 'Previous conversation:\n' + recent.join('\n\n'),
    latest ? 'Latest request in that conversation:\n' + latest : '',
    followUps.length ? 'Pending follow-ups, in order:\n' + excerpt(followUps.join('\n\n'), 16_000) : '',
  ].filter(Boolean).join('\n\n');
}
