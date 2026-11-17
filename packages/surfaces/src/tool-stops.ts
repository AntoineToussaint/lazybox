import type { JournalEvent } from '@lazybox/protocol';
import type { ToolEntry } from './tool-lines';

/**
 * A tool call that did not fail but was stopped, by the person or by
 * Lazybox: an interrupt (Interrupt, Send now, a review starting) or a
 * declined approval. The provider reports both as a failed call with its own
 * words ("The user doesn't want to proceed with this tool use…"); the line
 * says Stopped or Denied, quietly, and keeps those words in its details.
 *
 * Which one it was is the journal's: the turn's accepted operations say
 * whether it was interrupted or an approval declined.
 */
export type TurnStops = { interrupted?: boolean; declined?: boolean };

const decoder = new TextDecoder();
const data = (event: NonNullable<JournalEvent['event']>): Record<string, any> => {
  if (!event.providerDataJson.length) return {};
  try { const value = JSON.parse(decoder.decode(event.providerDataJson)); return value && typeof value === 'object' ? value : {}; } catch { return {}; }
};

/** What a main-agent event says of the turn's stops. */
export function noteStops(stops: TurnStops | undefined, event: NonNullable<JournalEvent['event']>): TurnStops | undefined {
  if (event.type === 'turn_started') return undefined;
  if (event.type !== 'operation_accepted') return stops;
  const accepted = data(event);
  // The Runner names the operation; a journal from before it did has an
  // interrupt as the one operation with no text, no request and no sub-agent.
  const kind = typeof accepted.kind === 'string' ? accepted.kind
    : accepted.response && typeof accepted.response === 'object' ? accepted.response.kind
      : !event.text && !event.requestId && !event.agentId ? 'interrupt' : '';
  if (kind === 'interrupt') return { ...stops, interrupted: true };
  if (kind === 'approve' && accepted.response?.decision === 'decline') return { ...stops, declined: true };
  return stops;
}

/** Claude's words for a call it did not run because it was stopped. */
export function stopText(output: string | undefined): boolean {
  const text = (output || '').trim();
  return text.startsWith("The user doesn't want to proceed with this tool use.") || text.startsWith('The user doesn’t want to proceed with this tool use.') || text === 'Request canceled by provider';
}
/** The Claude worker's words for a declined approval (permissions.ts). */
export function declineText(output: string | undefined): boolean {
  const text = (output || '').trim();
  return text === 'Declined by operator' || /^The user declined this .+ request/.test(text);
}

/** A failed call that was stopped or denied, as the line shows it. */
export function stoppedEntry(entry: ToolEntry, stops: TurnStops | undefined): ToolEntry {
  if (entry.status !== 'failed') return entry;
  if (declineText(entry.output) || (stopText(entry.output) && stops?.declined && !stops.interrupted)) return { ...entry, status: 'denied' };
  if (stopText(entry.output)) return { ...entry, status: 'stopped' };
  return entry;
}
