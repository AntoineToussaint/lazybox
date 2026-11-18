import type { Collection, Profile, Session } from '@lazybox/protocol';
import {
  ageLabel, buildInbox, conversationDetails, conversationName, phoneSections, pinnedAt, rowPlan,
  type Attention, type AttentionState, type RowPlan, type Seen,
} from '@lazybox/surfaces/models';

/**
 * The iPhone Inbox as data: the same attention rules, row plans and sections
 * as the phone web app (surfaces/models), shaped for a native SectionList.
 * Pure, so tests pin what each row says and offers.
 */
export type InboxRow = {
  key: string;
  sessionId: string;
  title: string;
  /** "Beta release · Claude · Opus 5.5" */
  meta: string;
  state: AttentionState;
  unread: boolean;
  pinned: boolean;
  /** "now", "4m", "3h" */
  age: string;
  /** One short phrase: "Approval", "Question · 2 waiting", "Failed". */
  reason: string;
  /** The agent's last line, the question, or the failure. */
  line: string;
  /** What the row lets the person do without opening the conversation. */
  plan: RowPlan;
  /** A working conversation the person may stop from the row. */
  stoppable: boolean;
  attention: Attention;
};
export type InboxSection = { id: string; title: string; state?: AttentionState; count: number; rows: InboxRow[]; /** Rows not shown until "Show more". */ hidden: number };
export type InboxView = { sections: InboxSection[]; summary: string; needsYou: number; empty: boolean };

/** Done conversations shown before "Show more", as on the phone web app. */
export const DONE_SHOWN = 5;

export function inboxView({ sessions, collections, profiles, seen, respond, now, showAllDone = false }: {
  sessions: readonly Session[]; collections: readonly Collection[]; profiles: readonly Profile[]; seen: Seen; respond: boolean; now: number; showAllDone?: boolean;
}): InboxView {
  const inbox = buildInbox(sessions, seen);
  const pinned = sessions.filter(session => pinnedAt(session) > 0).sort((a, b) => pinnedAt(a) - pinnedAt(b)).map(session => session.conversationId || session.id);
  const workspace = (session: Session) => collections.find(value => value.id === session.collectionId)?.name || '';
  const row = (attention: Attention): InboxRow => ({
    key: attention.key, sessionId: attention.session.id, title: conversationName(attention.session),
    meta: [workspace(attention.session), conversationDetails(attention.session, profiles, false)].filter(Boolean).join(' · '),
    state: attention.state, unread: attention.unread && attention.state !== 'needs-input', pinned: pinned.includes(attention.key),
    age: ageLabel(attention.at, now), reason: attention.reason, line: attention.line, plan: rowPlan(attention, respond),
    stoppable: respond && attention.state === 'working' && !attention.session.closed, attention,
  });
  const sections = phoneSections(inbox, pinned).map(section => {
    const limit = section.state === 'idle' && !showAllDone ? DONE_SHOWN : section.rows.length;
    return { id: section.id, title: section.title, state: section.state, count: section.rows.length, rows: section.rows.slice(0, limit).map(row), hidden: Math.max(0, section.rows.length - limit) };
  });
  const needsYou = inbox.counts['needs-input'];
  const toLook = inbox.counts.failed + inbox.counts.review;
  const summary = needsYou ? `${needsYou} waiting for you` : toLook ? `${toLook} to look at` : inbox.rows.length ? 'Nothing needs you' : '';
  return { sections, summary, needsYou, empty: inbox.rows.length === 0 };
}
