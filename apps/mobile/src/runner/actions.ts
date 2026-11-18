import { useCallback, useEffect, useRef, useState } from 'react';
import { ActionSheetIOS, Alert } from 'react-native';
import { randomUUID } from 'expo-crypto';
import { ConnectError, type RunnerClient } from '@lazybox/client-core';
import { Decision } from '@lazybox/protocol';
import { humanError, type Risk } from '@lazybox/surfaces/models';
import { haptic } from '../ui/kit';

type Operation = Parameters<RunnerClient['apply']>[0]['operation'];

/** Apply one operation to a conversation; a fresh operation id each time (an answer is not a message). */
export async function applyOperation(api: RunnerClient, sessionId: string, operation: Operation): Promise<void> {
  try { await api.apply({ sessionId, operationId: randomUUID(), operation }, { timeoutMs: 30000 }); }
  catch (error) { throw new Error(humanError(ConnectError.from(error))); }
}
export const approval = (requestId: string, accept: boolean, reason = ''): Operation =>
  ({ case: 'approval', value: { requestId, decision: accept ? Decision.ACCEPT : Decision.DECLINE, always: '', reason } });
export const answer = (requestId: string, answers: Record<string, { values: string[] }>): Operation =>
  ({ case: 'answer', value: { requestId, answers } });

/** Asks before allowing what the computer flags (risks, or the provider's default-to-no), as the PWA's sheet does. Cancel is the default. */
export function confirmAllow({ what, risks, defaultToNo, agentName }: { what: string; risks: Risk[]; defaultToNo: boolean; agentName: string }): Promise<boolean> {
  const lines = [what, '', ...risks.map(risk => `⚠︎ ${risk.text}`), ...(defaultToNo && !risks.length ? [`${agentName} suggests saying no unless you are sure.`] : [])];
  haptic.warning();
  return new Promise(resolve => Alert.alert('Allow this?', lines.join('\n').trim(), [
    { text: 'Cancel', style: 'cancel', onPress: () => resolve(false) },
    { text: 'Allow once', onPress: () => resolve(true) },
  ], { cancelable: true, onDismiss: () => resolve(false) }));
}

/** Deny, with an optional reason for the agent. Resolves undefined when cancelled. */
export function askDenyReason(what: string, agentName: string): Promise<string | undefined> {
  return new Promise(resolve => Alert.prompt('Deny this?', `${what}\n\nTell ${agentName} why (optional).`, [
    { text: 'Cancel', style: 'cancel', onPress: () => resolve(undefined) },
    { text: 'Deny', style: 'destructive', onPress: (reason?: string) => resolve(reason?.trim() || '') },
  ], 'plain-text'));
}

export function actionSheet(title: string, message: string, choices: { label: string; destructive?: boolean; run(): void }[]) {
  haptic.select();
  ActionSheetIOS.showActionSheetWithOptions({
    title, message, options: [...choices.map(choice => choice.label), 'Cancel'], cancelButtonIndex: choices.length,
    destructiveButtonIndex: choices.flatMap((choice, i) => choice.destructive ? [i] : []),
  }, index => { if (index < choices.length) choices[index].run(); });
}

export type Deferred = { label: string; phase: 'waiting' | 'sending' | 'sent' | 'failed'; error?: string };
export const UNDO_MS = 4500;

/**
 * An answer from an Inbox row waits a moment for Undo, then goes (REMOTE.md:
 * 4.5 s). Leaving the screen sends what is waiting at once. Once sent, the row
 * says so until the Runner no longer lists the request.
 */
export function useDeferred() {
  const [states, setStates] = useState<Record<string, Deferred>>({});
  const timers = useRef(new Map<string, { timer: ReturnType<typeof setTimeout>; run(): void }>());
  const set = (id: string, value: Deferred | undefined) => setStates(all => { const next = { ...all }; if (value) next[id] = value; else delete next[id]; return next; });
  const schedule = useCallback((id: string, label: string, commit: () => Promise<void>, delay = UNDO_MS) => {
    const run = () => {
      timers.current.delete(id);
      set(id, { label, phase: 'sending' });
      commit().then(() => { haptic.success(); set(id, { label, phase: 'sent' }); },
        error => { haptic.error(); set(id, undefined); Alert.alert('Not sent', error instanceof Error ? error.message : String(error)); });
    };
    set(id, { label, phase: delay ? 'waiting' : 'sending' });
    if (!delay) { run(); return; }
    timers.current.set(id, { timer: setTimeout(run, delay), run });
  }, []);
  const cancel = useCallback((id: string) => { const entry = timers.current.get(id); if (entry) { clearTimeout(entry.timer); timers.current.delete(id); } haptic.tap(); set(id, undefined); }, []);
  /** Forget what the Runner no longer lists (answered, here or on the computer). */
  const settle = useCallback((present: ReadonlySet<string>) => setStates(all => {
    const gone = Object.keys(all).filter(id => all[id].phase === 'sent' && !present.has(id));
    if (!gone.length) return all;
    const next = { ...all }; for (const id of gone) delete next[id]; return next;
  }), []);
  // Leaving the screen sends what waits, at once: an answer never silently stays behind.
  useEffect(() => () => { for (const entry of timers.current.values()) { clearTimeout(entry.timer); entry.run(); } }, []);
  return { states, schedule, cancel, settle };
}
