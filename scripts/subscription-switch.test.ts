import { create } from '@bufbuild/protobuf';
import { JournalEventSchema, ProfileSchema, SessionSchema, type Profile, type Session } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { backAt, backWhen, bestChoice, comeback, exitFault, failureSentence, faultSentence, limitHeadline, limitStatus, resetTime, sessionLimit, signInDefault, subscriptionChoices, subscriptionName, switchedLine, switchesItself, switchLabel, switchNote, transportFault, turnFault, untilText } from '../packages/surfaces/src/subscription-model.ts';
import { foldTranscript, emptyTranscript } from '../packages/surfaces/src/transcript-model.ts';
import { liveStatus } from '../packages/surfaces/src/live-status.ts';
import { continuationPrompt } from '../packages/surfaces/src/continuation-prompt.ts';
import { deriveAttention } from '../packages/surfaces/src/attention.ts';

const json = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
// 2026-11-16 12:00 local time: resets later today read as a time of day.
const noon = new Date(2026, 10, 16, 12, 0, 0).getTime();
const at = (hours: number, minutes = 0) => new Date(2026, 10, 16, hours, minutes).getTime() / 1000;
const time = (seconds: number) => new Date(seconds * 1000).toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
const session = (init: Partial<Session> = {}): Session => create(SessionSchema, { id: 's', status: 'failed', provider: 'claude', profileId: 'work-2', ...init });
const profile = (id: string, label: string, availability?: Record<string, unknown>, isolation = 'environment'): Profile => create(ProfileSchema, { id, label, provider: 'claude', isolation, managed: true, availability: availability as any });
const profiles = [
  profile('work-1', 'Work subscription 1', { state: 'available', windows: [{ name: 'five_hour', usedPercent: 37 }] }),
  profile('work-2', 'Work subscription 2', { state: 'exhausted', resetAt: BigInt(at(18)) }),
  profile('work-3', 'Work subscription 3', { state: 'exhausted', resetAt: BigInt(at(15)) }),
  profile('claude-default', '', { state: 'unknown' }, 'inherited'),
];

test('reset times read as a person says them', () => {
  assert.equal(resetTime(BigInt(at(18)), noon), time(at(18)));
  assert.equal(resetTime(at(9) + 86400, noon), `tomorrow ${time(at(9))}`);
  assert.equal(resetTime(0n, noon), '');
  assert.equal(resetTime(at(11), noon), '', 'a reset in the past is not news');
});

test('a limited subscription says when it is back, the same way everywhere', () => {
  assert.equal(backAt(BigInt(at(18)), noon), `back at ${time(at(18))}`);
  assert.equal(backAt(at(9) + 86400, noon), `back tomorrow at ${time(at(9))}`);
  assert.equal(backAt(at(11), noon), '', 'a reset in the past is not a promise');
  assert.equal(backWhen(at(14, 10), noon), `back at ${time(at(14, 10))} (in 2h 10m)`);
  assert.equal(untilText(at(12, 45), noon), 'in 45m');
  assert.equal(untilText(at(12) + 3 * 86400 + 4 * 3600, noon), 'in 3d 4h');
  assert.equal(untilText(at(11), noon), '');
  const windows = (five: number, week: number, fiveReset: number, weekReset: number) => [{ name: 'five_hour', usedPercent: five, resetAt: BigInt(fiveReset) }, { name: 'seven_day', usedPercent: week, resetAt: BigInt(weekReset) }];
  // Back when every window at 100% has reset: the latest of them.
  assert.deepEqual(comeback(profile('a', 'A', { state: 'exhausted', windows: windows(100, 100, at(14), at(18)) }), noon), { kind: 'back', resetAt: at(18) });
  assert.deepEqual(comeback(profile('a', 'A', { state: 'exhausted', windows: windows(100, 64, at(14), at(18)) }), noon), { kind: 'back', resetAt: at(14) }, 'a window below its limit does not hold it');
  assert.deepEqual(comeback(profile('a', 'A', { state: 'exhausted', resetAt: BigInt(at(15)) }), noon), { kind: 'back', resetAt: at(15) }, 'the account reset stands in for windows');
  assert.deepEqual(comeback(profile('a', 'A', { state: 'exhausted', windows: windows(100, 10, at(11), at(18)) }), noon), { kind: 'due', resetAt: at(11) });
  assert.deepEqual(comeback(profile('a', 'A', { state: 'exhausted' }), noon), { kind: 'unknown' });
  assert.deepEqual(comeback(profile('a', 'A', { state: 'available', windows: windows(40, 10, at(14), at(18)) }), noon), { kind: 'available' });
  assert.deepEqual(comeback(profile('a', 'A', { state: 'auth_required' }), noon), { kind: 'sign-in' });
});

test('a limit is one plain sentence, never a provider token', () => {
  assert.equal(limitHeadline('Work subscription 2', 'exhausted', BigInt(at(18)), noon), `Work subscription 2 reached its limit · back at ${time(at(18))} (in 6h)`);
  assert.equal(limitHeadline('Work subscription 2', 'exhausted', 0n, noon), 'Work subscription 2 reached its limit');
  assert.equal(limitHeadline('Work', 'throttled', BigInt(at(12, 2)), noon), `Work is rate limited · back at ${time(at(12, 2))} (in 2m)`);
  assert.equal(limitHeadline('Work', 'auth_required', 0n, noon), 'Work needs you to sign in again');
  for (const [status, text] of [['throttled', 'rate_limit'], ['provider_error', ''], ['sdk_error', 'unknown_error'], ['context_full', 'context_length_exceeded']]) {
    const sentence = failureSentence(status, text);
    assert.doesNotMatch(sentence, /^[a-z_]+$|rate_limit|provider_error|exhausted/i, `${status}/${text} → ${sentence}`);
  }
  assert.match(failureSentence('unknown', 'model_not_found'), /Choose another model/);
  assert.equal(failureSentence('provider_error', 'The model refused the request.'), 'The model refused the request.', 'provider prose is kept');
  assert.equal(faultSentence('provider delivery failed; reconcile before continuing'), 'A message could not be delivered to the agent. Check the conversation before sending again.');
  assert.match(faultSentence('unknown'), /Send a message to continue/);
  // A message the provider refused is a turn's failure: the next one continues (agent.DeliveryRejectedFault).
  assert.match(faultSentence('delivery_rejected'), /Send a message to continue/);
  assert.doesNotMatch(faultSentence('provider_failure'), /provider_failure/);
});

test('only a running conversation stopped at an account limit gets the card', () => {
  assert.equal(sessionLimit(session({ fault: 'exhausted' })), 'exhausted');
  assert.equal(sessionLimit(session({ fault: 'throttled' })), 'throttled');
  assert.equal(sessionLimit(session({ fault: 'exhausted', closed: true })), '');
  assert.equal(sessionLimit(session({ fault: 'context_full' })), '', 'a full context is not fixed by another subscription');
  assert.equal(sessionLimit(session({ fault: 'provider delivery failed; reconcile before continuing' })), '');
});

test('the header says what the conversation waits for, not "Needs attention"', () => {
  const limited = session({ fault: 'exhausted', account: { state: 'exhausted', resetAt: BigInt(at(18)) } as any });
  assert.equal(limitStatus(limited, noon), `Limit reached · back at ${time(at(18))}`);
  assert.deepEqual(liveStatus(limited, noon), { tone: 'waiting', label: `Limit reached · back at ${time(at(18))}`, text: `Limit reached · back at ${time(at(18))}` });
  assert.equal(limitStatus(session({ fault: 'exhausted', account: { state: 'exhausted' } as any }), noon), 'Limit reached');
  const switching = session({ fault: 'exhausted', status: 'switching_profile', account: { state: 'exhausted' } as any, agents: [{ id: 'root', status: 'switching_profile', phaseSinceUnixMillis: BigInt(noon - 3000) } as any] });
  assert.equal(liveStatus(switching, noon).text, 'Limit reached · switching · 3s');
  assert.equal(liveStatus(session({ status: 'switching_profile', account: { state: 'available' } as any }), noon).label, 'Switching subscription', 'a switch the person asked for is not a limit');
  assert.equal(liveStatus(session({ fault: 'provider_failure' }), noon).label, 'Needs attention');
  const inbox = deriveAttention(limited);
  assert.equal(inbox.reason, 'Limit reached');
});

test('a full weekly window blocks switching despite an available status', () => {
  const full = profile('full', 'Apple', { state: 'available', windows: [{ name: 'seven_day', usedPercent: 100, resetAt: BigInt(at(18)) }] });
  const nearlyFull = profile('nearly-full', 'CLI', { state: 'unknown', windows: [{ name: 'seven_day', usedPercent: 99.9 }] });
  const choices = subscriptionChoices([full, nearlyFull], 'claude', '', noon);
  assert.equal(bestChoice(choices)?.id, 'nearly-full');
  assert.match(choices.find(choice => choice.id === 'full')?.detail || '', /^Limit reached/);
  assert.match(choices[0].detail, /99% used/);
  assert.equal(subscriptionChoices([full], 'claude', '', at(19) * 1000)[0].detail, 'Reset elapsed · usage unconfirmed');
});

test('the best other subscription is the ready one; a picker lists the current one first, then available, then by name', () => {
  const choices = subscriptionChoices(profiles, 'claude', 'work-2', noon);
  assert.deepEqual(choices.map(c => c.id), ['work-2', 'work-1', 'claude-default', 'work-3']);
  assert.equal(bestChoice(choices)?.id, 'work-1', 'known to be available first');
  assert.equal(choices.find(c => c.id === 'work-1')?.detail, 'Available · 37% used');
  assert.equal(choices.find(c => c.id === 'work-3')?.detail, `Limit reached · back at ${time(at(15))}`);
  assert.equal(choices[0].current, true);
  assert.ok(choices.every(c => c.provider === 'claude'), 'only the conversation\'s own provider');
  assert.equal(subscriptionName(profiles[3], 'claude-default'), 'Claude CLI sign-in');
  const onlyLimited = subscriptionChoices(profiles.slice(1, 3), 'claude', 'work-2', noon);
  assert.equal(bestChoice(onlyLimited), undefined, 'nothing ready: no automatic best');
  // A reset that passed makes a subscription ready again.
  assert.equal(subscriptionChoices(profiles.slice(1, 3), 'claude', 'work-2', at(16) * 1000).find(c => c.id === 'work-3')?.ready, true);
});

// Switching by itself is an extension's account policy (EXTENSIONS.md): by
// default nothing switches, whatever pool a conversation was saved with, and
// the limit card's action is the person's: "Switch to …".
test('by default a conversation never switches by itself; the person switches', () => {
  const pooled = create(SessionSchema, { provider: 'claude', profileId: 'work-2', rotationProfileIds: ['work-1', 'work-3'] });
  const unpooled = create(SessionSchema, { provider: 'claude', profileId: 'work-2', rotationProfileIds: [] });
  assert.equal(switchesItself(pooled, [], 'claude'), false, 'no account policy: a saved pool switches nothing');
  assert.equal(switchesItself(pooled, undefined, 'claude'), false, 'an older Runner reports no providers');
  assert.equal(switchesItself(pooled, ['codex'], 'claude'), false, 'a policy for Codex does not cover Claude');
  assert.equal(switchesItself(pooled, ['claude'], 'claude'), true, 'the policy on, with a pool');
  assert.equal(switchesItself(unpooled, ['claude'], 'claude'), false, 'opted out at start');
  assert.equal(switchLabel('Work subscription 1'), 'Switch to Work subscription 1');
});

test('an automatic switch is one quiet line; the limit itself adds nothing to the conversation', () => {
  const names = (id: string) => subscriptionName(profiles.find(p => p.id === id), id);
  const records = [
    { cursor: 1n, event: { type: 'operation_accepted', text: 'Fix the flaky login test' } },
    { cursor: 2n, event: { type: 'failure', status: 'exhausted', text: 'rate_limit', account: { state: 'exhausted', resetAt: BigInt(at(18)) } } },
    { cursor: 3n, event: { type: 'capacity_observation', status: 'exhausted' } },
    { cursor: 4n, event: { type: 'profile_switching', status: 'exhausted' } },
    { cursor: 5n, event: { type: 'profile_changed', text: 'work-1', status: 'checkpoint_handoff', providerDataJson: json({ from: 'work-2', fromState: 'exhausted', resetAt: at(18), requested: false }) } },
    { cursor: 6n, event: { type: 'message_delta', text: 'Picking up where I stopped.' } },
    { cursor: 7n, event: { type: 'failure', status: 'provider_error', text: 'sdk_error' } },
  ].map(r => create(JournalEventSchema, r as any));
  const folded = foldTranscript(emptyTranscript(), records);
  assert.deepEqual(folded.messages.map(m => m.type), ['operation_accepted', 'switched', 'message_delta', 'failure']);
  const line = folded.messages[1];
  assert.equal(switchedLine(line.switched!, names, noon), `Switched to Work subscription 1 · Work subscription 2 is back at ${time(at(18))}`);
  assert.doesNotMatch(folded.messages[3].text, /sdk_error/);
  // A switch the person asked for while nothing was limited, and a retry after the reset.
  assert.equal(switchedLine(switchNote('work-3', 'checkpoint_next_message', { from: 'work-1', fromState: 'available' }), names, noon), 'Switched to Work subscription 3');
  assert.equal(switchedLine(switchNote('work-2', 'retry_same_profile', { from: 'work-2' }), names, noon), 'Continued on Work subscription 2');
  // A journal written before switches carried their origin still reads.
  assert.equal(switchedLine(switchNote('work-1', 'checkpoint_handoff', {}), names, noon), 'Switched to Work subscription 1');
});

test('a new Codex subscription signs in with the browser only from the Runner\'s computer', () => {
  // The desktop app (tauri://localhost, http://tauri.localhost) and a page on loopback are on the Runner's computer.
  for (const host of ['localhost', 'tauri.localhost', '127.0.0.1', '127.0.0.2', '[::1]', '::1']) assert.equal(signInDefault(host), 'browser', host);
  // Any other page is another device: Codex's callback to 127.0.0.1 cannot reach it, so it gets a code.
  for (const host of ['mac.tailnet.ts.net', '192.168.1.20', 'localhost.example.com', '127.0.0.1.example.com', '']) assert.equal(signInDefault(host), 'device', host);
});

test('an agent process that ended is not a turn fault the next message continues (mirrors agent.TurnFault)', () => {
  for (const fault of ['provider_error', 'provider_failure', 'invalid_request', 'delivery_rejected']) assert.equal(turnFault(fault), true, fault);
  for (const fault of ['provider_process_exit', 'sdk_error', 'provider stream ended: EOF', 'native_exit_unconfirmed', 'exhausted', '']) assert.equal(turnFault(fault), false, fault);
  // An exit is not a lost stream: Resume continues it in a new process (mirrors agent.ProcessExitFault).
  assert.equal(exitFault('provider_process_exit'), true);
  assert.equal(transportFault('provider_process_exit'), false);
  assert.equal(exitFault('provider stream ended: EOF'), false);
  assert.match(faultSentence('provider_process_exit'), /stopped unexpectedly\. Resume/);
});

test('long Codex handoff keeps the opening, latest decisions, and pending follow-ups', () => {
  const messages = [
    { id: 'opening', turnId: '1', type: 'operation_accepted', text: 'Open by validating the migration plan.' },
    ...Array.from({ length: 20 }, (_, index) => ({ id: String(index), turnId: String(index), type: 'message_delta', text: 'Long earlier output '.repeat(800) })),
    { id: 'latest', turnId: '22', type: 'operation_accepted', text: 'Keep backwards compatibility.' },
    { id: 'result', turnId: '22', type: 'message_delta', text: 'The migration passes. Rollback tests remain.' },
  ];
  const prompt = continuationPrompt('Migration', messages, [{ operationId: 'next', text: 'Check staging too.' }], 'Use the test account.');
  assert.match(prompt, /Open by validating the migration plan/);
  assert.match(prompt, /Keep backwards compatibility/);
  assert.match(prompt, /Rollback tests remain/);
  assert.match(prompt, /Check staging too[.].*Use the test account/s);
  assert.match(prompt, /older exchanges omitted/);
  assert.ok(prompt.length < 60_000);
});
