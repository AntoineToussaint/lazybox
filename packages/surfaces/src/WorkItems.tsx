import { useEffect, useId, useRef, useState, type KeyboardEvent } from 'react';
import { ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Collection, Item, ItemDetail } from '@lazybox/protocol';

/** An item's reference as the sheet holds it (the protocol's ItemRef, as plain data). */
type ItemRef = { provider: string; key: string };
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { ITEM_SCOPES, itemNoun, itemRepository, nextScope, providerName, quotedTitle, stateTone, updatedAgo, type ItemScope, type QuotedItem } from './item-context';
import './work-items.css';

/**
 * Work items in the UI (PROJECTS_AND_INTEGRATIONS.md): the issue a New task
 * starts from (its chip, the exact context the agent will get, the picker),
 * the chip of the quoted item in a conversation's first message, and the
 * workspace header's origin chip. Pure parts in item-context.ts.
 */

const failure = (error: unknown) => ConnectError.from(error).rawMessage || String(error);
const refKey = (ref: Pick<ItemRef, 'provider' | 'key'>) => `${ref.provider || 'github'}:${ref.key}`;

/** An item's state as a small dot; its words are in the title. */
function StateDot({ category, label }: { category?: string; label?: string }) {
  return <span className="item-state-dot" data-tone={stateTone(category)} title={label} aria-hidden="true"/>;
}

/** Item reads, shared by the sheet and the headers for a few minutes. */
const details = new Map<string, { at: number; value: Promise<ItemDetail> }>();
const DETAIL_TTL = 5 * 60_000;
function readDetail(api: RunnerClient, ref: ItemRef, fresh = false): Promise<ItemDetail> {
  const key = refKey(ref), cached = details.get(key);
  if (cached && !fresh && Date.now() - cached.at < DETAIL_TTL) return cached.value;
  const value = api.getItem({ ref, comments: 0 }, { timeoutMs: 25_000 });
  details.set(key, { at: Date.now(), value });
  value.catch(() => { if (details.get(key)?.value === value) details.delete(key); });
  return value;
}

/** An item with its context, loaded for the sheet; retry() reads it again. */
export function useItemDetail(api: RunnerClient, ref: ItemRef | undefined) {
  const [state, setState] = useState<{ key: string; detail?: ItemDetail; error?: string }>({ key: '' });
  const [attempt, setAttempt] = useState(0);
  const key = ref ? refKey(ref) : '';
  useEffect(() => {
    if (!ref) { setState({ key: '' }); return; }
    let alive = true;
    setState(current => current.key === key && current.detail ? current : { key });
    readDetail(api, ref, attempt > 0).then(detail => { if (alive) setState({ key, detail }); }, error => { if (alive) setState({ key, error: failure(error) }); });
    return () => { alive = false; };
  }, [api, key, attempt]);
  const current = state.key === key ? state : { key };
  return { detail: current.detail, error: current.error, loading: !!ref && !current.detail && !current.error, retry: () => setAttempt(n => n + 1) };
}

/** The workspaces already started from an item: "already has a workspace". */
export function useStartedFrom(api: RunnerClient, ref: ItemRef | undefined) {
  const [found, setFound] = useState<{ key: string; workspaces: Collection[] }>({ key: '', workspaces: [] });
  const key = ref ? refKey(ref) : '';
  useEffect(() => {
    if (!ref) return;
    let alive = true;
    api.findItemWorkspaces({ ref }, { timeoutMs: 10_000 }).then(result => { if (alive) setFound({ key, workspaces: result.workspaces }); }, () => { if (alive) setFound({ key, workspaces: [] }); });
    return () => { alive = false; };
  }, [api, key]);
  return found.key === key ? found.workspaces : [];
}

/**
 * The item a New task starts from: its key, title and provider, and (folded)
 * exactly what the agent will receive, with the comments toggle.
 */
export function ItemSource({ refValue, detail, error, loading, withoutComments, disabled, onComments, onRetry, onRemove }: { refValue: ItemRef; detail?: ItemDetail; error?: string; loading: boolean; withoutComments: boolean; disabled?: boolean; onComments(include: boolean): void; onRetry(): void; onRemove(): void }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const item = detail?.item;
  const count = detail ? Math.max(detail.commentCount, detail.comments.length) : 0;
  const shown = detail ? (withoutComments ? detail.contextWithoutComments : detail.context) : '';
  const bytes = new TextEncoder().encode(shown).length;
  return <div className="item-source" data-state={error ? 'failed' : loading ? 'loading' : 'ready'}>
    <div className="item-source-chip" role="group" aria-label={`From ${refValue.key}`}>
      <span className="item-source-icon"><Icon name="issue" size={14}/></span>
      <span className="item-source-key">{refValue.key}</span>
      {item && <StateDot category={item.state?.category} label={item.state?.label}/>}
      <span className="item-source-title" title={item?.title}>{item ? item.title : loading ? 'Reading the issue…' : 'Could not read it'}</span>
      <small className="item-source-provider">{providerName(refValue.provider)}</small>
      {detail && <button type="button" className="item-source-toggle" aria-expanded={open} aria-controls={`${id}-preview`} disabled={disabled} title={open ? 'Hide what the agent gets' : 'Show exactly what the agent gets'} onClick={() => setOpen(value => !value)}>
        <Icon name={open ? 'collapse' : 'expand'} size={12}/><span>{open ? 'Hide' : 'What the agent gets'}</span>
      </button>}
      <button type="button" className="item-source-remove" aria-label={`Remove ${refValue.key}`} title="Start without the issue" disabled={disabled} onClick={onRemove}><Icon name="close" size={12}/></button>
    </div>
    {error && <p className="item-source-error" role="alert">{error} <button type="button" className="link-button" onClick={onRetry}>Retry</button><span> · the task can start without its text.</span></p>}
    {open && detail && <div className="item-source-preview" id={`${id}-preview`}>
      <div className="item-source-preview-bar">
        <label className="item-source-comments">
          <input type="checkbox" checked={!withoutComments} disabled={disabled || !count} onChange={event => onComments(event.target.checked)}/>
          {count ? `Include the comments (${detail.comments.length < count ? `the last ${detail.comments.length} of ${count}` : count})` : 'No comments'}
        </label>
        <small>{(bytes / 1024).toFixed(1)} KB · quoted as external content: the agent treats it as data, not instructions</small>
      </div>
      <pre className="item-source-text" tabIndex={0} aria-label="What the agent gets">{shown}</pre>
    </div>}
  </div>;
}

/**
 * The picker behind "From an issue…": it searches as you type and opens on
 * the issues assigned to you. ↑↓ choose, Enter takes one, Tab changes the
 * scope, Esc closes. A pasted link or owner/name#N finds that one issue.
 */
export function ItemPicker({ onPick, onClose }: { onPick(item: Item): void; onClose(): void }) {
  const { api } = useRunner();
  const [query, setQuery] = useState(''), [scope, setScope] = useState<ItemScope>('mine');
  const [result, setResult] = useState<{ items: Item[]; error: string; loading: boolean }>({ items: [], error: '', loading: true });
  const [active, setActive] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const id = useId();
  useEffect(() => { input.current?.focus(); }, []);
  useEffect(() => {
    const control = new AbortController();
    setResult(current => ({ ...current, loading: true, error: '' }));
    const timer = setTimeout(() => {
      api.searchItems({ provider: 'github', query, scope, cursor: '' }, { signal: control.signal, timeoutMs: 25_000 })
        .then(page => { if (!control.signal.aborted) { setResult({ items: page.items, error: '', loading: false }); setActive(0); } },
          error => { if (!control.signal.aborted) setResult({ items: [], error: failure(error), loading: false }); });
    }, query ? 250 : 0);
    return () => { control.abort(); clearTimeout(timer); };
  }, [api, query, scope]);
  useEffect(() => { document.getElementById(`${id}-item-${active}`)?.scrollIntoView({ block: 'nearest' }); }, [active]);
  const items = result.items;
  const keys = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing) return;
    switch (event.key) {
      case 'ArrowDown': event.preventDefault(); setActive(a => items.length ? (a + 1) % items.length : 0); return;
      case 'ArrowUp': event.preventDefault(); setActive(a => items.length ? (a - 1 + items.length) % items.length : 0); return;
      case 'Enter': event.preventDefault(); event.stopPropagation(); if (items[active]) onPick(items[active]); return;
      case 'Escape': event.preventDefault(); event.stopPropagation(); onClose(); return;
      case 'Tab': event.preventDefault(); setScope(s => nextScope(s, event.shiftKey ? -1 : 1)); return;
    }
  };
  const label = ITEM_SCOPES.find(s => s.id === scope)!.label;
  const empty = result.loading ? 'Searching…' : result.error ? result.error : scope === 'all' && !query.trim() ? 'Type to search every issue you can read on GitHub' : query ? `No open issue matches “${query}”` : scope === 'mine' ? 'No open issue is assigned to you. Tab looks wider.' : 'No open issue involves you.';
  return <div className="item-picker" role="dialog" aria-label="Start from an issue" onMouseDown={event => { if (event.target !== input.current) event.preventDefault(); }}>
    <div className="item-picker-search">
      <Icon name="search" size={13}/>
      <input ref={input} role="combobox" aria-expanded="true" aria-controls={`${id}-items`} aria-activedescendant={items.length ? `${id}-item-${active}` : undefined} aria-label="Search issues" autoComplete="off" spellCheck={false}
        placeholder="Search issues, or paste a link…" value={query} onChange={event => setQuery(event.target.value)} onKeyDown={keys} onBlur={event => { if (!event.currentTarget.closest('.item-picker')?.contains(event.relatedTarget as Node | null)) onClose(); }}/>
    </div>
    <div className="item-picker-scopes" role="tablist" aria-label="Which issues">
      {ITEM_SCOPES.map(s => <button key={s.id} type="button" role="tab" tabIndex={-1} aria-selected={s.id === scope} onClick={() => { setScope(s.id); input.current?.focus(); }}>{s.label}</button>)}
      <small aria-hidden="true"><kbd>Tab</kbd> {label === 'All of GitHub' ? 'back to yours' : 'wider'}</small>
    </div>
    <ul className="item-picker-list" id={`${id}-items`} role="listbox" aria-label={`Issues · ${label}`} aria-busy={result.loading || undefined}>
      {items.map((item, index) => {
        const key = item.ref?.key || '';
        return <li key={key} id={`${id}-item-${index}`} role="option" aria-selected={index === active} className={index === active ? 'active' : undefined}
          onPointerMove={() => setActive(index)} onClick={() => onPick(item)}>
          <StateDot category={item.state?.category} label={item.state?.label}/>
          <span className="item-picker-repo">{itemRepository(key)}</span>
          <span className="item-picker-number">#{key.split('#').pop()}</span>
          <span className="item-picker-title">{item.title}</span>
          <span className="item-picker-labels">{item.labels.slice(0, 3).map(name => <span key={name}>{name}</span>)}</span>
          <small className="item-picker-time" title={item.updatedAtUnixMillis ? new Date(Number(item.updatedAtUnixMillis)).toLocaleString() : undefined}>{updatedAgo(Number(item.updatedAtUnixMillis))}</small>
        </li>;
      })}
      {!items.length && <li className="item-picker-empty" aria-disabled="true" role={result.error ? 'alert' : undefined}>{empty}</li>}
    </ul>
    <p className="item-picker-keys" aria-hidden="true"><kbd>↑</kbd><kbd>↓</kbd> choose · <kbd>↵</kbd> start from it · <kbd>Esc</kbd> close</p>
  </div>;
}

/**
 * "acme/api#812 already has a workspace, <name>": open that one, or start
 * another (the start then makes a second workspace, on its own branch).
 */
export function StartedNotice({ refValue, workspaces, onOpen, onAnother }: { refValue: ItemRef; workspaces: Collection[]; onOpen(workspace: Collection): void; onAnother(): void }) {
  const first = workspaces[0];
  if (!first) return null;
  const more = workspaces.length > 1 ? ` (and ${workspaces.length - 1} more)` : '';
  return <div className="item-started" role="status">
    <Icon name="collections" size={13}/>
    <span><strong>{refValue.key}</strong> already has a workspace, <em>{first.name}</em>{more}</span>
    <button type="button" onClick={() => onOpen(first)}>Open it</button>
    <button type="button" className="link-button" onClick={onAnother}>Start another</button>
  </div>;
}

/**
 * The quoted item a message carries, as a chip: its key and title, opening
 * the issue; expanded, exactly what the agent received.
 */
export function QuotedItemChip({ item }: { item: QuotedItem }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const title = quotedTitle(item.quoted);
  return <div className="item-quoted">
    <div className="item-quoted-chip">
      <Icon name="issue" size={13}/>
      <a href={item.url} target="_blank" rel="noreferrer" className="item-quoted-key">{item.key}</a>
      {title && <span className="item-quoted-title" title={title}>{title}</span>}
      <small>{item.noun}</small>
      <button type="button" className="item-quoted-toggle" aria-expanded={open} aria-controls={`${id}-text`} aria-label={`What the agent got from ${item.key}`} title={open ? 'Hide what the agent got' : 'Show exactly what the agent got'} onClick={() => setOpen(value => !value)}>
        <Icon name={open ? 'collapse' : 'expand'} size={12}/>
      </button>
    </div>
    {open && <pre className="item-source-text" id={`${id}-text`} tabIndex={0} aria-label={`What the agent got from ${item.key}`}>{item.text}</pre>}
  </div>;
}

/**
 * The workspace's origin in its header: the item's key, title and state,
 * opening it in the browser or the Web panel (the window's link routing).
 * Its state is read from the provider once in a while; until then (or when
 * it cannot be read) the key alone shows.
 */
export function ItemOriginChip({ workspace }: { workspace?: Collection }) {
  const { api } = useRunner();
  const origin = workspace?.origin;
  const [item, setItem] = useState<Item>();
  const key = origin ? refKey(origin) : '';
  useEffect(() => {
    setItem(undefined);
    if (!origin?.key) return;
    let alive = true;
    readDetail(api, origin).then(detail => { if (alive) setItem(detail.item); }, () => { /* the key alone */ });
    return () => { alive = false; };
  }, [api, key]);
  if (!origin?.key) return null;
  const url = item?.url || (origin.provider === 'github' || !origin.provider ? `https://github.com/${itemRepository(origin.key)}/issues/${origin.key.split('#').pop()}` : '');
  const tip = [`Started from ${origin.key}`, item?.title, item?.state?.label].filter(Boolean).join(' · ');
  const body = <>
    <Icon name="issue" size={12}/>
    <span className="item-origin-key">{origin.key}</span>
    {item && <span className="item-origin-title">{item.title}</span>}
    {item && <StateDot category={item.state?.category} label={item.state?.label}/>}
  </>;
  return url
    ? <a className="item-origin-chip" href={url} target="_blank" rel="noreferrer" title={`${tip}\nOpen the ${itemNoun(item?.kind || 'issue')}`} aria-label={`Started from ${origin.key}${item ? `: ${item.title}, ${item.state?.label || ''}` : ''}`}>{body}</a>
    : <span className="item-origin-chip" title={tip}>{body}</span>;
}
