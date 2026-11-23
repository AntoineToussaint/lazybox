import type { ItemLink } from '@lazybox/protocol';

/**
 * The pull requests workspaces deliver (GITHUB.md "Tracked pull requests"),
 * the pure half: where each stands, what needs the person first, and the
 * words for it. The Runner tracks them and emits each transition once; this
 * only arranges what it last read. Shared by the header's pills, the
 * sidebar's badge, the Pull requests panel, the palette and the Inbox.
 */

/** Where a pull request stands, most urgent first. */
export type PullStatus = 'checks-failed' | 'changes-requested' | 'conflicts' | 'ready' | 'checks-running' | 'waiting' | 'draft' | 'merged' | 'closed' | 'unread';
export const statusOrder: readonly PullStatus[] = ['checks-failed', 'changes-requested', 'conflicts', 'ready', 'checks-running', 'waiting', 'draft', 'unread', 'merged', 'closed'];
/** What the person can act on: the Inbox's rows and the panel's first group. */
export const needsYou: ReadonlySet<PullStatus> = new Set(['checks-failed', 'changes-requested', 'conflicts', 'ready']);

export const statusLabel: Record<PullStatus, string> = {
  'checks-failed': 'Checks failed', 'changes-requested': 'Changes requested', conflicts: 'Conflicts', ready: 'Ready to merge',
  'checks-running': 'Checks running', waiting: 'Waiting for review', draft: 'Draft', merged: 'Merged', closed: 'Closed', unread: 'Not read yet',
};
/** The dot's tone, shared with the sidebar's pull request chip (workspace-git.css). */
export type PullTone = 'failure' | 'changes' | 'success' | 'pending' | 'none' | 'draft' | 'merged' | 'closed';
export const statusTone: Record<PullStatus, PullTone> = {
  'checks-failed': 'failure', 'changes-requested': 'changes', conflicts: 'changes', ready: 'success', 'checks-running': 'pending',
  waiting: 'none', draft: 'draft', merged: 'merged', closed: 'closed', unread: 'none',
};
export const statusIcon: Record<PullStatus, string> = {
  'checks-failed': 'alert', 'changes-requested': 'review', conflicts: 'alert', ready: 'check', 'checks-running': 'working',
  waiting: 'pullRequest', draft: 'pullRequest', merged: 'merge', closed: 'close', unread: 'pullRequest',
};

export function pullStatus(link: Pick<ItemLink, 'item'>): PullStatus {
  const item = link.item, pr = item?.pullRequest, state = item?.state?.category;
  if (!item || !pr || !state) return 'unread';
  if (state === 'merged') return 'merged';
  if (state === 'closed' || state === 'done') return 'closed';
  if (pr.checks === 'failure') return 'checks-failed';
  if (pr.review === 'changes_requested') return 'changes-requested';
  if (pr.mergeableState === 'dirty') return 'conflicts';
  if (pr.draft || state === 'started') return 'draft';
  if (pr.checks === 'pending') return 'checks-running';
  const green = pr.checks === 'success' || pr.checks === 'none';
  if (green && pr.review !== 'review_required' && (pr.mergeable || pr.mergeableState === 'clean' || pr.mergeableState === 'unstable' || pr.mergeableState === 'has_hooks')) return 'ready';
  return 'waiting';
}

/** owner/name#812 → { repository, number }. */
export function refParts(key: string): { repository: string; number: number } {
  const [repository = '', number = ''] = key.split('#');
  return { repository, number: Number(number) || 0 };
}

export type TrackedPull = {
  /** Stable per workspace and pull request. */
  key: string;
  link: ItemLink;
  status: PullStatus;
  /** The dot: the status's tone, except that one waiting for review shows its checks (green once they passed). */
  tone: PullTone;
  repository: string;
  number: number;
  title: string;
  url: string;
  /** "lint, unit-api (2 of 14)" for failing checks; "Review required · checks passed" for one waiting; else the status in words. */
  detail: string;
  /** When it changed last, for "Merged this week" and ages (Unix ms). */
  at: number;
};

const millis = (value: bigint | number | undefined) => Number(value || 0);

export function failingDetail(link: ItemLink): string {
  const pr = link.item?.pullRequest;
  const names = (pr?.failingChecks || []).map(check => check.name);
  if (!names.length) return '';
  const shown = names.slice(0, 3).join(', ') + (names.length > 3 ? ', …' : '');
  return `${shown} (${names.length} of ${pr?.checksTotal || names.length})`;
}

/** An open pull request waiting on others, in words: whose turn it is, and its checks. */
export function waitingDetail(link: ItemLink): string {
  const pr = link.item?.pullRequest;
  const review = pr?.review === 'review_required' ? 'Review required' : pr?.review === 'approved' ? 'Approved' : statusLabel.waiting;
  return pr?.checks === 'success' ? `${review} · checks passed` : review;
}

export function trackedPull(link: ItemLink): TrackedPull {
  const key = link.item?.ref?.key || '';
  const { repository, number } = refParts(key);
  const status = pullStatus(link);
  const detail = status === 'checks-failed' ? failingDetail(link) || statusLabel[status] : status === 'waiting' ? waitingDetail(link) : statusLabel[status];
  const tone = status === 'waiting' && link.item?.pullRequest?.checks === 'success' ? 'success' : statusTone[status];
  // An ended pull request is as old as its end; an open one, its last change.
  const closed = millis(link.item?.pullRequest?.closedAtUnixMillis);
  const at = (status === 'merged' || status === 'closed') && closed ? closed : Math.max(millis(link.item?.updatedAtUnixMillis), millis(link.linkedAtUnixMillis));
  return { key: `${link.collectionId}:${key}`, link, status, tone, repository: link.item?.repository || repository, number, title: link.item?.title || '', url: link.item?.url || (repository ? `https://github.com/${repository}/pull/${number}` : ''), detail, at };
}

const rank = (pull: TrackedPull) => statusOrder.indexOf(pull.status);
const byUrgency = (a: TrackedPull, b: TrackedPull) => rank(a) - rank(b) || b.at - a.at || (a.key < b.key ? -1 : 1);

/** The pull requests delivered (or started from), not mentions. */
export function deliveredPulls(links: readonly ItemLink[]): TrackedPull[] {
  return links.filter(link => link.role !== 'mentions' && link.role !== 'origin' && (!link.item?.kind || link.item.kind === 'pull_request')).map(trackedPull);
}

const WEEK = 7 * 24 * 3600_000;
export type PullGroups = { needsYou: TrackedPull[]; waiting: TrackedPull[]; merged: TrackedPull[]; mentioned: ItemLink[] };
/** The Pull requests panel: needs you, waiting on others, merged this week; closed ones are left out. */
export function groupPulls(links: readonly ItemLink[], now: number): PullGroups {
  const pulls = deliveredPulls(links).sort(byUrgency);
  return {
    needsYou: pulls.filter(pull => needsYou.has(pull.status) && !pull.link.collectionDeleted),
    waiting: pulls.filter(pull => !needsYou.has(pull.status) && pull.status !== 'merged' && pull.status !== 'closed' && !pull.link.collectionDeleted),
    merged: pulls.filter(pull => pull.status === 'merged' && now - pull.at < WEEK).sort((a, b) => b.at - a.at),
    mentioned: links.filter(link => link.role === 'mentions' && !link.collectionDeleted),
  };
}

/** A workspace's pull requests for its header: open ones by urgency, then the ones merged today. */
export function workspacePulls(links: readonly ItemLink[], workspaceId: string, now: number): TrackedPull[] {
  return deliveredPulls(links.filter(link => link.collectionId === workspaceId && !link.collectionDeleted))
    .filter(pull => pull.status !== 'closed' && (pull.status !== 'merged' || now - pull.at < 24 * 3600_000))
    .sort(byUrgency);
}

/** The sidebar row's one badge: its most urgent pull request, and how many others it has. */
export type RowPull = { pull: TrackedPull; tone: PullTone; text: string; more: number; label: string };
export function rowPull(links: readonly ItemLink[], workspaceId: string, now: number): RowPull | undefined {
  const pulls = workspacePulls(links, workspaceId, now);
  const first = pulls[0];
  if (!first) return;
  const more = pulls.length - 1;
  const words = first.status === 'waiting' || first.detail.toLowerCase() === statusLabel[first.status].toLowerCase() ? pullWords(first) : `${statusLabel[first.status].toLowerCase()}: ${first.detail}`;
  const label = `Pull request #${first.number} · ${words}${first.title ? ` — ${first.title}` : ''}${more ? `, and ${more} more` : ''}`;
  return { pull: first, tone: first.tone, text: `#${first.number}`, more, label };
}

/** How a pull request reads in a label (a pill's, a badge's): its detail when waiting, else its status. */
export function pullWords(pull: Pick<TrackedPull, 'status' | 'detail'>): string {
  return (pull.status === 'waiting' ? pull.detail.replace(' · ', ', ') : statusLabel[pull.status]).toLowerCase();
}

/** An Inbox row: a pull request that needs the person, with its prepared action. */
export type PullInboxRow = TrackedPull & {
  /** What dismissing it hides: this status on this head. A new failure or review brings it back. */
  dismissKey: string;
  action: 'send' | 'review' | 'merge' | 'open';
  line: string;
};
export function pullReactionKey(pull: TrackedPull): string {
  const pr = pull.link.item?.pullRequest;
  const base = `${pull.key}:${pull.status}:${pr?.headSha || ''}`;
  if (pr?.observationId) return JSON.stringify([base, pull.link.sessionId, pr.observationId]);
  if (pull.status === 'checks-failed' && pr?.failingChecks?.length) {
    return base + ':' + JSON.stringify(pr.failingChecks.map(check => JSON.stringify([check.name, check.url])).sort());
  }
  if (pull.status === 'changes-requested') return base + ':' + (pull.link.item?.updatedAtUnixMillis || 0n);
  return base;
}
export function pullInboxRows(links: readonly ItemLink[], dismissed: ReadonlySet<string>): PullInboxRow[] {
  const rows: PullInboxRow[] = [];
  for (const pull of deliveredPulls(links)) {
    if (!needsYou.has(pull.status) || pull.link.collectionDeleted) continue;
    const dismissKey = pullReactionKey(pull);
    if (dismissed.has(dismissKey)) continue;
    const action = pull.status === 'checks-failed' ? 'send' : pull.status === 'changes-requested' ? 'review' : pull.status === 'ready' ? 'merge' : 'open';
    const what = pull.status === 'checks-failed' ? `checks failed · ${pull.detail}` : pull.status === 'ready' ? 'approved · checks green' : statusLabel[pull.status].toLowerCase();
    rows.push({ ...pull, dismissKey, action, line: `${pull.repository}#${pull.number} ${what}` });
  }
  return rows.sort(byUrgency);
}

/** Dismissals are kept per window; a long list forgets the oldest. */
export function parseDismissed(text: string | null): string[] {
  try {
    const value = text ? JSON.parse(text) : [];
    return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === 'string').slice(-500) : [];
  } catch { return []; }
}

/** "Send the failures to Claude": the agent the delivering conversation runs. */
export function agentName(provider: string | undefined): string {
  return provider === 'codex' ? 'Codex' : provider === 'claude' ? 'Claude' : 'the agent';
}

/** The confirmation a write asks before it runs. */
export function confirmation(pull: TrackedPull, action: 'send' | 'review' | 'merge', agent: string): string {
  if (action === 'send') return `Send the failing checks of #${pull.number} (${pull.detail}) to ${agent}?`;
  if (action === 'review') return `Import the review of #${pull.number} and send it to ${agent}?`;
  return `Merge #${pull.number} into ${pull.link.item?.pullRequest?.base || 'its base'}?`;
}
