import type { AgentActivity, Session } from '@lazybox/protocol';
import { activityAt, attentionTitle, type Attention, type AttentionState, type Inbox } from './attention';
import { conversationName } from './conversation-model';
import { toolLabel } from './live-status';

/**
 * The Agents list, the pure half (AgentsPanel.tsx renders it): which
 * conversations it shows, in which sections, in what order, what the filter
 * keeps, and the sub-agents nested under each. Pins and archive marks are the
 * Runner's (Session.pinned_at / archived_at), so every window and a paired
 * phone show the same ones.
 */

const millis = (value: bigint | number | undefined) => { const n = Number(value || 0); return Number.isFinite(n) && n > 0 ? n : 0; };
export const pinnedAt = (session: Session) => millis(session.pinnedAtUnixMillis);
export const archivedAt = (session: Session) => millis(session.archivedAtUnixMillis);
export const isPinned = (session: Session) => pinnedAt(session) > 0;
/** Archived and not waiting for the person: an agent blocked on an answer is never out of sight. */
export const isArchivedAway = (session: Session) => archivedAt(session) > 0 && !session.pending.length && session.status !== 'waiting_input' && session.status !== 'waiting_approval';

/** By Runner only when the window shows several (MULTI_RUNNER.md). */
export type Grouping = 'priority' | 'workspace' | 'runner' | 'recent';
export type AgentSection = { id: string; title: string; state?: AttentionState; rows: Attention[] };

/** What the filter matches: the title, the workspace, what the row says, the model. */
export function matchesFilter(row: Attention, workspace: string, filter: string): boolean {
  const words = filter.toLocaleLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return true;
  const text = [conversationName(row.session), workspace, row.reason, row.line, attentionTitle[row.state], row.session.model, row.session.provider, ...row.session.agents.filter(agent => agent.parentId).map(agent => agent.name)].join(' ').toLocaleLowerCase();
  return words.every(word => text.includes(word));
}

/**
 * The list's sections. Pinned conversations come first, oldest pin first, so
 * a pin stays where it was put. By priority, the rest follow the Inbox's
 * order (needs input, failed, ready for review, working, done; newest first
 * within). By workspace, each workspace is a section in the order of its most
 * urgent conversation, its rows in the same urgency order.
 */
export function agentSections(inbox: Pick<Inbox, 'rows'>, grouping: Grouping, workspaceName: (row: Attention) => string, filter = '', runnerName: (row: Attention) => string = () => ''): AgentSection[] {
  const rows = inbox.rows.filter(row => !isArchivedAway(row.session) && matchesFilter(row, workspaceName(row), filter));
  const pins = rows.filter(row => isPinned(row.session)).sort((a, b) => pinnedAt(a.session) - pinnedAt(b.session) || (a.key < b.key ? -1 : 1));
  const rest = rows.filter(row => !isPinned(row.session));
  const sections: AgentSection[] = pins.length ? [{ id: 'pinned', title: 'Pinned', rows: pins }] : [];
  if (grouping === 'recent') {
    const recent = rows.map(row => ({ ...row, at: Math.max(activityAt(row.session), row.at) })).sort((a, b) => b.at - a.at);
    return recent.length ? [{ id: 'recent', title: 'Recent activity', rows: recent }] : [];
  }
  if (grouping === 'priority') {
    const states = [...new Set(rest.map(row => row.state))];
    return [...sections, ...states.map(state => ({ id: state, title: attentionTitle[state], state, rows: rest.filter(row => row.state === state) }))];
  }
  // Two Runners may have a workspace with the same id: a section is one Runner's workspace.
  const group = grouping === 'runner' ? (row: Attention) => `r:${row.runner || ''}` : (row: Attention) => row.runner ? `w:${row.runner}:${row.session.collectionId}` : `w:${row.session.collectionId}`;
  const byGroup = new Map<string, Attention[]>();
  for (const row of rest) byGroup.set(group(row), [...(byGroup.get(group(row)) || []), row]);
  return [...sections, ...[...byGroup].map(([id, list]) => ({ id, title: grouping === 'runner' ? runnerName(list[0]) : workspaceName(list[0]), rows: list }))];
}

/**
 * An agent tree in a section: the agents an orchestrator started follow it,
 * one level in, when they are in the same section (Session.lineage); an agent
 * whose orchestrator is elsewhere keeps its place and says who started it.
 */
export function nestByLineage(rows: readonly Attention[]): { row: Attention; depth: 0 | 1 }[] {
  const conversation = (row: Attention) => row.session.conversationId || row.session.id;
  const present = new Set(rows.map(conversation));
  const nested = (row: Attention) => !!row.session.lineage?.parentConversationId && present.has(row.session.lineage.parentConversationId);
  const out: { row: Attention; depth: 0 | 1 }[] = [];
  for (const row of rows) {
    if (nested(row)) continue;
    out.push({ row, depth: 0 });
    for (const child of rows) if (nested(child) && child.session.lineage!.parentConversationId === conversation(row)) out.push({ row: child, depth: 1 });
  }
  return out;
}

/** Where an agent came from, for its row: "started by Search lead · Ship search". */
export function lineageLabel(session: Session): string {
  const l = session.lineage;
  if (!l?.parentConversationId) return '';
  return ['started by ' + (l.parentTitle || 'an orchestrator'), l.missionTitle].filter(Boolean).join(' · ');
}

/** The rows in reading order, skipping folded sections: what j/k walk. */
export function visibleRows(sections: readonly AgentSection[], folded: readonly string[]): Attention[] {
  return sections.filter(section => !folded.includes(section.id)).flatMap(section => section.id === 'recent' ? section.rows : nestByLineage(section.rows).map(item => item.row));
}

export type SubagentRow = { id: string; name: string; status: string; tone: 'busy' | 'waiting'; detail: string; since: number };
const working = new Set(['starting', 'running', 'switching_profile']);
const waiting = new Set(['waiting_input', 'waiting_approval']);
/** A conversation's sub-agents that are still at work, newest first: they nest under it, one level. */
export function liveSubagents(session: Session): SubagentRow[] {
  if (session.closed) return [];
  return session.agents.filter((agent: AgentActivity) => agent.parentId && agent.id !== 'root' && (working.has(agent.status) || waiting.has(agent.status)))
    .map(agent => ({
      id: agent.id,
      name: agent.name || 'Sub-agent',
      status: agent.status,
      tone: waiting.has(agent.status) ? 'waiting' as const : 'busy' as const,
      detail: waiting.has(agent.status) ? 'Waiting' : agent.phase === 'tool' && agent.tool ? toolLabel(agent.tool) : agent.phase === 'thinking' ? 'Thinking' : 'Working',
      since: millis(agent.phaseSinceUnixMillis) || millis(agent.turnStartedAtUnixMillis) || millis(agent.updatedAtUnixMillis),
    }))
    .sort((a, b) => b.since - a.since || (a.id < b.id ? -1 : 1));
}

/** One line for a row's meta: "Alpha feature · Question · Which version?". */
export function metaLine(row: Attention, workspace: string, withWorkspace: boolean): string {
  const what = row.state === 'needs-input' || row.state === 'failed' ? [row.reason, row.line].filter(Boolean).join(' · ') : row.reason;
  return [withWorkspace ? workspace : '', what].filter(Boolean).join(' · ');
}
