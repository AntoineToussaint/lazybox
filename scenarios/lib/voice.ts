// A fake speech engine for dictation scenarios (VOICE.md): Web Speech's shape
// (SpeechRecognition, speechSynthesis), driven by the scenario instead of a
// microphone, as scripts/test-voice.mjs drives it. `w.voice()` installs it;
// `hears('…')` makes the recognizer hear words, a few at a time, as a person
// speaks them.
import { step, type Step } from './step.ts';
import { expect as playwrightExpect } from '@playwright/test';

/** Runs in the page before the app: replaces Web Speech with a scripted one. */
export function fakeSpeech() {
  Object.defineProperty(navigator, 'platform', { get: () => 'MacIntel' });
  const synth = { getVoices: () => [{ voiceURI: 'voice-en', name: 'Samantha', lang: 'en-US' }], speak() {}, cancel() {}, addEventListener() {}, removeEventListener() {} };
  Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true });
  (window as any).SpeechSynthesisUtterance = class { text: string; rate = 1; voice = null; lang = ''; constructor(text: string) { this.text = text; } };
  const result = (text: string, final: boolean) => Object.assign([{ transcript: text }], { isFinal: final });
  const speech: any = { active: null, created: 0 };
  class FakeRecognition {
    results: any[] = []; lang = ''; ended = false; onresult?: (e: unknown) => void; onend?: () => void;
    constructor() { speech.created++; }
    start() { speech.active = this; }
    emit(index: number) { this.onresult?.({ resultIndex: index, results: this.results }); }
    finish() { if (this.ended) return; this.ended = true; if (speech.active === this) speech.active = null; this.onend?.(); }
    stop() { if (speech.active === this) speech.active = null; setTimeout(() => { const i = this.results.findIndex(r => !r.isFinal); if (i >= 0) { this.results[i] = result(this.results[i][0].transcript, true); this.emit(i); } this.finish(); }, 40); }
    abort() { if (speech.active === this) speech.active = null; setTimeout(() => this.finish(), 0); }
  }
  speech.say = (text: string, final = false) => {
    const r = speech.active; if (!r) throw new Error('nothing is listening');
    let i = r.results.findIndex((x: any) => !x.isFinal); if (i < 0) i = r.results.length;
    r.results[i] = result(text, final); r.emit(i);
  };
  (window as any).__speech = speech;
  (window as any).SpeechRecognition = FakeRecognition; (window as any).webkitSpeechRecognition = FakeRecognition;
}

/**
 * The microphone hears `words`: partial results grow a few words at a time
 * (the live, marked text in the message box), then settle as final unless
 * `final: false`.
 */
export function hears(words: string, options: { final?: boolean; wordsPerSecond?: number } = {}): Step {
  return step('person', `say aloud “${words}”`, async ctx => {
    const listening = () => ctx.page.evaluate(() => !!(window as any).__speech?.active);
    await ctx.check('the dictation is listening', () => playwrightExpect.poll(listening, { timeout: ctx.timeout }).toBe(true));
    const all = words.split(/\s+/);
    if (ctx.video) {
      const per = 1 / (options.wordsPerSecond ?? 2.6);
      for (let n = 1; n < all.length; n++) {
        await ctx.page.evaluate(text => (window as any).__speech.say(text, false), all.slice(0, n).join(' '));
        await ctx.beat(per * (0.75 + ctx.random() * 0.5));
      }
    }
    await ctx.page.evaluate(([text, final]) => (window as any).__speech.say(text, final), [words, options.final ?? true] as const);
    await ctx.beat(0.5);
  });
}
