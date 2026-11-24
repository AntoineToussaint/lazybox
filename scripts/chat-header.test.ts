import { create } from '@bufbuild/protobuf';
import { SessionSchema } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fitHeader, HEADER_GAP, HEADER_ORDER, HEADER_PRIORITY, MORE_WIDTH, reserve, STAT_WIDTH, type HeaderItem } from '../packages/surfaces/src/header-layout.ts';
import { agentModelText, conversationName, conversationOption, currentSession, cutAtWord, modelName, titleFromMessage } from '../packages/surfaces/src/conversation-model.ts';
import { contextLevel, percentLabel } from '../packages/surfaces/src/usage-model.ts';

// The header from the report: every item present, with realistic widths.
const everything: HeaderItem[] = [
  { id: 'status', width: 72 }, { id: 'branch', width: 196 }, { id: 'model', width: 118 }, { id: 'account', width: 104 },
  { id: 'prompts', width: STAT_WIDTH.prompts }, { id: 'background', width: STAT_WIDTH.background }, { id: 'usage', width: STAT_WIDTH.usage }, { id: 'tools', width: STAT_WIDTH.tools },
];
const lineWidth = (items: readonly HeaderItem[], ids: readonly string[], folded: boolean) =>
  items.filter(item => ids.includes(item.id)).reduce((sum, item) => sum + item.width + HEADER_GAP, 0) + (folded ? MORE_WIDTH : 0);

test('a wide header shows everything, in line order, and folds nothing', () => {
  const plan = fitHeader(everything, 1400);
  assert.deepEqual(plan.folded, []);
  assert.deepEqual(plan.inline, HEADER_ORDER.filter(id => everything.some(item => item.id === id)));
});

test('a narrow header keeps the most important items and folds the rest whole', () => {
  const plan = fitHeader(everything, 480);
  assert.ok(plan.inline.includes('status'), 'the status never folds');
  assert.ok(plan.inline.includes('branch') && plan.inline.includes('model'), 'who and where come before the numbers');
  assert.ok(plan.folded.includes('prompts') && plan.folded.includes('tools'), 'the least important numbers fold first');
  assert.ok(lineWidth(everything, plan.inline, true) <= 480, 'what stays fits, with the ⋯ button');
  assert.deepEqual([...plan.inline, ...plan.folded].sort(), everything.map(item => item.id).sort(), 'every item is somewhere, once');
});

test('nothing is cut: at every width the line fits what it shows', () => {
  for (let width = 120; width <= 1400; width += 7) {
    const plan = fitHeader(everything, width);
    if (plan.inline.length === 1) continue; // the status alone may need the room it has
    assert.ok(lineWidth(everything, plan.inline, plan.folded.length > 0) <= width, `overflow at ${width}`);
  }
});

test('narrowing only folds more: an item that folds at a width stays folded below it', () => {
  let previous = new Set<string>();
  for (let width = 1400; width >= 200; width -= 5) {
    const folded = new Set(fitHeader(everything, width).folded);
    for (const id of previous) {
      // A long item folding can let a smaller, less important one back in; never the other way round.
      if (!folded.has(id)) assert.ok(everything.find(item => item.id === id)!.width <= Math.max(...[...folded].map(f => everything.find(item => item.id === f)!.width), 0), `${id} came back at ${width}`);
    }
    previous = folded;
  }
});

test('a long branch folds without taking the smaller items after it along', () => {
  const items = everything.map(item => item.id === 'branch' ? { ...item, width: 420 } : item);
  const plan = fitHeader(items, 500);
  assert.ok(plan.folded.includes('branch'));
  assert.ok(plan.inline.includes('model') && plan.inline.includes('usage'), plan.inline.join());
});

test('priority puts the status, branch and model first and the prompt count last', () => {
  assert.deepEqual(HEADER_PRIORITY.slice(0, 3), ['status', 'branch', 'model']);
  assert.equal(HEADER_PRIORITY.at(-1), 'prompts');
});

test('a ticking status keeps its slot: widths are reserved in steps', () => {
  assert.equal(reserve(130), reserve(141));
  assert.equal(reserve(0), 0);
  assert.ok(reserve(145) >= 145);
});

test('a conversation is called by one name everywhere', () => {
  const session = (title: string, workingDirectory = '/w/x') => create(SessionSchema, { id: 's', title, workingDirectory });
  assert.equal(conversationName(session('Conversation title design')), 'Conversation title design');
  assert.equal(conversationName(session('strongly-tight-collie', '/w/strongly-tight-collie')), 'New conversation', 'a folder name is not a title');
  assert.equal(conversationName(undefined, 'Conversation'), 'Conversation');
});

test('the conversation list names a conversation in words, and an id only to tell two alike apart', () => {
  const session = (id: string, title: string, profileId = 'claude-default') => create(SessionSchema, { id, title, profileId, workingDirectory: '/w/x' });
  const provider = (profileId: string) => profileId.startsWith('codex') ? 'Codex' : 'Claude';
  const label = (profileId: string) => profileId === 'claude-work' ? 'Work Claude' : undefined;
  const cache = session('alpha-session', 'Add a response cache'), notes = session('beta-session', 'Release notes', 'codex-default'), work = session('work-1', 'Add a response cache', 'claude-work');
  assert.equal(conversationOption(cache, [cache, notes, work], provider, label), 'Add a response cache · Claude', 'no id fragment after a subscription with no name');
  assert.equal(conversationOption(work, [cache, notes, work], provider, label), 'Add a response cache · Claude · Work Claude');
  const twin = session('3f9a1c77', 'Add a response cache');
  assert.equal(conversationOption(twin, [cache, twin], provider, label), 'Add a response cache · Claude · 3f9a1c', 'two that read the same are told apart');
});

test('a title made from the first message is cut at a word, without the words that only open a request', () => {
  const prompt = 'I want to start a plan, we want to look at SOTA but the goal is to write down a design for titles';
  assert.equal(titleFromMessage(prompt), 'Start a plan, we want to look at SOTA but the goal…');
  assert.equal(cutAtWord('the quick brown fox jumps over the lazy dog', 20), 'the quick brown fox…');
  assert.equal(cutAtWord('x'.repeat(40), 10), 'x'.repeat(9) + '…');
  for (const text of [prompt, 'word '.repeat(40), 'Investigate why the runner restarts kill the agents when the session host is slow']) {
    const title = titleFromMessage(text);
    const last = title.replace(/…$/, '').split(' ').at(-1)!.toLowerCase();
    assert.ok(text.toLowerCase().includes(last + ' ') || text.toLowerCase().endsWith(last), `${title} ends inside a word`);
  }
});

test('the model chip says the model as people do', () => {
  assert.equal(modelName('claude-opus-5-5'), 'Opus 5.5');
  assert.equal(modelName('claude-sonnet-4-5-20250929'), 'Sonnet 4.5');
  assert.equal(modelName('claude-fable-5-1 (requested)'), 'Fable 5.1');
  assert.equal(modelName('claude-opus-4-7[1m]'), 'Opus 4.7 (1M)');
  assert.equal(modelName('claude-haiku-5'), 'Haiku 5');
  assert.equal(modelName('gpt-5.1-codex-max'), 'GPT-5.1 Codex Max');
  assert.equal(modelName('claude-reported-model'), 'claude-reported-model', 'an id it does not know stays as it is');
  assert.equal(modelName(''), '');
});

test('background work that is running keeps its place right after the status', () => {
  const running = everything.map(item => item.id === 'background' ? { ...item, urgent: true } : item);
  const narrow = fitHeader(running, 300);
  assert.ok(narrow.inline.includes('background'), narrow.inline.join());
  assert.ok(!fitHeader(everything, 300).inline.includes('background'), 'idle background work folds first');
});

test('the artifacts chip is one of the numbers: it folds whole, after the usage and before the account', () => {
  const items: HeaderItem[] = [...everything, { id: 'artifacts', width: STAT_WIDTH.artifacts }];
  assert.ok(HEADER_PRIORITY.indexOf('artifacts') > HEADER_PRIORITY.indexOf('usage') && HEADER_PRIORITY.indexOf('artifacts') < HEADER_PRIORITY.indexOf('account'));
  assert.deepEqual(fitHeader(items, 1600).folded, [], 'wide: on the line');
  // Narrower, it folds into ⋯ before the conversation's model does, and what stays fits.
  const tight = fitHeader(items, 520);
  assert.ok(tight.inline.includes('model') && tight.folded.includes('artifacts'), tight.inline.join());
  assert.ok(lineWidth(items, tight.inline, true) <= 520, 'what stays fits');
});

test('the header and the message box say the agent and model the same way', () => {
  assert.equal(agentModelText('Claude', 'claude-opus-5-5'), 'Claude · Opus 5.5');
  assert.equal(agentModelText('Claude', 'claude-opus-5-5 (requested)'), 'Claude · Opus 5.5');
  assert.equal(agentModelText('Codex', 'gpt-5'), 'Codex · GPT-5');
  assert.equal(agentModelText('Codex', ''), 'Codex', 'the agent alone before a model is known');
});

test('a context meter warns from 60% and is danger from 85%', () => {
  assert.deepEqual([0, 40.4, 59.9, 60, 84.9, 85, 100].map(contextLevel), ['normal', 'normal', 'normal', 'warn', 'warn', 'danger', 'danger']);
  assert.deepEqual([undefined, 0.05, 40.4, 70].map(percentLabel), ['—', '<0.1%', '40.4%', '70%']);
});

test('a window that moved to another execution never shows the session it left', () => {
  const fork = create(SessionSchema, { id: 'fork-session', conversationId: 'fork-conversation' });
  const source = create(SessionSchema, { id: 'alpha-session', conversationId: 'alpha-conversation' });
  // Moved to the source while the fork's session is still held, and nothing read yet: none.
  assert.equal(currentSession('alpha-session', fork, undefined), undefined);
  // The new execution's reader already has its session (a continuation carries it): that one.
  assert.equal(currentSession('alpha-session', fork, source), source);
  // Its own session read: that one, whatever the reader holds.
  const read = create(SessionSchema, { id: 'alpha-session', conversationId: 'alpha-conversation', title: 'Read' });
  assert.equal(currentSession('alpha-session', read, source), read);
  assert.equal(currentSession('', undefined, undefined), undefined);
});
