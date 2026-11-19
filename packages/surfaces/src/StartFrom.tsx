import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { RepositoryBranches } from '@lazybox/protocol';
import { Icon } from './Icons';
import { isDefaultStart, rankBranches, startLabel, type BranchOption, type BranchSection } from './start-from-model';
import './start-from.css';

/**
 * "Start from" (WORKSPACES.md): the branch of origin a repository's new
 * workspace branches from. Each chosen repository shows a small chip, "from
 * main ▾"; clicking it or ⌥B opens a searchable picker of origin's branches,
 * asked of origin only when the picker opens and kept for a minute. The
 * picker offers recent branches. A new sheet selects the default.
 */

type Entry = { info?: RepositoryBranches; remoteAt?: number; loading?: Promise<void>; asking?: Promise<RepositoryBranches> };
/** Per Runner client: what each repository's branches were, so reopening the sheet is instant. */
const caches = new WeakMap<RunnerClient, Map<string, Entry>>();
const cacheOf = (api: RunnerClient) => { let cache = caches.get(api); if (!cache) caches.set(api, cache = new Map()); return cache; };
/** How long origin's branch list is reused before the picker asks origin again. */
const REMOTE_FRESH_MS = 60_000;

/**
 * Each repository's branches as the Runner knows them without asking origin
 * (its default branch and remembered start), loaded once per repository.
 */
export function useRepositoryStarts(api: RunnerClient, identities: readonly string[]): [Record<string, RepositoryBranches | undefined>, (identity: string, info: RepositoryBranches) => void] {
  const cache = cacheOf(api);
  const [, setRevision] = useState(0);
  const key = identities.join('\n');
  useEffect(() => {
    let live = true;
    for (const identity of identities) {
      const entry = cache.get(identity) || {};
      if (entry.info || entry.loading) continue;
      entry.loading = api.listRepositoryBranches({ repositoryIdentity: identity, remote: false }, { timeoutMs: 10_000 })
        .then(info => { entry.info = info; }, () => { /* the chip says "default branch"; the Runner still decides */ })
        .finally(() => { entry.loading = undefined; if (live) setRevision(n => n + 1); });
      cache.set(identity, entry);
    }
    return () => { live = false; };
  }, [api, key]);
  const infos: Record<string, RepositoryBranches | undefined> = {};
  for (const identity of identities) infos[identity] = cache.get(identity)?.info;
  const update = (identity: string, info: RepositoryBranches) => {
    const entry = cache.get(identity) || {};
    entry.info = info;
    if (info.remote) entry.remoteAt = Date.now();
    cache.set(identity, entry);
    setRevision(n => n + 1);
  };
  return [infos, update];
}

/**
 * After a preparation: the Runner now remembers the choices (and may have
 * just cloned a repository), so the next sheet asks it again rather than
 * trusting what this window knew.
 */
export function forgetStarts(api: RunnerClient, identities: readonly string[]) {
  const cache = cacheOf(api);
  for (const identity of identities) cache.delete(identity);
}

/** The chip on a chosen repository: "from main ▾". A branch other than the default reads plainly. */
export function StartFromChip({ identity, repository, branch, info, disabled, expanded, onOpen }: {
  identity: string; repository: string; branch: string; info?: RepositoryBranches; disabled?: boolean; expanded: boolean; onOpen(): void;
}) {
  const label = startLabel(branch, info);
  const quiet = isDefaultStart(branch, info);
  return <button type="button" className="start-from-chip" data-start-from={identity} data-default={quiet || undefined} disabled={disabled}
    aria-haspopup="dialog" aria-expanded={expanded} aria-label={`${repository} starts from ${label}. Change (⌥B)`} title={`New branch from origin's ${label} · change (⌥B)`}
    onClick={event => { event.stopPropagation(); onOpen(); }}>
    <span className="start-from-from">from</span><span className="start-from-name">{label}</span><Icon name="chevron" size={11}/>
  </button>;
}

const SECTION: Record<BranchSection, string> = { default: '', recent: 'Recent', active: 'Active', all: 'All branches', typed: '' };

/**
 * The branch picker for one repository: type to filter, ↑↓ and Enter to
 * choose, Escape to close. It lists what the Runner knows at once and asks
 * origin for its branches when opened (at most once a minute).
 */
export function StartFromPicker({ api, identity, repository, info, current, onInfo, onPick, onClose }: {
  api: RunnerClient; identity: string; repository: string; info?: RepositoryBranches; current: string;
  onInfo(info: RepositoryBranches): void; onPick(branch: string): void; onClose(): void;
}) {
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const [asking, setAsking] = useState(false);
  const [failure, setFailure] = useState('');
  useEffect(() => { input.current?.focus(); }, []);
  // Origin's branches, lazily: only now the picker is open, and reused for a minute.
  useEffect(() => {
    const cache = cacheOf(api);
    const entry = cache.get(identity) || {};
    if (info?.unregistered || (entry.remoteAt && Date.now() - entry.remoteAt < REMOTE_FRESH_MS)) return;
    let live = true;
    setAsking(true);
    // One question to origin at a time per repository, however often the picker mounts.
    entry.asking ||= api.listRepositoryBranches({ repositoryIdentity: identity, remote: true }, { timeoutMs: 40_000 })
      .finally(() => { entry.asking = undefined; });
    cache.set(identity, entry);
    entry.asking
      .then(next => { if (live) onInfo(next); }, failure => { if (live) setFailure(String(failure).replace(/^(Connect)?Error: (\[\w+\] )?/, '')); })
      .finally(() => { if (live) setAsking(false); });
    return () => { live = false; };
  }, [api, identity]);
  const options = useMemo(() => rankBranches(info, query), [info, query]);
  useEffect(() => { setActive(0); }, [query]);
  useEffect(() => { document.getElementById(`${id}-branch-${active}`)?.scrollIntoView({ block: 'nearest' }); }, [active]);
  const pick = (option: BranchOption | undefined) => { if (option) onPick(option.name); };
  const keys = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing) return;
    switch (event.key) {
      case 'ArrowDown': event.preventDefault(); setActive(a => options.length ? (a + 1) % options.length : 0); return;
      case 'ArrowUp': event.preventDefault(); setActive(a => options.length ? (a - 1 + options.length) % options.length : 0); return;
      case 'Home': if (!query) { event.preventDefault(); setActive(0); } return;
      case 'End': if (!query) { event.preventDefault(); setActive(Math.max(options.length - 1, 0)); } return;
      case 'Enter': if (event.metaKey || event.ctrlKey) return; event.preventDefault(); event.stopPropagation(); pick(options[active]); return;
      case 'Escape': event.preventDefault(); event.stopPropagation(); onClose(); return;
      case 'Tab': onClose(); return;
    }
  };
  let previous: BranchSection | undefined;
  return <div className="start-from-picker" role="dialog" aria-label={`Start ${repository} from`} onMouseDown={event => { if (event.target !== input.current) event.preventDefault(); }}>
    <div className="start-from-search">
      <Icon name="branch" size={13}/>
      <input ref={input} role="combobox" aria-expanded="true" aria-controls={`${id}-branches`} aria-autocomplete="list" aria-activedescendant={options.length ? `${id}-branch-${active}` : undefined}
        aria-label={`Branch of ${repository} to start from`} placeholder="Search branches, or type one…" spellCheck={false} autoComplete="off"
        value={query} onChange={event => setQuery(event.target.value)} onKeyDown={keys} onBlur={event => {
          const to = event.relatedTarget as HTMLElement | null;
          // Its own chip toggles it closed on the click; anywhere else closes it now.
          if (!event.currentTarget.closest('.start-from-picker')?.contains(to) && to?.dataset?.startFrom !== identity) onClose();
        }}/>
      {asking && <small className="start-from-asking" role="status">Asking origin…</small>}
    </div>
    <ul className="start-from-list" id={`${id}-branches`} role="listbox" aria-label={`Branches of ${repository}`}>
      {options.map((option, index) => {
        // Sections only for the whole list: a search is ranked by how well each name matches.
        const header = !query.trim() && option.section !== previous ? SECTION[option.section] : '';
        previous = option.section;
        return [
          header && <li key={`h-${option.section}`} role="presentation" className="start-from-section">{header}</li>,
          <li key={option.name + option.section} id={`${id}-branch-${index}`} role="option" aria-selected={index === active} className={index === active ? 'active' : undefined}
            data-current={option.name === current || (!current && option.name === info?.defaultBranch) || undefined}
            onPointerMove={() => setActive(index)} onClick={() => pick(option)}>
            {option.section === 'typed'
              ? <><Icon name="plus" size={12}/><span className="start-from-option">Start from <code>{option.name}</code></span><small>checked with origin</small></>
              : <><span className="start-from-option mono">{option.name}</span>{option.name === info?.defaultBranch && <small>default</small>}{(option.name === current || (!current && option.name === info?.defaultBranch)) && <Icon name="check" size={12}/>}</>}
          </li>,
        ];
      })}
      {!options.length && <li className="start-from-empty" aria-disabled="true">{info?.unregistered ? 'Its branches are listed once the repository is on this Runner. Type a branch name.' : query ? `No branch matches “${query}”` : asking ? 'Asking origin for its branches…' : 'No branches known yet.'}</li>}
    </ul>
    {(failure || info?.error) && <p className="start-from-note" role="status">{info?.error || `Couldn't ask origin: ${failure}`}</p>}
  </div>;
}
