import { create } from '@bufbuild/protobuf';
import { PullRequestStatusSchema, RepositoryChangesSchema, ChangedFileSchema, CommitSchema } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { checksSummary, commitPaths, defaultCommitMessage, defaultPullRequest, draftRequest, extractDraft, failingChecksMessage, mergeBlocker, nextStep, splitPullRequestDraft } from '../packages/surfaces/src/pull-request-model.ts';

const file = (path: string, indexStatus: string, worktreeStatus = ' ', originalPath = '') => create(ChangedFileSchema, { path, indexStatus, worktreeStatus, originalPath });
const repo = (fields: Record<string, unknown> = {}) => create(RepositoryChangesSchema, { member: 'api', branch: 'feature/steady-otter', ...fields });
const pr = (fields: Record<string, unknown> = {}) => create(PullRequestStatusSchema, { found: true, number: 12n, state: 'open', mergeableState: 'clean', checksState: 'success', githubRepository: 'team/api', branch: 'feature/steady-otter', ...fields });

test('commit drafts prefer the conversation, then describe the files', () => {
  assert.equal(defaultCommitMessage(repo({ files: [file('a.ts', 'M', 'M')] }), 'Ship the API contract'), 'Ship the API contract');
  assert.equal(defaultCommitMessage(repo({ files: [file('src/new-view.tsx', '?', '?')] })), 'Add new-view.tsx');
  assert.equal(defaultCommitMessage(repo({ files: [file('a.ts', 'M'), file('b.ts', ' ', 'M'), file('c.ts', 'M')] })), 'Update a.ts, b.ts and c.ts');
  assert.equal(defaultCommitMessage(repo({ files: ['a', 'b', 'c', 'd'].map(name => file(name, 'D')) })), 'Remove 4 files in api');
  assert.equal(defaultCommitMessage(repo({ files: [file('a', 'M')] }), 'Untitled conversation'), 'Update a');
  assert.equal(defaultCommitMessage(repo({ files: [file('a', 'M')] }), 'x'.repeat(90)).length, 72);
});

test('pull request drafts list the commits oldest first', () => {
  const commits = [create(CommitSchema, { subject: 'Second' }), create(CommitSchema, { subject: 'First' })];
  assert.deepEqual(defaultPullRequest(repo({ commits })), { title: 'Steady otter', body: '## Changes\n\n- First\n- Second\n' });
  assert.equal(defaultPullRequest(repo({ commits }), 'Ship it').title, 'Ship it');
  assert.equal(defaultPullRequest(repo({ commits: commits.slice(1) }), 'Ship it').title, 'First');
});

test('merge is offered only when GitHub says the pull request is green and mergeable', () => {
  assert.equal(mergeBlocker(pr()), '');
  assert.equal(mergeBlocker(pr({ mergeableState: 'unstable' })), '');
  assert.equal(mergeBlocker(pr({ checksState: 'failure' })), 'Checks are failing');
  assert.equal(mergeBlocker(pr({ checksState: 'pending' })), 'Waiting for checks to finish');
  assert.equal(mergeBlocker(pr({ draft: true })), 'Draft — mark it ready for review on GitHub first');
  assert.equal(mergeBlocker(pr({ mergeableState: 'blocked', reviewDecision: 'review_required' })), 'Waiting for a required review');
  assert.equal(mergeBlocker(pr({ mergeableState: 'dirty' })), 'Conflicts with the base branch');
  assert.equal(mergeBlocker(pr({ mergeableState: 'unknown' })), 'GitHub is still checking mergeability');
  assert.equal(mergeBlocker(pr({ state: 'merged' })), 'Already merged');
});

test('one next step is primary along commit, push, create, fix and merge', () => {
  const pushed = { upstream: 'origin/feature/steady-otter', githubRepository: 'team/api', commits: [create(CommitSchema, { subject: 'Add' })] };
  assert.equal(nextStep(repo({ ...pushed, files: [file('a', 'M')] })), 'commit');
  assert.equal(nextStep(repo({ ...pushed, ahead: 2 })), 'push');
  assert.equal(nextStep(repo(pushed), pr({ found: false, state: '' })), 'create');
  assert.equal(nextStep(repo(pushed), pr({ checksState: 'failure', checks: [{ name: 'test', status: 'completed', conclusion: 'failure' }] })), 'fix');
  assert.equal(nextStep(repo(pushed), pr()), 'merge');
  assert.equal(nextStep(repo(pushed), pr({ state: 'merged' })), 'none');
  assert.equal(nextStep(repo({ ...pushed, githubRepository: '' }), pr({ found: false, unavailable: 'not GitHub' })), 'none');
});

test('failing checks become an agent message with names and log links', () => {
  const status = pr({ checksState: 'failure', checks: [
    { name: 'test', status: 'completed', conclusion: 'failure', url: 'https://github.com/team/api/actions/runs/9', summary: '2 tests failed' },
    { name: 'lint', status: 'completed', conclusion: 'timed_out', url: '' },
    { name: 'build', status: 'completed', conclusion: 'success', url: 'https://x' },
  ] });
  const text = failingChecksMessage('api', status);
  assert.match(text, /pull request #12 \(team\/api, branch feature\/steady-otter/);
  assert.match(text, /- test \(failure\): https:\/\/github.com\/team\/api\/actions\/runs\/9 — 2 tests failed/);
  assert.match(text, /- lint \(timed out\)$/m);
  assert.doesNotMatch(text, /build/);
  assert.equal(checksSummary(status), '2 failing · 1 passed');
});

test('agent drafts are read back from their fenced block only', () => {
  const request = draftRequest('commit', 'api', ['a.ts', 'b.ts'], 'n1');
  assert.match(request, /```lazybox-commit/);
  assert.match(request, /\[draft n1\]/);
  assert.equal(extractDraft('Sure.\n```lazybox-commit\nAdd the handler\n\nWhy it matters.\n```\nDone.', 'commit'), 'Add the handler\n\nWhy it matters.');
  assert.equal(extractDraft('```lazybox-commit\nunfinished', 'commit'), undefined);
  assert.equal(extractDraft('```lazybox-commit\nAdd', 'pr'), undefined);
  assert.deepEqual(splitPullRequestDraft('# Add the handler\n\nBody\nmore'), { title: 'Add the handler', body: 'Body\nmore' });
  assert.deepEqual(commitPaths([{ path: 'b', originalPath: 'a' }, { path: 'c', originalPath: '' }]), ['b', 'a', 'c']);
});
