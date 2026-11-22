// Voice (VOICE.md) against the fake Runner, with a fake speech engine in the
// page: a stand-in for the Web Speech API whose partial and final results the
// test says (window.__speech.say), and a speechSynthesis that records what it
// reads. Covers the live transcript in the message box, editing it, the send
// and cancel commands, Esc, hold and tap of ⌥Space and of Right ⌥ alone, a pause
// that ends the recognition, ⌘↵ while listening (sent now while the agent works), Vim mode, voice commands
// in the palette (with a confirmation), read-aloud, Settings → Voice, and a
// browser without speech recognition. No microphone and no model are used.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
const root = (status, updated) => [{ id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated) }];
Object.assign(f.sessions[0], { title: 'Index the API', status: 'done', busy: false, agents: root('done', now - 60_000) });
Object.assign(f.sessions[1], { title: 'Ship release notes', status: 'waiting_input', agents: root('waiting_input', now - 300_000),
  pending: [{ type: 'question', requestId: 'q-release', providerDataJson: data({ questions: [{ question: 'Which version should the notes cover?' }] }) }] });
const step = s => ({ when: 'always', ...s });
f.power.setFile({ actions: [{ id: 'continue', title: 'Tell the agent to continue', icon: 'play', key: '', where: ['palette'], confirm: false, steps: [step({ kind: 'send', text: 'Continue.', to: 'current' })] }] });
const applied = text => f.mutations.some(m => m.method === 'Apply' && m.body.message?.text === text);
const sent = () => f.mutations.filter(m => m.method === 'Apply' && m.body.message).map(m => m.body.message.text);
const alpha = f.openEvents('alpha-session');
alpha.push({ type: 'operation_accepted', text: 'Index the API', turnId: 't0' });
alpha.push({ type: 'message_delta', text: 'Indexed.', turnId: 't0' });
alpha.push({ type: 'turn_ended', turnId: 't0' });

/** The fake speech engine: Web Speech's shape, driven by the test. */
function fakeSpeech({ recognition = true } = {}) {
  Object.defineProperty(navigator, 'platform', { get: () => 'MacIntel' });
  window.__spoken = [];
  const synth = {
    current: null,
    getVoices: () => [{ voiceURI: 'voice-en', name: 'Samantha', lang: 'en-US' }, { voiceURI: 'voice-fr', name: 'Thomas', lang: 'fr-FR' }],
    speak(u) { window.__spoken.push({ text: u.text, rate: u.rate, voice: u.voice?.voiceURI || '' }); this.current = u; },
    cancel() { const u = this.current; this.current = null; u?.onerror?.(); },
    addEventListener() {}, removeEventListener() {},
  };
  window.__finishSpeaking = () => { const u = synth.current; synth.current = null; u?.onend?.(); };
  window.__speaking = () => !!synth.current;
  Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true });
  window.SpeechSynthesisUtterance = class { constructor(text) { this.text = text; this.rate = 1; this.voice = null; this.lang = ''; this.onend = null; this.onerror = null; } };
  if (!recognition) { delete window.SpeechRecognition; delete window.webkitSpeechRecognition; Object.defineProperty(window, 'webkitSpeechRecognition', { value: undefined, configurable: true }); return; }
  const result = (text, final) => Object.assign([{ transcript: text }], { isFinal: final });
  class FakeRecognition {
    constructor() { this.results = []; this.lang = ''; this.ended = false; window.__speech.created++; }
    start() { window.__speech.active = this; window.__speech.lang = this.lang; }
    emit(index) { this.onresult?.({ resultIndex: index, results: this.results }); }
    finish() { if (this.ended) return; this.ended = true; if (window.__speech.active === this) window.__speech.active = null; this.onend?.(); }
    // The person stopped: it hears nothing more; what was still being recognized settles, then it ends.
    stop() { if (window.__speech.active === this) window.__speech.active = null; setTimeout(() => { const i = this.results.findIndex(r => !r.isFinal); if (i >= 0) { this.results[i] = result(this.results[i][0].transcript, true); this.emit(i); } this.finish(); }, 40); }
    // Aborted: it hears nothing more at once; its end follows.
    abort() { if (window.__speech.active === this) window.__speech.active = null; setTimeout(() => this.finish(), 0); }
    // The browser ended it on its own after a pause: what was being recognized settles, then it ends.
    pause() { const i = this.results.findIndex(r => !r.isFinal); if (i >= 0) { this.results[i] = result(this.results[i][0].transcript, true); this.emit(i); } this.finish(); }
  }
  window.__speech = {
    active: null, created: 0, lang: '',
    /** What the recognizer heard of the current stretch; `final` settles it and starts the next. */
    say(text, final = false) {
      const r = this.active; if (!r) throw new Error('nothing is listening');
      let i = r.results.findIndex(x => !x.isFinal); if (i < 0) i = r.results.length;
      r.results[i] = result(text, final); r.emit(i);
    },
    /** A pause long enough that the browser ends the recognition by itself. */
    pause() { const r = this.active; if (!r) throw new Error('nothing is listening'); r.pause(); },
  };
  window.SpeechRecognition = FakeRecognition; window.webkitSpeechRecognition = FakeRecognition;
}

f.remote.tailscale = { ...f.remote.tailscale, enabled: true, state: 'serving' };
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url }, '/remote/v1/': { target: f.url } } } });
let browser;
try {
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  browser = await (process.env.TEST_CHROMIUM ? chromium : webkit).launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  await context.addInitScript(fakeSpeech);
  await context.addInitScript(() => {
    Object.defineProperty(navigator, 'mediaDevices', { configurable: true, value: {
      getUserMedia: async () => ({ getTracks: () => [{ stop() {} }] }),
    } });
    Object.defineProperty(window, 'MediaRecorder', { configurable: true, value: class {
      state = 'inactive';
      mimeType = 'audio/webm';
      start() { this.state = 'recording'; }
      stop() {
        this.state = 'inactive';
        this.ondataavailable?.({ data: new Blob(['fixture audio']) });
        queueMicrotask(() => this.onstop?.());
      }
    } });

    const decode = AudioContext.prototype.decodeAudioData;
    AudioContext.prototype.decodeAudioData = function (bytes, ...callbacks) {
      if (new TextDecoder().decode(bytes) !== 'fixture audio') return decode.call(this, bytes, ...callbacks);
      const buffer = this.createBuffer(1, 160, 16000);
      buffer.getChannelData(0).fill(0.3);
      return Promise.resolve(buffer);
    };
  });
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  const errors = []; page.on('pageerror', e => errors.push(String(e)));
  await page.route('**/WisprStatus', route => route.fulfill({ json: { configured: true } }));
  let wisprAudio;
  await page.route('**/TranscribeWispr', route => {
    wisprAudio = route.request().postDataJSON().wav;
    return route.fulfill({ json: { text: 'Direct from Wispr Flow' } });
  });
  await page.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  const message = page.getByRole('textbox', { name: 'Message', exact: true });
  const pill = page.locator('.voice-pill');
  const live = page.locator('[data-dictation-live]');
  const say = (text, final = false) => page.evaluate(([t, fin]) => window.__speech.say(t, fin), [text, final]);
  const hold = async (key = 'Space', shift = false) => { await page.keyboard.down('Alt'); if (shift) await page.keyboard.down('Shift'); await page.keyboard.down(key); };
  const release = async (key = 'Space', shift = false) => { await page.waitForTimeout(450); await page.keyboard.up(key); if (shift) await page.keyboard.up('Shift'); await page.keyboard.up('Alt'); };
  const listening = () => expect.poll(() => page.evaluate(() => !!window.__speech.active)).toBe(true);
  // A first load from a cold dev server can outlast expect's 5 s: these waits have the page's timeout.
  await expect(message).toBeEnabled({ timeout: 15000 });

  // The mic is in the composer row; its tooltip names the key.
  const mic = page.getByRole('button', { name: 'Dictate', exact: true });
  await expect(mic).toBeVisible();
  await expect(mic).toHaveAttribute('title', /hold Right\s⌥ to talk, tap it twice to keep listening/);

  // Started from the mic before the key ever worked here: the pill says another app may have the key.
  await mic.click();
  await listening();
  await expect(pill.getByRole('note')).toContainText('If Right ⌥ does nothing, another app (ChatGPT, Raycast, Alfred) may use it.');
  await expect(pill.getByRole('note').getByRole('button', { name: 'Pick another key' })).toBeVisible();
  await page.waitForTimeout(200); await page.screenshot({ path: '/tmp/lazybox-voice-key-tip.png' });
  await page.keyboard.press('Escape');
  await expect(pill).toHaveCount(0);

  // Hold ⌥Space: the pill shows, words appear live (marked) after what was typed, and settle on release.
  await message.fill('Note:');
  await hold();
  await expect(pill).toBeVisible();
  await expect(pill).toContainText('Listening');
  await expect(pill.locator('.voice-pill-dot')).toBeVisible();
  await expect(pill).toContainText('Online', { message: 'browser recognition is never called on-device unless the browser says so' });
  await listening();
  await say('fix the login');
  await expect(message).toHaveValue('Note: fix the login');
  await expect(live).toHaveText('fix the login');
  await expect(message).toHaveAttribute('aria-busy', 'true');
  await page.waitForTimeout(300); await page.screenshot({ path: '/tmp/lazybox-voice-live.png' });
  await say('fix the login bug', true);
  await expect(message).toHaveValue('Note: fix the login bug');
  await expect(live).toHaveCount(0);
  await say('in the API');
  await expect(live).toHaveText('in the API');
  await release();
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Note: fix the login bug in the API');
  assert.deepEqual(sent(), [], 'dictation never sends by itself');
  // The result is ordinary text: edit it, then it is the draft.
  await message.press('End');
  await page.keyboard.type(' please');
  await expect(message).toHaveValue('Note: fix the login bug in the API please');

  // A quick tap keeps listening; "new line" goes on, "send" with sending by voice off only stops.
  await message.fill('');
  await page.keyboard.press('Alt+Space');
  await listening();
  await expect(pill).toContainText('Tap Right ⌥ to finish');
  await expect(pill).toContainText('⌘↵ send');
  await say('first line new line', true);
  await say('second line send', true);
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('First line\nSecond line');
  await expect(page.locator('.toast').filter({ hasText: 'Press ↵ to send' })).toBeVisible();
  assert.deepEqual(sent(), []);

  // Esc cancels: the draft is back as it was. "Annule" does the same.
  await message.fill('Keep this');
  await hold();
  await listening();
  await say('and not this');
  await expect(message).toHaveValue('Keep this and not this');
  await page.keyboard.press('Escape');
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Keep this');
  await page.keyboard.up('Space'); await page.keyboard.up('Alt');
  await page.keyboard.press('Alt+Space');
  await listening();
  await say('pas ça annule', true);
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Keep this');

  // The pill's own buttons: Stop keeps the words, × cancels.
  await page.keyboard.press('Alt+Space');
  await listening();
  await say('kept by the button');
  await pill.getByRole('button', { name: 'Stop dictation' }).click();
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Keep this kept by the button');
  await mic.click();
  await listening();
  await expect(page.locator('.composer-mic')).toHaveAccessibleName('Stop dictation');
  await expect(pill.getByRole('note'), 'the key has worked here: no word about other apps').toHaveCount(0);
  await say('dropped');
  await pill.getByRole('button', { name: 'Cancel dictation' }).click();
  await expect(message).toHaveValue('Keep this kept by the button');

  // A pause long enough that the browser ends its recognition: listening goes on in a new one, and the words before the pause stay.
  await message.fill('');
  await page.keyboard.press('Alt+Space');
  await listening();
  await say('look at the logs');
  await say(' ');
  await expect(message).toHaveValue('Look at the logs');
  const recognitions = await page.evaluate(() => window.__speech.created);
  await page.evaluate(() => { window.__speech.say('', true); window.__speech.pause(); });
  await expect.poll(() => page.evaluate(() => window.__speech.created)).toBe(recognitions + 1);
  await listening();
  await expect(pill).toContainText('Listening');
  await expect(message).toHaveValue('Look at the logs');
  await say('and fix them');
  await expect(message).toHaveValue('Look at the logs and fix them');
  await say('and fix them', true);
  await page.evaluate(() => window.__speech.pause());
  await listening();
  await say('then run the tests');
  await expect(message).toHaveValue('Look at the logs and fix them then run the tests');
  await pill.getByRole('button', { name: 'Stop dictation' }).click();
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Look at the logs and fix them then run the tests');

  // The microphone button stops listening and keeps an editable draft.
  await message.fill('');
  const dictationButton = page.locator('.composer-mic');
  await mic.click();
  await listening();
  await say('send from the microphone');
  await expect(dictationButton).toHaveAccessibleName('Stop dictation');
  await dictationButton.click();
  await expect(mic).toBeVisible();
  await expect(message).toHaveValue('Send from the microphone');
  assert.equal(applied('Send from the microphone'), false);

  // A second tap of the shortcut also leaves a draft.
  await message.fill('');
  await page.keyboard.press('Alt+Space');
  await listening();
  await say('send with the same shortcut');
  await page.keyboard.press('Alt+Space');
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Send with the same shortcut');
  assert.equal(applied('Send with the same shortcut'), false);

  // ⌘↵ while listening: the last words settle, then the message goes as with ↵.
  await message.fill('');
  await page.keyboard.press('Alt+Space');
  await listening();
  // The key comes in the same task as the words, before React shows them: what a busy machine does to a
  // real ⌘↵ pressed right after speaking. It still sends them (the real key press is below, while the agent works).
  await page.evaluate(() => { window.__speech.say('ship the fix'); document.activeElement.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', metaKey: true, bubbles: true, cancelable: true })); });
  await expect.poll(() => applied('Ship the fix')).toBe(true);
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('');

  // ⌘↵ is Send now: while the agent works, what was said is sent now (the turn stops for it), not queued.
  const busySession = f.sessions[0];
  Object.assign(busySession, { busy: true, status: 'running', queued: 0, queuedMessages: [] });
  const hint = page.locator('.composer .keyboard-hint').first();
  await expect(hint).toHaveText('↵ queues · ⌘↵ sends now', { timeout: 5000 });
  await message.click();
  await page.keyboard.press('Alt+Space');
  await listening();
  await say('stop and ship it');
  await page.keyboard.press('Meta+Enter');
  await expect.poll(() => applied('Stop and ship it')).toBe(true);
  const nowOp = f.mutations.findLast(m => m.method === 'Apply' && m.body.message?.text === 'Stop and ship it').body.operationId;
  await expect.poll(() => f.mutations.filter(m => m.method === 'Apply' && m.body.deliverQueued).map(m => m.body.deliverQueued)).toEqual([{ operationId: nowOp, how: 'now' }]);
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('');
  Object.assign(busySession, { busy: false, status: 'done' }); delete busySession.queuedMessages;
  await expect(hint).not.toHaveText(/queues/, { timeout: 5000 });

  // Right ⌥ alone: held, it talks; released, the words settle.
  await message.click();
  await page.keyboard.down('AltRight');
  await listening();
  await expect(pill).toContainText('Release to finish');
  await say('right option works', true);
  await page.keyboard.up('AltRight');
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Right option works');
  // With another key it is an ordinary ⌥ (⌥e, ⌥←): nothing starts, the key goes through.
  const before = await page.evaluate(() => window.__speech.created);
  await page.keyboard.down('AltRight'); await page.keyboard.press('ArrowLeft'); await page.waitForTimeout(400); await page.keyboard.up('AltRight');
  await page.keyboard.press('AltRight'); await page.waitForTimeout(500);
  assert.equal(await page.evaluate(() => window.__speech.created), before, 'Right ⌥ with another key, or tapped once, never listens');
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Right option works');
  // Held a little long before the other key (a slow ⌥ chord): the dictation it started is dropped.
  await page.keyboard.down('AltRight');
  await listening();
  await page.keyboard.press('ArrowRight');
  await expect(pill).toHaveCount(0);
  await page.keyboard.up('AltRight');
  await expect(message).toHaveValue('Right option works');
  // ⌥→ moved the caret past "works"; the dropped dictation leaves it there, so the next one goes at the end.
  await page.waitForTimeout(100);
  assert.equal(await message.evaluate(box => box.selectionStart), 'Right option works'.length, 'a dropped dictation never moves the caret back');
  // Tapped twice, it keeps listening; tapped once more, it keeps the draft.
  await page.keyboard.press('AltRight'); await page.keyboard.press('AltRight');
  await listening();
  await say('and keeps listening');
  await page.waitForTimeout(500);
  await expect(pill).toContainText('Tap Right ⌥ to finish');
  await page.keyboard.press('AltRight');
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('Right option works and keeps listening');
  assert.equal(applied('Right option works and keeps listening'), false);

  // Typing while listening takes over: the words stay, the microphone closes.
  await message.fill('');
  await page.keyboard.press('Alt+Space');
  await listening();
  await say('typed over');
  await page.keyboard.type('!');
  await expect(pill).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => window.__speech.active)).toBe(null);
  await expect(message).toHaveValue('Typed over!');

  // Settings → Voice: engine, privacy, permissions, commands; turn on sending by voice.
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Voice' }).click();
  const settings = page.locator('.voice-settings');
  await expect(settings.getByRole('heading', { name: 'Voice' })).toBeVisible();
  await expect(settings.getByRole('link', { name: 'Wispr setup' })).toHaveAttribute('href', 'https://api-docs.wisprflow.ai/quickstart');
  await settings.getByLabel('Engine').selectOption('wispr');
  await expect(settings.getByRole('note', { name: 'Privacy' })).toContainText('to Wispr Flow');
  const work = page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true });
  await work.click();
  await message.fill('');
  await page.getByRole('button', { name: 'Dictate', exact: true }).click();
  await expect(pill).toContainText('Listening');
  await page.locator('.composer-mic').click();
  await expect(message).toHaveValue('Direct from Wispr Flow');
  assert.equal(Buffer.from(wisprAudio, 'base64').subarray(0, 4).toString(), 'RIFF');
  assert.equal(applied('Direct from Wispr Flow'), false);
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Voice' }).click();
  await settings.getByLabel('Engine').selectOption('browser');
  await expect(settings).toContainText('Browser speech recognition');
  await expect(settings.getByRole('note', { name: 'Privacy' })).toContainText('Your browser sends the audio to its speech service');
  await expect(settings.getByRole('note', { name: 'Privacy' })).not.toContainText('On-device');
  await expect(settings.getByRole('switch', { name: 'Send when I say “send”' })).not.toBeChecked();
  await settings.getByRole('switch', { name: 'Send when I say “send”' }).check();
  await expect(settings.getByLabel('Send', { exact: true })).toHaveValue('send, send it, envoie, envoyer, envoie-le');
  await settings.getByLabel('Language').selectOption('fr-FR');
  await page.screenshot({ path: '/tmp/lazybox-voice-settings.png', fullPage: true });
  // A new dictation key, recorded; a key you type is refused.
  const dictateRow = settings.locator('.settings-row', { hasText: 'Into the message box' });
  await dictateRow.getByRole('button', { name: 'Change…' }).click();
  await page.keyboard.press('v');
  await expect(dictateRow.getByRole('alert')).toContainText('needs one key held');
  await page.keyboard.press('Control+Alt+m');
  await expect(dictateRow.locator('kbd')).toHaveText(['⌃⌥M']);
  // A modifier alone: Right ⌥ is taken, a left one is refused.
  await dictateRow.getByRole('button', { name: 'Change…' }).click();
  await page.keyboard.press('AltLeft');
  await expect(dictateRow.getByRole('alert')).toContainText('Left ⌥ alone is pressed with other keys');
  await page.keyboard.press('AltRight');
  await expect(dictateRow.locator('kbd')).toHaveText(['Right ⌥']);
  await dictateRow.getByRole('button', { name: 'Reset' }).click();
  await expect(dictateRow.locator('kbd')).toHaveText(['Right ⌥', '⌥Space']);
  await expect(dictateRow).toContainText('If ⌥Space does nothing, another app (ChatGPT, Raycast, Alfred) may use it.');
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();

  // Saying "envoie" (in French now) sends: the message without the command word.
  await message.fill('');
  await page.keyboard.press('Alt+Space');
  await listening();
  assert.equal(await page.evaluate(() => window.__speech.lang), 'fr-FR');
  await say('corrige le test virgule puis relance envoie', true);
  await expect.poll(() => applied('Corrige le test, puis relance')).toBe(true);
  await expect(message).toHaveValue('');

  // Vim mode: dictation inserts in normal mode and Esc cancels it without leaving normal mode.
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Shortcuts' }).click();
  await page.getByRole('switch', { name: 'Vim editing in the message box' }).check();
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();
  await message.click();
  await page.keyboard.type('vim text');
  await page.keyboard.press('Escape');
  await expect(message).toHaveAttribute('data-vim-mode', 'normal');
  await page.keyboard.press('Alt+Space');
  await listening();
  await say('plus des mots');
  await page.keyboard.press('Escape');
  await expect(pill).toHaveCount(0);
  await expect(message).toHaveValue('vim text');
  await expect(message).toHaveAttribute('data-vim-mode', 'normal');
  await hold();
  await listening();
  await say('dicté', true);
  await release();
  await expect(message).toHaveValue('vim text dicté');
  await expect(message).toHaveAttribute('data-vim-mode', 'normal');
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Shortcuts' }).click();
  await page.getByRole('switch', { name: 'Vim editing in the message box' }).uncheck();
  await page.getByRole('tab', { name: 'Voice' }).click();
  await settings.getByLabel('Language').selectOption('');
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();

  // Voice commands: hold ⌥⇧Space, say it, release. A clear match runs.
  const palette = page.getByRole('dialog', { name: 'Command palette' });
  const search = palette.getByRole('combobox', { name: 'Search commands' });
  const workspace = page.getByRole('combobox', { name: 'Workspace', exact: true });
  await page.locator('.workspace-brand').click();
  await hold('Space', true);
  await expect(palette).toBeVisible();
  await listening();
  await expect(pill).toContainText('Say a command');
  await say('next agent that needs me');
  await expect(search).toHaveValue('Next agent that needs me');
  await expect(palette.getByRole('option').first()).toContainText('Next conversation that needs you');
  await page.waitForTimeout(300); await page.screenshot({ path: '/tmp/lazybox-voice-palette.png' });
  await say('next agent that needs me', true);
  await release('Space', true);
  await expect(palette).toHaveCount(0);
  await expect(workspace).toHaveValue('beta');
  await expect(page.locator('.toast').filter({ hasText: 'Next conversation that needs you' })).toContainText('You said “Next agent that needs me”');
  // "switch to alpha" goes to the workspace.
  await hold('Space', true);
  await listening();
  await say('switch to alpha', true);
  await release('Space', true);
  await expect(workspace).toHaveValue('alpha');
  // Something vague stays in the palette for the person to pick.
  await hold('Space', true);
  await listening();
  await say('conversation', true);
  await release('Space', true);
  await expect(palette).toBeVisible();
  await expect(palette.getByRole('status').filter({ hasText: 'Several things match' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(palette).toHaveCount(0);
  // The person's own action asks first; "yes" runs it.
  await hold('Space', true);
  await listening();
  await say('tell the agent to continue', true);
  await release('Space', true);
  await expect(palette.getByRole('status').filter({ hasText: 'Run “Tell the agent to continue”?' })).toBeVisible();
  assert.equal(applied('Continue.'), false, 'nothing runs before the yes');
  await hold('Space');
  await listening();
  await say('yes', true);
  await release('Space');
  await expect(palette).toHaveCount(0);
  await expect.poll(() => applied('Continue.')).toBe(true);
  // The mic in the palette works too, and Esc while listening cancels the voice, not the palette.
  await page.keyboard.press('Meta+k');
  await palette.getByRole('button', { name: 'Say a command' }).click();
  await listening();
  await say('open set');
  await page.keyboard.press('Escape');
  await expect(palette).toBeVisible();
  await expect(search).toHaveValue('');
  await page.keyboard.press('Escape');
  await expect(palette).toHaveCount(0);

  // Read aloud: the first paragraph of a reply that ends while the conversation is open; Esc stops it.
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Voice' }).click();
  await settings.getByLabel('Agent replies').selectOption('summary');
  await settings.getByRole('switch', { name: 'Conversations that need you' }).check();
  await settings.getByLabel('Voice', { exact: true }).selectOption('voice-fr');
  await settings.getByRole('button', { name: 'Play a sample' }).click();
  await expect.poll(() => page.evaluate(() => window.__spoken.at(-1))).toEqual({ text: 'Voici comment Lazybox lira les réponses de vos agents.', rate: 1, voice: 'voice-fr' });
  await page.evaluate(() => window.__finishSpeaking());
  await settings.getByLabel('Voice', { exact: true }).selectOption('');
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();
  await expect(message).toBeVisible();
  const spokenBefore = await page.evaluate(() => window.__spoken.length);
  alpha.push({ type: 'operation_accepted', text: 'Why is the login test flaky?', turnId: 't1' });
  alpha.push({ type: 'message_delta', text: '## Found it\n\nThe **login** test raced the `session` cache.\n\nI added a wait and ran it 50 times.', turnId: 't1' });
  alpha.push({ type: 'turn_ended', turnId: 't1' });
  await expect.poll(() => page.evaluate(() => window.__spoken.at(-1)?.text)).toBe('Found it. The login test raced the session cache.');
  assert.equal(await page.evaluate(() => window.__spoken.length), spokenBefore + 1, 'the replayed history is never read');
  await expect(page.locator('.voice-pill.is-reading')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('.voice-pill.is-reading')).toHaveCount(0);
  assert.equal(await page.evaluate(() => window.__speaking()), false);
  // Never while dictating.
  await message.click();
  await page.keyboard.press('Alt+Space');
  await listening();
  alpha.push({ type: 'message_delta', text: 'Another reply.', turnId: 't2' });
  alpha.push({ type: 'turn_ended', turnId: 't2' });
  await page.waitForTimeout(600);
  assert.equal(await page.evaluate(() => window.__spoken.length), spokenBefore + 1);
  await page.keyboard.press('Escape');

  // A fresh question waits for dictation to end, then reads the question and its short choices.
  await page.keyboard.press('Alt+Space');
  await listening();
  const releaseSession = f.sessions[1];
  Object.assign(releaseSession, { agents: root('waiting_input', Date.now()), pending: [{
    type: 'question', requestId: 'q-build', providerDataJson: data({ questions: [{
      question: 'Which build should I verify?', options: [{ label: 'Dev' }, { label: 'Working' }],
    }] }),
  }] });
  await expect(page.locator('.agent-row').filter({ hasText: 'Ship release notes' })).toContainText('Which build should I verify?', { timeout: 15000 });
  assert.equal(await page.evaluate(() => window.__spoken.length), spokenBefore + 1);
  await page.keyboard.press('Escape');
  const announcement = 'Beta release. Ship release notes. Which build should I verify? Choices: Dev or Working.';
  await expect.poll(() => page.evaluate(() => window.__spoken.at(-1)?.text)).toBe(announcement);
  await page.evaluate(() => window.__finishSpeaking());
  await page.keyboard.press('Meta+k');
  await search.fill('>repeat last announcement');
  await palette.getByRole('option').first().click();
  await expect.poll(() => page.evaluate(() => window.__spoken.length)).toBe(spokenBefore + 3);
  await page.evaluate(() => window.__finishSpeaking());
  await page.keyboard.press('Meta+k');
  await search.fill('>read latest reply');
  await palette.getByRole('option').first().click();
  await expect.poll(() => page.evaluate(() => window.__spoken.at(-1)?.text)).toBe('Another reply.');
  await page.evaluate(() => window.__finishSpeaking());

  // The registry: palette entries and the shortcuts overlay list the voice actions with their keys.
  await page.keyboard.press('Meta+k');
  await search.fill('>dictate');
  await expect(palette.getByRole('option').first()).toContainText('Dictate');
  await expect(palette.getByRole('option').first().locator('kbd')).toHaveText(['Right ⌥']);
  await page.keyboard.press('Escape');
  await hold('Space', true);
  await listening();
  await say('open that', true);
  await release('Space', true);
  await expect(page.getByRole('heading', { name: 'Ship release notes' })).toBeVisible();
  assert.deepEqual(errors, []);

  // A browser without speech recognition: no mic button, and Settings says what to use instead.
  const plain = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  await plain.addInitScript(fakeSpeech, { recognition: false });
  const other = await plain.newPage();
  await other.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  await expect(other.getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled({ timeout: 15000 });
  await expect(other.getByRole('button', { name: 'Dictate', exact: true })).toHaveCount(0);
  await other.getByRole('button', { name: 'Settings', exact: true }).click();
  await other.getByRole('tab', { name: 'Voice' }).click();
  await expect(other.locator('.voice-settings')).toContainText('This browser has no speech recognition');
  // ⌥Space says so instead of failing silently.
  await other.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();
  await other.getByRole('textbox', { name: 'Message', exact: true }).click();
  await other.keyboard.press('Alt+Space');
  await expect(other.locator('.voice-pill.is-error')).toContainText('no speech recognition');
  await plain.close();
  // The paired phone uses the same stop-and-review flow, with a separate Send button.
  f.remote.phoneUrl = origin;
  const offer = await fetch(`${f.url}/lazybox.bench.v1.RunnerService/BeginDevicePairing`, {
    method: 'POST', headers: { authorization: `Bearer ${f.token}`, 'content-type': 'application/json' }, body: JSON.stringify({ grants: ['respond'] }),
  });
  const phoneContext = await browser.newContext({ viewport: { width: 390, height: 844 } });
  await phoneContext.addInitScript(fakeSpeech);
  const phone = await phoneContext.newPage();
  await phone.goto((await offer.json()).url);
  await phone.getByRole('textbox', { name: 'Name this phone' }).fill('Dictation phone');
  await phone.getByRole('button', { name: 'Pair this phone' }).click();
  await phone.getByRole('button', { name: 'Open inbox' }).click();
  await phone.locator('.remote-row').filter({ hasText: 'Index the API' }).locator('.remote-row-open').click();
  await phone.getByRole('button', { name: 'Read latest reply' }).click();
  await expect.poll(() => phone.evaluate(() => window.__spoken.at(-1)?.text)).toBe('Another reply.');
  await phone.getByRole('button', { name: 'Stop reading reply' }).click();
  const phoneMic = phone.locator('.remote-mic');
  await phoneMic.click();
  await expect.poll(() => phone.evaluate(() => !!window.__speech.active)).toBe(true);
  await phone.evaluate(() => window.__speech.say('draft from the phone'));
  await phoneMic.click();
  await expect(phone.getByRole('textbox', { name: 'Message' })).toHaveValue('Draft from the phone');
  await expect(phoneMic).toHaveAccessibleName('Dictate');
  assert.equal(applied('Draft from the phone'), false);
  await phone.getByRole('button', { name: 'Send message' }).click();
  await expect.poll(() => applied('Draft from the phone')).toBe(true);
  await expect(phone.getByRole('textbox', { name: 'Message' })).toHaveValue('');
  await phoneMic.click();
  await expect.poll(() => phone.evaluate(() => !!window.__speech.active)).toBe(true);
  await phone.evaluate(() => window.__speech.say('keep this with the original conversation'));
  await phone.getByRole('button', { name: 'Back to inbox' }).click();
  await phone.locator('.remote-row').filter({ hasText: 'Ship release notes' }).locator('.remote-row-open').click();
  await expect(phone.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('');
  await phone.getByRole('button', { name: 'Back to inbox' }).click();
  await phone.locator('.remote-row').filter({ hasText: 'Index the API' }).locator('.remote-row-open').click();
  await expect(phone.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('Keep this with the original conversation');
  assert.equal(applied('Keep this with the original conversation'), false);
  await phoneContext.close();
  console.log('voice: dictation, commands, palette, read-aloud and settings passed');
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
