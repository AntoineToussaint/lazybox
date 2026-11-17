import type { UserAction, UserActionStep } from '@lazybox/protocol';
import { actionDefinition } from './actions';

/**
 * The person's own actions (POWER.md): named steps from their actions file,
 * which the Runner parses and validates. This module runs them; what each step
 * touches (the Runner, the layout) is injected, so the rules here are tested
 * without a window.
 *
 * A step with `when: succeeded` or `when: failed` runs on the result of the
 * last step without a condition, so both branches of "run the tests; if
 * failed ask for a fix; if succeeded open changes" are about the tests. A
 * failed step ends the action unless a `when: failed` step follows it before
 * the next unconditional step; a failing branch step ends it too. Every step
 * can be cancelled.
 */

/** Where an action runs: the workspace and conversation in view, or the ones a menu names. */
export type ActionTarget = { collectionId: string; sessionId: string };
export type StepOutcome = { ok: boolean; detail: string };

/** What steps act on. Each call throws (or returns ok: false) with a sentence a person can act on. */
export type ActionRuntime = {
  /** Sends text to a conversation ("current", "new", or an id or title) and returns the conversation it went to. */
  send(target: ActionTarget, text: string, to: string, signal: AbortSignal): Promise<{ sessionId: string; collectionId: string; turnEndedAt: number }>;
  snippet(key: string, signal: AbortSignal): Promise<string>;
  /** Waits for the conversation to be idle (a turn that ended after `since`, when given) or to need input. */
  wait(sessionId: string, until: 'idle' | 'needs-input', timeoutMs: number, signal: AbortSignal, since?: number): Promise<StepOutcome>;
  openPanel(kind: string, target: ActionTarget): void | Promise<void>;
  openUrl(url: string): void | Promise<void>;
  openFile(target: ActionTarget, path: string, line: number): void | Promise<void>;
  runWorkspaceAction(collectionId: string, action: string, timeoutMs: number, signal: AbortSignal): Promise<StepOutcome>;
  runCommand(sessionId: string, command: string, timeoutMs: number, signal: AbortSignal): Promise<StepOutcome>;
  setPermission(sessionId: string, mode: string, signal: AbortSignal): Promise<void>;
  /** Runs an action of the app's registry (a `do:` step, `lazybox do`), as its key or the palette would. */
  runAppAction(id: string, argument: string, target: ActionTarget): StepOutcome | Promise<StepOutcome>;
};

export type StepState = 'pending' | 'running' | 'succeeded' | 'failed' | 'skipped';
export type ActionProgress = { index: number; total: number; label: string; states: StepState[] };
export type ActionResult = { ok: boolean; cancelled: boolean; failedStep?: number; detail: string; states: StepState[] };

export const DEFAULT_STEP_TIMEOUT_MS = 30 * 60_000;

/** One step in a few words, for progress and for Settings. */
export function stepLabel(step: UserActionStep): string {
  if (step.name) return step.name;
  const to = step.to && step.to !== 'current' ? (step.to === 'new' ? ' to a new conversation' : ` to “${step.to}”`) : '';
  switch (step.kind) {
    case 'send': return step.snippet ? `Send snippet ${step.snippet}${to}` : `Send “${clip(step.text, 40)}”${to}`;
    case 'open': return step.panel ? `Open ${step.panel}` : step.url ? `Open ${clip(step.url.replace(/^https?:\/\//, ''), 40)}` : `Open ${step.file}${step.line ? `:${step.line}` : ''}`;
    case 'run': return `Run ${step.action}`;
    case 'command': return `Run \`${clip(step.command, 40)}\``;
    case 'permission': return `Set permission to ${step.permission}`;
    case 'wait': return step.until === 'needs-input' ? 'Wait until the agent needs input' : 'Wait until the agent is idle';
    case 'do': return `${actionDefinition(step.doAction)?.title || step.doAction}${step.doArgument ? ` (${clip(step.doArgument, 30)})` : ''}`;
  }
  return step.kind;
}
const clip = (text: string, n: number) => { const line = text.replace(/\s+/g, ' ').trim(); return line.length > n ? line.slice(0, n - 1) + '…' : line; };

/** The action's steps as one line: "Run test → if failed: Send … → if succeeded: Open changes". */
export function actionSummary(action: UserAction): string {
  return action.steps.map((step, i) => (i && step.when && step.when !== 'always' ? `if ${step.when}: ` : '') + stepLabel(step)).join(' → ');
}

const timeoutOf = (step: UserActionStep) => step.timeoutSeconds ? step.timeoutSeconds * 1000 : DEFAULT_STEP_TIMEOUT_MS;

export function cancelledError() { return new DOMException('The action was cancelled', 'AbortError'); }
function check(signal: AbortSignal) { if (signal.aborted) throw cancelledError(); }
const message = (error: unknown) => {
  const raw = error instanceof Error ? error.message : String(error);
  // ConnectError prefixes "[code] "; the Runner's sentence is what helps.
  return raw.replace(/^\[[a-z_]+\]\s*/, '');
};

export async function runUserAction(action: UserAction, runtime: ActionRuntime, start: ActionTarget, options: { signal: AbortSignal; onProgress?(progress: ActionProgress): void }): Promise<ActionResult> {
  const { signal } = options;
  const states: StepState[] = action.steps.map(() => 'pending');
  const target: ActionTarget = { ...start };
  // `when` looks at the last step without a condition: in "run tests; when
  // failed: ask for a fix; when succeeded: open changes" both branches are
  // about the tests, not about each other.
  let subject: 'succeeded' | 'failed' = 'succeeded', handled: number | undefined, detail = '';
  const report = (index: number) => options.onProgress?.({ index, total: action.steps.length, label: stepLabel(action.steps[index]), states: [...states] });
  const stop = (i: number, why: string): ActionResult => {
    for (let j = i + 1; j < states.length; j++) if (states[j] === 'pending') states[j] = 'skipped';
    return { ok: false, cancelled: false, failedStep: i, detail: why, states };
  };
  for (let i = 0; i < action.steps.length; i++) {
    const step = action.steps[i];
    const when = step.when || 'always';
    if (when !== 'always' && when !== subject) { states[i] = 'skipped'; continue; }
    if (signal.aborted) return { ok: false, cancelled: true, detail: 'Cancelled', states };
    states[i] = 'running'; report(i);
    let outcome: StepOutcome;
    try { outcome = await runStep(step, runtime, target, signal); }
    catch (error) {
      if (signal.aborted || (error instanceof DOMException && error.name === 'AbortError')) { states[i] = 'failed'; return { ok: false, cancelled: true, failedStep: i, detail: 'Cancelled', states }; }
      outcome = { ok: false, detail: message(error) };
    }
    states[i] = outcome.ok ? 'succeeded' : 'failed';
    detail = outcome.detail;
    report(i);
    if (when === 'always') {
      subject = outcome.ok ? 'succeeded' : 'failed';
      if (!outcome.ok) {
        // A failure goes on only to the steps that handle it.
        const rest = action.steps.slice(i + 1);
        const next = rest.findIndex(s => (s.when || 'always') === 'always');
        if (!(next < 0 ? rest : rest.slice(0, next)).some(s => s.when === 'failed')) return stop(i, outcome.detail);
        handled = i;
      }
    } else if (!outcome.ok) return stop(i, outcome.detail);
  }
  // A failure its handlers dealt with is reported, not fatal.
  return { ok: true, cancelled: false, failedStep: handled, detail, states };
}

async function runStep(step: UserActionStep, runtime: ActionRuntime, target: ActionTarget, signal: AbortSignal): Promise<StepOutcome> {
  check(signal);
  switch (step.kind) {
    case 'send': {
      const text = step.snippet ? await runtime.snippet(step.snippet, signal) : step.text;
      if (!text.trim()) return { ok: false, detail: step.snippet ? `Snippet ${step.snippet} is empty` : 'Nothing to send' };
      const sent = await runtime.send(target, text, step.to || 'current', signal);
      // Later steps act on the conversation the message went to.
      target.sessionId = sent.sessionId; if (sent.collectionId) target.collectionId = sent.collectionId;
      if (!step.wait) return { ok: true, detail: 'Sent' };
      return runtime.wait(sent.sessionId, 'idle', timeoutOf(step), signal, sent.turnEndedAt);
    }
    case 'open':
      if (step.panel) await runtime.openPanel(step.panel, target);
      else if (step.url) await runtime.openUrl(step.url);
      else {
        if (!target.collectionId) return { ok: false, detail: 'Open a workspace first: files belong to one' };
        await runtime.openFile(target, step.file, step.line);
      }
      return { ok: true, detail: 'Opened' };
    case 'run':
      if (!target.collectionId) return { ok: false, detail: 'Open a workspace first: workspace actions belong to one' };
      return runtime.runWorkspaceAction(target.collectionId, step.action, timeoutOf(step), signal);
    case 'command':
      if (!target.sessionId) return { ok: false, detail: 'Open a conversation first: the command runs as one of its commands' };
      return runtime.runCommand(target.sessionId, step.command, timeoutOf(step), signal);
    case 'permission':
      if (!target.sessionId) return { ok: false, detail: 'Open a conversation first' };
      await runtime.setPermission(target.sessionId, step.permission, signal);
      return { ok: true, detail: `Permission set to ${step.permission}` };
    case 'wait':
      if (!target.sessionId) return { ok: false, detail: 'Open a conversation first' };
      return runtime.wait(target.sessionId, step.until === 'needs-input' ? 'needs-input' : 'idle', timeoutOf(step), signal);
    case 'do':
      return runtime.runAppAction(step.doAction, step.doArgument, target);
  }
  return { ok: false, detail: `This Lazybox does not know the step “${step.kind}”` };
}

/** Resolves a conversation named in a step: an id, a conversation id, or a title (exact, then a unique prefix). */
export function resolveConversation<T extends { id: string; conversationId?: string; title?: string; collectionId?: string; closed?: boolean; successorExecutionId?: string }>(sessions: readonly T[], reference: string, collectionId = ''): T | undefined {
  const live = sessions.filter(s => !s.successorExecutionId);
  const byId = live.find(s => s.id === reference || s.conversationId === reference) || sessions.find(s => s.id === reference);
  if (byId) return byId;
  const wanted = reference.trim().toLowerCase();
  const scoped = (list: T[]) => collectionId ? [...list.filter(s => s.collectionId === collectionId), ...list.filter(s => s.collectionId !== collectionId)] : list;
  const exact = scoped(live).filter(s => (s.title || '').toLowerCase() === wanted);
  if (exact.length) return exact[0];
  const prefix = scoped(live).filter(s => (s.title || '').toLowerCase().startsWith(wanted));
  return prefix.length === 1 || (prefix.length > 1 && collectionId && prefix.filter(s => s.collectionId === collectionId).length === 1) ? prefix[0] : undefined;
}

/** A workspace action named by a step: its key ("api/test"), or a name only one member has. */
export function resolveWorkspaceAction<T extends { key: string; name: string }>(actions: readonly T[], reference: string): { action?: T; problem?: string } {
  const exact = actions.find(a => a.key === reference);
  if (exact) return { action: exact };
  const named = actions.filter(a => a.name === reference);
  if (named.length === 1) return { action: named[0] };
  if (named.length > 1) return { problem: `Several repositories have an action “${reference}”: name one of ${named.map(a => a.key).join(', ')}` };
  return { problem: actions.length ? `This workspace has no action “${reference}” (it has ${actions.map(a => a.key).join(', ')})` : `This workspace has no actions; add one in .lazybox/workspace.yaml` };
}
