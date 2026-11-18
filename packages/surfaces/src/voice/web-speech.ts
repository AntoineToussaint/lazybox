import type { RecognitionEvent, RecognizerLanguage, RecognizerStatus, SpeechRecognizerHost, SpeechSynthesizerHost, SpeechVoice, VoicePermission } from '@lazybox/platform';

/**
 * The browser's own speech services (Web Speech): recognition where the
 * browser has it (Chrome, Edge, Safari), speechSynthesis for read-aloud.
 *
 * Where audio goes is the browser's business: Chrome sends it to Google's
 * speech service unless it can recognize the language on the device
 * (`processLocally`, where offered), Safari to Apple's. The status says
 * on-device only when the browser confirms it.
 */
type ResultList = ArrayLike<ArrayLike<{ transcript: string }> & { isFinal: boolean }>;
type Recognition = {
  lang: string; continuous: boolean; interimResults: boolean; maxAlternatives: number; processLocally?: boolean;
  onresult: ((event: { resultIndex: number; results: ResultList }) => void) | null;
  onerror: ((event: { error: string; message?: string }) => void) | null;
  onend: (() => void) | null;
  onspeechstart: (() => void) | null;
  onspeechend: (() => void) | null;
  start(): void; stop(): void; abort(): void;
};
type RecognitionClass = (new () => Recognition) & { available?(options: { langs: string[]; processLocally: boolean }): Promise<string> };

function recognitionClass(): RecognitionClass | undefined {
  const scope = globalThis as unknown as { SpeechRecognition?: RecognitionClass; webkitSpeechRecognition?: RecognitionClass };
  return scope.SpeechRecognition || scope.webkitSpeechRecognition;
}

const browserLanguage = () => (typeof navigator !== 'undefined' && navigator.language) || 'en-US';
/** Common recognition languages, the person's own first. Browsers do not list theirs. */
const offered = ['en-US', 'en-GB', 'fr-FR', 'fr-CA', 'de-DE', 'es-ES', 'it-IT', 'pt-BR', 'nl-NL', 'ja-JP', 'zh-CN'];
function languageName(code: string): string {
  try { return new Intl.DisplayNames([browserLanguage()], { type: 'language' }).of(code) || code; } catch { return code; }
}

async function locallyAvailable(Recognition: RecognitionClass, language: string): Promise<boolean> {
  if (typeof Recognition.available !== 'function') return false;
  try { return await Recognition.available({ langs: [language], processLocally: true }) === 'available'; } catch { return false; }
}

const errors: Record<string, string> = {
  'not-allowed': 'The microphone is blocked for this site. Allow it in your browser’s site settings.',
  'service-not-allowed': 'This browser does not allow speech recognition here.',
  'audio-capture': 'No microphone was found.',
  network: 'The browser’s speech service could not be reached.',
  'language-not-supported': 'This browser cannot recognize that language.',
  'no-speech': 'Nothing was heard.',
};

/** How long listening goes on with nothing heard, across new recognitions, before it ends by itself. */
const QUIET_LIMIT_MS = 60_000;
/** New recognitions in a row that hear nothing: one that ends at once is not restarted forever. */
const QUIET_ROUNDS = 5;

/** The Web Speech recognizer, or undefined where the browser has none. */
export function webRecognizer(): SpeechRecognizerHost | undefined {
  const Recognition = recognitionClass();
  if (!Recognition) return undefined;
  return {
    id: 'browser',
    label: 'Browser speech recognition',
    async status(language): Promise<RecognizerStatus> {
      let microphone: VoicePermission = 'not-determined';
      try {
        const state = (await navigator.permissions.query({ name: 'microphone' as PermissionName })).state;
        microphone = state === 'granted' ? 'granted' : state === 'denied' ? 'denied' : 'not-determined';
      } catch { /* The browser asks when dictation starts. */ }
      const code = language || browserLanguage();
      return { available: true, onDevice: await locallyAvailable(Recognition, code), microphone, speech: 'granted', reason: '', language: browserLanguage() };
    },
    async languages(): Promise<RecognizerLanguage[]> {
      const own = typeof navigator !== 'undefined' ? [...navigator.languages || []] : [];
      return [...new Set([...own.filter(code => /^[a-z]{2,3}-[A-Z]{2}$/.test(code)), ...offered])].map(code => ({ code, name: languageName(code) }));
    },
    async start(options, listener: (event: RecognitionEvent) => void) {
      const lang = options.language || browserLanguage();
      const local = await locallyAvailable(Recognition, lang);
      // The browser ends a recognition on its own after a pause. Until the
      // person stops, a new one goes on listening; its results continue the
      // segment numbers, so nothing said before the pause is replaced.
      let recognition: Recognition | undefined;
      let ended = false, stopped = false, failed = false, heard = false, offset = 0, quietRounds = 0, lastHeard = Date.now();
      const finish = () => { if (!ended) { ended = true; listener({ kind: 'end' }); } };
      const begin = () => {
        const current = new Recognition();
        current.lang = lang;
        current.continuous = true;
        current.interimResults = true;
        current.maxAlternatives = 1;
        if ('processLocally' in current && local) current.processLocally = true;
        const round: { text: string; final: boolean }[] = [];
        let heardThisRound = false;
        current.onresult = event => {
          for (let i = event.resultIndex; i < event.results.length; i++) {
            const result = event.results[i];
            let text = result[0]?.transcript || '';
            // An empty update can arrive at a pause without a new transcript.
            if (!text.trim()) {
              if (!result.isFinal || !round[i]?.text) continue;
              text = round[i].text;
            }
            round[i] = { text, final: result.isFinal };
            heard = heardThisRound = true;
            lastHeard = Date.now();
            listener({ kind: 'result', segment: offset + i, text, final: result.isFinal });
          }
        };
        current.onerror = event => {
          if (event.error === 'aborted') return;
          // Silence after speech is a pause, not a problem.
          if (event.error === 'no-speech' && heard) return;
          failed = true;
          listener({ kind: 'error', code: event.error, message: errors[event.error] || event.message || `Speech recognition failed (${event.error}).` });
        };
        current.onend = () => {
          if (current !== recognition || ended) return;
          // What this round still had in progress is what was said.
          round.forEach((r, i) => { if (r && !r.final && r.text.trim()) listener({ kind: 'result', segment: offset + i, text: r.text, final: true }); });
          offset += round.length;
          quietRounds = heardThisRound ? 0 : quietRounds + 1;
          if (!stopped && !failed && heard && quietRounds < QUIET_ROUNDS && Date.now() - lastHeard < QUIET_LIMIT_MS) { begin(); return; }
          finish();
        };
        current.onspeechstart = () => listener({ kind: 'activity', speaking: true });
        current.onspeechend = () => listener({ kind: 'activity', speaking: false });
        recognition = current;
        current.start();
      };
      begin();
      return {
        async stop() { stopped = true; recognition?.stop(); },
        async cancel() { stopped = true; recognition?.abort(); finish(); },
      };
    },
  };
}

type Synthesis = {
  getVoices(): { voiceURI: string; name: string; lang: string; default?: boolean }[];
  speak(utterance: unknown): void; cancel(): void;
  addEventListener?(type: 'voiceschanged', listener: () => void): void;
  removeEventListener?(type: 'voiceschanged', listener: () => void): void;
};
type UtteranceClass = new (text: string) => { rate: number; lang: string; voice: unknown; onend: (() => void) | null; onerror: (() => void) | null };

/** speechSynthesis, or undefined where the browser has none. */
export function webSynthesizer(): SpeechSynthesizerHost | undefined {
  const scope = globalThis as unknown as { speechSynthesis?: Synthesis; SpeechSynthesisUtterance?: UtteranceClass };
  const synthesis = scope.speechSynthesis, Utterance = scope.SpeechSynthesisUtterance;
  if (!synthesis || !Utterance) return undefined;
  // Some browsers fill the list a moment after the page loads.
  const loaded = () => new Promise<ReturnType<Synthesis['getVoices']>>(resolve => {
    const now = synthesis.getVoices();
    if (now.length) { resolve(now); return; }
    const done = () => { synthesis.removeEventListener?.('voiceschanged', done); resolve(synthesis.getVoices()); };
    synthesis.addEventListener?.('voiceschanged', done);
    setTimeout(done, 1000);
  });
  return {
    async voices(): Promise<SpeechVoice[]> {
      return (await loaded()).map(voice => ({ id: voice.voiceURI, name: voice.name, language: voice.lang }));
    },
    async speak(text, { voice, rate }) {
      const voices = voice ? await loaded() : [];
      await new Promise<void>(resolve => {
        const utterance = new Utterance(text);
        utterance.rate = rate;
        const chosen = voices.find(v => v.voiceURI === voice);
        if (chosen) { utterance.voice = chosen; utterance.lang = chosen.lang; }
        utterance.onend = () => resolve();
        utterance.onerror = () => resolve();
        synthesis.cancel();
        synthesis.speak(utterance);
      });
    },
    async stop() { synthesis.cancel(); },
  };
}
