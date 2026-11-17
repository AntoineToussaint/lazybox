import { test } from 'node:test';
import assert from 'node:assert/strict';
import { CommandCatalog } from './catalog.js';

test('the initialize response lists commands with their words, and the models', () => {
  const catalog = new CommandCatalog();
  const event = catalog.fromInitialize({
    commands: [
      { name: 'compact', description: 'Clear history but keep a summary', argumentHint: '<instructions>', builtin: true },
      { name: '/review-pr', description: 'Review a pull request', argumentHint: '<number>', aliases: ['rpr', 'bad alias'] },
      { name: 'compact', description: 'A project command shadowed by the builtin', argumentHint: '' },
      { name: 'has space', description: 'invalid', argumentHint: '' },
      { description: 'no name' },
    ],
    models: [{ value: 'opus', displayName: 'Opus', description: 'Most capable' }, { value: 'opus', displayName: 'dup', description: '' }, { value: 'two words', displayName: 'x', description: '' }],
  });
  assert.deepEqual(event, { type: 'command_catalog', data: {
    commands: [
      { name: 'compact', description: 'Clear history but keep a summary', argumentHint: '<instructions>', builtin: true },
      { name: 'review-pr', description: 'Review a pull request', argumentHint: '<number>', aliases: ['rpr'] },
    ],
    models: [{ value: 'opus', displayName: 'Opus', description: 'Most capable' }],
  } });
  assert.equal(catalog.fromInitialize({ commands: [
    { name: 'compact', description: 'Clear history but keep a summary', argumentHint: '<instructions>', builtin: true },
    { name: 'review-pr', description: 'Review a pull request', argumentHint: '<number>', aliases: ['rpr'] },
  ] }), undefined, 'an unchanged catalog is said once');
});

test('names from a turn never replace the described list, and terminal-only commands are marked', () => {
  const catalog = new CommandCatalog();
  const names = catalog.fromInitMessage({ slash_commands: ['/compact', 'context', 'exit'], terminal_slash_commands: ['exit'] });
  assert.deepEqual(names?.data, { commands: ['compact', 'context', 'exit'].map(name => ({ name, description: '', argumentHint: '' })), models: [], terminal: ['exit'] });
  catalog.fromInitialize({ commands: [{ name: 'compact', description: 'Summarize', argumentHint: '' }], models: [] });
  const again = catalog.fromInitMessage({ slash_commands: ['compact', 'context'], terminal_slash_commands: ['exit'] });
  assert.equal(again, undefined, 'the same terminal list and the described commands: nothing new');
  const changed = catalog.fromChanged([{ name: 'compact', description: 'Summarize', argumentHint: '' }, { name: 'deploy', description: 'Ship it', argumentHint: '[env]' }]);
  assert.deepEqual(changed?.data.commands.map(c => c.name), ['compact', 'deploy']);
  assert.deepEqual(changed?.data.terminal, ['exit']);
});

test('sizes are bounded', () => {
  const catalog = new CommandCatalog();
  const event = catalog.fromChanged(Array.from({ length: 500 }, (_, i) => ({ name: `c${i}`, description: 'x'.repeat(1000), argumentHint: 'y'.repeat(500) })));
  assert.equal(event?.data.commands.length, 400);
  assert.equal(event?.data.commands[0].description.length, 300);
  assert.equal(event?.data.commands[0].argumentHint.length, 120);
  assert.equal(catalog.fromChanged('not a list'), undefined);
  assert.equal(new CommandCatalog().fromInitialize({ commands: [], models: [] }), undefined, 'nothing known is nothing to say');
});
