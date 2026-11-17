import { useCallback, useEffect, useRef, useState } from 'react';
import type { CodeRevert, TurnCheckpoint, TurnCheckpoints } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';
import type { TranscriptMessage } from './transcript-model';

/**
 * Where each turn's checkpoint marker goes: after the last message of the
 * turn, the one before the next prompt. Keys are message ids; values are the
 * operation id of the prompt that started the turn. The last turn's marker
 * goes after the last message, shown once that turn has ended.
 */
export function turnEnds(messages: readonly TranscriptMessage[]): Map<string, string> {
  const ends = new Map<string, string>();
  let operation = '';
  for (let i = 0; i < messages.length; i++) {
    const message = messages[i];
    if (message.type !== 'operation_accepted') continue;
    if (operation && i > 0) ends.set(messages[i - 1].id, operation);
    operation = message.operationId || '';
  }
  if (operation && messages.length) ends.set(messages[messages.length - 1].id, operation);
  return ends;
}

export const plural = (n: number, word: string) => `${n.toLocaleString()} ${word}${n === 1 ? '' : 's'}`;

/** "3 files changed · +40 −12", or what an empty turn is. */
export function turnSummary(turn: TurnCheckpoint): string {
  if (!turn.endedAtUnixMillis) return 'Checkpoint saved before this turn';
  if (!turn.files) return 'No file changes';
  const counted = lineCounts(turn.additions, turn.deletions);
  return `${plural(turn.files, 'file')} changed${counted ? ' · ' + counted : ''}`;
}

/** "+40 −2", "+40" or "−2": the line counts that are not zero. */
export function lineCounts(additions: number, deletions: number): string {
  return [additions ? `+${additions.toLocaleString()}` : '', deletions ? `−${deletions.toLocaleString()}` : ''].filter(Boolean).join(' ');
}

/** The latest revert, while it can still be undone. */
export function undoable(reverts: readonly CodeRevert[]): CodeRevert | undefined {
  const last = reverts.at(-1);
  return last && !last.undoneBy ? last : undefined;
}

type Loaded = { turns: Map<string, TurnCheckpoint>; byNumber: Map<number, TurnCheckpoint>; reverts: CodeRevert[] };
const empty: Loaded = { turns: new Map(), byNumber: new Map(), reverts: [] };
const index = (value: TurnCheckpoints): Loaded => ({
  turns: new Map(value.turns.map(turn => [turn.operationId, turn])),
  byNumber: new Map(value.turns.map(turn => [turn.turn, turn])),
  reverts: value.reverts,
});

/**
 * A conversation's turn checkpoints. Reloaded when a turn starts or ends
 * (busy changes) or a prompt appears in the conversation (latest, the last
 * prompt's operation id: a short turn can start and end between two status
 * polls), and briefly polled after a turn ends, since the Runner records the
 * after-checkpoint just after the turn. `reload` follows a revert.
 */
export function useTurnCheckpoints(api: RunnerClient, sessionId: string, busy: boolean, enabled: boolean, latest = '') {
  const [loaded, setLoaded] = useState<Loaded>(empty);
  const [revision, setRevision] = useState(0);
  const reload = useCallback(() => setRevision(value => value + 1), []);
  useEffect(() => { setLoaded(empty); }, [sessionId]);
  useEffect(() => {
    if (!enabled || !sessionId) return;
    const control = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined, attempts = 0;
    const load = async () => {
      try {
        const value = await api.listTurnCheckpoints({ sessionId }, { signal: control.signal, timeoutMs: 15000 });
        if (control.signal.aborted) return;
        setLoaded(index(value));
        // The turn just ended, or just started: its checkpoint is a moment behind.
        const behind = value.turns.some(turn => !turn.endedAtUnixMillis) || (!!latest && !value.turns.some(turn => turn.operationId === latest));
        if (!busy && behind && ++attempts < 10) timer = setTimeout(() => void load(), 600);
      } catch { /* checkpoints are an addition to the conversation; it reads without them */ }
    };
    void load();
    return () => { control.abort(); if (timer) clearTimeout(timer); };
  }, [api, sessionId, busy, enabled, revision, latest]);
  return { ...loaded, reload };
}

/** Asking a workspace window to show one turn's changes on its Changes page. */
export type TurnChangesRequest = { workspaceId: string; sessionId: string; turn: number };
const EVENT = 'lazybox-turn-changes';
const requested = new Map<string, TurnChangesRequest>();

/** True when a workspace window took the request; otherwise the caller shows the changes itself. */
export function showTurnChanges(request: TurnChangesRequest): boolean {
  requested.set(request.workspaceId, request);
  const event = new CustomEvent<TurnChangesRequest>(EVENT, { detail: request, cancelable: true });
  window.dispatchEvent(event);
  if (!event.defaultPrevented) requested.delete(request.workspaceId);
  return event.defaultPrevented;
}

/** A workspace shows its Changes page when one of its turns is asked for. */
export function useTurnChangesHost(workspaceId: string, show: () => void) {
  const ref = useRef(show); ref.current = show;
  useEffect(() => {
    if (!workspaceId) return;
    const listener = (event: Event) => {
      const request = (event as CustomEvent<TurnChangesRequest>).detail;
      if (request.workspaceId !== workspaceId || event.defaultPrevented) return;
      event.preventDefault(); ref.current();
    };
    window.addEventListener(EVENT, listener);
    return () => window.removeEventListener(EVENT, listener);
  }, [workspaceId]);
}

/** The Changes page takes the turn asked for: one asked before it mounted, then each later one. */
export function useRequestedTurn(workspaceId: string, take: (request: TurnChangesRequest) => void) {
  const ref = useRef(take); ref.current = take;
  useEffect(() => {
    const queued = requested.get(workspaceId);
    if (queued) { requested.delete(workspaceId); ref.current(queued); }
    const listener = (event: Event) => {
      const request = (event as CustomEvent<TurnChangesRequest>).detail;
      if (request.workspaceId !== workspaceId) return;
      // After the host's listener: the page is being shown; take it now.
      queueMicrotask(() => { if (requested.get(workspaceId) === request) { requested.delete(workspaceId); ref.current(request); } });
    };
    window.addEventListener(EVENT, listener);
    return () => window.removeEventListener(EVENT, listener);
  }, [workspaceId]);
}

/** Compare values of the Changes page for a turn: `:turn:<session>:<turn>`. */
export const turnCompare = (sessionId: string, turn: number) => `:turn:${sessionId}:${turn}`;
export function parseTurnCompare(value: string): { sessionId: string; turn: number } | undefined {
  const match = /^:turn:(.+):(\d+)$/.exec(value);
  return match ? { sessionId: match[1], turn: Number(match[2]) } : undefined;
}

/** What a reverted file will become, from the person's side. */
export function revertAction(status: string): string {
  return status === 'A' ? 'Restored' : status === 'D' ? 'Removed' : status === 'R' ? 'Renamed back' : 'Put back';
}
