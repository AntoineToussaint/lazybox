import assert from 'node:assert/strict';
import test from 'node:test';
import { ModelSwitch } from './model.ts';

test('between turns a model change applies at once', async () => {
  const set: string[] = [];
  const models = new ModelSwitch(async model => { set.push(model); });
  assert.equal(await models.change('sonnet'), 'acknowledged');
  assert.deepEqual(set, ['sonnet']);
});

test('during a turn a model change waits for it to end; the latest wins', async () => {
  const set: string[] = [];
  const models = new ModelSwitch(async model => { set.push(model); });
  models.turnStarted();
  assert.equal(await models.change('sonnet'), 'accepted');
  assert.equal(await models.change('haiku'), 'accepted');
  assert.deepEqual(set, [], 'the running turn keeps its model');
  assert.equal(models.pending, 'haiku');
  await models.turnEnded();
  assert.deepEqual(set, ['haiku']);
  assert.equal(models.pending, undefined);
  await models.turnEnded();
  assert.deepEqual(set, ['haiku'], 'applied once');
});

test('a change that fails at the end of the turn is reported, not thrown', async () => {
  const failures: string[] = [];
  const models = new ModelSwitch(async () => { throw new Error('unknown model'); }, model => failures.push(model));
  models.turnStarted();
  await models.change('nope');
  await models.turnEnded();
  assert.deepEqual(failures, ['nope']);
});

test('a model is one word', async () => {
  const models = new ModelSwitch(async () => {});
  await assert.rejects(models.change(''));
  await assert.rejects(models.change('two words'));
});

test('the queued model change carries reasoning effort to the next turn', async () => {
  const applied: unknown[] = [];
  const models = new ModelSwitch(async (model, effort) => { applied.push({ model, effort }); });
  models.turnStarted();
  assert.equal(await models.change('sonnet', 'high'), 'accepted');
  assert.deepEqual(applied, []);
  await models.turnEnded();
  assert.deepEqual(applied, [{ model: 'sonnet', effort: 'high' }]);
});

test('the next prompt waits for the deferred SDK model acknowledgement', async () => {
  let acknowledge!: () => void;
  const models = new ModelSwitch(() => new Promise<void>(resolve => { acknowledge = resolve; }));
  models.turnStarted();
  await models.change('sonnet');
  const finished = models.turnEnded();
  let delivered = false;
  const message = models.beforeMessage().then(() => { delivered = true; });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(delivered, false);
  acknowledge();
  await Promise.all([finished, message]);
  assert.equal(delivered, true);
});

test('refused idle selection preserves the prior model; deferred failure blocks delivery until corrected', async () => {
  const models = new ModelSwitch(async model => { if (model === 'unknown') throw new Error('unsupported model'); });
  await assert.rejects(models.change('unknown'), /unsupported/);
  await models.beforeMessage();
  models.turnStarted();
  await models.change('unknown');
  await models.turnEnded();
  await assert.rejects(models.beforeMessage(), /unsupported/);
  await models.change('sonnet');
  await models.beforeMessage();
});
