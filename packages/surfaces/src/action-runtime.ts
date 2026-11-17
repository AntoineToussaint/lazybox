import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { readLaunchChoices } from './view-state';
import { resolveConversation, resolveWorkspaceAction, cancelledError, type ActionRuntime, type ActionTarget, type StepOutcome } from './user-actions';

/** What the window does for an action: navigation stays in the workspace view. */
export type ActionNavigation = {
  openPanel(kind: string, target: ActionTarget): void;
  openFile(target: ActionTarget, path: string, line: number): void;
  showSession(target: ActionTarget): void;
  /** Runs an action of the app's registry in this window (PowerLayer). */
  runAppAction(id: string, argument: string, target: ActionTarget): StepOutcome | Promise<StepOutcome>;
};

const sleep = (ms: number, signal: AbortSignal) => new Promise<void>((resolve, reject) => {
  if (signal.aborted) { reject(cancelledError()); return; }
  const timer = setTimeout(() => { signal.removeEventListener('abort', abort); resolve(); }, ms);
  const abort = () => { clearTimeout(timer); reject(cancelledError()); };
  signal.addEventListener('abort', abort, { once: true });
});

const turnEndedAt = (session?: Session) => {
  const root = session?.agents.find(agent => agent.id === 'root') || session?.agents.find(agent => !agent.parentId);
  return Number(root?.turnEndedAtUnixMillis || 0n);
};
const duration = (ms: number) => ms >= 60_000 ? `${Math.round(ms / 60_000)} min` : `${Math.round(ms / 1000)} s`;

/** The Runner calls behind each step, with the owner's own rules (script approval, read-only conversations). */
export function actionRuntime(api: RunnerClient, navigation: ActionNavigation, store?: ViewStateStore, poll = 500): ActionRuntime {
  const waitCommand = async (commandId: string, timeoutMs: number, signal: AbortSignal, what: string): Promise<StepOutcome> => {
    try {
      const status = await api.waitCommand({ commandId }, { signal, timeoutMs });
      if (status.captureError) return { ok: false, detail: `${what}: ${status.captureError}` };
      if (status.exitCode === 0) return { ok: true, detail: `${what} succeeded` };
      return { ok: false, detail: status.exitCode !== undefined ? `${what} exited ${status.exitCode}` : status.signal ? `${what} was stopped (${status.signal})` : `${what} did not finish` };
    } catch (error) {
      if (signal.aborted) throw cancelledError();
      if (ConnectError.from(error).code === Code.DeadlineExceeded) return { ok: false, detail: `${what} was still running after ${duration(timeoutMs)}` };
      throw error;
    }
  };
  const live = async (sessionId: string, signal: AbortSignal) => {
    const { session } = await api.getSession({ sessionId }, { signal });
    if (session?.closed) {
      // A saved conversation continues where it was, as sending from its chat does.
      const resumed = await api.resumeSession({ sessionId }, { signal });
      return resumed.session || session;
    }
    return session;
  };
  return {
    async send(target, text, to, signal) {
      let sessionId = '', collectionId = target.collectionId;
      if (to === 'new') {
        if (!target.collectionId) throw new Error('Open a workspace first: a new conversation starts in one');
        const info = await api.info({}, { signal });
        const current = info.sessions.find(s => s.id === target.sessionId);
        const remembered = readLaunchChoices(store).profile;
        const profile = info.profiles.find(p => p.id === current?.profileId) || info.profiles.find(p => p.id === remembered) || info.profiles.find(p => p.managed) || info.profiles[0];
        if (!profile) throw new Error('No subscription to start a conversation on; add one in Settings');
        const opened = await api.openSession({ profileId: profile.id, collectionId: target.collectionId, options: {} }, { signal });
        sessionId = opened.session?.id || '';
        navigation.showSession({ collectionId, sessionId });
      } else if (to === 'current' || !to) {
        if (!target.sessionId) throw new Error('Open a conversation first, or send to: new');
        sessionId = target.sessionId;
      } else {
        const { sessions } = await api.listHistory({ limit: 500 }, { signal });
        const found = resolveConversation(sessions, to, target.collectionId);
        if (!found) throw new Error(`No conversation “${to}”: name one by its title or id`);
        sessionId = found.id; collectionId = found.collectionId || collectionId;
      }
      const session = await live(sessionId, signal);
      sessionId = session?.id || sessionId;
      const since = turnEndedAt(session);
      await api.apply({ sessionId, operationId: crypto.randomUUID(), operation: { case: 'message', value: { text } } }, { signal });
      return { sessionId, collectionId: session?.collectionId || collectionId, turnEndedAt: since };
    },
    async snippet(key, signal) {
      const { snippets } = await api.listSnippets({}, { signal });
      const found = snippets.find(s => s.key === key);
      if (!found) throw new Error(`No snippet “${key}”`);
      return found.body;
    },
    async wait(sessionId, until, timeoutMs, signal, since) {
      const deadline = Date.now() + timeoutMs;
      for (;;) {
        const { session } = await api.getSession({ sessionId }, { signal });
        if (!session) return { ok: false, detail: 'The conversation is gone' };
        const pending = session.pending.length > 0;
        if (session.fault) return { ok: false, detail: `The agent stopped: ${session.fault}` };
        if (until === 'needs-input' && pending) return { ok: true, detail: 'The agent needs input' };
        if (until === 'idle') {
          if (pending) return { ok: false, detail: 'The agent is waiting for your input' };
          const ended = since === undefined || turnEndedAt(session) > since;
          if (!session.busy && !session.queued && ended) {
            const root = session.agents.find(agent => agent.id === 'root');
            return root?.lastTurnStatus === 'failed' ? { ok: false, detail: 'The turn failed' } : { ok: true, detail: 'The agent finished its turn' };
          }
          if (session.closed) return { ok: false, detail: 'The conversation ended' };
        }
        if (Date.now() >= deadline) return { ok: false, detail: `Still waiting after ${duration(timeoutMs)}` };
        await sleep(poll, signal);
      }
    },
    openPanel: (kind, target) => navigation.openPanel(kind, target),
    runAppAction: (id, argument, target) => navigation.runAppAction(id, argument, target),
    openUrl: url => { window.open(url, '_blank', 'noopener'); },
    openFile: (target, path, line) => navigation.openFile(target, path, line),
    async runWorkspaceAction(collectionId, reference, timeoutMs, signal) {
      const runtime = await api.getWorkspaceRuntime({ collectionId }, { signal });
      const { action, problem } = resolveWorkspaceAction(runtime.actions, reference);
      if (!action) return { ok: false, detail: problem! };
      if (action.blocked) return { ok: false, detail: action.blocked };
      const result = await api.startWorkspaceAction({ collectionId, action: action.key }, { signal });
      const started = result.runtime?.actions.find(a => a.key === action.key)?.runState;
      if (!started?.commandId) return { ok: false, detail: started?.error || `${action.name} did not start` };
      return waitCommand(started.commandId, timeoutMs, signal, action.name);
    },
    async runCommand(sessionId, command, timeoutMs, signal) {
      const started = await api.startCommand({ sessionId, title: command, argv: ['/bin/sh', '-lc', command] }, { signal });
      return waitCommand(started.id, timeoutMs, signal, `\`${command.length > 40 ? command.slice(0, 39) + '…' : command}\``);
    },
    async setPermission(sessionId, mode, signal) {
      await api.apply({ sessionId, operationId: crypto.randomUUID(), operation: { case: 'permission', value: { mode } } }, { signal });
    },
  };
}
