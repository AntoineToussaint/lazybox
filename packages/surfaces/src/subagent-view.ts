import { createContext, useContext, useSyncExternalStore } from 'react';
import type { Session } from '@lazybox/protocol';
import { ancestry, canStopSubagent, emptySubagents, panelAgent, type Subagent, type SubagentStatus, type Subagents } from './subagent-model';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';

export type SubagentPanelMode = 'float' | 'dock';
export type StopOutcome = 'stopping' | 'gone' | 'failed';
/** Cleared and seen items remembered per conversation: the newest. */
const CLEARED_LIMIT = 1000;
const clearedKey = (conversation: string) => `lazybox.background-work.cleared.${conversation}`;
const seenKey = (conversation: string) => `lazybox.background-work.seen.${conversation}`;
const load = (key: string): Set<string> => {
  let ids: unknown = [];
  try { ids = JSON.parse(localStorage.getItem(key) || '[]'); } catch { /* nothing kept, or storage unavailable */ }
  return new Set(Array.isArray(ids) ? ids.filter((id): id is string => typeof id === 'string') : []);
};
const save = (key: string, ids: Set<string>) => { try { localStorage.setItem(key, JSON.stringify([...ids].slice(-CLEARED_LIMIT))); } catch { /* holds for this window */ } };
export type SubagentActions = {
  /** Scroll the conversation to a transcript message (a top-level card). */
  revealMessage(messageId: string): void;
  /**
   * Stop one running sub-agent; present only when the provider supports it.
   * 'stopping': the Runner took it; 'gone': the Runner says it is not running
   * (its end was never reported here); 'failed': refused, and said so.
   */
  stop?(agentId: string): Promise<StopOutcome>;
  openFile?(reference: FileLink, how: OpenModifiers): void;
};

/**
 * What one chat view shows of its sub-agents: which cards are open, which
 * full results are shown, the panel, and what the Runner says of each. One
 * store per chat, so a card re-renders only for its own sub-agent or a
 * change of this presentation, never for a keystroke in the composer.
 */
export class SubagentView {
  index: Subagents = emptySubagents();
  expanded = new Set<string>();
  results = new Set<string>();
  /**
   * Sub-agents the person has looked at: a failure among them no longer
   * asks for attention. Kept per conversation, across a reopen, a resume
   * and a restart, like `cleared`.
   */
  seen = new Set<string>();
  /** Sub-agents the person asked to stop here: Stopping… until they end, then Stopped by you. */
  stopping = new Set<string>();
  panel: { agentId: string; mode: SubagentPanelMode } | null = null;
  /** A nested card to scroll to once its parent is open. */
  flash: { agentId: string; nonce: number } | null = null;
  runner: Record<string, string> = {};
  /**
   * Sub-agents the Runner said were not running when asked to stop them,
   * though the journal never said they ended: ended unreported, at once,
   * whatever the next session snapshot says.
   */
  gone = new Set<string>();
  /** Finished items cleared from Background work in this conversation; the transcript keeps them. */
  cleared = new Set<string>();
  /** The last clear, for its Undo. */
  lastCleared: string[] = [];
  private conversation = '';
  /** The Runner's names, for a sub-agent whose journal never named it. */
  names: Record<string, string> = {};
  canStop = false;
  /** Runner ids of the sub-agents it knows by the provider's id: the ones a stop can address. */
  native = new Set<string>();
  /** The conversation's folder: tool lines show paths inside it relative to it. */
  root = '';
  actions: SubagentActions = { revealMessage() {} };
  /** Bumped by every change of presentation. */
  version = 0;
  private listeners = new Set<() => void>();
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  notify = () => { this.version++; for (const listener of this.listeners) listener(); };

  /** Adopt the latest fold; listeners are told after the render that carried it commits. */
  setIndex(index: Subagents) { this.index = index; }
  setSession(session: Session | undefined) {
    const runner: Record<string, string> = {};
    const names: Record<string, string> = {};
    const native = new Set<string>();
    for (const agent of session?.agents || []) if (agent.id !== 'root') {
      runner[agent.id] = agent.status;
      if (agent.name && agent.name !== 'Agent') names[agent.id] = agent.name;
      if (agent.nativeId) native.add(agent.id);
    }
    const canStop = !!session?.capabilities?.stopAgent && !session.closed;
    const changed = canStop !== this.canStop || native.size !== this.native.size || [...native].some(id => !this.native.has(id))
      || Object.keys(runner).length !== Object.keys(this.runner).length || Object.entries(runner).some(([id, status]) => this.runner[id] !== status);
    this.native = native;
    const renamed = Object.entries(names).some(([id, name]) => this.names[id] !== name);
    const root = session?.workingDirectory || '', moved = root !== this.root;
    for (const id of this.gone) if (!runner[id] || ['running', 'starting', 'waiting_input', 'waiting_approval'].includes(runner[id])) runner[id] = 'unreported';
    this.runner = runner; this.names = names; this.canStop = canStop; this.root = root;
    return changed || renamed || moved;
  }
  /**
   * A different conversation (by its conversation id, which a resume or a
   * restart keeps): its own cleared and seen items, nothing else carried
   * over. True when it changed.
   */
  setConversation(conversation: string): boolean {
    if (conversation === this.conversation) return false;
    this.conversation = conversation; this.gone.clear(); this.stopping.clear(); this.lastCleared = [];
    this.cleared = load(clearedKey(conversation));
    this.seen = load(seenKey(conversation));
    return true;
  }
  private saveCleared() { save(clearedKey(this.conversation), this.cleared); }
  private see(id: string) { if (this.seen.has(id)) return; this.seen.add(id); save(seenKey(this.conversation), this.seen); }
  /** Hides finished items from Background work; the transcript keeps them. Undo brings them back. */
  clearFinished(ids: readonly string[]) {
    if (!ids.length) return;
    for (const id of ids) this.cleared.add(id);
    this.lastCleared = [...ids];
    this.saveCleared(); this.notify();
  }
  undoClear() {
    for (const id of this.lastCleared) this.cleared.delete(id);
    this.lastCleared = [];
    this.saveCleared(); this.notify();
  }
  dismissUndo() { if (this.lastCleared.length) { this.lastCleared = []; this.notify(); } }
  toggle(id: string, open = !this.expanded.has(id)) {
    if (open) { this.expanded.add(id); this.see(id); } else this.expanded.delete(id);
    this.notify();
  }
  toggleResult(id: string) { if (this.results.has(id)) this.results.delete(id); else { this.results.add(id); this.see(id); } this.notify(); }
  open(id: string, mode?: SubagentPanelMode) { this.panel = { agentId: id, mode: mode || this.panel?.mode || 'dock' }; this.see(id); this.notify(); }
  closePanel() { this.panel = null; this.notify(); }
  setMode(mode: SubagentPanelMode) { if (this.panel) { this.panel = { ...this.panel, mode }; this.notify(); } }
  markSeen(ids: string[]) { for (const id of ids) this.see(id); this.notify(); }
  /** Scroll to a sub-agent's card, opening every card on the way so it can be seen. */
  reveal(id: string) {
    if (this.index.byId[id]?.kind === 'command') { this.open(id); return; }
    const chain = ancestry(this.index, id);
    if (!chain.length) return;
    for (const member of chain) this.expanded.add(member);
    this.see(id);
    const top = this.index.byId[chain[0]];
    this.flash = chain.length > 1 ? { agentId: id, nonce: Date.now() } : null;
    this.notify();
    if (top?.anchor) this.actions.revealMessage(top.anchor);
  }
  /** Whether its Stop is offered: see `canStopSubagent`. */
  canStopAgent(agent: Subagent, status: SubagentStatus): boolean {
    return !!this.actions.stop && canStopSubagent(agent, status, this);
  }
  /** Asked to stop and not ended yet: here, or (from the journal) anywhere. */
  isStopping(agent: Subagent): boolean { return this.stopping.has(agent.id) || !!agent.stopRequested; }
  /**
   * Stops one sub-agent, by the id the Runner knows it by. It reads as
   * stopping until its end is reported; a refused stop is said and undone.
   */
  async stop(agent: Subagent) {
    const id = agent.id;
    if (!this.actions.stop || this.isStopping(agent)) return;
    this.stopping.add(id); this.notify();
    const outcome = await this.actions.stop(agent.runnerId || id);
    if (outcome === 'stopping') return;
    this.stopping.delete(id);
    // The Runner has nothing running by that name: it ended, unreported. Say so here at once.
    if (outcome === 'gone') for (const known of new Set([id, agent.runnerId || id])) { this.gone.add(known); this.runner = { ...this.runner, [known]: 'unreported' }; }
    this.notify();
  }
}

export const SubagentContext = createContext<SubagentView | null>(null);
export function useSubagentView(): SubagentView | null { return useContext(SubagentContext); }

const noop = () => () => {};
/** One sub-agent, re-rendering only when it changes. */
export function useSubagent(view: SubagentView | null, id: string): Subagent | undefined {
  return useSyncExternalStore(view ? view.subscribe : noop, () => view && panelAgent(view.index, id) || undefined);
}
/** The presentation (open cards, panel…); re-renders on any change to it. */
export function usePresentation(view: SubagentView | null): number {
  return useSyncExternalStore(view ? view.subscribe : noop, () => view?.version ?? 0);
}
/** The whole index, for the tray and the panel. */
export function useSubagentIndex(view: SubagentView | null): Subagents {
  return useSyncExternalStore(view ? view.subscribe : noop, () => view?.index ?? emptyIndex);
}
const emptyIndex = emptySubagents();
