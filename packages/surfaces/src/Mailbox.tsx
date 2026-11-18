import { useSyncExternalStore } from 'react';
import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Collection, MailboxItem, ReleaseWatchItem } from '@lazybox/protocol';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { humanError } from './human-error';
import type { ViewTarget } from './workspace-model';

type Snapshot<T> = { items: readonly T[]; error: string; truncated: boolean; loaded: boolean };
const empty: Snapshot<never> = { items: [], error: '', truncated: false, loaded: false };

class InboxSourcePoll<T> {
  enabled = false;
  snapshot: Snapshot<T> = empty;
  private listeners = new Set<() => void>();
  private request?: AbortController;
  private timer?: ReturnType<typeof setTimeout>;

  constructor(
    private preference: string,
    private interval: number,
    private read: (signal: AbortSignal) => Promise<{ items: T[]; truncated: boolean }>,
  ) {}

  state = () => this.snapshot;
  active = () => this.enabled;

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) {
      this.readPreference();
      window.addEventListener('storage', this.preferenceChanged);
      window.addEventListener(this.preference, this.preferenceChanged);
      document.addEventListener('visibilitychange', this.visible);
      void this.load();
    }
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size) return;
      window.removeEventListener('storage', this.preferenceChanged);
      window.removeEventListener(this.preference, this.preferenceChanged);
      document.removeEventListener('visibilitychange', this.visible);
      this.stop();
    };
  };

  configure = (enabled: boolean) => {
    try { localStorage.setItem(this.preference, enabled ? 'on' : 'off'); } catch { /* this window still keeps the choice */ }
    this.applyPreference(enabled);
    window.dispatchEvent(new Event(this.preference));
  };

  private readPreference() {
    try { this.applyPreference(localStorage.getItem(this.preference) === 'on'); } catch { /* storage is optional */ }
  }

  private preferenceChanged = () => this.readPreference();
  private visible = () => { if (!document.hidden) void this.load(); };

  private applyPreference(enabled: boolean) {
    if (enabled === this.enabled) return;
    this.enabled = enabled;
    if (enabled) {
      this.emit(empty);
      void this.load();
    } else {
      this.stop();
      this.emit(empty);
    }
  }

  private stop() {
    clearTimeout(this.timer);
    this.request?.abort();
    this.request = undefined;
  }

  private emit(next: Snapshot<T>) {
    this.snapshot = next;
    for (const listener of this.listeners) listener();
  }

  load = async () => {
    if (!this.enabled || !this.listeners.size || this.request || document.hidden) return;
    clearTimeout(this.timer);
    const control = this.request = new AbortController();
    let retry = true;
    try {
      const { items, truncated } = await this.read(control.signal);
      if (!control.signal.aborted) this.emit({ items, truncated, loaded: true, error: '' });
    } catch (error) {
      if (control.signal.aborted) return;
      const code = ConnectError.from(error).code;
      retry = code !== Code.Unimplemented;
      this.emit({ ...empty, loaded: true, error: retry ? humanError(error) : 'Update this Runner to use this GitHub integration.' });
    } finally {
      if (this.request === control) this.request = undefined;
      if (!control.signal.aborted && retry && this.enabled) this.timer = setTimeout(this.load, this.interval);
    }
  };
}

const mailboxPolls = new WeakMap<RunnerClient, InboxSourcePoll<MailboxItem>>();
const releasePolls = new WeakMap<RunnerClient, InboxSourcePoll<ReleaseWatchItem>>();

function useInboxSource<T>(polls: WeakMap<RunnerClient, InboxSourcePoll<T>>, preference: string, interval: number,
  read: (api: RunnerClient, signal: AbortSignal) => Promise<{ items: T[]; truncated: boolean }>) {
  const { api } = useRunner();
  let poll = polls.get(api);
  if (!poll) {
    poll = new InboxSourcePoll(preference, interval, signal => read(api, signal));
    polls.set(api, poll);
  }
  const state = useSyncExternalStore(poll.subscribe, poll.state);
  const enabled = useSyncExternalStore(poll.subscribe, poll.active);
  return { ...state, enabled, configure: poll.configure, refresh: poll.load };
}

function useMailbox() {
  return useInboxSource(mailboxPolls, 'lazybox:github-mailbox', 120_000,
    (api, signal) => api.listMailbox({}, { signal, timeoutMs: 30_000 }));
}

function useReleaseWatch() {
  return useInboxSource(releasePolls, 'lazybox:release-watch', 300_000,
    (api, signal) => api.listReleaseWatch({}, { signal, timeoutMs: 45_000 }));
}

export function useGitHubInboxCount() {
  const mailbox = useMailbox();
  const releases = useReleaseWatch();
  return mailbox.items.length + releases.items.length;
}

const reasons: Record<string, string> = {
  review_requested: 'Review requested',
  assign: 'Assigned to you',
  mention: 'Mentioned you',
  team_mention: 'Mentioned your team',
  comment: 'New comment',
  author: 'Activity on your item',
  subscribed: 'Subscribed activity',
  state_change: 'State changed',
  ci_activity: 'CI activity',
};

export function MailboxSettings() {
  const mailbox = useMailbox();
  return <label>
    <input type="checkbox" checked={mailbox.enabled} onChange={event => mailbox.configure(event.target.checked)}/>
    {' '}Show GitHub Mailbox in Inbox on this device
  </label>;
}

export function GitHubInboxSections({ collections, onOpen }: {
  collections: readonly Collection[];
  onOpen(target: ViewTarget): void;
}) {
  const mailbox = useMailbox();
  const releases = useReleaseWatch();
  return <>
    {mailbox.enabled && <GitHubInboxSection title="GitHub Mailbox" icon="inbox" source={mailbox} collections={collections} onOpen={onOpen}
      emptyMessage="No unread GitHub notifications."
      itemTitle={item => item.title} itemDetail={item => reasons[item.reason] || 'GitHub activity'}/>}
    {releases.enabled && <GitHubInboxSection title="Release Watch" icon="bell" source={releases} collections={collections} onOpen={onOpen}
      emptyMessage="No failed deployments or approvals waiting for you."
      itemTitle={item => item.environment || 'Deployment'}
      itemDetail={item => item.reason === 'deployment_approval' ? 'Deployment needs approval' : 'Deployment failed'}/>}
  </>;
}

export function ReleaseWatchSettings() {
  const releases = useReleaseWatch();
  return <label>
    <input type="checkbox" checked={releases.enabled} onChange={event => releases.configure(event.target.checked)}/>
    {' '}Show GitHub deployments and approval requests in Inbox on this device
  </label>;
}

function GitHubInboxSection<T extends { id: string; repository: string; url: string; workspaceIds: string[] }>({
  title, icon, source, collections, onOpen, emptyMessage, itemTitle, itemDetail,
}: {
  title: string;
  icon: string;
  source: Snapshot<T> & { refresh(): void };
  collections: readonly Collection[];
  onOpen(target: ViewTarget): void;
  emptyMessage: string;
  itemTitle(item: T): string;
  itemDetail(item: T): string;
}) {
  return <section className="inbox-group" aria-label={title}>
    <h3><Icon name={icon} size={13}/>{title}<span>{source.items.length}</span></h3>
    {source.error ? <p role="alert">{source.error} <button onClick={source.refresh}>Retry {title}</button></p>
      : !source.loaded ? <p role="status">Loading GitHub activity…</p>
      : !source.items.length ? <p>{emptyMessage}</p> : <ul>
        {source.items.map(item => <li className="inbox-item pr-inbox-item" key={item.id}>
          <a className="inbox-row" href={item.url} target="_blank" rel="noreferrer">
            <Icon name="external" size={13}/><span className="pr-inbox-text">
              <strong>{itemTitle(item)}</strong>
              <small>{item.repository} · {itemDetail(item)}</small>
            </span>
          </a>
          <div className="pr-inbox-actions">
            {collections.filter(workspace => item.workspaceIds.includes(workspace.id)).map(workspace =>
              <button key={workspace.id} onClick={() => onOpen({ collectionId: workspace.id, sessionId: '', commandId: '' })}>{workspace.name}</button>)}
          </div>
        </li>)}
      </ul>}
    {source.truncated && <p>Some GitHub activity could not be loaded.</p>}
  </section>;
}
