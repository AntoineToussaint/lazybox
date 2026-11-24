import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cdCommand, folderFor, legacyTerminalCommand, parseTerminalChoice, resumeCommand, reviewArgument, shellQuote, terminalFor, terminalTitle, workspaceFolder } from '../packages/surfaces/src/handoff-model.ts';
import { DEFAULT_SOUNDS, parseSoundSettings } from '../packages/surfaces/src/sounds.ts';

test('shell quoting keeps plain paths readable and makes the rest safe', () => {
  assert.equal(shellQuote('/Users/me/work/alpha'), '/Users/me/work/alpha');
  assert.equal(shellQuote('/Users/me/My Work'), `'/Users/me/My Work'`);
  assert.equal(shellQuote(`/w/it's`), `'/w/it'\\''s'`);
  assert.equal(shellQuote('$(rm -rf ~)'), `'$(rm -rf ~)'`);
  assert.equal(shellQuote('a;b'), `'a;b'`);
  assert.equal(cdCommand('/w/My App'), `cd '/w/My App'`);
});

test('an interactive command is reviewed with every character visible, and its folder named from the workspace', () => {
  assert.equal(reviewArgument('login'), 'login');
  assert.equal(reviewArgument('team blue'), `'team blue'`);
  assert.equal(reviewArgument(''), `''`);
  // ESC, CR and a right-to-left override would hide or reorder what runs: each is an escape.
  assert.equal(reviewArgument('safe\u001b[2Kgone'), `$'safe\\e[2Kgone'`);
  assert.equal(reviewArgument('a\rb\nc\td'), `$'a\\rb\\nc\\td'`);
  assert.equal(reviewArgument('txt.\u202Eexe'), `$'txt.\\u202eexe'`);
  assert.equal(reviewArgument("it's\u200b"), `$'it\\'s\\u200b'`);
  assert.equal(reviewArgument('C1\u0085'), `$'C1\\x85'`);
  assert.equal(workspaceFolder('/w/tapir', '/w/tapir'), 'workspace root');
  assert.equal(workspaceFolder('/w/tapir/', '/w/tapir/lazybox/scripts'), 'lazybox/scripts');
  assert.equal(workspaceFolder('/w/tapir', '/w/tapirs/x'), '/w/tapirs/x', 'a sibling folder is not inside the workspace');
  assert.equal(workspaceFolder('', '/w/tapir/lazybox'), '/w/tapir/lazybox');
  assert.equal(workspaceFolder('', '/'), '/');
});

test('Continue in Claude Code / Codex: the provider\'s resume command, in the conversation\'s folder', () => {
  const claude = resumeCommand({ provider: 'claude', providerReference: '4f2c-91aa', workingDirectory: '/w/alpha' }, { isolation: 'inherited' });
  assert.deepEqual(claude, { cli: 'Claude Code', command: 'cd /w/alpha && claude --resume 4f2c-91aa' });
  const codex = resumeCommand({ provider: 'codex', providerReference: 'thread 7', workingDirectory: '/w/My App' });
  assert.deepEqual(codex, { cli: 'Codex', command: `cd '/w/My App' && codex resume 'thread 7'` });
  // The provider can come from the profile.
  assert.equal(resumeCommand({ providerReference: 'x' }, { provider: 'codex' })?.command, 'codex resume x');
});

test('Continue in… says why when the command would not find the conversation, and is absent where there is no such CLI', () => {
  assert.match(resumeCommand({ provider: 'claude', workingDirectory: '/w' })!.unavailable!, /has not started/);
  assert.match(resumeCommand({ provider: 'claude', providerReference: 'x' }, { isolation: 'environment', label: 'Work Max' })!.unavailable!, /Work Max.*apart/);
  assert.equal(resumeCommand({ provider: 'gemini', providerReference: 'x' }), undefined, 'an ACP agent');
  assert.equal(resumeCommand({ provider: 'claude', providerReference: 'x', purpose: 'help' }), undefined, 'the assistant');
});

test('the terminal choice: yours, else the first installed in Lazybox\'s order', () => {
  assert.deepEqual(parseTerminalChoice(null), { app: '' });
  assert.deepEqual(parseTerminalChoice('{"app":"xterm"}'), { app: '' });
  // A command line in the page's storage is never part of the choice: the host keeps its own.
  assert.deepEqual(parseTerminalChoice('{"app":"custom","command":"alacritty --working-directory {dir}"}'), { app: 'custom' });
  assert.equal(legacyTerminalCommand('{"app":"custom","command":" alacritty --working-directory {dir} "}'), 'alacritty --working-directory {dir}');
  assert.equal(legacyTerminalCommand('{"app":"iterm"}'), '');
  assert.equal(legacyTerminalCommand('{'), '');
  assert.deepEqual(terminalFor({ app: '' }, ['terminal', 'iterm']), { app: 'iterm' });
  assert.deepEqual(terminalFor({ app: '' }, ['terminal']), { app: 'terminal' });
  assert.equal(terminalFor({ app: '' }, []), undefined);
  assert.deepEqual(terminalFor({ app: 'warp' }, []), { app: 'warp' }, 'a chosen app is asked for even when not found');
  // Custom: only when the host keeps a command, and the request never carries it.
  assert.equal(terminalFor({ app: 'custom' }, ['terminal']), undefined);
  assert.equal(terminalFor({ app: 'custom' }, ['terminal'], '  '), undefined);
  assert.deepEqual(terminalFor({ app: 'custom' }, ['terminal'], 'kitty --directory {dir}'), { app: 'custom' });
  assert.equal(terminalTitle('ghostty'), 'Ghostty');
  assert.equal(terminalTitle('custom', '/opt/bin/kitty --directory {dir}'), 'kitty');
});

test('the folder a terminal opens at: a folder itself, a file\'s folder', () => {
  assert.equal(folderFor('/w/alpha', 'api/src', true), '/w/alpha/api/src');
  assert.equal(folderFor('/w/alpha/', 'api/src/main.go', false), '/w/alpha/api/src');
  assert.equal(folderFor('/w/alpha', 'README.md', false), '/w/alpha');
  assert.equal(folderFor('/w/alpha', '', true), '/w/alpha');
  assert.equal(folderFor('/w/alpha', './api/', true), '/w/alpha/api');
});

test('sound settings are read defensively', () => {
  assert.deepEqual(parseSoundSettings(null), DEFAULT_SOUNDS);
  assert.deepEqual(parseSoundSettings('{'), DEFAULT_SOUNDS);
  const parsed = parseSoundSettings(JSON.stringify({ volume: 3, backgroundOnly: true, sources: { request: { kind: 'system', name: 'Glass' }, done: { kind: 'file', name: 'ding.wav' } } }));
  assert.equal(parsed.volume, 1);
  assert.equal(parsed.backgroundOnly, true);
  assert.deepEqual(parsed.sources, { request: { kind: 'system', name: 'Glass' }, done: { kind: 'file', name: 'ding.wav' } });
  // An unknown system sound or a malformed source falls back to Lazybox's chime.
  const odd = parseSoundSettings(JSON.stringify({ volume: 'loud', sources: { request: { kind: 'system', name: '../../etc' }, done: { kind: 'url', name: 'http://x' } } }));
  assert.equal(odd.volume, DEFAULT_SOUNDS.volume);
  assert.deepEqual(odd.sources, DEFAULT_SOUNDS.sources);
});
