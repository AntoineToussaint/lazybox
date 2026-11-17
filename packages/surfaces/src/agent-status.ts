import type { Session } from '@lazybox/protocol';
import { limitStatus } from './subscription-model';
export function statusLabel(status: string): string {
  return ({ starting: 'Starting', ready: 'Ready', running: 'Running', waiting_input: 'Waiting for your answer', waiting_approval: 'Waiting for approval', done: 'Turn done', interrupted: 'Interrupted', failed: 'Needs attention', stopped: 'Session ended', unknown: 'Status unknown', switching_profile: 'Switching subscription' } as Record<string, string>)[status] || 'Status unknown';
}
const millis = (value: bigint | number | undefined) => { const n = Number(value || 0); return Number.isFinite(n) && n > 0 ? n : 0; };
/**
 * True for a session that opened and never ran a turn. Providers such as the
 * Claude SDK report `session_ready` only after the first prompt, so "starting"
 * would otherwise last until the person sends something: it is ready for them.
 */
export function awaitingFirstMessage(session: Session): boolean {
  if (session.closed || session.fault || session.busy || session.queued > 0 || session.pending.length) return false;
  if (session.status !== 'starting' && session.status !== 'ready') return false;
  const root = session.agents.find(agent => agent.id === 'root');
  return !root || (!root.lastTurnStatus && !millis(root.turnStartedAtUnixMillis) && !millis(root.turnEndedAtUnixMillis) && root.status !== 'running');
}
export function sessionStatus(session?: Session): string {
  if (!session) return 'Connecting…';
  if (session.fault === 'native_exit_unconfirmed') return 'Status unknown';
  if (session.historical && session.status === 'interrupted') return 'Interrupted';
  if (session.closed) return 'Saved';
  const limit = limitStatus(session);
  if (limit) return limit;
  if (awaitingFirstMessage(session)) return 'Ready';
  if (session.status) return statusLabel(session.status);
  if (session.fault) return 'Needs attention';
  return session.busy ? 'Running' : 'Ready';
}
export function accountLabel(state = ''): string {
  return ({ auth_required: 'Sign-in required', exhausted: 'Subscription limit reached', throttled: 'Rate limited', billing_error: 'Billing issue', context_full: 'Context window full', budget_exceeded: 'Session budget reached', available: 'Available', unknown: 'Not checked' } as Record<string, string>)[state] || 'Not checked';
}
