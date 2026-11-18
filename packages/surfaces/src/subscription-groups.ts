import { agentIcon, agentTitle } from './providers';
import { orderSubscriptions, subscriptionDetail, subscriptionName, subscriptionReady, type SubscriptionLike } from './subscription-model';

/**
 * Subscriptions grouped by provider, the one way every list shows them
 * (Settings → Subscriptions, the switch dialog and limit card, the
 * new-conversation and new-task pickers): Claude, then Codex, then ACP agents
 * by name; within a group, orderSubscriptions (the current one first, then
 * available, then by name). Pure.
 */

type ProfileLike = { provider: string; label?: string };

export type ProviderGroup<T> = {
  provider: string;
  /** "Claude", "Codex", "Gemini CLI". */
  title: string;
  /** Icons.tsx name. */
  icon: string;
  items: T[];
};

/** Where a provider's group sits: Claude, Codex, then every other agent. */
const providerRank = (provider: string) => provider === 'claude' ? 0 : provider === 'codex' ? 1 : 2;

/** Compares providers in group order: Claude, Codex, then the others by name. */
export function compareProviders(a: string, b: string, profiles: readonly ProfileLike[] = []): number {
  return providerRank(a) - providerRank(b) || agentTitle(a, profiles, a).localeCompare(agentTitle(b, profiles, b)) || (a < b ? -1 : a > b ? 1 : 0);
}

/** Items in groups by provider, groups in order (compareProviders); each group keeps the items' own order. */
export function groupByProvider<T>(items: readonly T[], providerOf: (item: T) => string, profiles: readonly ProfileLike[] = []): ProviderGroup<T>[] {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const provider = providerOf(item) || '';
    const group = groups.get(provider);
    if (group) group.push(item); else groups.set(provider, [item]);
  }
  return [...groups.keys()].sort((a, b) => compareProviders(a, b, profiles))
    .map(provider => ({ provider, title: agentTitle(provider, profiles, provider || 'Other'), icon: agentIcon(provider), items: groups.get(provider)! }));
}

/** Subscriptions by provider, each group in the one order (orderSubscriptions). */
export function subscriptionGroups<T extends SubscriptionLike>(profiles: readonly T[], options: { currentId?: string; now?: number } = {}): ProviderGroup<T>[] {
  return groupByProvider(orderSubscriptions(profiles, options), profile => profile.provider, profiles);
}

/**
 * A subscription as a picker option says it: its name, and when it cannot
 * take work now, why and when it is back ("Work · Limit reached · back at
 * 6:00 PM").
 */
export function subscriptionOption(profile: SubscriptionLike, now = Date.now()): string {
  const name = subscriptionName(profile, profile.id);
  return subscriptionReady(profile, now) ? name : `${name} · ${subscriptionDetail(profile, now)}`;
}
