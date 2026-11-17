import type { Profile, RepositoryGitSummary, Session, WorkspaceGitSummary } from '@lazybox/protocol';
import { conversationDetails, conversationName } from './conversation-model';
import type { RowPull } from './pull-requests-model';

/**
 * A workspace row's git line and agent line, the pure half (WorkspaceRow.tsx
 * renders them). The git line answers, at a glance and without opening the
 * workspace: which branch, is there uncommitted work, is it pushed, does it
 * need a rebase, and how is its pull request doing. Every number has words in
 * the tooltip and for screen readers.
 */

export type GitBadgeKind = 'dirty' | 'ahead' | 'behind' | 'behind-default';
export type GitBadge = { kind: GitBadgeKind; count: number; text: string; label: string };
export type PullTone = 'success' | 'failure' | 'changes' | 'pending' | 'none' | 'draft' | 'merged' | 'closed';
export type PullBadge = { member: string; number: number; url: string; tone: PullTone; text: string; label: string };
export type RowGit = {
  /** The branch every repository is on, or the first one's. */
  branch: string;
  /** "+1" when the repositories are on different branches (the tooltip names them). */
  more: string;
  badges: GitBadge[];
  pulls: PullBadge[];
  /** Per repository, one line each: the tooltip. */
  title: string;
  /** The whole line in words, for screen readers. */
  label: string;
  /** Some repository could not be read. */
  problem: string;
};

const plural = (n: number, one: string, many = one + 's') => `${n} ${n === 1 ? one : many}`;
const shortRef = (ref: string) => ref.replace(/^origin\//, '');

export function pullTone(pr: NonNullable<RepositoryGitSummary['pullRequest']>): PullTone {
  if (pr.state === 'merged') return 'merged';
  if (pr.state === 'closed') return 'closed';
  if (pr.draft) return 'draft';
  if (pr.checksState === 'failure') return 'failure';
  if (pr.checksState === 'pending') return 'pending';
  if (pr.checksState === 'success') return 'success';
  return 'none';
}
const toneWords: Record<PullTone, string> = { success: 'checks passed', failure: 'checks failing', changes: 'changes requested', pending: 'checks running', none: 'no checks', draft: 'draft', merged: 'merged', closed: 'closed' };

function pullBadge(repo: RepositoryGitSummary): PullBadge | undefined {
  const pr = repo.pullRequest;
  if (!pr || !pr.number) return;
  const tone = pullTone(pr);
  const review = pr.reviewDecision === 'changes_requested' ? ' · changes requested' : pr.reviewDecision === 'approved' ? ' · approved' : '';
  return { member: repo.member, number: Number(pr.number), url: pr.url, tone, text: `#${pr.number}`, label: `Pull request #${pr.number} · ${toneWords[tone]}${review}${pr.title ? ` — ${pr.title}` : ''}` };
}

/** One repository in words: "api · feature/x · 2 uncommitted · 1 not pushed · 3 behind main · PR #12 checks failing". */
export function repositoryLine(repo: RepositoryGitSummary): string {
  if (repo.error) return `${repo.member} · unavailable: ${repo.error}`;
  const parts = [repo.member, repo.branch || `detached at ${repo.head.slice(0, 8)}`];
  if (repo.dirty) parts.push(`${repo.dirtyTruncated ? 'over ' : ''}${repo.dirty} uncommitted`);
  if (repo.ahead) parts.push(repo.upstream ? `${repo.ahead} not pushed` : `${plural(repo.ahead, 'commit')}, never pushed`);
  else if (repo.branch && !repo.upstream) parts.push('not pushed');
  if (repo.behind) parts.push(`${repo.behind} behind ${repo.upstream}`);
  if (repo.behindDefault && repo.defaultBranch) parts.push(`${repo.behindDefault} behind ${shortRef(repo.defaultBranch)}`);
  const pr = pullBadge(repo);
  if (pr) parts.push(pr.label.replace(/ — .*$/, ''));
  return parts.join(' · ');
}

/**
 * The git line for a workspace, or undefined before the Runner answered.
 * tracked is the workspace's most urgent tracked pull request (checks
 * failed, then changes requested, then ready to merge, …): it is the row's
 * one pull request badge, whatever branch it is on; without it, each
 * repository's branch pull request shows, as before tracking.
 */
export function rowGit(summary: WorkspaceGitSummary | undefined, tracked?: RowPull): RowGit | undefined {
  if (!summary) return;
  const repos = summary.repositories;
  const readable = repos.filter(repo => !repo.error);
  const problem = repos.filter(repo => repo.error).map(repo => `${repo.member}: ${repo.error}`).join('\n');
  const branches = [...new Set(readable.map(repo => repo.branch || 'detached'))];
  const sum = (pick: (repo: RepositoryGitSummary) => number) => readable.reduce((total, repo) => total + pick(repo), 0);
  const dirty = sum(repo => repo.dirty), ahead = sum(repo => repo.ahead), behind = sum(repo => repo.behind), behindDefault = sum(repo => repo.behindDefault);
  const defaults = [...new Set(readable.filter(repo => repo.behindDefault).map(repo => shortRef(repo.defaultBranch)))];
  const badges: GitBadge[] = [];
  if (dirty) badges.push({ kind: 'dirty', count: dirty, text: String(dirty), label: `${plural(dirty, 'uncommitted file')}` });
  if (ahead) badges.push({ kind: 'ahead', count: ahead, text: `↑${ahead}`, label: `${plural(ahead, 'commit')} not pushed` });
  if (behind) badges.push({ kind: 'behind', count: behind, text: `↓${behind}`, label: `${plural(behind, 'pushed commit')} not pulled` });
  if (behindDefault) badges.push({ kind: 'behind-default', count: behindDefault, text: `⇣${behindDefault}`, label: `${plural(behindDefault, 'commit')} behind ${defaults.join(', ') || 'the default branch'}` });
  const pulls = tracked
    ? [{ member: tracked.pull.link.member, number: tracked.pull.number, url: tracked.pull.url, tone: tracked.tone, text: tracked.text, label: tracked.label }, ...(tracked.more ? [{ member: '', number: 0, url: '', tone: 'none' as const, text: `+${tracked.more}`, label: '' }] : [])]
    : readable.map(pullBadge).filter((value): value is PullBadge => !!value);
  const branch = branches[0] || '';
  const more = branches.length > 1 ? `+${branches.length - 1}` : '';
  const title = repos.map(repositoryLine).join('\n');
  const words = [
    branch ? (branches.length > 1 ? `Branches ${branches.join(', ')}` : `Branch ${branch}`) : '',
    ...badges.map(badge => badge.label),
    ...pulls.map(pull => pull.label),
    !readable.length && repos.length ? 'Git state unavailable' : '',
  ].filter(Boolean);
  return { branch, more, badges, pulls, title, label: words.join(', ') || 'No repositories', problem };
}

/** The conversation a row speaks for: one that needs the person, else one at work, else the newest. */
export function featuredConversation(sessions: readonly Session[]): Session | undefined {
  const rank = (session: Session) => {
    if (session.closed) return 3;
    if (session.status === 'waiting_approval' || session.status === 'waiting_input' || session.fault) return 0;
    if (session.busy || session.status === 'running' || session.status === 'starting') return 1;
    return 2;
  };
  return [...sessions].sort((a, b) => rank(a) - rank(b) || (a.createdAtUnixMillis === b.createdAtUnixMillis ? 0 : a.createdAtUnixMillis > b.createdAtUnixMillis ? -1 : 1))[0];
}

/** What the agent line says: the conversation's title (its task), never "Claude · model". Those are the tooltip. */
export function agentLine(session: Session, profiles: readonly Profile[], others = 0): { title: string; more: string; tooltip: string } {
  const title = conversationName(session);
  const more = others > 0 ? `+${others}` : '';
  const tooltip = [title, conversationDetails(session, profiles), others > 0 ? `${plural(others, 'other conversation')} in this workspace` : ''].filter(Boolean).join('\n');
  return { title, more, tooltip };
}
