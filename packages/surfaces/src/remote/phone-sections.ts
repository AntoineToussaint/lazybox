import { attentionTitle, type Attention, type AttentionState, type Inbox } from '../attention';
import { isArchivedAway } from '../agents-model';

export type PhoneSection = { id: string; title: string; state?: AttentionState; rows: Attention[] };

/**
 * Pure: a phone Inbox's sections (the web phone and the iPhone app alike).
 * Pinned conversations stay together below what needs the person: a pin
 * never hides a request. Archived ones (on the computer) are not listed,
 * unless one waits for the person. `pinned` is in pin order (conversation keys).
 */
export function phoneSections(inbox: Pick<Inbox, 'rows' | 'groups'>, pinned: readonly string[]): PhoneSection[] {
  const shown = (row: Attention) => !isArchivedAway(row.session);
  const pinnedRows = inbox.rows.filter(row => shown(row) && row.state !== 'needs-input' && pinned.includes(row.key)).sort((a, b) => pinned.indexOf(a.key) - pinned.indexOf(b.key));
  const out: PhoneSection[] = [];
  for (const group of inbox.groups) {
    const rows = group.rows.filter(row => shown(row) && (group.state === 'needs-input' || !pinned.includes(row.key)));
    if (rows.length) out.push({ id: group.state, title: attentionTitle[group.state], state: group.state, rows });
    if (group.state === 'needs-input' && pinnedRows.length) out.push({ id: 'pinned', title: 'Pinned', rows: pinnedRows });
  }
  if (pinnedRows.length && !out.some(section => section.id === 'pinned')) out.unshift({ id: 'pinned', title: 'Pinned', rows: pinnedRows });
  return out;
}
