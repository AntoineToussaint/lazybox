import { create } from '@bufbuild/protobuf';
import { JournalEventSchema, ReviewCommentSchema, ReviewRunSchema } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { loopLine, loopNeedsYou, loopActive, reviewTabBadge, unavailableAgents, reviewerModeOf, reviewButtonTitle, reviewInProgress, reviewAgain, endedByHand, findingsKept } from '../packages/surfaces/src/review-model.ts';
import { authorLabel, countsSummary, dispositionLabel, isPending, matchesFilter, matchesQuery, parseSeverity, reviewAuthors, reviewerChoices, reviewMessage, runSummary, severityRank } from '../packages/surfaces/src/review-model.ts';
import { appendTranscript, emptyTranscript } from '../packages/surfaces/src/transcript-model.ts';
import { reviewRequest } from '../packages/surfaces/src/slash-commands.ts';

const comment = (fields: Record<string, unknown> = {}) => create(ReviewCommentSchema, { id: 'rc-1', member: 'api', path: 'src/handler.go', side: 'new', startLine: 3, endLine: 4, snippet: ['func Handle() error {', '\tlog.Print("handle")'], body: 'Return the error.', head: 'a'.repeat(40), view: 'uncommitted', disposition: 'open', freshness: 'current', currentStartLine: 3, currentEndLine: 4, ...fields });
const finding = (fields: Record<string, unknown> = {}) => comment({ id: 'rc-f', author: 'agent', authorLabel: 'Codex', authorSessionId: 'review-1', severity: 'blocker', title: 'The error is logged, not returned', reviewId: 'rv-1', disposition: 'proposed', body: 'Return it so callers can retry.', ...fields });

test('severity words are the Runner\'s: a typo is refused, never demoted', () => {
  for (const [word, want] of [['blocker', 'blocker'], [' Critical ', 'blocker'], ['HIGH', 'blocker'], ['medium', 'major'], ['low', 'minor'], ['nitpick', 'nit'], ['info', 'nit']] as const) assert.equal(parseSeverity(word), want, word);
  for (const word of ['', 'blokker', 'urgent', 'constructor', 'toString']) assert.equal(parseSeverity(word), undefined, word);
  assert.ok(severityRank('blocker') < severityRank('major') && severityRank('nit') < severityRank(''));
});

test('a finding is triaged before it is pending, and dismissed is done', () => {
  assert.equal(dispositionLabel(finding()), 'To triage');
  assert.equal(isPending(finding()), false);
  assert.ok(matchesFilter(finding(), 'proposed') && !matchesFilter(finding(), 'pending'));
  assert.equal(dispositionLabel(finding({ disposition: 'open' })), 'Accepted');
  assert.equal(dispositionLabel(comment()), 'Pending');
  assert.ok(matchesFilter(finding({ disposition: 'dismissed' }), 'settled'));
  assert.equal(authorLabel(finding()), 'from Codex');
  assert.equal(authorLabel(comment()), '');
});

test('the tray filters by state, author and severity together', () => {
  const mine = comment(), codex = finding(), claude = finding({ id: 'rc-g', authorLabel: 'Claude', severity: 'minor' });
  assert.deepEqual(reviewAuthors([codex, mine, claude]).map(a => a.label), ['Everyone', 'You', 'Claude', 'Codex']);
  assert.deepEqual(reviewAuthors([codex]).map(a => a.value), ['', 'Codex']);
  const all = [mine, codex, claude];
  const shown = (author: string, severity = '') => all.filter(c => matchesQuery(c, { filter: 'all', author, severity })).map(c => c.id);
  assert.deepEqual(shown(''), ['rc-1', 'rc-f', 'rc-g']);
  assert.deepEqual(shown('you'), ['rc-1']);
  assert.deepEqual(shown('Codex'), ['rc-f']);
  assert.deepEqual(shown('', 'minor'), ['rc-g']);
  assert.deepEqual(all.filter(c => matchesQuery(c, { filter: 'proposed', author: '', severity: '' })).map(c => c.id), ['rc-f', 'rc-g']);
});

test('accepted findings go to the author exactly as filed: severity, title, id and author', () => {
  const text = reviewMessage([finding({ disposition: 'open' }), comment({ id: 'rc-2', startLine: 9, endLine: 9, snippet: ['x'], body: 'Mine.' })]);
  assert.match(text, /^Please address these 2 review comments/);
  assert.match(text, /### 1\. api\/src\/handler\.go:3–4\n\n\*\*Blocker: The error is logged, not returned\*\* \(finding rc-f, from Codex\)\n\n_uncommitted changes on aaaaaaaa_\n\n```go\nfunc Handle\(\) error \{\n\tlog\.Print\("handle"\)\n```\n\nReturn it so callers can retry\./);
  // A person's own comment reads as before.
  assert.match(text, /### 2\. api\/src\/handler\.go:9\n\n_uncommitted changes on aaaaaaaa_\n\n```go\nx\n```\n\nMine\./);
});

test('the default reviewer is the setting\'s subscription, else the other provider than the author\'s', () => {
  const profiles = [{ id: 'claude-default', provider: 'claude' }, { id: 'codex-default', provider: 'codex' }, { id: 'codex-saved', provider: 'codex', label: 'Saved Codex' }];
  assert.deepEqual(reviewerChoices(profiles, 'claude').map(c => c.label), ['Codex', 'Claude', 'Codex · codex-default', 'Codex · Saved Codex']);
  assert.equal(reviewerChoices(profiles, 'codex')[0].label, 'Claude');
  const chosen = reviewerChoices(profiles, 'claude', 'codex-saved');
  assert.deepEqual([chosen[0].label, chosen[0].profileId, chosen.length], ['Codex · Saved Codex', 'codex-saved', 4]);
  // Only one provider signed in: it reviews (a fresh context of the same agent).
  assert.deepEqual(reviewerChoices([{ id: 'claude-default', provider: 'claude' }], 'claude').map(c => c.key), ['claude']);
  assert.deepEqual(reviewerChoices([], 'claude'), []);
  // A setting naming a subscription that is gone: the other agent stands in, and says why.
  const gone = reviewerChoices(profiles, 'claude', 'missing')[0];
  assert.deepEqual([gone.label, gone.note], ['Codex', 'The reviewer you chose is not available, so Codex reviews.']);
});

test('the default reviewer: the other agent, the same one, or a fixed one, as the Runner decides', () => {
  const profiles = [{ id: 'claude-default', provider: 'claude' }, { id: 'codex-default', provider: 'codex' }, { id: 'codex-saved', provider: 'codex', label: 'Saved Codex' }, { id: 'acp-gemini', provider: 'acp:gemini', label: 'Gemini CLI' }];
  const first = (author: string, setting = {}, unavailable: string[] = []) => { const c = reviewerChoices(profiles, author, setting, new Set(unavailable))[0]; return [c.key, c.isDefault, c.note || '', c.model || '']; };
  // The other agent (the default): Claude's work to Codex, Codex's to Claude, an ACP agent's to Claude.
  assert.deepEqual(first('claude'), ['codex', true, '', '']);
  assert.deepEqual(first('codex', { reviewerMode: 'other' }), ['claude', true, '', '']);
  assert.deepEqual(first('acp:gemini'), ['claude', true, '', '']);
  // The other agent not installed (or no account): the author's own agent, a fresh conversation, said quietly.
  assert.deepEqual(first('claude', {}, ['codex']), ['claude', true, 'Codex is not available, so a fresh Claude conversation reviews.', '']);
  assert.ok(!reviewerChoices(profiles, 'claude', {}, new Set(['codex'])).some(c => c.provider === 'codex'), 'an agent that cannot run is not offered');
  // The same agent.
  assert.deepEqual(first('codex', { reviewerMode: 'same' }), ['codex', true, '', '']);
  assert.deepEqual(first('claude', { reviewerMode: 'same' }, ['claude']), ['codex', true, 'Claude is not available, so Codex reviews.', '']);
  // Fixed: a subscription with its model, an agent (an ACP one too); not available, the other agent stands in.
  assert.deepEqual(first('claude', { reviewerMode: 'fixed', reviewer: 'codex-saved', reviewerModel: 'gpt-5.2-codex' }), ['profile:codex-saved', true, '', 'gpt-5.2-codex']);
  assert.deepEqual(first('claude', { reviewerMode: 'fixed', reviewer: 'acp:gemini' }), ['acp:gemini', true, '', '']);
  assert.deepEqual(first('claude', { reviewerMode: 'fixed', reviewer: 'acp:gemini' }, ['acp:gemini']), ['codex', true, 'Gemini CLI is not available, so Codex reviews.', '']);
  // Every agent is offered, ACP ones included, and each subscription of an agent with several.
  assert.deepEqual(reviewerChoices(profiles, 'claude').map(c => c.label), ['Codex', 'Claude', 'Gemini CLI', 'Codex · codex-default', 'Codex · Saved Codex']);
  // An older setting without a mode: a reviewer named is a fixed one.
  assert.deepEqual([reviewerModeOf({ reviewer: 'codex-saved' }), reviewerModeOf({}), reviewerModeOf({ reviewerMode: 'same', reviewer: 'x' })], ['fixed', 'other', 'same']);
  assert.deepEqual([...unavailableAgents({ providers: [{ provider: 'claude', state: 'ready' }, { provider: 'codex', state: 'not_installed' }, { provider: 'acp:goose', state: 'no_subscription' }, { provider: 'acp:qwen', state: 'limited' }] })], ['codex', 'acp:goose']);
  // The Review button says who reviews, and the key that does it.
  assert.match(reviewButtonTitle({ label: 'Codex' }, '] ] Shift+r'), /^Review with Codex \(\] \] (⇧|Shift\+)r\)$/);
  assert.equal(reviewButtonTitle({ label: 'Claude', note: 'Codex is not available, so a fresh Claude conversation reviews.' }, ''), 'Review with Claude\nCodex is not available, so a fresh Claude conversation reviews.');
});

test('a review in one line: running, done with its counts, failed with why', () => {
  const run = (fields: Record<string, unknown>) => create(ReviewRunSchema, { reviewerLabel: 'Codex', scopeLabel: 'Turn 3', state: 'done', ...fields });
  assert.equal(runSummary(run({ state: 'running' })), 'Codex is reviewing turn 3');
  assert.equal(runSummary(run({ state: 'running', blockers: 1 })), 'Codex is reviewing turn 3 · 1 blocker so far');
  assert.equal(runSummary(run({ blockers: 2, minors: 3 })), 'Codex reviewed turn 3 · 2 blockers, 3 minor');
  assert.equal(runSummary(run({ scopeLabel: 'PR #12' })), 'Codex reviewed PR #12 · no open findings');
  assert.equal(runSummary(run({ state: 'failed', detail: 'The reviewer conversation ended before it finished.' })), 'The reviewer conversation ended before it finished.');
  assert.equal(countsSummary({ blockers: 0, majors: 1, minors: 0, nits: 2 }), '1 major, 2 nits');
});

test('a review of the conversation is one card where it started, updated when it finishes', () => {
  let state = emptyTranscript();
  const data = (fields: Record<string, unknown>) => new TextEncoder().encode(JSON.stringify({ review: 'rv-1', workspace: 'alpha', reviewerSession: 'review-1', reviewerLabel: 'Codex', scopeLabel: 'Turn 1', ...fields }));
  const add = (type: string, text: string, providerDataJson?: Uint8Array) => { state = appendTranscript(state, create(JournalEventSchema, { cursor: state.cursor + 1n, event: { type, text, turnId: 't', ...(providerDataJson ? { providerDataJson } : {}) } })); };
  add('operation_accepted', 'Do it');
  add('review_started', 'Codex is reviewing turn 1', data({}));
  add('message_delta', 'later');
  add('review_finished', 'Codex reviewed turn 1 · 1 blocker', data({ blockers: 1 }));
  const cards = state.messages.filter(m => m.type === 'review');
  assert.equal(cards.length, 1);
  assert.deepEqual([cards[0].review?.finished, cards[0].review?.blockers, cards[0].review?.reviewerSession, state.messages.indexOf(cards[0])], [true, 1, 'review-1', 1]);
  // Cancelled: the same card says so, even without the Runner's review record.
  add('review_finished', 'Review cancelled · 1 finding kept', data({ blockers: 1, state: 'cancelled', stoppedBy: 'You', stoppedAt: 1700000000 }));
  assert.equal(state.messages.filter(m => m.type === 'review').length, 1);
  assert.equal(state.messages.find(m => m.type === 'review')?.review?.state, 'cancelled');
  // An event without the review's data is not a card.
  add('review_started', 'malformed', new TextEncoder().encode('{}'));
  assert.equal(state.messages.filter(m => m.type === 'review').length, 1);
});

test('/review names its reviewer and a focus: none is the default, a provider, a subscription, or the tray', () => {
  const reviewers = [{ key: 'codex', label: 'Codex' }, { key: 'claude', label: 'Claude' }, { key: 'profile:codex-saved', label: 'Codex · Saved Codex' }];
  assert.deepEqual(reviewRequest(reviewers, ''), { reviewer: reviewers[0], focus: '' });
  assert.deepEqual(reviewRequest(reviewers, ' Claude '), { reviewer: reviewers[1], focus: '' });
  assert.deepEqual(reviewRequest(reviewers, 'cl'), { reviewer: reviewers[1], focus: '' });
  assert.deepEqual(reviewRequest(reviewers, 'codex-saved'), { reviewer: reviewers[2], focus: '' });
  assert.deepEqual(reviewRequest(reviewers, 'tray'), { tray: true });
  // Words that name no reviewer are the focus, for the default reviewer.
  assert.deepEqual(reviewRequest(reviewers, 'the retry loop in handler.go'), { reviewer: reviewers[0], focus: 'the retry loop in handler.go' });
  assert.deepEqual(reviewRequest(reviewers, 'claude  the retry loop'), { reviewer: reviewers[1], focus: 'the retry loop' });
  assert.deepEqual(reviewRequest(reviewers, 'gemini'), { reviewer: reviewers[0], focus: 'gemini' });
  assert.deepEqual(reviewRequest(reviewers, 'tray of tests'), { reviewer: reviewers[0], focus: 'tray of tests' });
  assert.deepEqual(reviewRequest([], ''), { reviewer: undefined, focus: '' });
});

test('a loop in one line: its round, who it waits for, and why it stopped', () => {
  const run = (fields: Record<string, unknown>) => create(ReviewRunSchema, { reviewerLabel: 'Codex', sourceLabel: 'Claude', scopeLabel: 'Turn 3', loop: 'ask', round: 1, maxRounds: 3, phase: 'reviewing', ...fields });
  assert.equal(loopLine(run({})), 'Round 1 of 3 · reviewing');
  assert.equal(loopLine(run({ round: 2 })), 'Round 2 of 3 · re-reviewing the fix');
  assert.equal(loopLine(run({ phase: 'triage' })), 'Round 1 of 3 · waiting for you to send findings');
  assert.equal(loopLine(run({ phase: 'triage', loop: 'auto' })), 'Round 1 of 3 · sending findings');
  assert.equal(loopLine(run({ phase: 'fixing', paused: true })), 'Round 1 of 3 · Claude is fixing · paused');
  assert.equal(loopLine(run({ stopReason: 'clean', round: 2, phase: 'done' })), 'Clean ✓ after 2 rounds');
  // The loop iterates on blockers: majors left open are never "clean"; minors and nits are said, not alarming.
  assert.equal(loopLine(run({ stopReason: 'clean', phase: 'done', majors: 2 })), 'No blockers · 2 major open');
  assert.equal(loopLine(run({ stopReason: 'clean', phase: 'done', majors: 1, minors: 2 })), 'No blockers · 1 major, 2 minor open');
  assert.equal(loopLine(run({ stopReason: 'clean', phase: 'done', minors: 1, nits: 2 })), 'Clean ✓ after 1 round · 1 minor, 2 nits open');
  assert.equal(loopLine(run({ stopReason: 'stuck', phase: 'done' })), 'Stuck: the same blocker is still open after two fixes · needs you');
  assert.equal(loopLine(run({ stopReason: 'max_rounds', phase: 'done' })), 'Stopped at its 3 rounds with a blocker open · needs you');
  assert.equal(loopLine(run({ loop: 'off', phase: 'done' })), '');
  assert.ok(loopNeedsYou(run({ phase: 'triage' })) && loopNeedsYou(run({ stopReason: 'stuck' })) && !loopNeedsYou(run({ phase: 'triage', loop: 'auto' })) && !loopNeedsYou(run({ stopReason: 'clean' })));
  assert.ok(loopActive(run({ phase: 'fixing' })) && !loopActive(run({ stopReason: 'stopped' })) && !loopActive(run({ loop: 'off' })));
});

test('a review tab says how the review went: a live dot, then its findings, done, or failed', () => {
  const run = (fields: Record<string, unknown>) => create(ReviewRunSchema, { reviewerLabel: 'Codex', state: 'done', ...fields });
  assert.deepEqual(reviewTabBadge(run({ state: 'running' })), { text: '', tone: 'busy', label: 'Codex is reviewing' });
  assert.deepEqual(reviewTabBadge(run({ state: 'running', majors: 1 })), { text: '1', tone: 'busy', label: 'Codex is reviewing · 1 major so far' });
  assert.deepEqual(reviewTabBadge(run({ majors: 2, nits: 1 })), { text: '3 findings', tone: 'warn', label: 'Codex is done · 2 major, 1 nit' });
  assert.equal(reviewTabBadge(run({ blockers: 1 })).tone, 'failed');
  assert.deepEqual(reviewTabBadge(run({})), { text: 'done', tone: 'done', label: 'Codex is done · no open findings' });
  assert.equal(reviewTabBadge(run({ state: 'failed', detail: 'The reviewer stopped.' })).text, 'failed');
  assert.deepEqual(reviewTabBadge(run({ state: 'cancelled', majors: 2 })), { text: 'cancelled', tone: 'done', label: "Codex's review was cancelled · 2 findings kept" });
  assert.equal(reviewTabBadge(run({ state: 'stopped' })).text, 'stopped');
});

test('a review the person cancelled or stopped says so, keeps its findings, and can be started again', () => {
  const run = (fields: Record<string, unknown>) => create(ReviewRunSchema, { reviewerLabel: 'Codex', reviewerProvider: 'codex', sourceLabel: 'Claude', scopeLabel: 'Changes', scope: 'changes', loop: 'ask', round: 1, maxRounds: 3, phase: 'reviewing', state: 'running', ...fields });
  // Under way: running, or its loop waiting for the next round; never once it ended.
  assert.equal(reviewInProgress(run({})), true);
  assert.equal(reviewInProgress(run({ state: 'done', phase: 'fixing' })), true);
  assert.equal(reviewInProgress(run({ state: 'done', phase: 'done', stopReason: 'clean' })), false);
  assert.equal(reviewInProgress(run({ state: 'cancelled', phase: 'done', stopReason: 'cancelled' })), false);
  const cancelled = run({ state: 'cancelled', phase: 'done', stopReason: 'cancelled', round: 2, blockers: 1, minors: 1, stoppedBy: 'You' });
  assert.equal(runSummary(cancelled), 'Review cancelled · 2 findings kept');
  assert.equal(loopLine(cancelled), 'You cancelled it in round 2');
  assert.equal(endedByHand(cancelled), true);
  assert.equal(findingsKept(run({ nits: 1 })), '1 finding kept');
  const stopped = run({ state: 'stopped', phase: 'done', stopReason: 'ended', detail: "You ended Codex's conversation; the review stopped." });
  assert.equal(runSummary(stopped), 'Review stopped · 0 findings kept');
  assert.equal(loopLine(stopped), "You ended Codex's conversation; the review stopped.");
  assert.equal(loopNeedsYou(stopped), false);
  // Review again: the same scope, by the same reviewer.
  assert.deepEqual(reviewAgain(cancelled), { scope: { scope: 'changes', compare: '' }, choice: { key: 'codex', label: 'Codex', provider: 'codex', profileId: '' } });
  assert.deepEqual(reviewAgain(run({ scope: 'turn', turn: 3, reviewerProfileId: 'codex-work' })).scope, { scope: 'turn', turn: 3 });
  assert.equal(reviewAgain(run({ reviewerProfileId: 'codex-work' })).choice.profileId, 'codex-work');
  assert.deepEqual(reviewAgain(run({ scope: 'pr', member: 'api' })).scope, { scope: 'pr', member: 'api' });
});

test('the agents asking each other, and a loop asking you, are labelled lines', () => {
  let state = emptyTranscript();
  const data = new TextEncoder().encode(JSON.stringify({ review: 'rv-1', workspace: 'alpha', reviewerSession: 'review-1' }));
  const add = (type: string, text: string) => { state = appendTranscript(state, create(JournalEventSchema, { cursor: state.cursor + 1n, event: { type, text, turnId: 't', providerDataJson: data } })); };
  add('review_exchange', 'Codex asked Claude: Should Handle log?');
  add('review_escalated', 'The blocker is still open after two fixes: decide how to proceed.');
  assert.deepEqual(state.messages.map(m => [m.type, m.text, m.review?.review]), [['exchange', 'Codex asked Claude: Should Handle log?', 'rv-1'], ['review-alert', 'The blocker is still open after two fixes: decide how to proceed.', 'rv-1']]);
});
