import type { Profile, Session } from '@lazybox/protocol';

/**
 * Subscription limits in plain words. The Runner reports provider tokens
 * ("exhausted", "rate_limit", "provider_error"); nothing here shows one.
 */

/** Account states a switch of subscription (or a reset) resolves. */
export const LIMIT_STATES = ['exhausted', 'throttled', 'auth_required', 'billing_error'] as const;
export type LimitState = typeof LIMIT_STATES[number];
export const isLimitState = (state = ''): state is LimitState => (LIMIT_STATES as readonly string[]).includes(state);

const seconds = (value: bigint | number | undefined) => { const n = Number(value || 0); return Number.isFinite(n) && n > 0 ? n : 0; };

/** The limit a running conversation stopped at, or ''. */
export function sessionLimit(session?: Session): LimitState | '' {
  if (!session || session.closed) return '';
  return isLimitState(session.fault) ? session.fault : '';
}

/** A reset as its day ('' today, "tomorrow", "Mon", "Nov 20") and time of day; undefined for none or one passed. */
function resetParts(resetAt: bigint | number | undefined, now: number, locale?: string): { day: string; time: string; date: boolean } | undefined {
  const at = seconds(resetAt) * 1000;
  if (!at || at <= now) return undefined;
  const date = new Date(at), today = new Date(now);
  const time = date.toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit' });
  const day = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((day(date) - day(today)) / 86400000);
  if (days === 0) return { day: '', time, date: false };
  if (days === 1) return { day: 'tomorrow', time, date: false };
  if (days < 7) return { day: date.toLocaleDateString(locale, { weekday: 'short' }), time, date: false };
  return { day: date.toLocaleDateString(locale, { month: 'short', day: 'numeric' }), time, date: true };
}

/**
 * When a reset happens, as a person says it: "6:00 PM" today, "tomorrow 9:00 AM",
 * "Mon 6:00 PM" this week, else the date. '' for none, or one already passed.
 */
export function resetTime(resetAt: bigint | number | undefined, now = Date.now(), locale?: string): string {
  const parts = resetParts(resetAt, now, locale);
  if (!parts) return '';
  return !parts.day ? parts.time : `${parts.day}${parts.date ? ',' : ''} ${parts.time}`;
}

/**
 * When a limited subscription can be used again, said the one way every
 * surface says it: "back at 6:00 PM", "back tomorrow at 9:00 AM", "back Mon at
 * 6:00 PM", "back Nov 20 at 6:00 PM". '' for none, or one already passed.
 */
export function backAt(resetAt: bigint | number | undefined, now = Date.now(), locale?: string): string {
  const parts = resetParts(resetAt, now, locale);
  return parts ? `back ${parts.day ? `${parts.day} ` : ''}at ${parts.time}` : '';
}

/** How long until a moment, roughly: "in 2h 10m", "in 45m", "in 3d 4h". '' once passed. */
export function untilText(resetAt: bigint | number | undefined, now = Date.now()): string {
  const remaining = seconds(resetAt) * 1000 - now;
  if (remaining <= 0) return '';
  const minutes = Math.ceil(remaining / 60_000), hours = Math.floor(minutes / 60), days = Math.floor(hours / 24);
  if (days >= 1) return `in ${days}d${hours % 24 ? ` ${hours % 24}h` : ''}`;
  if (hours >= 1) return `in ${hours}h${minutes % 60 ? ` ${minutes % 60}m` : ''}`;
  return `in ${minutes}m`;
}

/** "back at 6:00 PM (in 2h 10m)". '' for none, or one already passed. */
export function backWhen(resetAt: bigint | number | undefined, now = Date.now()): string {
  const back = backAt(resetAt, now);
  return back && `${back} (${untilText(resetAt, now)})`;
}

/**
 * When a saved subscription can take work again, by its own last report. A
 * limit lasts until every window at 100% has reset, so the latest of those is
 * when it is back; the account's reset time stands in when no window says.
 * `due` means that moment has passed and the report is stale.
 */
export type Comeback =
  | { kind: 'available' }
  | { kind: 'back' | 'due'; resetAt: number }
  | { kind: 'unknown' | 'sign-in' | 'billing' };

export function comeback(profile: Profile | undefined, now = Date.now()): Comeback {
  const usage = profile?.availability, state = usage?.state || 'unknown';
  if (state === 'auth_required') return { kind: 'sign-in' };
  if (state === 'billing_error') return { kind: 'billing' };
  const full = (usage?.windows || []).filter(w => Number.isFinite(w.usedPercent) && w.usedPercent >= 100);
  if (state !== 'exhausted' && state !== 'throttled' && !full.length) return { kind: 'available' };
  const resets = full.map(w => seconds(w.resetAt)).filter(Boolean);
  const resetAt = resets.length ? Math.max(...resets) : seconds(usage?.resetAt);
  if (!resetAt) return { kind: 'unknown' };
  return { kind: resetAt * 1000 > now ? 'back' : 'due', resetAt };
}

/** What naming, ordering and grouping a subscription read of it: a Profile, or as much of one as a list has. */
export type SubscriptionLike = Pick<Profile, 'id' | 'provider'> & Partial<Pick<Profile, 'label' | 'isolation' | 'availability'>>;

/** A subscription's name: its label, else the CLI sign-in the Runner inherited. */
export function subscriptionName(profile: Omit<SubscriptionLike, 'id'> | undefined, id = ''): string {
  if (profile?.label) return profile.label;
  const provider = profile?.provider || (id.startsWith('codex') ? 'codex' : id.startsWith('claude') ? 'claude' : '');
  if (profile?.isolation === 'inherited' || id === 'claude-default' || id === 'codex-default') return `${provider === 'codex' ? 'Codex' : 'Claude'} CLI sign-in`;
  return id || 'This subscription';
}

/** The one sentence a stopped conversation leads with. */
export function limitHeadline(name: string, state: LimitState, resetAt: bigint | number | undefined, now = Date.now()): string {
  const back = backWhen(resetAt, now);
  switch (state) {
    case 'exhausted': return `${name} reached its limit${back ? ` · ${back}` : ''}`;
    case 'throttled': return `${name} is rate limited${back ? ` · ${back}` : ''}`;
    case 'auth_required': return `${name} needs you to sign in again`;
    case 'billing_error': return `${name} has a billing problem`;
  }
}

export type SubscriptionChoice = { id: string; provider: string; name: string; ready: boolean; detail: string; resetAt: number; current: boolean };

/** A zero reset means a known limit with no recovery time. */
function limitResets(profile: SubscriptionLike): number[] {
  const usage = profile.availability;
  const resets = (usage?.windows || []).filter(usageWindow => usageWindow.usedPercent >= 100).map(usageWindow => seconds(usageWindow.resetAt));
  if (usage?.state === 'exhausted' || usage?.state === 'throttled') resets.push(seconds(usage.resetAt));
  return resets;
}

function readiness(profile: SubscriptionLike, now: number): 0 | 1 | 2 {
  const state = profile.availability?.state;
  if (state === 'auth_required' || state === 'billing_error') return 2;
  const resets = limitResets(profile);
  if (resets.some(reset => !reset || reset * 1000 > now)) return 2;
  return state === 'available' && !resets.length ? 0 : 1;
}

/** Can this subscription take work now, as far as anyone has observed? */
export function subscriptionReady(profile: SubscriptionLike, now: number): boolean {
  return readiness(profile, now) !== 2;
}

/** How a subscription stands, in one line. */
export function subscriptionDetail(profile: SubscriptionLike, now: number): string {
  const usage = profile.availability;
  if (usage?.state === 'auth_required') return 'Sign-in required';
  if (usage?.state === 'billing_error') return 'Billing issue';
  const resets = limitResets(profile);
  const rank = readiness(profile, now);
  if (rank === 2) {
    const back = resets.includes(0) ? '' : backAt(Math.max(...resets), now);
    const label = usage?.state === 'throttled' ? 'Rate limited' : 'Limit reached';
    return back ? `${label} · ${back}` : label;
  }
  if (resets.length) return 'Reset elapsed · usage unconfirmed';
  const used = Math.max(-1, ...(usage?.windows || []).map(w => Number.isFinite(w.usedPercent) ? w.usedPercent : -1));
  const prefix = rank === 0 ? 'Available' : 'Ready';
  return used >= 0 ? `${prefix} · ${Math.min(99, Math.round(used))}% used` : prefix;
}

/** Sort by reported capacity, possible capacity, then unavailable. */
export function standing(profile: SubscriptionLike, now = Date.now()): 0 | 1 | 2 {
  return readiness(profile, now);
}

/**
 * Subscriptions in the one order every list uses: the current one first (when
 * there is one), then by standing (available, not known to be limited,
 * limited), then by name. Grouping by provider is subscription-groups.ts.
 */
export function orderSubscriptions<T extends SubscriptionLike>(profiles: readonly T[], { currentId = '', now = Date.now() }: { currentId?: string; now?: number } = {}): T[] {
  const rows = profiles.map(profile => ({ profile, current: !!currentId && profile.id === currentId, standing: standing(profile, now), name: subscriptionName(profile, profile.id) }));
  return rows.sort((a, b) => Number(b.current) - Number(a.current) || a.standing - b.standing || a.name.localeCompare(b.name, undefined, { sensitivity: 'base', numeric: true }) || (a.profile.id < b.profile.id ? -1 : a.profile.id > b.profile.id ? 1 : 0)).map(row => row.profile);
}

/**
 * The subscriptions a conversation of `provider` can continue on, in the one
 * order every list uses (orderSubscriptions).
 */
export function subscriptionChoices(profiles: readonly Profile[], provider: string, currentId: string, now = Date.now()): SubscriptionChoice[] {
  return orderSubscriptions(profiles.filter(p => p.provider === provider), { currentId, now })
    .map(p => ({ id: p.id, provider: p.provider, name: subscriptionName(p, p.id), ready: subscriptionReady(p, now), detail: subscriptionDetail(p, now), resetAt: seconds(p.availability?.resetAt), current: p.id === currentId }));
}

/** The subscription "Switch to …" offers: the best other one that is ready. */
export function bestChoice(choices: readonly SubscriptionChoice[]): SubscriptionChoice | undefined {
  return choices.find(c => !c.current && c.ready);
}

/** The limit card's manual action: the person chooses where the conversation goes. */
export const switchLabel = (name: string) => `Switch to ${name}`;

/**
 * Whether a conversation of provider moves to another account by itself at a
 * limit. Only an enabled extension's account policy does that
 * (RunnerSettings.automaticSwitchingProviders, EXTENSIONS.md): a pool saved
 * with the conversation switches nothing without one. Without it, Lazybox
 * offers the switch and the person chooses.
 */
export function switchesItself(session: Pick<Session, 'rotationProfileIds'> | undefined, automaticProviders: readonly string[] | undefined, provider: string): boolean {
  return !!automaticProviders?.includes(provider) && (session?.rotationProfileIds.length || 0) > 0;
}

/** A switch as the Runner journaled it (profile_changed). */
export type SwitchNote = { from: string; to: string; mode: string; requested: boolean; fromState: string; resetAt: number };

export function switchNote(to: string, mode: string, data: Record<string, unknown>): SwitchNote {
  return { to, mode, from: typeof data.from === 'string' ? data.from : '', requested: data.requested === true, fromState: typeof data.fromState === 'string' ? data.fromState : '', resetAt: seconds(typeof data.resetAt === 'number' ? data.resetAt : 0) };
}

/** The one quiet line a switch leaves in the conversation. */
export function switchedLine(note: SwitchNote, nameFor: (id: string) => string, now = Date.now()): string {
  if (note.mode === 'retry_same_profile') return `Continued on ${nameFor(note.to)}`;
  const line = `Switched to ${nameFor(note.to)}`;
  if (!note.from || note.from === note.to || !isLimitState(note.fromState)) return line;
  const back = backAt(note.resetAt, now), from = nameFor(note.from);
  if (back && (note.fromState === 'exhausted' || note.fromState === 'throttled')) return `${line} · ${from} is ${back}`;
  return `${line} · ${from} ${note.fromState === 'auth_required' ? 'needs sign-in' : note.fromState === 'billing_error' ? 'has a billing problem' : 'reached its limit'}`;
}

const looksLikeCode = (text: string) => /^[a-z0-9_.:-]+$/i.test(text.trim());
const codeSentences: Record<string, string> = {
  rate_limit: 'The provider is limiting requests right now.',
  model_not_found: 'The provider could not find this model. Choose another model and send your message again.',
  context_full: 'The conversation no longer fits in the model’s context window. Start a new conversation to continue.',
  context_length_exceeded: 'The conversation no longer fits in the model’s context window. Start a new conversation to continue.',
  budget_exceeded: 'This conversation reached its turn or spending limit.',
  error_max_turns: 'This conversation reached its turn limit.',
  error_max_budget_usd: 'This conversation reached its spending limit.',
  provider_process_exit: 'The agent process stopped unexpectedly.',
  delivery_failed: 'Your message could not be delivered to the agent.',
  delivery_rejected: 'The agent refused your message. Send a message to continue the conversation.',
  account_state_write_failed: 'Lazybox could not save the subscription status.',
  authentication_failed: 'The subscription needs you to sign in again.',
  billing_error: 'The subscription has a billing problem.',
  overloaded_error: 'The provider is overloaded. Try again in a moment.',
  native_exit_unconfirmed: 'Lazybox could not confirm that the agent process stopped.',
};

/**
 * Claude's own report that a turn ended in the middle of its work
 * ("[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use"):
 * a diagnostic for Anthropic, never words for a person.
 */
export const isProviderDiagnostic = (text = '') => /^\s*\[ede_diagnostic\]/i.test(text);

/** A failure the agent reported, as a sentence. Provider prose is kept; bare codes and diagnostics are translated. */
export function failureSentence(status = '', text = ''): string {
  const body = text.trim();
  if (isProviderDiagnostic(body)) return 'Claude stopped unexpectedly.';
  if (body && !looksLikeCode(body)) return body;
  return codeSentences[body] || codeSentences[status] || 'The agent stopped after an error from the provider.';
}

/**
 * A fault from the provider ending one turn with an error (an API error, an
 * image it could not read). The conversation is intact and the next message
 * continues it. Mirrors agent.TurnFault in the Runner.
 */
export function turnFault(fault = ''): boolean {
  if (!fault || isLimitState(fault)) return false;
  // The agent process ended, or the worker's SDK stream failed: nothing takes the next message.
  if (fault === 'provider_process_exit' || fault === 'sdk_error') return false;
  return !['provider delivery failed', 'event capture failed', 'provider stream ended', 'invalid or excessive pending provider requests', 'native_exit_unconfirmed', 'provider startup failed'].some(prefix => fault.startsWith(prefix));
}

/**
 * A fault that says only that the Runner lost its stream from the agent (or
 * could not record it), not that the provider or the agent failed: Resume
 * re-attaches the live agent in place, and the Runner settles it on its own
 * when it re-adopts the agent. Mirrors agent.TransportFault in the Runner.
 */
export function transportFault(fault = ''): boolean {
  return fault.startsWith('provider stream ended') || fault.startsWith('event capture failed');
}

/**
 * The agent's process exited: nothing is left to re-attach, and Resume
 * continues its conversation in a new process. Mirrors agent.ProcessExitFault.
 */
export function exitFault(fault = ''): boolean {
  return fault === 'provider_process_exit';
}

/** Why a conversation needs attention, for a fault that is not a subscription limit. */
export function faultSentence(fault: string): string {
  if (!fault) return '';
  if (fault === 'provider delivery failed; reconcile before continuing') return 'A message could not be delivered to the agent. Check the conversation before sending again.';
  if (fault.startsWith('provider stream ended')) return 'The connection to the agent ended unexpectedly.';
  if (exitFault(fault)) return 'The agent process stopped unexpectedly. Resume continues the conversation in a new one.';
  if (fault.startsWith('event capture failed')) return 'Lazybox could not record this conversation. Its agent keeps running: Resume reconnects to it.';
  if (fault === 'unknown') return 'The provider ended the turn without a recognized reason. Send a message to continue the conversation.';
  if (fault === 'provider_failure' || fault === 'provider_error' || fault === 'sdk_error') return 'The agent stopped after an error from the provider.';
  if (looksLikeCode(fault)) return codeSentences[fault] || `The agent stopped (${fault.replaceAll('_', ' ')}).`;
  return fault;
}

/** The header's words for a conversation at a limit, or undefined. */
export function limitStatus(session: Session | undefined, now = Date.now()): string | undefined {
  if (!session || session.closed) return undefined;
  if (session.status === 'switching_profile') return isLimitState(session.account?.state) ? 'Limit reached · switching' : undefined;
  const limit = sessionLimit(session);
  if (!limit) return undefined;
  if (limit === 'auth_required') return 'Sign-in required';
  if (limit === 'billing_error') return 'Billing issue';
  const back = backAt(session.account?.resetAt, now);
  return back ? `Limit reached · ${back}` : 'Limit reached';
}

/** How a person signs in to a new subscription (AddSubscriptionRequest.sign_in_method). */
export type SignInMethod = 'browser' | 'device';

/**
 * The sign-in a new Codex subscription starts with. The browser sign-in calls
 * back to Codex on the Runner's computer (127.0.0.1), so it works only from a
 * window on that computer. An owner window reaches its Runner on loopback, so
 * a page served from loopback or from the desktop app's own origin is there;
 * any other page (another device) gets a one-time code instead.
 */
export function signInDefault(hostname: string): SignInMethod {
  const host = hostname.toLowerCase().replace(/^\[|\]$/g, '');
  return host === 'localhost' || host.endsWith('.localhost') || host === '::1' || /^127(\.\d{1,3}){3}$/.test(host) ? 'browser' : 'device';
}
