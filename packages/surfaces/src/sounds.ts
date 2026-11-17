/**
 * Notification sounds, one per kind:
 * - "request": an agent needs you (a question, an approval, a failure);
 * - "done": an agent finished its turn.
 *
 * Each kind plays the person's choice (Settings → Notifications → Sounds):
 * Lazybox's own soft chime (synthesized with Web Audio, no audio file), a
 * macOS system sound (Glass, Ping…, played by the desktop host), or a short
 * sound file of their own. One volume for all, and optionally only while
 * Lazybox is in the background. The choice belongs to this computer (its
 * speakers), so it is kept in this browser's storage, not on the Runner.
 *
 * Browsers start an AudioContext suspended until the person interacts with
 * the page, so the first click or key in the window unlocks it; files play
 * through that same context.
 */
import { useSyncExternalStore } from 'react';

export type AlertSound = 'request' | 'done';
export const ALERT_SOUNDS: readonly AlertSound[] = ['request', 'done'];
/** Lazybox's chime, a system sound by name, or the file stored for that kind. */
export type SoundSource = { kind: 'builtin' } | { kind: 'system'; name: string } | { kind: 'file'; name: string };
export type SoundSettings = {
  /** 0 (silent) to 1. */
  volume: number;
  /** Play only when no Lazybox window is in front. */
  backgroundOnly: boolean;
  sources: Record<AlertSound, SoundSource>;
};
export const DEFAULT_SOUNDS: SoundSettings = { volume: 0.5, backgroundOnly: false, sources: { request: { kind: 'builtin' }, done: { kind: 'builtin' } } };
/** The alert sounds every Mac has in /System/Library/Sounds. */
export const SYSTEM_SOUNDS = ['Basso', 'Blow', 'Bottle', 'Frog', 'Funk', 'Glass', 'Hero', 'Morse', 'Ping', 'Pop', 'Purr', 'Sosumi', 'Submarine', 'Tink'] as const;
/** A sound file is a short alert, not music: kept small enough for this browser's storage. */
export const SOUND_FILE_LIMIT = 512 * 1024;

const settingsKey = 'lazybox:sounds:v1';
const fileKey = (kind: AlertSound) => `lazybox:sound-file:${kind}`;
const changed = 'lazybox-sounds';

/** Pure: stored settings, with anything unknown or out of range replaced by the default. */
export function parseSoundSettings(text: string | null): SoundSettings {
  let value: unknown;
  try { value = JSON.parse(text || 'null'); } catch { return DEFAULT_SOUNDS; }
  if (!value || typeof value !== 'object') return DEFAULT_SOUNDS;
  const raw = value as Partial<{ volume: unknown; backgroundOnly: unknown; sources: Record<string, unknown> }>;
  const volume = typeof raw.volume === 'number' && Number.isFinite(raw.volume) ? Math.max(0, Math.min(1, raw.volume)) : DEFAULT_SOUNDS.volume;
  const source = (kind: AlertSound): SoundSource => {
    const s = raw.sources?.[kind] as { kind?: unknown; name?: unknown } | undefined;
    if (s?.kind === 'system' && typeof s.name === 'string' && (SYSTEM_SOUNDS as readonly string[]).includes(s.name)) return { kind: 'system', name: s.name };
    if (s?.kind === 'file' && typeof s.name === 'string' && s.name && s.name.length <= 255) return { kind: 'file', name: s.name };
    return { kind: 'builtin' };
  };
  return { volume, backgroundOnly: raw.backgroundOnly === true, sources: { request: source('request'), done: source('done') } };
}

function storage(): Storage | undefined { try { return globalThis.localStorage; } catch { return undefined; } }
let cachedText: string | null | undefined, cached = DEFAULT_SOUNDS;
export function readSoundSettings(): SoundSettings {
  let text: string | null = null;
  try { text = storage()?.getItem(settingsKey) ?? null; } catch { /* unavailable storage: the defaults */ }
  if (text !== cachedText) { cachedText = text; cached = parseSoundSettings(text); }
  return cached;
}
export function writeSoundSettings(next: SoundSettings): void {
  try { storage()?.setItem(settingsKey, JSON.stringify(next)); } catch { /* kept for this window only */ cachedText = undefined; cached = next; }
  window.dispatchEvent(new Event(changed));
}
function subscribe(listener: () => void) {
  window.addEventListener('storage', listener); window.addEventListener(changed, listener);
  return () => { window.removeEventListener('storage', listener); window.removeEventListener(changed, listener); };
}
export function useSoundSettings(): [SoundSettings, (next: SoundSettings) => void] {
  return [useSyncExternalStore(subscribe, readSoundSettings), writeSoundSettings];
}

/** What the desktop app adds: macOS system sounds, played by the host. */
export type SystemSoundHost = { play(name: string, volume: number): Promise<void> };
let systemSounds: SystemSoundHost | undefined;
export function configureSystemSounds(host: SystemSoundHost | undefined) { systemSounds = host; }
export const systemSoundsAvailable = () => !!systemSounds;

type Context = AudioContext;
let context: Context | undefined;
const lastPlayed: Record<AlertSound, number> = { request: 0, done: 0 };
const notes: Record<AlertSound, [frequency: number, start: number, length: number][]> = {
  request: [[659.25, 0, 0.16], [987.77, 0.12, 0.24]],
  done: [[880, 0, 0.14], [659.25, 0.1, 0.26]],
};

function audio(): Context | undefined {
  if (context) return context;
  const Constructor = (globalThis as { AudioContext?: typeof AudioContext; webkitAudioContext?: typeof AudioContext }).AudioContext || (globalThis as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
  if (!Constructor) return undefined;
  try { context = new Constructor(); } catch { return undefined; }
  return context;
}

if (typeof window !== 'undefined') {
  const unlock = () => {
    if (context?.state === 'suspended') void context.resume().catch(() => {});
    if (context && context.state !== 'suspended') { window.removeEventListener('pointerdown', unlock, true); window.removeEventListener('keydown', unlock, true); }
  };
  window.addEventListener('pointerdown', unlock, true);
  window.addEventListener('keydown', unlock, true);
}

function chime(ctx: Context, kind: AlertSound, volume: number) {
  const start = ctx.currentTime + 0.01;
  const master = ctx.createGain();
  // 0.5, the default, is the chime's original level.
  master.gain.value = 0.14 * volume;
  master.connect(ctx.destination);
  for (const [frequency, offset, length] of notes[kind]) {
    const oscillator = ctx.createOscillator(), envelope = ctx.createGain();
    oscillator.type = 'sine';
    oscillator.frequency.value = frequency;
    envelope.gain.setValueAtTime(0.0001, start + offset);
    envelope.gain.exponentialRampToValueAtTime(1, start + offset + 0.012);
    envelope.gain.exponentialRampToValueAtTime(0.0001, start + offset + length);
    oscillator.connect(envelope); envelope.connect(master);
    oscillator.start(start + offset); oscillator.stop(start + offset + length + 0.02);
  }
}

// Decoded files, by kind and the stored data they came from.
const decoded = new Map<AlertSound, { data: string; buffer: Promise<AudioBuffer> }>();
function dataBytes(data: string): ArrayBuffer {
  const base64 = data.slice(data.indexOf(',') + 1), binary = atob(base64), bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes.buffer;
}
function bufferFor(ctx: Context, kind: AlertSound): Promise<AudioBuffer> | undefined {
  let data: string | null = null;
  try { data = storage()?.getItem(fileKey(kind)) ?? null; } catch { /* no file */ }
  if (!data) return undefined;
  const known = decoded.get(kind);
  if (known?.data === data) return known.buffer;
  const buffer = ctx.decodeAudioData(dataBytes(data));
  decoded.set(kind, { data, buffer });
  return buffer;
}
function playBuffer(ctx: Context, buffer: AudioBuffer, volume: number) {
  const source = ctx.createBufferSource(), gain = ctx.createGain();
  gain.gain.value = volume;
  source.buffer = buffer; source.connect(gain); gain.connect(ctx.destination);
  source.start();
}

/** True when a Lazybox window is in front: "only in the background" stays quiet then. */
const inFront = () => typeof document !== 'undefined' && document.visibilityState === 'visible' && document.hasFocus();

/**
 * Plays a kind's sound, at most once a second and a half per kind: a burst of
 * agents is one sound. `force` (a preview) plays now, even in front;
 * `settings` previews a choice before it is saved.
 */
export function playAlert(kind: AlertSound, { force = false, settings }: { force?: boolean; settings?: SoundSettings } = {}): boolean {
  const now = Date.now();
  if (!force && now - lastPlayed[kind] < 1500) return false;
  const chosen = settings || readSoundSettings();
  if (!force && chosen.backgroundOnly && inFront()) return false;
  const source = chosen.sources[kind];
  if (source.kind === 'system' && systemSounds) {
    lastPlayed[kind] = now;
    void systemSounds.play(source.name, chosen.volume).catch(() => { const ctx = audio(); if (ctx) chime(ctx, kind, chosen.volume); });
    return true;
  }
  const ctx = audio();
  if (!ctx) return false;
  lastPlayed[kind] = now;
  if (ctx.state === 'suspended') void ctx.resume().catch(() => {});
  const file = source.kind === 'file' ? bufferFor(ctx, kind) : undefined;
  if (file) void file.then(buffer => playBuffer(ctx, buffer, chosen.volume), () => chime(ctx, kind, chosen.volume));
  else chime(ctx, kind, chosen.volume);
  return true;
}

/**
 * Keeps a sound file for a kind, once it is known to decode: a short alert
 * (audio, at most SOUND_FILE_LIMIT). Resolves to the error to show, or ''.
 */
export async function storeSoundFile(kind: AlertSound, file: File): Promise<string> {
  if (!file.type.startsWith('audio/') && !/\.(aiff?|wav|mp3|m4a|aac|caf|ogg|flac)$/i.test(file.name)) return 'Choose an audio file (WAV, AIFF, MP3, M4A…).';
  if (file.size > SOUND_FILE_LIMIT) return `Choose a short sound: this one is ${Math.round(file.size / 1024)} KB, the limit is ${SOUND_FILE_LIMIT / 1024} KB.`;
  const data = await new Promise<string>((resolve, reject) => { const reader = new FileReader(); reader.onload = () => resolve(String(reader.result)); reader.onerror = () => reject(reader.error); reader.readAsDataURL(file); });
  const ctx = audio();
  if (ctx) { try { await ctx.decodeAudioData(dataBytes(data)); } catch { return 'Lazybox cannot play this file. Try a WAV, AIFF or MP3.'; } }
  try { storage()?.setItem(fileKey(kind), data); } catch { return 'There is no room left to keep this sound. Try a shorter one.'; }
  decoded.delete(kind);
  return '';
}
