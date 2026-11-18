import type { ReactNode } from 'react';
import type { SubscriptionLike } from './subscription-model';
import { Icon } from './Icons';
import { subscriptionGroups, subscriptionOption, type ProviderGroup } from './subscription-groups';
import './subscription-groups.css';

/** A provider group's small header: its icon and name ("Claude", "Codex", "Gemini CLI"). */
export function ProviderHeading({ group, id }: { group: Pick<ProviderGroup<unknown>, 'icon' | 'title' | 'provider'>; id?: string }) {
  return <div className="provider-group-heading" id={id} data-provider={group.provider}><Icon name={group.icon} size={12}/><span>{group.title}</span></div>;
}

/**
 * Groups, each under its header; one group shows no header (there is nothing
 * to tell apart). Keyboard order is the document's, so arrow keys and Tab go
 * from one group into the next.
 */
export function ProviderGroups<T>({ groups, className, children }: { groups: ProviderGroup<T>[]; className?: string; children(items: T[], group: ProviderGroup<T>): ReactNode }) {
  if (groups.length <= 1) return <>{groups.map(group => <div key={group.provider} className={className}>{children(group.items, group)}</div>)}</>;
  return <>{groups.map(group => <section key={group.provider} className="provider-group" data-provider={group.provider} aria-label={group.title}>
    <ProviderHeading group={group}/>
    <div className={className}>{children(group.items, group)}</div>
  </section>)}</>;
}

/**
 * A `<select>`'s subscription options in the one order, as `<optgroup>`s by
 * provider when there is more than one; a limited one says when it is back.
 */
export function SubscriptionOptions<T extends SubscriptionLike>({ profiles, now = Date.now(), label = subscriptionOption }: { profiles: readonly T[]; now?: number; label?(profile: T, now: number, grouped: boolean): string }) {
  const groups = subscriptionGroups(profiles, { now });
  const grouped = groups.length > 1;
  const options = (items: readonly T[]) => items.map(p => <option key={p.id} value={p.id}>{label(p, now, grouped)}</option>);
  return grouped ? <>{groups.map(group => <optgroup key={group.provider} label={group.title}>{options(group.items)}</optgroup>)}</> : <>{options(groups[0]?.items || [])}</>;
}
