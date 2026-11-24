import { create } from '@bufbuild/protobuf';
import { CollectionSchema, HostedProcessSchema, ItemLinkSchema, RepositoryGitSummarySchema, SessionSchema, UsageGroupSchema, WorkspaceActionSchema, WorkspaceGitSummarySchema, type RepositoryGitSummary, type Session } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ageLine, aheadBehind, changeWords, commandName, costLine, hoverAgents, hoverOrigin, hoverPulls, hoverRepositories, hoverRuns, placeHoverCard } from '../packages/surfaces/src/workspace-hover-model.ts';

const now = Date.UTC(2026, 10, 17, 12);
const minute = 60_000, day = 24 * 60 * minute;
const repo = (value: Partial<RepositoryGitSummary>) => create(RepositoryGitSummarySchema, { member: 'api', branch: 'feature/uploads', head: 'a'.repeat(40), defaultBranch: 'origin/main', ...value } as never);
const summary = (...repositories: RepositoryGitSummary[]) => create(WorkspaceGitSummarySchema, { collectionId: 'w', repositories });
const session = (value: Partial<Session>) => create(SessionSchema, { id: 's', collectionId: 'w', createdAtUnixMillis: BigInt(now - day), ...value } as never);

test('the age says when the workspace was made, else its first conversation, and its newest activity', () => {
  const sessions = [session({ id: 'a', createdAtUnixMillis: BigInt(now - 3 * day) }), session({ id: 'b', createdAtUnixMillis: BigInt(now - 5 * minute) })];
  assert.equal(ageLine(create(CollectionSchema, { createdAtUnixMillis: BigInt(now - 2 * day) }), sessions, now), 'created 2 days ago · active 5 min ago');
  assert.equal(ageLine(create(CollectionSchema, {}), sessions, now), 'started 3 days ago · active 5 min ago');
  assert.equal(ageLine(create(CollectionSchema, {}), [], now), '');
});

test('the origin reads "#123 Fix uploads", linked, from the tracked origin link', () => {
  const workspace = create(CollectionSchema, { id: 'w', origin: { provider: 'github', key: 'acme/api#123' } });
  const link = create(ItemLinkSchema, { collectionId: 'w', role: 'origin', item: { ref: { provider: 'github', key: 'acme/api#123' }, kind: 'issue', title: 'Fix uploads', url: 'https://github.com/acme/api/issues/123' } });
  assert.deepEqual(hoverOrigin(workspace, [link]), { text: '#123 Fix uploads', title: 'acme/api#123 — Fix uploads', url: 'https://github.com/acme/api/issues/123', icon: 'issue' });
  // Before the link is read: the key alone, not a link.
  assert.deepEqual(hoverOrigin(workspace, []), { text: '#123', title: 'acme/api#123', url: '', icon: 'issue' });
  // Another provider's key is its own name.
  assert.equal(hoverOrigin(create(CollectionSchema, { id: 'w', origin: { provider: 'linear', key: 'PAY-142' } }), [])?.text, 'PAY-142');
  assert.equal(hoverOrigin(create(CollectionSchema, { id: 'w' }), []), undefined);
});

test('changes read "+120 −30 · 6 files" or "clean", with words, and a floor when cut', () => {
  assert.equal(changeWords({ files: 6, additions: 120, deletions: 30, truncated: false, base: 'origin/main' }).text, '+120 −30 · 6 files');
  assert.equal(changeWords({ files: 6, additions: 120, deletions: 30, truncated: false, base: 'origin/main' }).label, '120 lines added, 30 lines deleted, in 6 files against main');
  assert.equal(changeWords({ files: 1, additions: 1, deletions: 0, truncated: true, base: '' }).text, '+1+ −0+ · 1 file+');
  assert.deepEqual([changeWords({ files: 0, additions: 0, deletions: 0, truncated: false, base: 'origin/main' }).text, changeWords({ files: 0, additions: 0, deletions: 0, truncated: false, base: '' }).clean], ['clean', true]);
});

test('repositories: owner/name, branch, ahead and behind main, and their counts once read', () => {
  const poll = summary(repo({ githubRepository: 'acme/api', aheadOfDefault: 3, behindDefault: 1, dirty: 2 }), repo({ member: 'web', branch: '', head: 'b'.repeat(40), error: '' }));
  const names = ['acme/api', 'web'];
  const pending = hoverRepositories(['api', 'web'], names, poll, undefined);
  assert.deepEqual(pending.map(r => [r.name, r.owner, r.short, r.branch, r.ahead, r.behind, r.changes]), [['acme/api', 'acme', 'api', 'feature/uploads', 3, 1, undefined], ['web', '', 'web', 'detached at bbbbbbbb', 0, 0, undefined]]);
  assert.deepEqual(aheadBehind(pending[0]), { text: '↑3 ↓1', label: '3 commits ahead of main, 1 commit behind main' });
  assert.deepEqual(aheadBehind(pending[1]), { text: '', label: '' });
  const counted = summary(repo({ githubRepository: 'acme/api', changes: { files: 6, additions: 120, deletions: 30, base: 'origin/main' } as never }), repo({ member: 'web' }));
  const read = hoverRepositories(['api', 'web'], names, poll, counted);
  assert.equal(read[0].changes?.text, '+120 −30 · 6 files');
  // Counted, but this repository has no count (an older Runner, or a count that failed): null, so the card says what it knows.
  assert.equal(read[1].changes, null);
  // An answer that failed altogether.
  assert.equal(hoverRepositories(['api'], names, poll, null)[0].changes, null);
  // Before the poll answered: the sidebar's label, nothing more.
  assert.deepEqual(hoverRepositories(['api'], ['acme/api'], undefined, undefined).map(r => [r.name, r.branch]), [['acme/api', '']]);
});

test('pull requests: the tracked ones with state, checks and review pills, else the branch pull request', () => {
  const tracked = (key: string, pullRequest: object, state = 'review', title = 'Fix uploads') => create(ItemLinkSchema, { collectionId: 'w', role: 'delivers', linkedAtUnixMillis: BigInt(now - minute),
    item: { ref: { provider: 'github', key }, kind: 'pull_request', title, url: `https://github.com/${key.replace('#', '/pull/')}`, state: { category: state }, updatedAtUnixMillis: BigInt(now - minute), pullRequest } } as never);
  const pulls = hoverPulls('w', [
    tracked('acme/api#812', { checks: 'failure', failingChecks: [{ name: 'lint' }, { name: 'unit' }], checksTotal: 9 }),
    tracked('acme/api#77', { checks: 'success', review: 'approved', mergeable: true, mergeableState: 'clean' }),
    tracked('acme/api#5', { draft: true, checks: 'pending' }, 'started'),
    tracked('acme/api#4', { checks: 'success', closedAtUnixMillis: BigInt(now - 60 * minute) }, 'merged'),
  ], undefined, now);
  assert.deepEqual(pulls.map(p => [p.number, p.pills.map(pill => `${pill.tone}:${pill.text}`)]), [
    [812, ['neutral:Open', 'failure:2 failing']],
    [77, ['neutral:Open', 'success:Checks', 'success:Approved']],
    [5, ['draft:Draft', 'pending:Checks']],
    [4, ['merged:Merged']],
  ]);
  assert.equal(pulls[1].label, 'Pull request #77 Fix uploads: open, checks passed, approved (ready to merge)');
  // Nothing tracked yet: what the sidebar's poll read for the branch.
  const fallback = hoverPulls('w', [], summary(repo({ githubRepository: 'acme/api', pullRequest: { number: 9n, title: 'Branch PR', url: 'u', state: 'open', checksState: 'success', reviewDecision: 'changes_requested' } as never })), now);
  assert.deepEqual(fallback.map(p => [p.number, p.tone, p.pills.map(pill => pill.text)]), [[9, 'success', ['Open', 'Checks', 'Changes requested']]]);
  // More checks than the Runner reads at once: a neutral pill that says so, never a green one.
  const big = hoverPulls('w', [], summary(repo({ githubRepository: 'acme/api', pullRequest: { number: 10n, title: 'Big matrix', url: 'u', state: 'open', checksState: 'incomplete', reviewDecision: '' } as never })), now);
  assert.deepEqual(big.map(p => [p.tone, p.pills.map(pill => `${pill.tone}:${pill.text}`), p.label]), [['none', ['neutral:Open', 'neutral:Checks not all read'], 'Pull request #10 Big matrix: open, checks not all read']]);
});

test('agents: what needs you first, then work, failures and done; +N beyond four; waiting counts requests', () => {
  const pending = [{ requestId: 'r1', type: 'approval' }, { requestId: 'r2', type: 'question' }];
  const sessions = [
    session({ id: 'done', title: 'Old work', status: 'ready', createdAtUnixMillis: BigInt(now - 2 * day) }),
    session({ id: 'busy', title: 'Build it', busy: true, status: 'running', provider: 'claude', model: 'claude-opus-5-5', modelReported: true }),
    session({ id: 'ask', title: 'Needs approval', status: 'waiting_approval', pending: pending as never }),
    session({ id: 'fault', title: 'Broke', fault: 'provider_failure' }),
    session({ id: 'd2', title: 'More', status: 'ready' }),
    session({ id: 'd3', title: 'Even more', status: 'ready' }),
  ];
  const agents = hoverAgents(sessions, []);
  assert.deepEqual(agents.shown.map(a => [a.id, a.state]), [['ask', 'needs-you'], ['fault', 'failed'], ['busy', 'working'], ['d2', 'done']]);
  assert.equal(agents.more, 2);
  assert.equal(agents.waiting, 2);
  assert.match(agents.shown[2].detail, /Claude/);
  // Five fit: no "+1 more".
  assert.deepEqual([hoverAgents(sessions.slice(0, 5), []).shown.length, hoverAgents(sessions.slice(0, 5), []).more], [5, 0]);
  // A closed conversation is done, whatever it last asked.
  assert.equal(hoverAgents([session({ closed: true, status: 'waiting_input' })], []).shown[0].state, 'done');
});

test('running: actions by name with their port, then other commands that listen, each once', () => {
  const actions = [
    create(WorkspaceActionSchema, { key: 'web/dev', name: 'web', port: 5173, url: 'http://127.0.0.1:5173/', runState: { state: 'running', commandId: 'c1' } }),
    create(WorkspaceActionSchema, { key: 'api/test', name: 'test', runState: { state: 'succeeded', commandId: 'c0' } }),
  ];
  const processes = [
    create(HostedProcessSchema, { id: 'h1', kind: 'command', state: 'running', workspaceId: 'w', commandId: 'c1', label: 'npm run dev', ports: [5173, 24678] }),
    create(HostedProcessSchema, { id: 'h2', kind: 'command', state: 'running', workspaceId: 'w', commandId: 'c2', label: 'go run ./cmd/api --port 8080', ports: [8080] }),
    create(HostedProcessSchema, { id: 'h3', kind: 'command', state: 'running', workspaceId: 'w', commandId: 'c3', label: 'go test ./...', ports: [] }),
    create(HostedProcessSchema, { id: 'h4', kind: 'command', state: 'running', workspaceId: 'other', commandId: 'c4', label: 'vite', ports: [3000] }),
    create(HostedProcessSchema, { id: 'h5', kind: 'agent', state: 'running', workspaceId: 'w', label: 'claude', ports: [9000] }),
  ];
  const runs = hoverRuns('w', actions, processes);
  assert.deepEqual(runs.map(r => [r.name, r.ports, r.url]), [['web', [5173, 24678], 'http://127.0.0.1:5173/'], ['go run ./cmd/api', [8080], '']]);
  assert.equal(runs[0].label, 'web on port 5173, 24678, running');
  assert.deepEqual(hoverRuns('w', [], []), []);
  assert.equal(commandName('npm run dev -- --port 5173'), 'npm run dev');
  assert.equal(commandName(''), 'Command');
});

test('cost shows once anything is priced', () => {
  assert.equal(costLine(undefined), undefined);
  assert.equal(costLine({ groups: [] }), undefined);
  assert.equal(costLine({ groups: [create(UsageGroupSchema, { nanos: 1_240_000_000n, records: 3n })] })?.text, '$1.24 so far');
});

test('the card sits beside the row, flips near the edge, and goes below it in a narrow window', () => {
  const row = { left: 0, right: 280, top: 100, bottom: 160 };
  const size = { width: 348, height: 300 };
  assert.deepEqual(placeHoverCard(row, size, { width: 1400, height: 900 }), { left: 286, top: 100, maxHeight: 884, side: 'right' });
  // Near the bottom: lifted to stay whole, still beside the row.
  assert.equal(placeHoverCard({ ...row, top: 800, bottom: 860 }, size, { width: 1400, height: 900 }).top, 592);
  // A sidebar on the right: the card goes left of it.
  const right = placeHoverCard({ left: 1100, right: 1380, top: 100, bottom: 160 }, size, { width: 1400, height: 900 });
  assert.deepEqual([right.side, right.left], ['left', 746]);
  // A phone-width window: below the row, never over it.
  const narrow = placeHoverCard({ left: 0, right: 375, top: 100, bottom: 160 }, size, { width: 375, height: 800 });
  assert.equal(narrow.side, 'below');
  assert.ok(narrow.top >= 160);
  const low = placeHoverCard({ left: 0, right: 375, top: 600, bottom: 660 }, size, { width: 375, height: 800 });
  assert.equal(low.side, 'above');
  assert.ok(low.top + size.height <= 600);
});
