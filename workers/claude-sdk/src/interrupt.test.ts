import assert from 'node:assert/strict';
import test from 'node:test';
import { TurnInterrupts, droppedOnInterrupt, interruptTurn } from './interrupt.ts';

test('an interrupted turn does not report its diagnostic as a provider error', () => {
  assert.equal(droppedOnInterrupt({ type: 'failure', status: 'provider_error' }), true);
  assert.equal(droppedOnInterrupt({ type: 'failure', status: 'unknown' }), true);
});

test('limits, sign-in and other events survive an interrupt', () => {
  for (const status of ['exhausted', 'throttled', 'auth_required', 'billing_error']) assert.equal(droppedOnInterrupt({ type: 'failure', status }), false, status);
  assert.equal(droppedOnInterrupt({ type: 'turn_ended', status: 'failed' }), false);
  assert.equal(droppedOnInterrupt({ type: 'tool_activity' }), false);
});

/** Sends a turn's events through the interrupts as the worker does; returns what reaches the Runner. */
function turn(interrupts: TurnInterrupts, during?: () => void) {
  const sent: { type: string; status?: string }[] = [];
  const pass = (event: { type: string; status?: string }) => { if (interrupts.pass(event)) sent.push(event); };
  interrupts.delivered();
  pass({ type: 'turn_started' });
  during?.();
  pass({ type: 'failure', status: 'provider_error' });
  pass({ type: 'turn_ended', status: 'failed' });
  return sent;
}

test('the interrupted turn ends as interrupted, without its diagnostic', () => {
  const interrupts = new TurnInterrupts();
  // The turn is marked as the interrupt is asked, before the SDK answers.
  const sent = turn(interrupts, () => { void interruptTurn(interrupts, async () => {}); });
  assert.deepEqual(sent, [{ type: 'turn_started' }, { type: 'turn_ended', status: 'interrupted' }]);
});

test('an interrupt that throws leaves no flag: the turn and the next keep their real failures', async () => {
  const interrupts = new TurnInterrupts();
  const sent: { type: string; status?: string }[] = [];
  const pass = (event: { type: string; status?: string }) => { if (interrupts.pass(event)) sent.push(event); };
  interrupts.delivered();
  pass({ type: 'turn_started' });
  await assert.rejects(interruptTurn(interrupts, async () => { throw new Error('control request failed'); }), /control request failed/);
  pass({ type: 'failure', status: 'provider_error' });
  pass({ type: 'turn_ended', status: 'failed' });
  assert.deepEqual(sent, [{ type: 'turn_started' }, { type: 'failure', status: 'provider_error' }, { type: 'turn_ended', status: 'failed' }]);
  assert.deepEqual(turn(interrupts), [{ type: 'turn_started' }, { type: 'failure', status: 'provider_error' }, { type: 'turn_ended', status: 'failed' }]);
});

test('an interrupt after the turn already ended does not reach the next turn', async () => {
  const interrupts = new TurnInterrupts();
  turn(interrupts);
  // The Runner asked before it heard the turn end.
  await interruptTurn(interrupts, async () => {});
  assert.deepEqual(turn(interrupts), [{ type: 'turn_started' }, { type: 'failure', status: 'provider_error' }, { type: 'turn_ended', status: 'failed' }]);
});

test('an interrupt between a message and its turn\'s first event stops that turn', async () => {
  const interrupts = new TurnInterrupts();
  interrupts.delivered();
  await interruptTurn(interrupts, async () => {});
  const sent = [{ type: 'turn_started' }, { type: 'failure', status: 'provider_error' }, { type: 'turn_ended', status: 'failed' }].filter(event => interrupts.pass(event));
  assert.deepEqual(sent, [{ type: 'turn_started' }, { type: 'turn_ended', status: 'interrupted' }]);
  // And only that one.
  assert.deepEqual(turn(interrupts), [{ type: 'turn_started' }, { type: 'failure', status: 'provider_error' }, { type: 'turn_ended', status: 'failed' }]);
});

test('an account limit in the interrupted turn still counts', async () => {
  const interrupts = new TurnInterrupts();
  interrupts.delivered();
  interrupts.pass({ type: 'turn_started' });
  await interruptTurn(interrupts, async () => {});
  assert.equal(interrupts.pass({ type: 'failure', status: 'exhausted' }), true);
});
