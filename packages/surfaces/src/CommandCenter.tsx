import { useEffect, useRef, useState } from 'react';
import type { Collection } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { actionDefinition, actionEnabled, canonicalKey, chordFromEvent, isMac, runAction, useActions, type ActionId } from './actions';
import { conversationKey, isListed } from './attention';
import { CommandPalette, needsYou } from './CommandPalette';
import { mergeConversations } from './conversation-model';
import { Icon } from './Icons';
import { chordBindings, ownedByFocus, useKeymap, type Keymap } from './keyboard';
import { useRunner } from './RunnerContext';
import { ShortcutsOverlay } from './ShortcutsOverlay';
import { dismissToast, showToast, useToasts, type Toast } from './toasts';
import { useMergedInbox } from './RunnerDirectory';
import { rowTarget } from './runner-directory';
import type { Workbench } from './workbench';
import type { ViewTarget } from './workspace-model';
import { VoiceLayer } from './voice/VoiceLayer';
import './command-center.css';

/** What a window lets the palette and the navigation keys reach. */
export type CommandNavigator = {
  collections: Collection[];
  /** What the window shows now. */
  current: ViewTarget;
  /** Show a conversation (with a session) or a workspace (without). */
  goTo(target: ViewTarget): void;
  /** A detached window: it navigates the main window, not itself. */
  detached?: boolean;
  workbench?: Workbench;
  panels?: { kind: string; title: string; icon: string }[];
  showPanel?(kind: string): void;
  /** Panels in windows of their own: "Put <title> back in main window". */
  windows?: { id: string; title: string; icon: string; putBack(): void }[];
  openSettings?(tab: string): void;
};

/** In Monaco these chords are the editor's own (comment, indent, find, multi-cursor…). */
const editorKeys = new Set(['k', '/', '[', ']', 'd', 'f', 'h', 'g', 'l', 'u', 'i', 'a', 'c', 'v', 'x', 'z', 'y', 's', 'e', 'Enter']);
/** Actions that make sense held down (and those the registry marks `repeat`, such as resizing). */
const repeatable = new Set<string>(['conversation.next', 'conversation.previous', 'attention.next', 'attention.previous']);

/**
 * Chords of the keymap (⌘K, ⌘J, ⌘1…) fire from anywhere in the window, text
 * fields included, except where a chord belongs to what has focus: the file
 * editor keeps its own (⌘K, ⌘/ …) and a terminal keeps Ctrl chords, which the
 * shell uses; the file editor keeps ⌘⌥← / ⌘⌥→ for its tabs (`ownedByFocus`).
 * Off a Mac, Alt with a digit or an arrow is a chord too (`isChord`).
 * Sequences (`] ] i`) and bare keys (`?`) stay with the keyboard dispatcher in
 * keyboard.ts, outside text fields.
 */
export function useChordKeys(keymap: Keymap) {
  useEffect(() => {
    const chords = chordBindings(keymap);
    const keydown = (event: KeyboardEvent) => {
      if (event.isComposing || event.defaultPrevented || !(event.ctrlKey || event.metaKey || (event.altKey && !isMac))) return;
      const binding = canonicalKey(chordFromEvent(event));
      const id = chords.get(binding);
      if (!id) return;
      if (document.querySelector('dialog[open]') && !actionDefinition(id)?.overDialogs) return;
      const target = event.target instanceof Element ? event.target : null;
      const key = binding.split('+').at(-1) || '';
      if (target?.closest('.monaco-editor') && !binding.includes('Shift+') && editorKeys.has(key)) return;
      if (target?.closest('.xterm') && !binding.includes('Meta+') && !binding.includes('Shift+')) return;
      if (ownedByFocus(binding, target)) return;
      if (!actionEnabled(id)) return;
      event.preventDefault(); event.stopPropagation();
      if (event.repeat && !repeatable.has(id) && !actionDefinition(id)?.repeat) return;
      runAction(id, event.repeat ? { repeat: true } : undefined, 'keys');
    };
    window.addEventListener('keydown', keydown, true);
    return () => window.removeEventListener('keydown', keydown, true);
  }, [keymap]);
}

/**
 * One per window: the chord keys, the command palette, the shortcuts overlay,
 * the navigation actions (⌘1…9, next conversation that needs you, next and
 * previous conversation) and the toasts.
 */
export function CommandCenter({ viewState, navigator }: { viewState?: ViewStateStore; navigator: CommandNavigator }) {
  const keymap = useKeymap(viewState);
  const { info } = useRunner();
  const inbox = useMergedInbox();
  const [palette, setPalette] = useState<{ query: string } | null>(null);
  const [shortcuts, setShortcuts] = useState(false);
  useChordKeys(keymap);
  const latest = useRef({ navigator, inbox, info }); latest.current = { navigator, inbox, info };

  /** Cycle through the conversations that need the person, across workspaces. */
  const jump = (step: 1 | -1) => {
    const { navigator, inbox } = latest.current;
    const rows = needsYou(inbox.rows);
    if (!rows.length) { showToast({ id: 'attention:none', tone: 'info', title: 'Nothing needs you right now' }); return; }
    const index = rows.findIndex(row => row.runner === inbox.here && row.session.id === navigator.current.sessionId);
    if (index >= 0 && rows.length === 1) { showToast({ id: 'attention:none', tone: 'info', title: 'This is the only conversation that needs you' }); return; }
    const row = rows[index < 0 ? (step > 0 ? 0 : rows.length - 1) : (index + step + rows.length) % rows.length];
    // Across Runners: the next one may be on another Runner, whose page then shows.
    navigator.goTo(row.runner === inbox.here ? { collectionId: row.session.collectionId, sessionId: row.session.id, commandId: '' } : rowTarget(row));
  };
  /** The next or previous conversation of this workspace, in sidebar order (newest first). */
  const stepConversation = (step: 1 | -1) => {
    const { navigator, info } = latest.current;
    const rows = mergeConversations([], info?.sessions || []).filter(session => session.collectionId === navigator.current.collectionId && isListed(session));
    if (!rows.length) { showToast({ id: 'conversation:none', tone: 'info', title: 'No conversations in this workspace yet' }); return; }
    const current = rows.find(session => session.id === navigator.current.sessionId);
    const index = current ? rows.findIndex(session => conversationKey(session) === conversationKey(current)) : -1;
    const next = rows[index < 0 ? 0 : (index + step + rows.length) % rows.length];
    if (next.id !== navigator.current.sessionId) navigator.goTo({ collectionId: next.collectionId, sessionId: next.id, commandId: '' });
  };
  const slots = Object.fromEntries(Array.from({ length: 9 }, (_, i) => [`workspace.select.${i + 1}`, {
    run: () => { const collection = latest.current.navigator.collections[i]; if (collection) latest.current.navigator.goTo({ collectionId: collection.id, sessionId: '', commandId: '' }); },
    enabled: () => latest.current.navigator.collections.length > i,
  }]));
  useActions({
    'palette.open': (argument?: unknown) => { setShortcuts(false); setPalette(value => value && typeof argument !== 'string' ? null : { query: typeof argument === 'string' ? argument : '' }); },
    'shortcuts.open': () => { setPalette(null); setShortcuts(value => !value); },
    'attention.next': () => jump(1),
    'attention.previous': () => jump(-1),
    'conversation.next': { run: () => stepConversation(1), enabled: () => !latest.current.navigator.detached && !!latest.current.navigator.current.collectionId },
    'conversation.previous': { run: () => stepConversation(-1), enabled: () => !latest.current.navigator.detached && !!latest.current.navigator.current.collectionId },
    'composer.focus': { run: () => document.querySelector<HTMLElement>('[data-agent-composer]:not(:disabled)')?.focus(), enabled: () => !!document.querySelector('[data-agent-composer]:not(:disabled)') },
    ...slots,
  } as Parameters<typeof useActions>[0]);
  return <>
    {palette && <CommandPalette keymap={keymap} navigator={navigator} viewState={viewState} initialQuery={palette.query} onClose={() => setPalette(null)}/>}
    {shortcuts && <ShortcutsOverlay keymap={keymap} onClose={() => setShortcuts(false)}/>}
    <VoiceLayer viewState={viewState} keymap={keymap} onOpenSettings={navigator.openSettings ? () => navigator.openSettings!('voice') : undefined}/>
    <ToastStack/>
  </>;
}

/** Toasts, newest at the bottom; each leaves on its own unless hovered or focused. */
export function ToastStack() {
  const toasts = useToasts();
  if (!toasts.length) return null;
  return <section className="toast-stack" aria-label="Notifications" aria-live="polite">
    {toasts.map(toast => <ToastCard key={toast.id} toast={toast}/>)}
  </section>;
}
const toneIcon: Record<Toast['tone'], string> = { 'needs-input': 'ask', failed: 'alert', review: 'eye', info: 'check' };
function ToastCard({ toast }: { toast: Toast }) {
  const [paused, setPaused] = useState(false);
  useEffect(() => {
    if (paused) return;
    const timer = setTimeout(() => dismissToast(toast.id), toast.timeout);
    return () => clearTimeout(timer);
  }, [paused, toast]);
  const open = () => { dismissToast(toast.id); toast.action?.run(); };
  return <article className="toast" data-tone={toast.tone} role="status"
    onPointerEnter={() => setPaused(true)} onPointerLeave={() => setPaused(false)} onFocus={() => setPaused(true)} onBlur={() => setPaused(false)}>
    <span className={`toast-icon attention-icon attention-${toast.tone}`} aria-hidden="true"><Icon name={toneIcon[toast.tone]} size={15}/></span>
    {toast.action ? <button type="button" className="toast-main" onClick={open}><strong>{toast.title}</strong>{toast.body && <span>{toast.body}</span>}</button>
      : <div className="toast-main"><strong>{toast.title}</strong>{toast.body && <span>{toast.body}</span>}</div>}
    {toast.button && <button type="button" className="toast-button" onClick={() => { dismissToast(toast.id); toast.button!.run(); }}>{toast.button.label}</button>}
    {toast.hint && <kbd className="toast-hint" title="Next conversation that needs you">{toast.hint}</kbd>}
    <button type="button" className="icon-button toast-close" aria-label="Dismiss" onClick={() => dismissToast(toast.id)}><Icon name="close" size={13}/></button>
  </article>;
}
