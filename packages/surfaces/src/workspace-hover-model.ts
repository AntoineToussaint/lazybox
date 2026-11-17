import type { Collection, HostedProcess, ItemLink, LineChanges, Profile, RepositoryGitSummary, Session, TelemetryReport, WorkspaceAction, WorkspaceGitSummary } from '@lazybox/protocol';
import { activityAt, conversationKey } from './attention';
import { conversationDetails, conversationName } from './conversation-model';
import { ago } from './artifact-model';
import { refParts, statusLabel, workspacePulls, type PullTone } from './pull-requests-model';
import { pullTone } from './workspace-row-model';
import { dollars, usageBreakdown } from './usage-model';

/**
 * The workspace hover card, the pure half (WorkspaceHover.tsx renders it):
 * what a workspace is, where its repositories stand, its pull requests, its
 * agents, what waits for the person, what serves, and what it cost — from
 * what the sidebar already has, plus what the card reads when it opens
 * (line counts, running actions and processes, usage). Every part that has
 * not answered yet is `undefined`, so the card shows a skeleton in its place,
 * never a wrong number.
 */

const plural = (n: number, one: string, many = one + 's') => `${n} ${n === 1 ? one : many}`;
const millis = (value: bigint | number | undefined) => { const n = Number(value || 0); return Number.isFinite(n) && n > 0 ? n : 0; };
const shortRef = (ref: string) => ref.replace(/^origin\//, '');

/** "created 2 days ago · active 5 min ago": when it was made (else its first conversation), and its newest activity. */
export function ageLine(collection: Pick<Collection, 'createdAtUnixMillis'>, sessions: readonly Session[], now: number): string {
  const created = millis(collection.createdAtUnixMillis);
  const first = sessions.reduce((min, s) => { const at = millis(s.createdAtUnixMillis); return at && (!min || at < min) ? at : min; }, 0);
  const active = sessions.reduce((max, s) => Math.max(max, activityAt(s)), 0);
  return [
    created ? `created ${ago(created, now)}` : first ? `started ${ago(first, now)}` : '',
    active ? `active ${ago(active, now)}` : '',
  ].filter(Boolean).join(' · ');
}

export type HoverOrigin = { text: string; title: string; url: string; icon: string };
/** The task or issue the workspace started from: "#123 Fix uploads", linked when its address is known. */
export function hoverOrigin(collection: Pick<Collection, 'id' | 'origin'>, links: readonly ItemLink[]): HoverOrigin | undefined {
  const link = links.find(value => value.collectionId === collection.id && value.role === 'origin');
  const item = link?.item;
  const key = item?.ref?.key || collection.origin?.key || '';
  if (!key) return;
  const provider = item?.ref?.provider || collection.origin?.provider || '';
  const { number } = refParts(key);
  // GitHub's keys are owner/name#123: "#123" reads; another provider's key is its own name ("PAY-142").
  const label = provider === 'github' && number ? `#${number}` : key;
  const title = item?.title || '';
  return { text: title ? `${label} ${title}` : label, title: [key, title].filter(Boolean).join(' — '), url: item?.url || '', icon: item?.kind === 'pull_request' ? 'pullRequest' : provider === 'github' ? 'issue' : 'tag' };
}

export type HoverChanges = { text: string; additions: number; deletions: number; files: number; clean: boolean; floor: boolean; label: string };
/** "+120 −30 · 6 files", or "clean"; the words for screen readers. */
export function changeWords(changes: Pick<LineChanges, 'files' | 'additions' | 'deletions' | 'truncated' | 'base'>): HoverChanges {
  const files = changes.files, additions = changes.additions, deletions = changes.deletions, floor = changes.truncated;
  if (!files) return { text: 'clean', additions: 0, deletions: 0, files: 0, clean: true, floor: false, label: changes.base ? `No changes against ${shortRef(changes.base)}` : 'No changes' };
  const more = floor ? '+' : '';
  const against = changes.base ? ` against ${shortRef(changes.base)}` : '';
  return {
    text: `+${additions}${more} −${deletions}${more} · ${plural(files, 'file')}${more}`, additions, deletions, files, clean: false, floor,
    label: `${floor ? 'At least ' : ''}${plural(additions, 'line')} added, ${plural(deletions, 'line')} deleted, in ${plural(files, 'file')}${against}`,
  };
}

export type HoverRepository = {
  member: string;
  /** owner/name when it is on GitHub (or known by that name), else its folder name. */
  name: string;
  owner: string;
  short: string;
  branch: string;
  /** Against the default branch: commits only on the branch, commits only on it. */
  ahead: number;
  behind: number;
  defaultBranch: string;
  /** Commits not pushed yet. */
  unpushed: number;
  /** Uncommitted files. */
  dirty: number;
  /** undefined: not counted yet; null: this Runner cannot count them (the card says what it knows). */
  changes?: HoverChanges | null;
  error: string;
};

/**
 * The workspace's repositories in its order, each with what the sidebar's
 * poll knows and, once the card asked, its line counts. `names` are the
 * sidebar's labels (owner/name for a GitHub repository).
 */
export function hoverRepositories(members: readonly string[], names: readonly string[], summary: WorkspaceGitSummary | undefined, counted: WorkspaceGitSummary | null | undefined): HoverRepository[] {
  return members.map((member, index) => {
    const repo = counted?.repositories.find(value => value.member === member) || summary?.repositories.find(value => value.member === member);
    const name = repo?.githubRepository || names[index] || member;
    const slash = name.indexOf('/');
    const tally = counted === undefined ? undefined : counted === null ? null : countedChanges(counted.repositories.find(value => value.member === member));
    return {
      member, name, owner: slash > 0 ? name.slice(0, slash) : '', short: slash > 0 ? name.slice(slash + 1) : name,
      branch: repo ? repo.branch || (repo.head ? `detached at ${repo.head.slice(0, 8)}` : '') : '',
      ahead: repo?.aheadOfDefault || 0, behind: repo?.behindDefault || 0, defaultBranch: shortRef(repo?.defaultBranch || ''),
      unpushed: repo?.upstream ? repo.ahead : 0, dirty: repo?.dirty || 0, changes: tally, error: repo?.error || '',
    };
  });
}
const countedChanges = (repo: RepositoryGitSummary | undefined) => repo?.changes ? changeWords(repo.changes) : null;

/** "↑3 ↓1" against the default branch, and its words. Empty when level with it. */
export function aheadBehind(repo: Pick<HoverRepository, 'ahead' | 'behind' | 'defaultBranch'>): { text: string; label: string } {
  const against = repo.defaultBranch || 'the default branch';
  const parts = [repo.ahead ? `↑${repo.ahead}` : '', repo.behind ? `↓${repo.behind}` : ''].filter(Boolean);
  const words = [repo.ahead ? `${plural(repo.ahead, 'commit')} ahead of ${against}` : '', repo.behind ? `${plural(repo.behind, 'commit')} behind ${against}` : ''].filter(Boolean);
  return { text: parts.join(' '), label: words.join(', ') };
}

export type PillTone = PullTone | 'neutral';
export type HoverPill = { text: string; tone: PillTone; icon?: string };
export type HoverPull = { key: string; number: number; title: string; url: string; repository: string; tone: PullTone; pills: HoverPill[]; label: string };

type PullFacts = { state: string; draft: boolean; checks: string; failing: number; review: string; conflicts: boolean };
function pullPills(facts: PullFacts): HoverPill[] {
  if (facts.state === 'merged') return [{ text: 'Merged', tone: 'merged', icon: 'merge' }];
  if (facts.state === 'closed') return [{ text: 'Closed', tone: 'closed' }];
  const pills: HoverPill[] = [facts.draft ? { text: 'Draft', tone: 'draft' } : { text: 'Open', tone: 'neutral' }];
  if (facts.checks === 'failure') pills.push({ text: facts.failing ? `${facts.failing} failing` : 'Checks failing', tone: 'failure', icon: 'cross' });
  else if (facts.checks === 'pending') pills.push({ text: 'Checks', tone: 'pending', icon: 'pending' });
  else if (facts.checks === 'success') pills.push({ text: 'Checks', tone: 'success', icon: 'check' });
  if (facts.conflicts) pills.push({ text: 'Conflicts', tone: 'changes', icon: 'alert' });
  if (facts.review === 'approved') pills.push({ text: 'Approved', tone: 'success' });
  else if (facts.review === 'changes_requested') pills.push({ text: 'Changes requested', tone: 'changes' });
  else if (facts.review === 'review_required') pills.push({ text: 'Review required', tone: 'neutral' });
  return pills;
}
const pillWords = (pills: readonly HoverPill[]) => pills.map(pill => pill.icon === 'check' ? 'checks passed' : pill.icon === 'pending' ? 'checks running' : pill.text.toLowerCase()).join(', ');

/**
 * The workspace's pull requests: the tracked ones (open by urgency, then
 * merged today), else each repository's branch pull request as the sidebar's
 * poll last read it.
 */
export function hoverPulls(workspaceId: string, links: readonly ItemLink[], summary: WorkspaceGitSummary | undefined, now: number): HoverPull[] {
  const tracked = workspacePulls(links, workspaceId, now);
  if (tracked.length) return tracked.map(pull => {
    const pr = pull.link.item?.pullRequest, state = pull.status === 'merged' ? 'merged' : pull.status === 'closed' ? 'closed' : 'open';
    const pills = pullPills({ state, draft: pull.status === 'draft' || !!pr?.draft, checks: pr?.checks || '', failing: pr?.failingChecks.length || 0, review: pr?.review || '', conflicts: pull.status === 'conflicts' });
    return { key: pull.key, number: pull.number, title: pull.title, url: pull.url, repository: pull.repository, tone: pull.tone, pills, label: `Pull request #${pull.number}${pull.title ? ` ${pull.title}` : ''}: ${pillWords(pills)} (${statusLabel[pull.status].toLowerCase()})` };
  });
  return (summary?.repositories || []).flatMap(repo => {
    const pr = repo.pullRequest;
    if (!pr || !pr.number) return [];
    const pills = pullPills({ state: pr.state, draft: pr.draft, checks: pr.checksState, failing: 0, review: pr.reviewDecision, conflicts: false });
    const number = Number(pr.number);
    return [{ key: `${repo.member}#${number}`, number, title: pr.title, url: pr.url, repository: repo.githubRepository, tone: pullTone(pr), pills, label: `Pull request #${number}${pr.title ? ` ${pr.title}` : ''}: ${pillWords(pills)}` }];
  });
}

export type AgentState = 'needs-you' | 'failed' | 'working' | 'done';
export const agentStateWords: Record<AgentState, string> = { 'needs-you': 'Needs you', failed: 'Failed', working: 'Working', done: 'Done' };
export type HoverAgent = { id: string; key: string; title: string; detail: string; state: AgentState; waiting: number };
const agentRank: Record<AgentState, number> = { 'needs-you': 0, failed: 1, working: 2, done: 3 };

export function agentState(session: Session): AgentState {
  if (!session.closed && (session.pending.length || session.status === 'waiting_approval' || session.status === 'waiting_input')) return 'needs-you';
  if (session.fault) return 'failed';
  if (!session.closed && (session.busy || session.status === 'running' || session.status === 'starting')) return 'working';
  return 'done';
}

/** The conversations in the workspace, what needs the person first; at most `limit`, and how many more. */
export function hoverAgents(sessions: readonly Session[], profiles: readonly Profile[], limit = 4): { shown: HoverAgent[]; more: number; waiting: number } {
  const rows = sessions.map(session => {
    const state = agentState(session);
    return { session, state, at: activityAt(session), waiting: state === 'needs-you' ? Math.max(1, session.pending.length) : 0 };
  }).sort((a, b) => agentRank[a.state] - agentRank[b.state] || b.at - a.at);
  const waiting = rows.reduce((sum, row) => sum + row.waiting, 0);
  const shown = rows.slice(0, rows.length === limit + 1 ? limit + 1 : limit).map(({ session, state, waiting }) => ({
    id: session.id, key: conversationKey(session), title: conversationName(session), detail: conversationDetails(session, profiles, false), state, waiting,
  }));
  return { shown, more: rows.length - shown.length, waiting };
}

export type HoverRun = { key: string; name: string; ports: number[]; url: string; label: string };
/**
 * What serves or runs in the workspace: its running actions ("web :5173"),
 * then other commands of it that listen on a port. A command an action
 * started is listed once, under the action's name.
 */
export function hoverRuns(workspaceId: string, actions: readonly WorkspaceAction[] | undefined, processes: readonly HostedProcess[] | undefined): HoverRun[] {
  const out: HoverRun[] = [], seen = new Set<string>();
  for (const action of actions || []) {
    const state = action.runState?.state;
    if (state !== 'running' && state !== 'stopping') continue;
    if (action.runState?.commandId) seen.add(action.runState.commandId);
    const listening = processes?.find(p => p.commandId && p.commandId === action.runState?.commandId)?.ports || [];
    const ports = action.port ? [action.port, ...listening.filter(port => port !== action.port)] : [...listening];
    out.push({ key: `action:${action.key}`, name: action.name, ports, url: action.url, label: `${action.name}${ports.length ? ` on port ${ports.join(', ')}` : ''}${state === 'stopping' ? ', stopping' : ', running'}` });
  }
  for (const p of processes || []) {
    if (p.workspaceId !== workspaceId || p.kind !== 'command' || p.state !== 'running' || !p.ports.length || (p.commandId && seen.has(p.commandId))) continue;
    const name = commandName(p.label);
    out.push({ key: `process:${p.id}`, name, ports: [...p.ports], url: '', label: `${name} on port ${p.ports.join(', ')}, running` });
  }
  return out;
}
/** A command's short name: "npm run dev -- --port 5173" → "npm run dev". */
export function commandName(label: string): string {
  const words = label.trim().split(/\s+/);
  const cut = words.findIndex(word => word === '--' || word.startsWith('-') || word === '|' || word === '&&');
  const name = (cut > 0 ? words.slice(0, cut) : words).join(' ');
  return name.length > 32 ? name.slice(0, 31) + '…' : name || 'Command';
}

/** "$1.24 so far", when any usage is priced; undefined otherwise. */
export function costLine(report: Pick<TelemetryReport, 'groups'> | undefined): { text: string; label: string } | undefined {
  if (!report) return;
  const usage = usageBreakdown(report.groups);
  if (usage.nanos === undefined) return;
  const partly = usage.unpriced.length > 0;
  return { text: `${dollars(usage.nanos)} so far`, label: `${partly ? 'At least ' : ''}${dollars(usage.nanos)} in tokens so far (API-equivalent estimate${partly ? '; some usage has no price' : ''})` };
}

type Rect = { left: number; top: number; right: number; bottom: number };
type Size = { width: number; height: number };
export type HoverPlacement = { left: number; top: number; maxHeight: number; side: 'right' | 'left' | 'below' | 'above' };
/**
 * Where the card goes: beside the row, right of it (the sidebar is on the
 * left), else left of it, its top level with the row's and kept inside the
 * window. Where neither side has room (a narrow window), below the row, else
 * above it, never over it.
 */
export function placeHoverCard(row: Rect, size: Size, viewport: Size, gap = 6, margin = 8): HoverPlacement {
  const fitsHeight = Math.min(size.height, viewport.height - margin * 2);
  const top = Math.round(Math.max(margin, Math.min(row.top, viewport.height - margin - fitsHeight)));
  const maxHeight = viewport.height - margin * 2;
  if (row.right + gap + size.width <= viewport.width - margin) return { left: Math.round(row.right + gap), top, maxHeight, side: 'right' };
  if (row.left - gap - size.width >= margin) return { left: Math.round(row.left - gap - size.width), top, maxHeight, side: 'left' };
  const left = Math.round(Math.max(margin, Math.min(row.left, viewport.width - margin - size.width)));
  const below = viewport.height - margin - (row.bottom + gap), above = row.top - gap - margin;
  if (below >= size.height || below >= above) return { left, top: Math.round(row.bottom + gap), maxHeight: Math.max(0, below), side: 'below' };
  return { left, top: Math.round(Math.max(margin, row.top - gap - size.height)), maxHeight: Math.max(0, above), side: 'above' };
}
