import assert from 'node:assert/strict';
import test from 'node:test';
import { generateTitle } from './title.ts';

test('a title comes from the SDK session, without persisting it in Claude’s transcript', async () => {
  const calls: unknown[] = [];
  const session = { generateSessionTitle: async (description: string, options?: { persist?: boolean }) => { calls.push([description, options]); return '  Conversation titles  '; } };
  assert.equal(await generateTitle(session, 'fix the titles'), 'Conversation titles');
  assert.deepEqual(calls, [['fix the titles', { persist: false }]]);
});

test('no title is an empty answer, not an error', async () => {
  assert.equal(await generateTitle({ generateSessionTitle: async () => null }, 'hi there'), '');
});

test('a CLI without the generator, or no description, fails so the Runner keeps its fallback', async () => {
  await assert.rejects(generateTitle({}, 'fix it'), /cannot generate/);
  await assert.rejects(generateTitle(undefined, 'fix it'), /cannot generate/);
  await assert.rejects(generateTitle({ generateSessionTitle: async () => 'x' }, '  '), /description required/);
  await assert.rejects(generateTitle({ generateSessionTitle: async () => 'x' }, 'x'.repeat(9000)), /too long/);
});
