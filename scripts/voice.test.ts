// Voice, the pure part (VOICE.md): spoken commands and punctuation in English
// and French, how stretches of speech become text and join a draft, how a
// spoken query ranks palette items and what it is allowed to run, read-aloud
// text, preferences, and the voice keys in the keymap.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  applyResult, applySpokenPunctuation, defaultCommandWords, defaultVoicePreferences, dictationView, elapsed, emptyDictation, joinSpoken, mergeDictation,
  parseSpokenCommand, parseVoicePreferences, settleAll, settleDictation, speakableText, spokenAnswer, spokenDecision, spokenScore, type DictationOptions,
} from '../packages/surfaces/src/voice/voice-model';
import { replyOfTurn } from '../packages/surfaces/src/voice/useReadAloudReplies';
import { keyboardPreset, migrateKeymap, validateKeymap, chordBindings } from '../packages/surfaces/src/keyboard';
import { actionDefinition, displayBinding } from '../packages/surfaces/src/actions';

const english: DictationOptions = { language: 'en-US', spokenPunctuation: true, spokenCommands: true, words: defaultCommandWords };
const french: DictationOptions = { ...english, language: 'fr-FR' };

test('a spoken command ends an utterance, in English and in French', () => {
  assert.deepEqual(parseSpokenCommand('Fix the login bug. Send.'), { text: 'Fix the login bug.', command: 'send' });
  assert.deepEqual(parseSpokenCommand('fix the login bug, send it'), { text: 'fix the login bug', command: 'send' });
  assert.deepEqual(parseSpokenCommand('corrige le test envoie'), { text: 'corrige le test', command: 'send' });
  assert.deepEqual(parseSpokenCommand('Corrige le test. Envoie-le !'), { text: 'Corrige le test.', command: 'send' });
  assert.deepEqual(parseSpokenCommand('first point new line'), { text: 'first point', command: 'newline' });
  assert.deepEqual(parseSpokenCommand('première ligne, à la ligne'), { text: 'première ligne', command: 'newline' });
  assert.deepEqual(parseSpokenCommand('Nouvelle ligne'), { text: '', command: 'newline' });
  assert.deepEqual(parseSpokenCommand('never mind cancel'), { text: 'never mind', command: 'cancel' });
  assert.deepEqual(parseSpokenCommand('oublie ça, annule'), { text: 'oublie ça', command: 'cancel' });
  assert.deepEqual(parseSpokenCommand('that is all. Stop.'), { text: 'that is all.', command: 'stop' });
  assert.deepEqual(parseSpokenCommand('Arrête la dictée'), { text: '', command: 'stop' });
  // Only at the end, never in the middle; a negation keeps the words as text.
  assert.deepEqual(parseSpokenCommand('send the logs to the agent'), { text: 'send the logs to the agent' });
  assert.deepEqual(parseSpokenCommand("please don't send"), { text: "please don't send" });
  assert.deepEqual(parseSpokenCommand('do not send'), { text: 'do not send' });
  assert.deepEqual(parseSpokenCommand('ne pas envoyer'), { text: 'ne pas envoyer' });
  // Words are compared without case or accents; the person's own words replace the defaults.
  assert.deepEqual(parseSpokenCommand('ship it GO', { ...defaultCommandWords, send: ['go'] }), { text: 'ship it', command: 'send' });
  assert.deepEqual(parseSpokenCommand('ship it send', { ...defaultCommandWords, send: ['go'] }), { text: 'ship it send' });
  assert.deepEqual(parseSpokenCommand('A LA LIGNE'), { text: '', command: 'newline' });
});

test('spoken punctuation becomes marks, with French spacing and capitals after a sentence', () => {
  assert.equal(applySpokenPunctuation('fix the bug comma then test it question mark', 'en-US'), 'fix the bug, then test it?');
  assert.equal(applySpokenPunctuation('done period next one', 'en-GB'), 'done. Next one');
  assert.equal(applySpokenPunctuation('first line new line second line', 'en'), 'first line\nSecond line');
  assert.equal(applySpokenPunctuation('est-ce que ça marche point d’interrogation', 'fr-FR'), 'est-ce que ça marche ?');
  assert.equal(applySpokenPunctuation('bonjour virgule voici le plan deux points tester', 'fr-FR'), 'bonjour, voici le plan : tester');
  assert.equal(applySpokenPunctuation('c’est fini point merci', 'fr'), 'c’est fini. Merci');
  // "point" is French only: in English it stays a word.
  assert.equal(applySpokenPunctuation('the point is this', 'en-US'), 'the point is this');
  assert.equal(applySpokenPunctuation('the point is this', ''), 'the point is this');
  assert.equal(applySpokenPunctuation('', 'en'), '');
});

test('stretches of speech become text: finals settle, live words stay marked, "new line" goes on', () => {
  let d = emptyDictation();
  ({ dictation: d } = applyResult(d, { segment: 0, text: 'fix the', final: false }, english));
  assert.deepEqual(dictationView(d, english), { text: 'Fix the', liveStart: 0 });
  ({ dictation: d } = applyResult(d, { segment: 0, text: 'fix the login bug', final: true }, english));
  assert.deepEqual(dictationView(d, english), { text: 'Fix the login bug', liveStart: 17 });
  ({ dictation: d } = applyResult(d, { segment: 1, text: 'and add a test', final: false }, english));
  assert.deepEqual(dictationView(d, english), { text: 'Fix the login bug and add a test', liveStart: 18 });
  let step = applyResult(d, { segment: 1, text: 'and add a test new line', final: true }, english);
  assert.equal(step.command, undefined, 'a new line is not an end');
  d = step.dictation;
  ({ dictation: d } = applyResult(d, { segment: 2, text: 'then deploy', final: true }, english));
  assert.equal(dictationView(d, english).text, 'Fix the login bug and add a test\nThen deploy');
  step = applyResult(d, { segment: 3, text: 'ship it send', final: true }, english);
  assert.equal(step.command, 'send');
  assert.equal(dictationView(step.dictation, english).text, 'Fix the login bug and add a test\nThen deploy ship it');
  // A late duplicate of a settled stretch changes nothing.
  assert.equal(applyResult(step.dictation, { segment: 3, text: 'other', final: true }, english).dictation, step.dictation);
  // French: punctuation and commands in one go.
  const fr = applyResult(emptyDictation(), { segment: 0, text: 'corrige le test virgule puis relance point d’interrogation envoie', final: true }, french);
  assert.equal(fr.command, 'send');
  assert.equal(dictationView(fr.dictation, french).text, 'Corrige le test, puis relance ?');
});

test('a pause starts a new stretch: the words before it stay, in order', () => {
  // macOS after a pause: segment 0 is settled, the new words are segment 1.
  let d = emptyDictation();
  ({ dictation: d } = applyResult(d, { segment: 0, text: 'look at the', final: false }, english));
  ({ dictation: d } = applyResult(d, { segment: 0, text: 'look at the logs', final: false }, english));
  ({ dictation: d } = applyResult(d, { segment: 0, text: 'look at the logs', final: true }, english));
  ({ dictation: d } = applyResult(d, { segment: 1, text: 'and fix', final: false }, english));
  assert.deepEqual(dictationView(d, english), { text: 'Look at the logs and fix', liveStart: 17 });
  ({ dictation: d } = applyResult(d, { segment: 1, text: 'and fix them', final: false }, english));
  assert.equal(dictationView(d, english).text, 'Look at the logs and fix them');
  // A later stretch arriving first never displaces an earlier one; they read in order.
  let out = applyResult(emptyDictation(), { segment: 2, text: 'third', final: false }, english).dictation;
  out = applyResult(out, { segment: 0, text: 'first', final: true }, english).dictation;
  out = applyResult(out, { segment: 1, text: 'second', final: false }, english).dictation;
  assert.equal(dictationView(out, english).text, 'First second third');
  // An earlier stretch's final that grows after a later one began still reads before it.
  let late = applyResult(emptyDictation(), { segment: 0, text: 'right option', final: false }, english).dictation;
  late = applyResult(late, { segment: 1, text: 'and keeps listening', final: false }, english).dictation;
  late = applyResult(late, { segment: 0, text: 'right option works', final: true }, english).dictation;
  assert.deepEqual(dictationView(late, english), { text: 'Right option works and keeps listening', liveStart: 19 });
  // The words of a settled stretch never change again.
  assert.equal(applyResult(d, { segment: 0, text: 'something else', final: false }, english).dictation, d);
});

test('a spoken command counts only at the end of what was said', () => {
  // "send" before a pause, then more words: the words were not a command.
  let d = applyResult(emptyDictation(), { segment: 0, text: 'fix the bug send', final: false }, english).dictation;
  d = applyResult(d, { segment: 1, text: 'and the test', final: false }, english).dictation;
  const late = applyResult(d, { segment: 0, text: 'fix the bug send', final: true }, english);
  assert.equal(late.command, undefined);
  assert.equal(dictationView(late.dictation, english).text, 'Fix the bug send and the test');
  // At a pause, the command of the last words applies, not one in an earlier stretch.
  d = applyResult(emptyDictation(), { segment: 0, text: 'please stop', final: false }, english).dictation;
  d = applyResult(d, { segment: 1, text: 'the server', final: false }, english).dictation;
  assert.equal(settleDictation(d, english).command, undefined);
  d = applyResult(d, { segment: 1, text: 'the server send', final: false }, english).dictation;
  const pause = settleDictation(d, english);
  assert.equal(pause.command, 'send');
  // At the end, only the last stretch's command counts.
  const ended = settleAll(d, english);
  assert.equal(ended.command, 'send');
  assert.equal(dictationView(ended.dictation, english).text, 'Please stop the server');
  // The last stretch, settled after earlier ones: its command applies.
  d = applyResult(emptyDictation(), { segment: 0, text: 'fix the bug', final: true }, english).dictation;
  assert.equal(applyResult(d, { segment: 1, text: 'cancel', final: true }, english).command, 'cancel');
});

test('one growing stretch (macOS) settles a command at a pause, then goes on after it', () => {
  let d = applyResult(emptyDictation(), { segment: 0, text: 'first line new line', final: false }, english).dictation;
  const settled = settleDictation(d, english);
  assert.equal(settled.command, undefined);
  assert.notEqual(settled.dictation, d, 'the new line applied');
  d = settled.dictation;
  assert.deepEqual(dictationView(d, english), { text: 'First line\n', liveStart: 11 });
  // The recognizer keeps reporting the whole stretch; only what follows is new.
  d = applyResult(d, { segment: 0, text: 'first line new line second line', final: false }, english).dictation;
  assert.deepEqual(dictationView(d, english), { text: 'First line\nSecond line', liveStart: 11 });
  // A pause without a command changes nothing.
  assert.equal(settleDictation(d, english).dictation, d);
  d = applyResult(d, { segment: 0, text: 'first line new line second line stop', final: false }, english).dictation;
  const stop = settleDictation(d, english);
  assert.equal(stop.command, 'stop');
  assert.equal(dictationView(stop.dictation, english).text, 'First line\nSecond line');
  // Without spoken commands a pause never acts, and the words are text.
  const plain = { ...english, spokenCommands: false };
  const said = applyResult(emptyDictation(), { segment: 0, text: 'please send', final: false }, plain).dictation;
  assert.equal(settleDictation(said, plain).dictation, said);
  assert.equal(dictationView(settleAll(said, plain).dictation, plain).text, 'Please send');
});

test('dictated text joins a draft where the caret was: spaces, capitals, the rest kept', () => {
  assert.deepEqual(mergeDictation('', '', 'Fix it'), { text: 'Fix it', start: 0, end: 6 });
  assert.deepEqual(mergeDictation('Look at', '', 'The logs'), { text: 'Look at the logs', start: 8, end: 16 });
  assert.equal(mergeDictation('Done.', '', 'next step').text, 'Done. Next step');
  assert.equal(mergeDictation('Line one\n', '', 'line two').text, 'Line one\nLine two');
  assert.equal(mergeDictation('Ask', 'about it', 'I think').text, 'Ask I think about it');
  assert.equal(mergeDictation('Check the', 'config', 'API').text, 'Check the API config');
  assert.equal(mergeDictation('Hello', '', ', world').text, 'Hello, world');
  assert.equal(mergeDictation('Draft ', ' end', 'middle').text, 'Draft middle end');
  assert.deepEqual(mergeDictation('keep', '', ''), { text: 'keep', start: 4, end: 4 });
  assert.equal(joinSpoken('First', '\nSecond'), 'First\nSecond');
  assert.equal(joinSpoken('Is it?', 'yes'), 'Is it? Yes');
});

test('a spoken query finds the palette item people mean, in their words', () => {
  const items = [
    { key: 'action:attention.next', title: 'Next conversation that needs you', extra: 'agent waiting approval question failed review jump' },
    { key: 'action:attention.previous', title: 'Previous conversation that needs you', extra: 'agent waiting approval question failed review jump back' },
    { key: 'action:conversation.next', title: 'Next conversation in this workspace', extra: 'agent down' },
    { key: 'action:changes.show', title: 'Review changes', extra: 'diff git commit pull request' },
    { key: 'panel:changes', title: 'Show Changes', extra: 'Changes' },
    { key: 'workspace:platform', title: 'platform', extra: 'api, web' },
    { key: 'workspace:beta', title: 'Beta release', extra: 'api, web' },
    { key: 'action:settings.open', title: 'Open settings', extra: 'preferences options configure' },
  ];
  const rank = (said: string) => items.map(item => ({ ...item, score: spokenScore(said, item.title, item.extra) })).filter(i => i.score > 0).sort((a, b) => b.score - a.score);
  assert.equal(rank('next agent that needs me')[0].key, 'action:attention.next');
  assert.equal(rank('agent suivant qui m’attend')[0].key, 'action:attention.next');
  assert.equal(rank('switch to platform')[0].key, 'workspace:platform');
  assert.equal(rank('go to beta')[0].key, 'workspace:beta');
  assert.match(rank('open changes')[0].key, /changes/);
  assert.equal(rank('ouvre les réglages')[0].key, 'action:settings.open');
  assert.equal(spokenScore('banana', 'Next conversation that needs you'), 0);
  assert.equal(spokenScore('the', 'Next conversation'), 0, 'filler words alone mean nothing');
  // What voice may run: a clear winner only; one that needs a yes is asked about.
  assert.deepEqual(spokenDecision(rank('switch to platform')), { kind: 'run', key: 'workspace:platform' });
  assert.deepEqual(spokenDecision([{ key: 'a', title: 'A', score: 90, confirm: true }]), { kind: 'confirm', key: 'a' });
  assert.deepEqual(spokenDecision([{ key: 'a', title: 'A', score: 90 }, { key: 'b', title: 'B', score: 85 }]), { kind: 'choose' });
  assert.deepEqual(spokenDecision([{ key: 'a', title: 'A', score: 40 }]), { kind: 'choose' });
  assert.deepEqual(spokenDecision([]), { kind: 'none' });
  assert.equal(spokenAnswer('Yes'), 'yes');
  assert.equal(spokenAnswer('oui vas-y'), 'yes');
  assert.equal(spokenAnswer('non'), 'no');
  assert.equal(spokenAnswer('no, cancel'), 'no');
  assert.equal(spokenAnswer('open settings'), undefined);
});

test('a reply read aloud: no Markdown, code named, links as words; the summary is the first paragraph', () => {
  const reply = '## Fixed the flaky test\n\nThe **login** test raced the `session` cache. See [the CI run](https://ci.example/1).\n\n```ts\nawait ready();\n```\n\n- Added a wait\n- Ran it 50 times\n\n| a | b |\n|---|---|';
  assert.equal(speakableText(reply, 'summary'), 'Fixed the flaky test. The login test raced the session cache. See the CI run.');
  assert.equal(speakableText(reply, 'full'), 'Fixed the flaky test. The login test raced the session cache. See the CI run. (code block). Added a wait. Ran it 50 times.');
  assert.equal(speakableText('Plain answer: https://example.com/x works', 'summary'), 'Plain answer: a link works.');
  assert.equal(speakableText('', 'full'), '');
  assert.ok(speakableText('word '.repeat(2000), 'full').length <= 4001);
  // The reply of a turn is the main agent's text in it.
  const messages = [
    { id: '1', type: 'operation_accepted', text: 'Do it', turnId: 't1' },
    { id: '2', type: 'message_delta', text: 'Working on it.', turnId: 't1' },
    { id: '3', type: 'message_delta', text: 'A helper speaks', turnId: 't1', agentId: 'child' },
    { id: '4', type: 'message_delta', text: 'Done.', turnId: 't1' },
    { id: '5', type: 'message_delta', text: 'Older', turnId: 't0' },
  ];
  assert.equal(replyOfTurn(messages, 't1'), 'Working on it.\n\nDone.');
  assert.equal(replyOfTurn(messages, 'missing'), '');
});

test('preferences: any saved shape reads safely; defaults never send for you', () => {
  assert.deepEqual(parseVoicePreferences(null), defaultVoicePreferences);
  assert.equal(defaultVoicePreferences.autoSend, false);
  assert.equal(defaultVoicePreferences.allowServer, false, 'on-device by default');
  const saved = parseVoicePreferences({ engine: 'browser', language: 'fr-FR', keyBehaviour: 'toggle', rate: 9, autoSend: 'yes', words: { send: 'go, Go , ship it', stop: 42 }, readReplies: 'loud', voice: 'com.apple.voice.Thomas' });
  assert.equal(saved.engine, 'browser');
  assert.equal(saved.language, 'fr-FR');
  assert.equal(saved.keyBehaviour, 'toggle');
  assert.equal(saved.rate, 2);
  assert.equal(saved.autoSend, false);
  assert.deepEqual(saved.words.send, ['go', 'ship it']);
  assert.deepEqual(saved.words.stop, defaultCommandWords.stop);
  assert.equal(saved.readReplies, 'off');
  assert.equal(parseVoicePreferences({ language: 'fr FR; rm' }).language, '');
  assert.equal(elapsed(7_400), '0:07');
  assert.equal(elapsed(92_000), '1:32');
});

test('voice keys: held chords owned by the voice layer, migrated into saved keymaps, validated', () => {
  const map = keyboardPreset('vim', true);
  // Right ⌥ alone, and ⌥Space (often another app's global shortcut) still works.
  assert.equal(map['voice.dictate'], 'AltRight or Alt+Space');
  assert.deepEqual(displayBinding('AltRight', true), ['Right\u00a0⌥'], 'one key: its name never splits');
  assert.deepEqual(displayBinding('MetaRight', false), ['Right\u00a0Win']);
  // ⌘↵ is Send now (a plain send while the agent is idle), also while dictating.
  assert.equal(map['chat.send'], 'Enter');
  assert.equal(map['chat.send-now'], 'Meta+Enter');
  assert.equal(map['voice.command'], 'Alt+Shift+Space');
  assert.ok(actionDefinition('voice.dictate')?.hold);
  // The chord dispatcher never runs a held key.
  const chords = chordBindings({ ...map, 'voice.dictate': 'Meta+Shift+d' });
  assert.ok(![...chords.values()].includes('voice.dictate'));
  // A keymap saved before voice gains its keys.
  const { ['voice.dictate']: _d, ['voice.command']: _c, ['voice.stop-reading']: _s, ...before } = map;
  const migrated = migrateKeymap({ ...before, __version: 4 }, [], true);
  assert.equal(migrated['voice.dictate'], 'AltRight or Alt+Space');
  assert.equal(validateKeymap(migrated), '');
  // A keymap saved with ⌥Space gains Right ⌥, first; one with its own key keeps it, after Right ⌥. ⌘↵ goes to Send now.
  assert.equal(migrateKeymap({ ...map, 'voice.dictate': 'Alt+Space', 'chat.send': 'Enter', __version: 8 }, [], true)['voice.dictate'], 'AltRight or Alt+Space');
  assert.equal(migrateKeymap({ ...map, 'voice.dictate': 'Ctrl+Alt+m', __version: 8 }, [], true)['voice.dictate'], 'AltRight or Ctrl+Alt+m');
  assert.equal(migrateKeymap({ ...map, 'chat.send': 'Enter', __version: 8 }, [], true)['chat.send'], 'Enter');
  assert.equal(migrateKeymap({ ...map, 'chat.send': 'Enter', __version: 8 }, [], true)['chat.send-now'], 'Meta+Enter');
  assert.equal(migrateKeymap({ ...map, 'voice.dictate': 'Alt+Space', __version: 10 }, [], true)['voice.dictate'], 'Alt+Space', 'a current save is kept as it is');
  // Right ⌥ or Right ⌘ alone can be held to talk; a left one cannot, nor can any other action be a modifier alone.
  assert.equal(validateKeymap({ ...map, 'voice.dictate': 'MetaRight' }), '');
  assert.equal(validateKeymap({ ...map, 'voice.command': 'AltRight or Alt+Shift+Space', 'voice.dictate': 'MetaRight' }), '');
  assert.match(validateKeymap({ ...map, 'voice.dictate': 'AltLeft' }), /Invalid key/);
  assert.match(validateKeymap({ ...map, 'palette.open': 'AltRight' }), /modifier alone/);
  assert.match(validateKeymap({ ...map, 'voice.command': 'AltRight' }), /conflicts/);
  // A key you type, or a sequence, cannot be held to talk.
  assert.match(validateKeymap({ ...map, 'voice.dictate': 'v' }), /held with/);
  assert.match(validateKeymap({ ...map, 'voice.dictate': '] ] v' }), /held with|conflicts/);
  assert.equal(validateKeymap({ ...map, 'voice.dictate': 'F5' }), '');
  assert.equal(validateKeymap({ ...map, 'voice.dictate': 'Ctrl+Alt+m' }), '');
});
