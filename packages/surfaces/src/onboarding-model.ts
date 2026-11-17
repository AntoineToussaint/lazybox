/**
 * Onboarding: the pure decisions. When the first-run welcome shows, when
 * "What's new" shows after an update, and which keys the welcome teaches.
 * Rendering lives in Onboarding.tsx; the release notes in release-notes.ts.
 */
import type { Keymap } from './keyboard';

/** Preference names (view-state.ts `readPreference` / `writePreference`). */
export const WELCOME_PREFERENCE = 'onboarding.welcome';
export const WHATS_NEW_PREFERENCE = 'onboarding.whats-new';

/** One change. A big release groups its changes (`### Group` in CHANGELOG.md). */
export type ReleaseNote = { title: string; body: string; group?: string };
export type Release = { id: string; date: string; title: string; items: ReleaseNote[] };

/** A release's items as runs of one group each, in order ('' for ungrouped items). */
export function releaseGroups(items: readonly ReleaseNote[]): { group: string; items: ReleaseNote[] }[] {
  const runs: { group: string; items: ReleaseNote[] }[] = [];
  for (const item of items) {
    const group = item.group || '';
    if (runs.at(-1)?.group !== group) runs.push({ group, items: [] });
    runs.at(-1)!.items.push(item);
  }
  return runs;
}

/**
 * The welcome is for a fresh install only: nothing set up yet. A person who
 * already has workspaces or conversations the first time this build runs is
 * an existing user; the welcome is recorded as seen without showing it.
 *
 * - `none`: seen before (or recorded), nothing to do;
 * - `wait`: the Runner has not answered yet, so "empty" is not known;
 * - `record`: an existing user, mark it seen silently;
 * - `show`: a fresh install, show it once.
 */
export type WelcomeDecision = 'none' | 'wait' | 'record' | 'show';
export function welcomeDecision(state: { seen: boolean; workspacesLoaded: boolean; workspaces: number; conversationsLoaded: boolean; conversations: number }): WelcomeDecision {
  if (state.seen) return 'none';
  // Anything the Runner already lists is enough to know this is not a first run.
  if ((state.workspacesLoaded && state.workspaces > 0) || (state.conversationsLoaded && state.conversations > 0)) return 'record';
  if (!state.workspacesLoaded || !state.conversationsLoaded) return 'wait';
  return 'show';
}

/**
 * What's new after an update, by release id (newest first in `releases`).
 * Nothing stored is a first run (or a fresh browser profile): the newest is
 * recorded silently, never shown. The stored id being an older release shows
 * every release since it; an id this build does not know (notes pruned, or a
 * downgrade) shows the newest release only.
 */
export type WhatsNewDecision = { action: 'none' } | { action: 'record'; id: string } | { action: 'show'; releases: Release[]; id: string };
export function whatsNewDecision(seen: string | null | undefined, releases: readonly Release[]): WhatsNewDecision {
  const newest = releases[0];
  if (!newest) return { action: 'none' };
  if (!seen) return { action: 'record', id: newest.id };
  if (seen === newest.id) return { action: 'none' };
  const index = releases.findIndex(release => release.id === seen);
  return { action: 'show', id: newest.id, releases: index > 0 ? releases.slice(0, index) : [newest] };
}

const arrows: Record<string, string> = { ArrowDown: '↓', ArrowUp: '↑', ArrowLeft: '←', ArrowRight: '→' };
/** One key binding as separate keys to draw ("] ] c" → ["]", "]", "c"]; "ArrowDown" → "↓"). */
export const keyParts = (binding: string) => binding.trim().split(/\s+/).filter(Boolean).map(key => arrows[key] || key);
/** The binding a lesson teaches when an action has several ("⌘⇧N or ] ] c"): its leader sequence, the system the welcome explains. */
const lessonKeys = (value: string | undefined) => { const all = (value || '').split(/\s+or\s+/).map(b => b.trim()).filter(Boolean); return keyParts(all.find(b => b.includes(' ')) || all[0] || ''); };

export type KeyLesson = { keys: string[][]; label: string; detail: string };
/**
 * The five keys that matter, from the person's own keymap (vim, arrows or
 * tmux preset, or their edits). `goToFile` is the platform's Go to file
 * chord, already formatted (⌘P on a Mac); `chord` formats a chord binding
 * such as New task's ("Mod+n" → ["⌘N"]).
 */
export function essentialKeys(map: Keymap, goToFile: string, chord: (binding: string) => string[] = keyParts): KeyLesson[] {
  const firstBinding = (value: string | undefined) => (value || '').split(/\s+or\s+/)[0]?.trim() || '';
  return [
    { keys: [keyParts(map['navigation.exit'])], label: 'Leave the agent', detail: 'From the message box to navigation; every other shortcut starts here.' },
    // One name for starting work: New task (the header's button, the welcome's step) makes the workspace and starts the agent.
    { keys: [chord(firstBinding(map['task.new']))], label: 'New task', detail: 'Repositories, an agent and what to do, on a branch of its own.' },
    { keys: [[goToFile]], label: 'Go to file', detail: 'Fuzzy-find any file in the workspace, from anywhere.' },
    { keys: [keyParts(map['inbox.open'])], label: 'Inbox', detail: 'Everything waiting for you: questions, approvals, finished work.' },
    { keys: [keyParts(map['navigation.next']), keyParts(map['navigation.previous'])], label: 'Next / previous', detail: 'Move through workspaces and conversations in navigation.' },
  ];
}
