import { create } from '@bufbuild/protobuf';
import { ItemLinkSchema, RepositoryGitSummarySchema, WorkspaceGitSummarySchema, type ItemLink } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { confirmation, groupPulls, parseDismissed, pullInboxRows, pullStatus, rowPull, trackedPull, workspacePulls } from '../packages/surfaces/src/pull-requests-model.ts';
import { rowGit } from '../packages/surfaces/src/workspace-row-model.ts';

const now = Date.UTC(2026, 10, 17, 12);
type Shape = { key?: string; workspace?: string; role?: string; state?: string; checks?: string; review?: string; mergeable?: boolean; mergeableState?: string; draft?: boolean; failing?: string[]; total?: number; closedAt?: number; deleted?: boolean; head?: string; sha?: string; title?: string };
const link = (s: Shape = {}): ItemLink => create(ItemLinkSchema, {
  collectionId: s.workspace ?? 'w1', collectionName: 'Payments', collectionDeleted: !!s.deleted, role: s.role ?? 'delivers', source: 'agent-output', sessionId: 's1', member: 'api',
  linkedAtUnixMillis: BigInt(now - 3600_000),
  item: s.state === undefined && s.checks === undefined ? { ref: { provider: 'github', key: s.key ?? 'acme/api#812' }, kind: 'pull_request' } : {
    ref: { provider: 'github', key: s.key ?? 'acme/api#812' }, kind: 'pull_request', title: s.title ?? 'Fix refund rounding', url: `https://github.com/${(s.key ?? 'acme/api#812').replace('#', '/pull/')}`, repository: 'acme/api',
    state: { category: s.state ?? 'review', label: 'Open' }, updatedAtUnixMillis: BigInt(now - 600_000),
    pullRequest: { head: s.head ?? 'fix', base: 'main', headSha: s.sha ?? 'abc', draft: !!s.draft, checks: s.checks ?? 'pending', review: s.review ?? '', mergeable: !!s.mergeable, mergeableState: s.mergeableState ?? '',
      failingChecks: (s.failing || []).map(name => ({ name, url: `https://github.com/acme/api/actions/runs/1/job/${name}` })), checksTotal: s.total ?? (s.failing?.length || 0), closedAtUnixMillis: BigInt(s.closedAt ?? 0) },
  },
} as never);

test('a pull request\'s status, most urgent first', () => {
  assert.equal(pullStatus(link()), 'unread');
  assert.equal(pullStatus(link({ checks: 'failure', review: 'changes_requested', failing: ['lint'] })), 'checks-failed');
  assert.equal(pullStatus(link({ checks: 'success', review: 'changes_requested' })), 'changes-requested');
  assert.equal(pullStatus(link({ checks: 'success', mergeableState: 'dirty' })), 'conflicts');
  assert.equal(pullStatus(link({ checks: 'success', review: 'approved', mergeable: true, mergeableState: 'clean' })), 'ready');
  assert.equal(pullStatus(link({ checks: 'none', mergeable: true, mergeableState: 'clean' })), 'ready', 'no checks and no review asked for: ready');
  assert.equal(pullStatus(link({ checks: 'success', review: 'review_required', mergeable: true, mergeableState: 'blocked' })), 'waiting');
  // Regression: GitHub calls a branch behind its base mergeable (no conflicts), but a repository
  // that requires up-to-date branches refuses to merge it. It needs an update, not a Merge button.
  assert.equal(pullStatus(link({ checks: 'success', review: 'approved', mergeable: true, mergeableState: 'behind' })), 'behind');
  assert.equal(pullStatus(link({ checks: 'success', review: 'review_required', mergeable: true, mergeableState: 'behind' })), 'waiting', 'a review it still needs comes first');
  assert.equal(pullStatus(link({ checks: 'failure', review: 'approved', mergeable: true, mergeableState: 'behind' })), 'checks-failed');
  assert.equal(pullStatus(link({ checks: 'pending' })), 'checks-running');
  assert.equal(pullStatus(link({ state: 'started', draft: true, checks: 'success' })), 'draft');
  assert.equal(pullStatus(link({ state: 'merged', checks: 'success' })), 'merged');
  assert.equal(pullStatus(link({ state: 'closed' })), 'closed');
});

test('failing checks are named with how many of how many', () => {
  const pull = trackedPull(link({ checks: 'failure', failing: ['lint', 'unit-api'], total: 14 }));
  assert.equal(pull.detail, 'lint, unit-api (2 of 14)');
  assert.equal(pull.number, 812);
  assert.equal(pull.repository, 'acme/api');
  assert.equal(confirmation(pull, 'send', 'Claude'), 'Send the failing checks of #812 (lint, unit-api (2 of 14)) to Claude?');
  assert.equal(confirmation(pull, 'merge', 'Claude'), 'Merge #812 into main?');
  const many = trackedPull(link({ checks: 'failure', failing: ['a', 'b', 'c', 'd', 'e'], total: 9 }));
  assert.equal(many.detail, 'a, b, c, … (5 of 9)');
});

test('the panel groups needs you, waiting on others and merged this week; mentions apart', () => {
  const links = [
    link({ key: 'acme/api#1', checks: 'pending' }),
    link({ key: 'acme/api#2', checks: 'success', review: 'approved', mergeable: true, mergeableState: 'clean' }),
    link({ key: 'acme/api#3', checks: 'failure', failing: ['lint'] }),
    link({ key: 'acme/api#4', state: 'merged', checks: 'success', closedAt: now - 2 * 24 * 3600_000 }),
    link({ key: 'acme/api#5', state: 'merged', checks: 'success', closedAt: now - 9 * 24 * 3600_000 }),
    link({ key: 'acme/api#6', state: 'closed' }),
    link({ key: 'acme/api#7', role: 'mentions' }),
    link({ key: 'acme/api#8', checks: 'failure', failing: ['x'], deleted: true }),
  ];
  const groups = groupPulls(links, now);
  assert.deepEqual(groups.needsYou.map(p => p.number), [3, 2], 'failing first, then ready; a deleted workspace\'s are history');
  assert.deepEqual(groups.waiting.map(p => p.number), [1]);
  assert.deepEqual(groups.merged.map(p => p.number), [4], 'merged within the week');
  assert.deepEqual(groups.mentioned.map(l => l.item?.ref?.key), ['acme/api#7']);
});

test('the header lists a workspace\'s open pull requests by urgency, and the ones merged today', () => {
  const links = [
    link({ key: 'acme/api#1', checks: 'pending' }),
    link({ key: 'acme/api#2', checks: 'failure', failing: ['lint'] }),
    link({ key: 'acme/api#3', state: 'merged', closedAt: now - 3600_000, checks: 'success' }),
    link({ key: 'acme/api#4', state: 'merged', closedAt: now - 3 * 24 * 3600_000, checks: 'success' }),
    link({ key: 'acme/api#9', workspace: 'w2', checks: 'failure', failing: ['x'] }),
  ];
  assert.deepEqual(workspacePulls(links, 'w1', now).map(p => p.number), [2, 1, 3]);
});

test('the sidebar row has one badge: the most urgent pull request, and how many more', () => {
  const links = [link({ key: 'acme/api#1', checks: 'success', review: 'changes_requested' }), link({ key: 'acme/api#2', checks: 'success', review: 'approved', mergeable: true, mergeableState: 'clean' })];
  const row = rowPull(links, 'w1', now)!;
  assert.equal(row.text, '#1');
  assert.equal(row.tone, 'changes');
  assert.equal(row.more, 1);
  assert.match(row.label, /^Pull request #1 · changes requested — Fix refund rounding, and 1 more$/);
  assert.equal(rowPull(links, 'w2', now), undefined);
  // It replaces the branch pull requests in the git line.
  const summary = create(WorkspaceGitSummarySchema, { collectionId: 'w1', repositories: [create(RepositoryGitSummarySchema, { member: 'api', branch: 'fix', pullRequest: { number: 7n, state: 'open', checksState: 'success' } } as never)] });
  const git = rowGit(summary, row)!;
  assert.deepEqual(git.pulls.map(p => [p.text, p.tone]), [['#1', 'changes'], ['+1', 'none']]);
  assert.deepEqual(rowGit(summary)!.pulls.map(p => p.text), ['#7'], 'without tracking, the branch pull request');
});

test('one waiting on a required review with green checks says so, with a green dot, and waits on others', () => {
  // acme/platform#1121 as GitHub reported it.
  const links = [link({ key: 'acme/platform#1121', checks: 'success', review: 'review_required', mergeableState: 'blocked', title: 'Retry failed uploads' })];
  const pull = trackedPull(links[0]);
  assert.deepEqual([pull.status, pull.tone, pull.detail], ['waiting', 'success', 'Review required · checks passed']);
  assert.deepEqual(groupPulls(links, now).waiting.map(p => p.number), [1121]);
  assert.equal(pullInboxRows(links, new Set()).length, 0, 'nothing for the person to do');
  const row = rowPull(links, 'w1', now)!;
  assert.equal(row.tone, 'success');
  assert.equal(row.label, 'Pull request #1121 · review required, checks passed — Retry failed uploads');
  // Still running, or no checks: the dot is not green.
  assert.equal(trackedPull(link({ checks: 'none', review: 'review_required', mergeableState: 'blocked' })).tone, 'none');
  assert.equal(trackedPull(link({ checks: 'none', review: 'review_required', mergeableState: 'blocked' })).detail, 'Review required');
});

test('inbox rows: what needs the person, with the reaction prepared; a dismissal holds until something new happens', () => {
  const failing = link({ key: 'acme/api#3', checks: 'failure', failing: ['lint'], total: 4, sha: 'abc' });
  const links = [failing, link({ key: 'acme/api#4', checks: 'success', review: 'changes_requested' }), link({ key: 'acme/api#5', checks: 'success', review: 'approved', mergeable: true, mergeableState: 'clean' }), link({ key: 'acme/api#6', checks: 'pending' })];
  const rows = pullInboxRows(links, new Set());
  assert.deepEqual(rows.map(r => [r.number, r.action]), [[3, 'send'], [4, 'review'], [5, 'merge']]);
  assert.equal(rows[0].line, 'acme/api#3 checks failed · lint (1 of 4)');
  assert.equal(rows[2].line, 'acme/api#5 approved · checks green');
  const dismissed = new Set([rows[0].dismissKey]);
  assert.deepEqual(pullInboxRows(links, dismissed).map(r => r.number), [4, 5]);
  // A new head failing again is news: the row comes back.
  const pushed = link({ key: 'acme/api#3', checks: 'failure', failing: ['lint'], total: 4, sha: 'def' });
  assert.deepEqual(pullInboxRows([pushed], dismissed).map(r => r.number), [3]);
  assert.deepEqual(parseDismissed('["a",3,"b"]'), ['a', 'b']);
  assert.deepEqual(parseDismissed('{'), []);
});

test('an approved, green pull request behind its required base offers Update branch, not Merge', () => {
  const rows = pullInboxRows([link({ key: 'codefly-dev/core#942', checks: 'success', review: 'approved', mergeable: true, mergeableState: 'behind', title: 'Solution Page' })], new Set());
  assert.equal(rows.length, 1);
  assert.equal(rows[0].action, 'update');
  assert.match(rows[0].line, /#942 checks green · out of date with main$/);
  assert.match(confirmation(rows[0], 'update', 'Claude'), /^Update #942 with main\? GitHub merges main into its branch/);
});

// The workspace card (canUpdateBranch) and the Inbox (pullStatus) decide "out of date" alike.
test('the workspace card and the Inbox agree on when a branch needs an update', async () => {
  const { canUpdateBranch } = await import('../packages/surfaces/src/pull-request-model.ts');
  for (const checks of ['success', 'none', 'pending', 'failure'])
    for (const review of ['approved', '', 'review_required', 'changes_requested'])
      for (const draft of [false, true])
        for (const mergeableState of ['behind', 'clean', 'blocked']) {
          const inbox = pullStatus(link({ checks, review, draft, mergeable: true, mergeableState, state: draft ? 'started' : 'review' })) === 'behind';
          const card = canUpdateBranch({ state: 'open', draft, mergeableState, checksState: checks, reviewDecision: review } as never);
          assert.equal(card, inbox, JSON.stringify({ checks, review, draft, mergeableState }));
        }
});
