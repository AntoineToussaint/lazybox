import { useEffect, useRef, useState } from 'react';
import { AppState, Keyboard } from 'react-native';
import { ExpoSpeechRecognitionModule as speech, useSpeechRecognitionEvent } from 'expo-speech-recognition';

type Phase = 'idle' | 'starting' | 'listening' | 'finishing';
type Capture = { transcript: string; started: boolean };

export function useDictation(enabled: boolean, insert: (text: string) => void) {
  const active = useRef<Capture | undefined>(undefined);
  const [phase, setPhase] = useState<Phase>('idle');
  const [preview, setPreview] = useState('');
  const [error, setError] = useState('');

  useEffect(() => {
    function cancel() {
      const capture = active.current;
      active.current = undefined;
      if (capture?.started) speech.abort();
    }
    if (!enabled) {
      cancel();
      setPhase('idle');
      setPreview('');
      setError('');
      return;
    }
    const subscription = AppState.addEventListener('change', state => {
      if (state !== 'background') return;
      cancel();
      setPhase('idle');
      setPreview('');
    });
    return () => { subscription.remove(); cancel(); };
  }, [enabled]);

  useSpeechRecognitionEvent('start', () => {
    if (active.current) setPhase(current => current === 'starting' ? 'listening' : current);
  });
  useSpeechRecognitionEvent('result', event => {
    const transcript = event.results[0]?.transcript.trim();
    if (!active.current || !transcript) return;
    active.current.transcript = transcript;
    setPreview(transcript);
  });
  useSpeechRecognitionEvent('error', event => {
    if (active.current) setError(event.error === 'no-speech' ? 'No speech heard. Try dictating again.' : event.message || 'Dictation stopped. Try again.');
  });
  useSpeechRecognitionEvent('end', () => {
    const capture = active.current;
    if (!capture?.started) return;
    active.current = undefined;
    if (capture.transcript) insert(capture.transcript);
    setPreview('');
    setPhase('idle');
  });

  async function toggle() {
    if (phase === 'listening') {
      setPhase('finishing');
      speech.stop();
      return;
    }
    if (!enabled || active.current) return;
    const capture = { transcript: '', started: false };
    active.current = capture;
    setError('');
    setPhase('starting');
    try {
      const permission = await speech.requestPermissionsAsync();
      if (active.current !== capture) return;
      if (!permission.granted) throw new Error('Allow microphone and speech recognition in iOS Settings to dictate.');
      if (!speech.isRecognitionAvailable()) throw new Error('Speech recognition is unavailable. Try again or type your message.');
      Keyboard.dismiss();
      speech.start({
        lang: Intl.DateTimeFormat().resolvedOptions().locale,
        continuous: true,
        interimResults: true,
        addsPunctuation: true,
      });
      capture.started = true;
    } catch (failure) {
      if (active.current !== capture) return;
      active.current = undefined;
      setPhase('idle');
      setError(failure instanceof Error ? failure.message : 'Dictation could not start.');
    }
  }

  return { phase, preview, error, toggle };
}
