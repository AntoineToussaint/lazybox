import type { RunnerClient } from '@lazybox/client-core';
import type { SpeechRecognizerHost, VoicePermission } from '@lazybox/platform';
import { humanError } from '../human-error';

const sampleRate = 16000;
const maxSeconds = 60;

function encodeWav(samples: Float32Array): Uint8Array<ArrayBuffer> {
  const wav = new Uint8Array(44 + samples.length * 2);
  const view = new DataView(wav.buffer);
  const write = (offset: number, text: string) => wav.set(new TextEncoder().encode(text), offset);
  write(0, 'RIFF');
  view.setUint32(4, wav.length - 8, true);
  write(8, 'WAVEfmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  write(36, 'data');
  view.setUint32(40, wav.length - 44, true);
  for (let index = 0; index < samples.length; index++) {
    const sample = Math.max(-1, Math.min(1, samples[index]));
    view.setInt16(44 + index * 2, sample * (sample < 0 ? 32768 : 32767), true);
  }
  return wav;
}

async function recordingWav(recording: Blob): Promise<Uint8Array<ArrayBuffer>> {
  const decoding = new AudioContext();
  try {
    const original = await decoding.decodeAudioData(await recording.arrayBuffer());
    const samples = Math.min(sampleRate * maxSeconds, Math.round(original.duration * sampleRate));
    if (!samples) return new Uint8Array(0);
    const offline = new OfflineAudioContext(1, samples, sampleRate);
    const source = offline.createBufferSource();
    source.buffer = original;
    source.connect(offline.destination);
    source.start();
    return encodeWav((await offline.startRendering()).getChannelData(0));
  } finally {
    await decoding.close();
  }
}

export function wisprRecognizer(api: RunnerClient): SpeechRecognizerHost | undefined {
  if (!globalThis.navigator?.mediaDevices?.getUserMedia || typeof MediaRecorder === 'undefined' ||
    typeof AudioContext === 'undefined' || typeof OfflineAudioContext === 'undefined') return undefined;
  return {
    id: 'wispr',
    label: 'Wispr Flow',
    stopGraceMs: 30_000,
    async status(language) {
      const { configured } = await api.wisprStatus({}, { timeoutMs: 4000 }).catch(() => ({ configured: false }));
      let microphone: VoicePermission = 'not-determined';
      try {
        const { state } = await navigator.permissions.query({ name: 'microphone' as PermissionName });
        if (state !== 'prompt') microphone = state;
      } catch { /* WKWebView may defer the question to recording time */ }
      return { available: configured && microphone !== 'denied', onDevice: false, microphone, speech: 'unsupported',
        reason: configured ? '' : 'Set LAZYBOX_WISPR_API_KEY on this Runner to use Wispr Flow.', language };
    },
    async languages() { return []; },
    async start(options, listener) {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true }).catch(error => {
        if ((error as DOMException).name === 'NotAllowedError') throw new Error('microphone-permission: Allow microphone access for Lazybox.');
        throw error;
      });
      const closeTracks = () => { for (const track of stream.getTracks()) track.stop(); };
      const control = new AbortController();
      const chunks: Blob[] = [];
      let recorder: MediaRecorder;
      try { recorder = new MediaRecorder(stream); } catch (error) { closeTracks(); throw error; }
      let timer: ReturnType<typeof setTimeout>;
      const stop = async () => {
        clearTimeout(timer);
        if (recorder.state !== 'inactive') recorder.stop();
        closeTracks();
      };
      recorder.ondataavailable = event => { if (event.data.size && !control.signal.aborted) chunks.push(event.data); };
      recorder.onerror = () => {
        void stop();
        if (!control.signal.aborted) listener({ kind: 'error', code: 'microphone', message: 'The microphone recording failed.' });
      };
      recorder.onstop = async () => {
        clearTimeout(timer);
        closeTracks();
        if (control.signal.aborted) return;
        try {
          if (chunks.length) {
            const wav = await recordingWav(new Blob(chunks, { type: recorder.mimeType }));
            if (wav.length && !control.signal.aborted) {
              const { text } = await api.transcribeWispr({ wav, language: options.language }, { signal: control.signal, timeoutMs: 25_000 });
              if (text && !control.signal.aborted) listener({ kind: 'result', segment: 0, text, final: true });
            }
          }
          if (!control.signal.aborted) listener({ kind: 'end' });
        } catch (error) {
          if (!control.signal.aborted) listener({ kind: 'error', code: 'wispr-flow', message: humanError(error) });
        }
      };
      try { recorder.start(); } catch (error) { closeTracks(); throw error; }
      timer = setTimeout(() => void stop(), maxSeconds * 1000);
      return { stop, cancel: async () => { control.abort(); chunks.length = 0; await stop(); } };
    },
  };
}
