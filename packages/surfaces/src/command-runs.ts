import { useEffect, useRef } from 'react';
import type { CommandReport, CommandStatus } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';

// Sizes read the way `lazybox run` prints them to the agent.
export function formatBytes(value: bigint | number): string {
  const n = Number(value);
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MiB`;
  if (n >= 1 << 10) return `${(n / (1 << 10)).toFixed(1)} KiB`;
  return `${n} B`;
}

// reportSummary labels a `lazybox run` with what its agent received,
// e.g. "exit 1 · agent saw 2.1 KiB of 1.4 MiB (go-test filter)". A run from a
// workspace terminal reached no agent yet: its label says what one would get.
export function reportSummary(report: CommandReport, fromTerminal = false): string {
  const filter = report.filter && report.filter !== 'passthrough' && report.filter !== 'raw' ? ` (${report.filter} filter)` : '';
  const outcome = report.piped ? 'Piped, exit unknown' : `Exit ${report.exitCode}`;
  const who = fromTerminal ? 'an agent would get' : 'the agent got';
  // A view no smaller than the output is the whole output (plus its header).
  const view = report.agentBytes >= report.outputBytes ? `${who} all of it (${formatBytes(report.outputBytes)})` : `${who} a ${formatBytes(report.agentBytes)} summary of ${formatBytes(report.outputBytes)}${filter}`;
  return `${outcome} · ${view}`;
}

/** A workspace run: published from a workspace terminal, not by an agent. */
export function isWorkspaceRun(command: Pick<CommandStatus, 'sessionId' | 'collectionId'>): boolean {
  return !command.sessionId && !!command.collectionId;
}

/** A run of the workspace runtime: an action, or a setup or archive script (WORKSPACE_RUNTIME.md). */
export function isRuntimeRun(command: Pick<CommandStatus, 'origin'>): boolean {
  return command.origin === 'action' || command.origin === 'setup' || command.origin === 'archive';
}

/**
 * A running command in words: "Running in the background · port 3000" for a
 * `lazybox run` that went on there (a server, a watcher), "Running · port 3000"
 * for another that listens, else "Running".
 */
export function runningLabel(command: Pick<CommandStatus, 'background' | 'ports'>): string {
  const port = command.ports.length ? ` · port ${command.ports.join(', ')}` : '';
  return command.background ? `Running in the background${port}` : `Running${port}`;
}

/** Where a workspace run came from, as a console list prefix: "from your terminal · ", "action · ". */
export function runOrigin(command: Pick<CommandStatus, 'sessionId' | 'collectionId' | 'origin'>): string {
  if (isRuntimeRun(command)) return `${command.origin} · `;
  return isWorkspaceRun(command) ? 'from your terminal · ' : '';
}

// composeRunMessage is what "Send to agent" adds to a message: the run's
// compact view (outcome first, failures kept, the full-output path), never the
// full output. It is fenced so the agent reads it as output, not as the
// person's words, with a fence longer than any backtick run inside it.
export function composeRunMessage(command: Pick<CommandStatus, 'title'>, view: string): string {
  const longest = Math.max(0, ...[...view.matchAll(/`+/g)].map(match => match[0].length));
  const fence = '`'.repeat(Math.max(3, longest + 1));
  return `Output of \`${command.title.replace(/`/g, "'")}\` from my terminal:\n${fence}text\n${view.replace(/\n+$/, '')}\n${fence}\n`;
}

/** appendToDraft adds text after a draft, on its own paragraph. */
export function appendToDraft(draft: string, text: string): string {
  if (!draft.trim()) return text;
  return draft.replace(/\s+$/, '') + '\n\n' + text;
}

// A run reaches a message box through a window event, so the console needs no
// handle on the chat: the composer showing that session takes it, once.
const COMPOSE_EVENT = 'lazybox-compose';
type ComposeDetail = { sessionId: string; text: string };

/** offerToComposer returns false when no composer shows that session. */
export function offerToComposer(sessionId: string, text: string): boolean {
  return !window.dispatchEvent(new CustomEvent<ComposeDetail>(COMPOSE_EVENT, { detail: { sessionId, text }, cancelable: true }));
}

export function useComposerOffers(sessionId: string, onOffer: (text: string) => void) {
  const handler = useRef(onOffer); handler.current = onOffer;
  useEffect(() => {
    if (!sessionId) return;
    const listener = (event: Event) => {
      const { detail } = event as CustomEvent<ComposeDetail>;
      if (detail?.sessionId !== sessionId || event.defaultPrevented) return;
      event.preventDefault(); handler.current(detail.text);
    };
    window.addEventListener(COMPOSE_EVENT, listener);
    return () => window.removeEventListener(COMPOSE_EVENT, listener);
  }, [sessionId]);
}

// useSharedRuns hands a conversation the runs a person shared with it from a
// terminal (`lazybox log --to`): once a run ends, the first message box
// showing the conversation claims it from the Runner and receives its
// compact view, so it is added once even with several windows open.
// onShared receives the session it was claimed for: a claim cannot be undone,
// so it is delivered even if the view has moved on (into that session's draft).
export function useSharedRuns(api: RunnerClient, sessionId: string, conversationId: string, onShared: (sessionId: string, command: CommandStatus, view: string) => void) {
  const handler = useRef(onShared); handler.current = onShared;
  useEffect(() => {
    if (!sessionId || !conversationId) return;
    const control = new AbortController(), tried = new Set<string>(); let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      try {
        const { commands } = await api.listCommands({ sessionId }, { signal: control.signal });
        for (const command of commands) {
          if (control.signal.aborted) return;
          if (command.shareWith !== conversationId || !command.done || tried.has(command.id)) continue;
          // Not cancelled with the view: a claim taken must reach a draft.
          const claim = await api.claimCommandShare({ commandId: command.id });
          tried.add(command.id);
          const view = claim.status?.report?.agentView;
          if (claim.claimed && view) handler.current(sessionId, claim.status!, view);
        }
      }
      catch { /* Retried on the next poll; a claim is only ever taken once. */ }
      finally { updating = false; }
    };
    void update(); const timer = setInterval(() => void update(), 1500);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, sessionId, conversationId]);
}

// RunTracker reports each command once, the first time it is listed. The first
// listing is the baseline: commands that already existed are never reported, so
// reloading or revisiting a session does not reopen a console the user closed.
export class RunTracker {
  private seen?: Set<string>;
  next(ids: readonly string[]): string[] {
    if (!this.seen) { this.seen = new Set(ids); return []; }
    const fresh = ids.filter(id => !this.seen!.has(id));
    for (const id of fresh) this.seen.add(id);
    return fresh;
  }
}
