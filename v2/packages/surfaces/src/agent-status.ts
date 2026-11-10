import type { Session } from '@lazybox/protocol';
export function statusLabel(status: string): string {
  return ({ starting: 'Starting', ready: 'Ready', running: 'Running', waiting_input: 'Waiting for your answer', waiting_approval: 'Waiting for approval', done: 'Turn done', interrupted: 'Interrupted', failed: 'Needs attention', stopped: 'Session ended', unknown: 'Status unknown', switching_profile: 'Switching subscription' } as Record<string, string>)[status] || 'Status unknown';
}
export function sessionStatus(session?: Session): string {
  if (!session) return 'Connecting…';
  if (session.fault === 'native_exit_unconfirmed') return 'Status unknown';
  if (session.historical && session.status === 'interrupted') return 'Interrupted';
  if (session.closed) return 'Saved';
  if (session.status) return statusLabel(session.status);
  if (session.fault) return 'Needs attention';
  return session.busy ? 'Running' : 'Ready';
}
export function accountLabel(state = ''): string {
  return ({ auth_required: 'Sign-in required', exhausted: 'Subscription limit reached', throttled: 'Rate limited', billing_error: 'Billing issue', context_full: 'Context window full', budget_exceeded: 'Session budget reached', available: 'Available', unknown: 'Not checked' } as Record<string, string>)[state] || 'Not checked';
}
