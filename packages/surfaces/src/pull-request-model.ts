import type { PullRequestCheck, PullRequestStatus, RepositoryChanges } from '@lazybox/protocol';

/** The PR loop's pure decisions: default drafts, what blocks a merge, and
 * the text handed to an agent. Rendering lives in PullRequestFlow.tsx. */

export type CheckState = 'failed' | 'running' | 'passed' | 'skipped';
export function checkState(check: PullRequestCheck): CheckState {
  if (['failure', 'timed_out', 'action_required', 'startup_failure'].includes(check.conclusion)) return 'failed';
  if (check.status !== 'completed') return 'running';
  if (['success', 'neutral'].includes(check.conclusion)) return 'passed';
  return 'skipped';
}
export const failingChecks = (pr: PullRequestStatus) => pr.checks.filter(check => checkState(check) === 'failed');

export function checksSummary(pr: PullRequestStatus): string {
  if (!pr.checks.length) return 'No checks';
  const count = (state: CheckState) => pr.checks.filter(check => checkState(check) === state).length;
  const parts = [[count('failed'), 'failing'], [count('running'), 'running'], [count('passed'), 'passed'], [count('skipped'), 'skipped']] as const;
  return parts.filter(([n]) => n > 0).map(([n, label]) => `${n} ${label}`).join(' · ');
}

const verb = (status: string) => status === '?' || status === 'A' ? 'Add' : status === 'D' ? 'Remove' : status === 'R' ? 'Rename' : 'Update';
const base = (path: string) => path.split('/').at(-1) || path;
const list = (names: string[]) => names.length < 2 ? names.join('') : `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;

/** A commit subject from the conversation that did the work, else from the files. */
export function defaultCommitMessage(repo: Pick<RepositoryChanges, 'member' | 'files'>, conversationTitle = ''): string {
  const title = conversationTitle.trim();
  if (title && !/^(untitled|new) conversation$/i.test(title)) return title.length > 72 ? title.slice(0, 71) + '…' : title;
  const files = repo.files;
  if (!files.length) return '';
  const verbs = new Set(files.map(file => verb(file.indexStatus === ' ' ? file.worktreeStatus : file.indexStatus)));
  const action = verbs.size === 1 ? [...verbs][0] : 'Update';
  return files.length <= 3 ? `${action} ${list(files.map(file => base(file.path)))}` : `${action} ${files.length} files in ${repo.member}`;
}

const humanize = (branch: string) => {
  const words = branch.split('/').at(-1)!.replace(/[-_]+/g, ' ').trim();
  return words ? words[0].toUpperCase() + words.slice(1) : branch;
};

/** A title and body from the branch's commits (oldest first). */
export function defaultPullRequest(repo: Pick<RepositoryChanges, 'branch' | 'commits'>, conversationTitle = ''): { title: string; body: string } {
  const commits = [...repo.commits].reverse();
  const title = commits.length === 1 ? commits[0].subject : conversationTitle.trim() || humanize(repo.branch);
  const body = commits.length ? `## Changes\n\n${commits.map(commit => `- ${commit.subject}`).join('\n')}\n` : '';
  return { title, body };
}

/** Why Merge is not offered yet; empty when GitHub reports the PR mergeable and green. */
export function mergeBlocker(pr: PullRequestStatus): string {
  if (pr.state !== 'open') return pr.state === 'merged' ? 'Already merged' : 'This pull request is closed';
  if (pr.draft) return 'Draft — mark it ready for review on GitHub first';
  if (pr.checksState === 'failure') return 'Checks are failing';
  if (pr.checksState === 'pending') return 'Waiting for checks to finish';
  if (pr.reviewDecision === 'changes_requested') return 'A reviewer requested changes';
  switch (pr.mergeableState) {
    case 'clean': case 'unstable': case 'has_hooks': return '';
    case 'dirty': return 'Conflicts with the base branch';
    case 'behind': return 'Behind the base branch; update it first';
    case 'blocked': return pr.reviewDecision === 'review_required' ? 'Waiting for a required review' : 'Blocked by branch protection';
    case 'draft': return 'Draft — mark it ready for review on GitHub first';
    default: return 'GitHub is still checking mergeability';
  }
}

/** What the person most likely does next, so that one action is primary. */
export type Step = 'commit' | 'push' | 'create' | 'merge' | 'fix' | 'none';
export function nextStep(repo: Pick<RepositoryChanges, 'files' | 'ahead' | 'upstream' | 'commits' | 'githubRepository'>, pr?: PullRequestStatus): Step {
  if (repo.files.length) return 'commit';
  if (repo.ahead > 0) return 'push';
  if (repo.githubRepository && pr && !pr.unavailable && !(pr.found && pr.state === 'open') && pr.state !== 'merged' && repo.upstream && repo.commits.length) return 'create';
  if (pr?.found && pr.state === 'open') return failingChecks(pr).length ? 'fix' : mergeBlocker(pr) ? 'none' : 'merge';
  return 'none';
}

/** The message handed to the agent when CI fails: names, results and log links. */
export function failingChecksMessage(member: string, pr: PullRequestStatus): string {
  const lines = failingChecks(pr).map(check => `- ${check.name} (${check.conclusion.replace('_', ' ')})${check.url ? `: ${check.url}` : ''}${check.summary ? ` — ${check.summary}` : ''}`);
  return `CI is failing on pull request #${pr.number} (${pr.githubRepository}, branch ${pr.branch}, repository "${member}" in this workspace). Read the logs, fix the cause, and tell me when it is ready to push:\n${lines.join('\n')}`;
}

export const DRAFT_TAGS = { commit: 'lazybox-commit', pr: 'lazybox-pr' } as const;
export type DraftKind = keyof typeof DRAFT_TAGS;

/** The request asking the conversation's agent to write a commit message or PR description. */
export function draftRequest(kind: DraftKind, member: string, subjects: string[], nonce: string): string {
  const tag = DRAFT_TAGS[kind];
  const shown = subjects.slice(0, 20).join(', ') + (subjects.length > 20 ? `, and ${subjects.length - 20} more` : '');
  return kind === 'commit'
    ? `Write a commit message for the uncommitted changes in the "${member}" repository of this workspace (${shown}). Do not run git or change any file. Reply with only the message in a \`\`\`${tag} fenced block: a subject line of at most 72 characters, a blank line, then a short body if it helps. [draft ${nonce}]`
    : `Write the pull request for the "${member}" repository of this workspace (commits: ${shown}). Do not run git, push or change any file. Reply with only a \`\`\`${tag} fenced block: the title on the first line, a blank line, then a Markdown description of what changed and why, and how it was tested. [draft ${nonce}]`;
}

/** The last complete fenced block of that kind in an agent's reply. */
export function extractDraft(text: string, kind: DraftKind): string | undefined {
  const pattern = new RegExp('```' + DRAFT_TAGS[kind] + '[^\\n]*\\n([\\s\\S]*?)\\n?```', 'g');
  let found: string | undefined;
  for (const match of text.matchAll(pattern)) found = match[1].trim();
  return found || undefined;
}

/** Splits a PR draft into its title (first line) and body. */
export function splitPullRequestDraft(draft: string): { title: string; body: string } {
  const [title, ...rest] = draft.split('\n');
  return { title: title.replace(/^#+\s*/, '').trim(), body: rest.join('\n').trim() };
}

/** Changed files' paths to commit, renames included with their original path. */
export function commitPaths(files: { path: string; originalPath: string }[]): string[] {
  return [...new Set(files.flatMap(file => file.originalPath ? [file.path, file.originalPath] : [file.path]))];
}
