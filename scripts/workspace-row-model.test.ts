import { create } from '@bufbuild/protobuf';
import { RepositoryGitSummarySchema, SessionSchema, WorkspaceGitSummarySchema, type RepositoryGitSummary } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { agentLine, featuredConversation, pullTone, repositoryLine, rowGit } from '../packages/surfaces/src/workspace-row-model.ts';

const repo = (value: Partial<RepositoryGitSummary>) => create(RepositoryGitSummarySchema, { member: 'api', branch: 'feature/x', head: 'a'.repeat(40), defaultBranch: 'origin/main', ...value } as never);
const summary = (...repositories: RepositoryGitSummary[]) => create(WorkspaceGitSummarySchema, { collectionId: 'w', repositories });

test('before the Runner answers there is no git line', () => {
  assert.equal(rowGit(undefined), undefined);
});

test('a clean pushed branch shows the branch and nothing to do', () => {
  const git = rowGit(summary(repo({ upstream: 'origin/feature/x' })))!;
  assert.equal(git.branch, 'feature/x');
  assert.equal(git.more, '');
  assert.deepEqual(git.badges, []);
  assert.deepEqual(git.pulls, []);
  assert.equal(git.label, 'Branch feature/x');
  assert.equal(git.title, 'api · feature/x');
});

test('uncommitted, unpushed, to pull and behind the default branch, summed across repositories', () => {
  const git = rowGit(summary(
    repo({ dirty: 2, ahead: 1, upstream: 'origin/feature/x', behind: 1, behindDefault: 3 }),
    repo({ member: 'web', dirty: 1, ahead: 2, behindDefault: 1 }),
  ))!;
  assert.deepEqual(git.badges.map(b => [b.kind, b.text]), [['dirty', '3'], ['ahead', '↑3'], ['behind', '↓1'], ['behind-default', '⇣4']]);
  assert.equal(git.label, 'Branch feature/x, 3 uncommitted files, 3 commits not pushed, 1 pushed commit not pulled, 4 commits behind main');
  assert.equal(git.title, [
    'api · feature/x · 2 uncommitted · 1 not pushed · 1 behind origin/feature/x · 3 behind main',
    'web · feature/x · 1 uncommitted · 2 commits, never pushed · 1 behind main',
  ].join('\n'));
});

test('repositories on different branches show the first and how many others', () => {
  const git = rowGit(summary(repo({}), repo({ member: 'web', branch: 'other' }), repo({ member: 'cli', branch: '' })))!;
  assert.equal(git.branch, 'feature/x');
  assert.equal(git.more, '+2');
  assert.match(git.label, /^Branches feature\/x, other, detached/);
  assert.equal(git.title, 'api · feature/x · not pushed\nweb · other · not pushed\ncli · detached at aaaaaaaa');
});

test('pull request tones: merged and closed first, then draft, then checks', () => {
  const pr = (value: object) => ({ number: 1n, title: '', url: '', state: 'open', draft: false, checksState: 'none', reviewDecision: '', observedAtUnixMillis: 0n, ...value }) as never;
  assert.equal(pullTone(pr({ state: 'merged', checksState: 'failure' })), 'merged');
  assert.equal(pullTone(pr({ state: 'closed' })), 'closed');
  assert.equal(pullTone(pr({ draft: true, checksState: 'failure' })), 'draft');
  assert.equal(pullTone(pr({ checksState: 'failure' })), 'failure');
  assert.equal(pullTone(pr({ checksState: 'pending' })), 'pending');
  assert.equal(pullTone(pr({ checksState: 'success' })), 'success');
  assert.equal(pullTone(pr({})), 'none');
});

test('a pull request in the line, with its review in words', () => {
  const pushed = repo({ upstream: 'origin/feature/x', pullRequest: { number: 12n, title: 'Harden it', url: 'u', state: 'open', draft: false, checksState: 'failure', reviewDecision: 'changes_requested', observedAtUnixMillis: 1n } as never });
  const git = rowGit(summary(pushed))!;
  assert.deepEqual(git.pulls.map(p => [p.text, p.tone, p.label]), [['#12', 'failure', 'Pull request #12 · checks failing · changes requested — Harden it']]);
  assert.equal(repositoryLine(pushed), 'api · feature/x · Pull request #12 · checks failing · changes requested');
});

test('a pull request with more checks than the Runner reads says so, never "no checks"', () => {
  const big = repo({ upstream: 'origin/feature/x', pullRequest: { number: 13n, title: 'Big matrix', url: 'u', state: 'open', draft: false, checksState: 'incomplete', reviewDecision: '', observedAtUnixMillis: 1n } as never });
  assert.deepEqual(rowGit(summary(big))!.pulls.map(p => [p.tone, p.label]), [['none', 'Pull request #13 · checks not all read — Big matrix']]);
});

test('an unreadable repository is a problem, not a broken line', () => {
  const git = rowGit(summary(repo({ dirty: 1 }), repo({ member: 'web', error: 'worktree missing' })))!;
  assert.equal(git.problem, 'web: worktree missing');
  assert.equal(git.badges[0].count, 1);
  assert.match(git.title, /^web · unavailable: worktree missing$/m);
  const none = rowGit(summary(repo({ error: 'gone' })))!;
  assert.equal(none.label, 'Git state unavailable');
});

test('the featured conversation: waiting on the person, then working, then the newest', () => {
  const s = (id: string, value: object) => create(SessionSchema, { id, createdAtUnixMillis: 0n, ...value } as never);
  const old = s('old', { createdAtUnixMillis: 1n }), recent = s('recent', { createdAtUnixMillis: 5n }), saved = s('saved', { closed: true, createdAtUnixMillis: 9n });
  assert.equal(featuredConversation([old, recent, saved])?.id, 'recent');
  const working = s('working', { busy: true, createdAtUnixMillis: 2n });
  assert.equal(featuredConversation([recent, working])?.id, 'working');
  const asking = s('asking', { status: 'waiting_approval', createdAtUnixMillis: 0n });
  assert.equal(featuredConversation([recent, working, asking])?.id, 'asking');
  assert.equal(featuredConversation([]), undefined);
});

test('the agent line is the task, with provider and model only in the tooltip', () => {
  const session = create(SessionSchema, { id: 's', title: 'Harden the handler', provider: 'claude', model: 'claude-opus-5-5', modelReported: true, workingDirectory: '/w/gentle-copper-otter' } as never);
  const line = agentLine(session, [], 2);
  assert.equal(line.title, 'Harden the handler');
  assert.equal(line.more, '+2');
  assert.equal(line.tooltip, 'Harden the handler\nClaude · claude-opus-5-5 · Loading account…\n2 other conversations in this workspace');
  // The folder name the Runner starts with is not a title.
  const untitled = create(SessionSchema, { id: 'u', title: 'gentle-copper-otter', workingDirectory: '/w/gentle-copper-otter', provider: 'codex' } as never);
  assert.equal(agentLine(untitled, []).title, 'New conversation');
  assert.equal(agentLine(untitled, []).more, '');
});
