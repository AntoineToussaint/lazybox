import type { Profile, ProviderAvailability, ProviderStatus, SubscriptionAvailability } from '@lazybox/protocol';
import { backAt } from './subscription-model';

/**
 * Which agent Ask Lazybox uses, from the Runner's availability report
 * (GetProviderAvailability): the pure decisions. Rendering lives in
 * HelpAssistant.tsx; the welcome uses `providerLine`.
 */

export type HelpProvider = 'claude' | 'codex';

/** One subscription Ask Lazybox could run on. */
export type HelpChoice = {
  provider: HelpProvider;
  profileId: string;
  /** "Claude" or "Codex": what the header says it uses. */
  agent: string;
  /** The subscription as Settings names it. */
  name: string;
  /** The program can run and the subscription can take work now. */
  usable: boolean;
  /** One plain line on its state: "Signed in", "Limit reached · back at 6:00 PM". */
  detail: string;
};

const agentNames: Record<string, string> = { claude: 'Claude', codex: 'Codex' };
const asProvider = (value: string): HelpProvider | undefined => value === 'claude' || value === 'codex' ? value : undefined;

/** A subscription's name, as Settings and the conversation header say it. */
export function choiceName(provider: string, subscription: Pick<SubscriptionAvailability, 'label' | 'isolation' | 'profileId'>): string {
  if (subscription.label) return subscription.label;
  if (subscription.isolation === 'inherited') return `${agentNames[provider] || provider} CLI sign-in`;
  return subscription.profileId;
}

const providerReady = (p: ProviderStatus) => p.installed && !p.error;
const subscriptionUsable = (s: SubscriptionAvailability) => s.state === 'available' || s.state === 'unverified';

function subscriptionDetail(s: SubscriptionAvailability, now: number): string {
  if (s.state === 'limited') { const back = backAt(s.resetAt, now); return back ? `Limit reached · ${back}` : 'Limit reached'; }
  if (s.state === 'unverified') return s.detail || 'Not checked yet';
  return s.detail || (s.state === 'available' ? 'Available' : s.state === 'billing' ? 'Billing problem' : 'Not signed in');
}

/**
 * Every subscription Ask Lazybox could use, best first: the usable ones of
 * the preferred provider (the person's last choice of agent), then the usable
 * ones of the others, then the rest, which a person may still pick.
 * Within a provider the Runner's order holds (best first).
 */
export function helpChoices(report: ProviderAvailability | undefined, preferred: string = 'claude', now = Date.now()): HelpChoice[] {
  if (!report) return [];
  const rows: (HelpChoice & { rank: number })[] = [];
  for (const status of report.providers) {
    const provider = asProvider(status.provider);
    if (!provider) continue;
    for (const s of status.subscriptions) {
      const usable = providerReady(status) && subscriptionUsable(s);
      const rank = (usable ? 0 : 2) + (provider === preferred ? 0 : 1);
      rows.push({ provider, profileId: s.profileId, agent: agentNames[provider], name: choiceName(provider, s), usable, detail: providerReady(status) ? subscriptionDetail(s, now) : status.installed ? 'Cannot run' : 'Not installed', rank });
    }
  }
  return rows.map((row, index) => ({ row, index })).sort((a, b) => a.row.rank - b.row.rank || a.index - b.index).map(({ row: { rank: _, ...choice } }) => choice);
}

export type HelpPick =
  | { kind: 'loading' }
  /** `fallback`: the preferred provider had nothing usable, so another one is used. */
  | { kind: 'ready'; choice: HelpChoice; fallback: boolean; chosen: boolean }
  | { kind: 'none' };

/**
 * The subscription Ask Lazybox starts on: the one the person chose in the
 * switch menu while it exists (usable or not: they chose it), else the best
 * usable one. `none` when nothing can take work.
 */
export function pickHelp(report: ProviderAvailability | undefined, preferred: string, chosenProfile = '', now = Date.now()): HelpPick {
  if (!report) return { kind: 'loading' };
  const choices = helpChoices(report, preferred, now);
  const chosen = chosenProfile ? choices.find(c => c.profileId === chosenProfile) : undefined;
  if (chosen) return { kind: 'ready', choice: chosen, fallback: false, chosen: true };
  const best = choices.find(c => c.usable);
  if (!best) return { kind: 'none' };
  return { kind: 'ready', choice: best, fallback: best.provider !== preferred, chosen: false };
}

/**
 * A stand-in report from Info's profiles, for a Runner that predates
 * GetProviderAvailability: programs are assumed installed, and a subscription
 * is usable unless its last observation says otherwise.
 */
export function reportFromProfiles(profiles: readonly Profile[], now = Date.now()): ProviderAvailability {
  const state = (p: Profile) => {
    const seen = p.availability?.state || 'unknown', reset = Number(p.availability?.resetAt || 0);
    if (seen === 'auth_required') return 'sign_in';
    if (seen === 'billing_error') return 'billing';
    if ((seen === 'exhausted' || seen === 'throttled') && (!reset || reset * 1000 > now)) return 'limited';
    return seen === 'available' ? 'available' : 'unverified';
  };
  const providers = (['claude', 'codex'] as const).map(provider => {
    const subscriptions = profiles.filter(p => p.provider === provider).map(p => ({ profileId: p.id, label: p.label, isolation: p.isolation, managed: p.managed, state: state(p), resetAt: p.availability?.resetAt || 0n, detail: '' }) as SubscriptionAvailability);
    const best = subscriptions.find(subscriptionUsable) || subscriptions[0];
    return { provider, title: provider === 'claude' ? 'Claude Code' : 'Codex', installed: true, path: '', version: '', source: '', error: '', installCommand: '', subscriptions, state: !best ? 'no_subscription' : subscriptionUsable(best) ? 'ready' : best.state } as unknown as ProviderStatus;
  });
  return { providers, checkedAtUnixMillis: BigInt(now) } as unknown as ProviderAvailability;
}

/** "Help · using Claude (Work subscription)". */
export const usingLine = (choice: Pick<HelpChoice, 'agent' | 'name'>) => `using ${choice.agent} (${choice.name})`;

/** What a provider needs before Ask Lazybox can use it, and the step that fixes it. */
export type ProviderNeed = {
  provider: string;
  title: string;
  /** Short state: "Not installed", "Not signed in", "Limit reached · back at 6:00 PM". */
  headline: string;
  /** One sentence on what to do. */
  detail: string;
  /** The fix: install (show `command`), subscription (Settings → Subscriptions), or none. */
  action: 'install' | 'subscription' | 'none';
  command: string;
  ready: boolean;
};

/** Each provider's state for the empty state and the welcome, in the Runner's order. */
export function providerNeeds(report: ProviderAvailability | undefined, now = Date.now()): ProviderNeed[] {
  // Ask Lazybox runs on Claude or Codex; ACP agents are not its to need.
  return (report?.providers || []).filter(p => asProvider(p.provider)).map(p => {
    const base = { provider: p.provider, title: p.title || agentNames[p.provider] || p.provider, command: p.installCommand, ready: false };
    const version = p.version ? ` ${p.version}` : '';
    switch (p.state) {
      case 'ready': return { ...base, ready: true, headline: `Installed${version} · ready`, detail: '', action: 'none' };
      case 'not_installed': return { ...base, headline: 'Not installed', action: 'install', detail: p.provider === 'claude'
        ? 'Claude Code ships with Lazybox, but this Runner cannot find it. Reinstall Lazybox, or install Claude Code and restart the Runner:'
        : `Install ${base.title} on the Runner's computer, then check again:` };
      case 'unusable': return { ...base, headline: `Installed${version} · cannot run`, detail: p.error, action: p.provider === 'claude' ? 'none' : 'install' };
      case 'no_subscription': return { ...base, headline: `Installed${version} · no subscription`, detail: 'Add a subscription in Settings → Subscriptions.', action: 'subscription' };
      case 'limited': {
        const soonest = p.subscriptions.find(s => s.state === 'limited');
        const back = soonest ? backAt(soonest.resetAt, now) : '';
        return { ...base, headline: `Installed${version} · limit reached${back ? ` · ${back}` : ''}`, detail: 'Every subscription is at its limit. Wait for the reset, or add another subscription.', action: 'subscription' };
      }
      case 'billing': return { ...base, headline: `Installed${version} · billing problem`, detail: 'Fix the plan with the provider, or add another subscription.', action: 'subscription' };
      default: return { ...base, headline: `Installed${version} · not signed in`, detail: 'Sign in by adding a subscription in Settings → Subscriptions.', action: 'subscription' };
    }
  });
}

/**
 * One line for the welcome: "Claude Code 2.1.311 installed · signed in",
 * "Codex not installed", "Codex 0.98.0 installed · not signed in".
 */
export function providerLine(p: ProviderStatus, now = Date.now()): string {
  const name = `${p.title || agentNames[p.provider] || p.provider}${p.version ? ` ${p.version}` : ''}`;
  if (!p.installed) return `${p.title || p.provider} not installed`;
  if (p.error) return `${name} installed · cannot run`;
  switch (p.state) {
    case 'ready': return `${name} installed · ${p.subscriptions.some(s => s.state === 'available') ? 'signed in' : 'ready'}`;
    case 'no_subscription': return `${name} installed · no subscription`;
    case 'limited': { const back = backAt(p.subscriptions.find(s => s.state === 'limited')?.resetAt, now); return `${name} installed · limit reached${back ? ` · ${back}` : ''}`; }
    case 'billing': return `${name} installed · billing problem`;
    default: return `${name} installed · not signed in`;
  }
}
