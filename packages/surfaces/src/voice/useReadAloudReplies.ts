import { useEffect, useRef } from 'react';
import type { JournalEvent } from '@lazybox/protocol';
import type { TranscriptMessage } from '../transcript-model';
import { readReply } from './voice-session';

/** The reply of the turn a `turn_ended` record closes: the main agent's text in that turn. */
export function replyOfTurn(messages: readonly TranscriptMessage[], turnId: string): string {
  const said = messages.filter(m => m.type === 'message_delta' && !m.agentId && (!turnId || m.turnId === turnId));
  if (said.length) return said.map(m => m.text).join('\n\n');
  return '';
}

export function latestAgentReply(messages: readonly TranscriptMessage[]): string {
  const last = messages.findLast(message => message.type === 'message_delta' && !message.agentId && message.text.trim());
  return last?.turnId ? replyOfTurn(messages, last.turnId) : (last?.text || '');
}

/** How old an ending may be and still count as news (a journal replayed on opening is history). */
const FRESH_MS = 10_000;

/**
 * Settings → Voice → Read aloud: a reply read when its turn ends while this
 * conversation is open. History is never read: only turns after what the
 * conversation held when it came up, and (where the Runner stamps them) that
 * ended moments ago. Once across windows.
 */
export function useReadAloudReplies(sessionID: string, transcript: { recent: readonly JournalEvent[]; messages: readonly TranscriptMessage[]; loading?: boolean; cursor: bigint }) {
  const seen = useRef<{ session: string; cursor: bigint | null }>({ session: '', cursor: null });
  useEffect(() => {
    if (!sessionID) return;
    const mark = seen.current;
    if (mark.session !== sessionID) { mark.session = sessionID; mark.cursor = null; }
    if (mark.cursor === null) {
      // The conversation as it came up: anything up to here is history.
      if (!transcript.loading) mark.cursor = transcript.cursor;
      return;
    }
    const since = mark.cursor;
    mark.cursor = transcript.cursor > since ? transcript.cursor : since;
    const ended = [...transcript.recent].reverse().find(record => record.cursor > since && record.event?.type === 'turn_ended' && (!record.event.agentId || record.event.agentId === 'root'));
    if (!ended) return;
    const at = Date.parse(ended.capturedAt || '');
    if (Number.isFinite(at) && Date.now() - at > FRESH_MS) return;
    const reply = replyOfTurn(transcript.messages, ended.event?.turnId || '');
    if (reply.trim()) readReply(`${sessionID}:${ended.cursor}`, reply);
  }, [sessionID, transcript.cursor, transcript.loading]);
}
