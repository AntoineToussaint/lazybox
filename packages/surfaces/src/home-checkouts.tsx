/**
 * Home checkouts in the window: the person's own clone of each repository,
 * outside every workspace, for logins, long-lived dev servers and trying a
 * module by hand (WORKSPACES.md, "Home checkouts"). Settings → Repositories
 * lists them (create one, use an existing folder, forget one); the
 * workspace's repositories dialog and the Terminal panel open a shell in one;
 * the chat offers "Run in <repo> home" (`code-runs.tsx`, `home-run.ts`).
 */
import { useEffect, useState, useSyncExternalStore } from 'react';
import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Collection, Repository, RepositoryHome } from '@lazybox/protocol';
import { copyText, openContextMenu, pageFilesAreHere, showMenuNotice } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import { openHomeShell } from './home-shell';
import { Icon } from './Icons';
import { SettingsGroup, SettingsRow, settingsError } from './settings-parts';
import { useRunner } from './RunnerContext';
import { shortPath } from './tool-lines';
import './home-checkouts.css';

// ---- What the window knows -------------------------------------------------

/** Every repository of the library, with its home; read once and shared, re-read when a home changes. */
let library: { api?: RunnerClient; at: number; repositories: Repository[]; loading?: Promise<void> } = { at: 0, repositories: [] };
const listeners = new Set<() => void>();
let revision = 0;
const emit = () => { revision++; for (const listener of listeners) listener(); };
function load(api: RunnerClient, force = false) {
  if (library.api === api && (library.loading || (!force && Date.now() - library.at < 15000))) return;
  library = { ...library, api, loading: api.listRepositories({}).then(value => { library = { api, at: Date.now(), repositories: value.repositories }; emit(); }, () => { library = { ...library, loading: undefined }; }) };
}
/** Re-read homes everywhere: after one is created, adopted or forgotten. */
export function homesChanged(api: RunnerClient) { load(api, true); }
export function useLibrary(api: RunnerClient): Repository[] {
  useEffect(() => { load(api); }, [api]);
  useSyncExternalStore(subscribe, () => revision);
  return library.api === api ? library.repositories : [];
}
function subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; }
/** The library as last read for `api` (reading it when stale), for code outside React. */
export function libraryOf(api: RunnerClient): Repository[] { load(api); return library.api === api ? library.repositories : []; }
export const subscribeLibrary = subscribe;

/** A member repository of a workspace with a home: what the Terminal panel's directory list offers. */
export type HomeChoice = { repositoryId: string; label: string; path: string };
export function useHomeChoices(api: RunnerClient, workspace?: Collection): HomeChoice[] {
  const repositories = useLibrary(api);
  const out: HomeChoice[] = [];
  for (const member of workspace?.members || []) {
    const repo = repositories.find(r => r.id === member.spec?.repositoryId);
    if (repo?.homePath && !out.some(choice => choice.repositoryId === repo.id)) out.push({ repositoryId: repo.id, label: `${member.spec?.name || repo.name} home`, path: repo.homePath });
  }
  return out;
}

// ---- Opening one -------------------------------------------------------------

export const EDITORS = [
  { id: 'vscode', label: 'VS Code' }, { id: 'cursor', label: 'Cursor' }, { id: 'zed', label: 'Zed' },
  { id: 'sublime', label: 'Sublime Text' }, { id: 'intellij', label: 'IntelliJ IDEA' },
] as const;
const EDITOR_KEY = 'lazybox.homeEditor';
/** The editor "Open in editor" uses: this computer's choice, kept in browser storage. */
export function homeEditor(): string {
  try { const saved = localStorage.getItem(EDITOR_KEY); if (EDITORS.some(e => e.id === saved)) return saved!; } catch { /* storage is a convenience */ }
  return 'vscode';
}
function setHomeEditor(id: string) { try { localStorage.setItem(EDITOR_KEY, id); } catch { /* storage is a convenience */ } }
const editorLabel = (id = homeEditor()) => EDITORS.find(e => e.id === id)?.label || 'your editor';

const reason = (error: unknown) => String(error instanceof Error ? error.message : error).replace(/^\[\w+\]\s*/, '');
async function openIn(api: RunnerClient, repositoryId: string, app: string, what: string) {
  try { await api.openRepositoryHome({ repositoryId, app }); showMenuNotice(`Opened in ${what}`); }
  catch (error) { showMenuNotice(`Could not open it in ${what}: ${reason(error)}`); }
}
const isMac = typeof navigator !== 'undefined' && /Mac/i.test(navigator.platform || navigator.userAgent);

/** One home's actions: a shell here (in this workspace's Terminal panel), Finder, the editor, its path. */
export function homeItems(api: RunnerClient, workspaceId: string, home: { repositoryId: string; name: string; path: string }, shell?: () => void): MenuItem[] {
  // On another computer's Runner the home is there: Finder and the editor would open on that computer's screen.
  const here = pageFilesAreHere();
  return [
    { label: 'Open in terminal', icon: 'terminal', disabled: !workspaceId ? 'Open a workspace: the shell runs in its Terminal panel' : undefined, run: shell || (() => void openHomeShell(api, workspaceId, { repositoryId: home.repositoryId, label: `${home.name} home` })) },
    here && { label: isMac ? 'Open in Finder' : 'Open in file manager', icon: 'folder', run: () => void openIn(api, home.repositoryId, 'finder', isMac ? 'Finder' : 'the file manager') },
    here && { label: `Open in ${editorLabel()}`, icon: 'pencil', run: () => void openIn(api, home.repositoryId, homeEditor(), editorLabel()) },
    'separator',
    { label: 'Copy path', icon: 'copy', run: () => void copyText(home.path, 'Copied the home checkout’s path') },
  ];
}

/** The state of a home in a few words: "main · 2 changed", "missing". */
export function homeState(home: Pick<RepositoryHome, 'exists' | 'branch' | 'detached' | 'dirty' | 'error' | 'behind' | 'defaultBranch'>): string {
  if (home.error) return home.error;
  const branch = home.detached ? 'detached HEAD' : home.branch;
  const behind = home.behind ? ` · ${home.behind} behind origin/${home.defaultBranch}` : '';
  return (home.dirty ? `${branch} · ${home.dirty} changed` : `${branch} · clean`) + behind;
}

// ---- Setting one -------------------------------------------------------------

/** Creates, adopts or forgets a home; '' or why not. */
async function setHome(api: RunnerClient, repositoryId: string, action: 'create' | 'adopt' | 'clear', path = ''): Promise<{ home?: RepositoryHome; error: string }> {
  try {
    const home = await api.setRepositoryHome({ repositoryId, action, path }, { timeoutMs: 10 * 60_000 });
    homesChanged(api);
    return { home, error: '' };
  } catch (error) { return { error: homeError(error) }; }
}
/** The Runner's refusal as the sentence it is ("That folder is a checkout of acme/web, not acme/api."). */
function homeError(error: unknown): string {
  const failure = ConnectError.from(error);
  if (failure.code === Code.Unavailable || failure.code === Code.DeadlineExceeded) return settingsError(error);
  const text = (failure.rawMessage || 'Could not change the home checkout').trim();
  return `${text[0].toUpperCase()}${text.slice(1)}${/[.!?]$/.test(text) ? '' : '.'}`;
}

/** One repository's home: where it is and what it holds, or how to get one. */
function HomeRow({ home, label = home.name, workspaceId, chooseFolder, onChanged, onOpened }: { home: RepositoryHome; /** What the row calls it: the workspace's folder name, else owner/repo. */ label?: string; workspaceId: string; chooseFolder?(): Promise<string | null>; onChanged(home: RepositoryHome): void; /** A shell was opened: a dialog gets out of its way. */ onOpened?(): void }) {
  const { api } = useRunner();
  const [busy, setBusy] = useState(''), [error, setError] = useState(''), [typing, setTyping] = useState(false), [path, setPath] = useState('');
  const act = async (action: 'create' | 'adopt' | 'clear', target = '') => {
    setBusy(action); setError('');
    const result = await setHome(api, home.repositoryId, action, target);
    setBusy('');
    if (result.error) setError(result.error);
    else if (result.home) { onChanged(result.home); setTyping(false); setPath(''); showMenuNotice(action === 'create' ? `Cloned ${home.name} into ${shortPath(result.home.path)}` : action === 'adopt' ? `${shortPath(result.home.path)} is ${home.name}’s home` : 'Forgot the home checkout; its files are unchanged'); }
  };
  // Update to latest main: fetch, then fast-forward only a clean default branch; otherwise the Runner says why.
  const update = async () => {
    setBusy('update'); setError('');
    try {
      const result = await api.updateRepositoryHome({ repositoryId: home.repositoryId }, { timeoutMs: 5 * 60_000 });
      if (result.home) onChanged(result.home);
      showMenuNotice(result.message);
      if (!result.updated && result.message.includes('Not updated')) setError(result.message);
    } catch (failure) { setError(homeError(failure)); }
    finally { setBusy(''); }
  };
  const adopt = async () => {
    if (!chooseFolder) { setTyping(true); return; }
    const chosen = await chooseFolder().catch(() => null);
    if (chosen) await act('adopt', chosen);
  };
  const short = label.split('/').pop() || label;
  const shell = () => { onOpened?.(); void openHomeShell(api, workspaceId, { repositoryId: home.repositoryId, label: `${short} home` }); };
  const menu = (target: HTMLElement, keyboard: boolean) => {
    const box = target.getBoundingClientRect();
    openContextMenu({ x: box.left, y: box.bottom + 4 }, [
      ...homeItems(api, workspaceId, { repositoryId: home.repositoryId, name: short, path: home.path }, shell),
      'separator',
      { label: `Update to latest ${home.defaultBranch || 'main'}`, icon: 'refresh', disabled: busy ? 'Busy' : undefined, run: () => void update() },
      { label: 'Use another folder…', icon: 'folder', run: () => void adopt() },
      { label: 'Forget this home', icon: 'close', disabled: home.kind === 'source' ? 'It is the folder this repository was added from' : undefined, run: () => void act('clear') },
    ], { keyboard, target, label: `${short} home` });
  };
  return <li className="home-checkout" aria-label={`${label} home checkout`}>
    <div className="home-checkout-text">
      <strong title={home.name}><Icon name="home" size={13}/>{label}</strong>
      {home.path
        ? <span className="home-checkout-where"><code title={home.path}>{shortPath(home.path)}</code>
          <small className={home.error ? 'is-missing' : home.dirty ? 'is-dirty' : ''}><Icon name="branch" size={11}/>{homeState(home)}{home.kind === 'source' && ' · the folder it was added from'}</small></span>
        : <small>No home checkout. Logins and dev servers you run by hand need one.</small>}
      {typing && <form className="home-checkout-path" onSubmit={event => { event.preventDefault(); if (path.trim()) void act('adopt', path.trim()); }}>
        <input autoFocus aria-label={`Existing checkout of ${home.name}`} placeholder="~/src/api" value={path} onChange={event => setPath(event.target.value)}
          onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); setTyping(false); } }}/>
        <button type="submit" disabled={!path.trim() || !!busy}>Use folder</button>
        <button type="button" onClick={() => setTyping(false)}>Cancel</button>
      </form>}
      {error && <p className="error" role="alert">{error}</p>}
    </div>
    <div className="home-checkout-actions">
      {home.path ? <>
        {/* The shell runs in a workspace's Terminal panel; without one (Settings), the folder opens instead. */}
        {workspaceId
          ? <button type="button" title={`A shell in ${shortPath(home.path)}, in this workspace’s Terminal panel`} onClick={shell}><Icon name="terminal" size={13}/>Open in terminal</button>
          : pageFilesAreHere() && <button type="button" title={home.path} onClick={() => void openIn(api, home.repositoryId, 'finder', isMac ? 'Finder' : 'the file manager')}><Icon name="folder" size={13}/>{isMac ? 'Open in Finder' : 'Open folder'}</button>}
        {home.behind > 0 && <button type="button" disabled={!!busy} title={`Fetch, then move ${home.defaultBranch} to origin/${home.defaultBranch} if it is checked out and clean; nothing is discarded`}
          onClick={() => void update()}><Icon name="refresh" size={13}/>{busy === 'update' ? 'Updating…' : `Update to latest ${home.defaultBranch}`}</button>}
        <button type="button" aria-label={`More for ${short} home`} aria-haspopup="menu" title="Finder, editor, path, another folder"
          onClick={event => menu(event.currentTarget, event.detail === 0)}><Icon name="more" size={13}/></button>
      </> : <>
        <button type="button" className="primary" disabled={!!busy || !home.suggestedPath} title={home.suggestedPath ? `A regular clone at ${home.suggestedPath}` : 'No default folder: use an existing one'}
          onClick={() => void act('create')}>{busy === 'create' ? 'Cloning…' : <>Create at <code className="home-checkout-path-tail"><span>{shortPath(home.suggestedPath)}</span></code></>}</button>
        <button type="button" disabled={!!busy} onClick={() => void adopt()}>{busy === 'adopt' ? 'Checking…' : 'Use an existing folder…'}</button>
      </>}
    </div>
  </li>;
}

/** The homes of `repositoryIds` (every repository when undefined), kept fresh while shown. */
function useHomes(repositoryIds?: readonly string[]) {
  const { api } = useRunner();
  const [homes, setHomes] = useState<RepositoryHome[]>(), [root, setRoot] = useState(''), [error, setError] = useState('');
  const key = repositoryIds?.join(',') ?? '*';
  const libraryRevision = useSyncExternalStore(subscribe, () => revision);
  useEffect(() => {
    if (repositoryIds && !repositoryIds.length) { setHomes([]); return; }
    const control = new AbortController();
    const read = () => void api.listRepositoryHomes({ repositoryIds: repositoryIds ? [...repositoryIds] : [] }, { signal: control.signal }).then(value => {
      if (control.signal.aborted) return;
      setHomes(value.homes); setRoot(value.root); setError('');
    }, failure => { if (!control.signal.aborted) setError(settingsError(failure, 'load')); });
    read();
    // Branch and changes move while the person works there.
    const timer = setInterval(read, 20000);
    const focus = () => read();
    window.addEventListener('focus', focus);
    return () => { control.abort(); clearInterval(timer); window.removeEventListener('focus', focus); };
  }, [api, key, libraryRevision]);
  const replace = (home: RepositoryHome) => setHomes(list => list?.map(item => item.repositoryId === home.repositoryId ? home : item));
  return { homes, root, error, replace };
}

/** Settings → Repositories → Home checkouts. */
export function RepositoryHomesSettings({ chooseFolder, workspaceId = '' }: { chooseFolder?(): Promise<string | null>; workspaceId?: string }) {
  const { homes, root, error, replace } = useHomes();
  const [editor, setEditor] = useState(homeEditor);
  return <SettingsGroup title="Home checkouts" className="home-checkouts-settings"
    description={<>Your own clone of each repository, outside every workspace: where you run logins, long-lived dev servers and try a module by hand. Workspaces stay separate worktrees; Lazybox never changes or deletes a home, and agents only read it. New homes go in <code>{root ? shortPath(root) : '~/lazybox/repos'}</code>.</>}>
    {error && <p className="error" role="alert">{error}</p>}
    {!homes ? <p role="status">Reading home checkouts…</p>
      : !homes.length ? <p className="module-empty">No repositories yet. Add one to a workspace, then give it a home here.</p>
      : <ul className="home-checkout-list">{homes.map(home => <HomeRow key={home.repositoryId} home={home} workspaceId={workspaceId} chooseFolder={chooseFolder} onChanged={replace}/>)}</ul>}
    <SettingsRow label="Editor" htmlFor="home-checkout-editor" description="What “Open in editor” opens a home checkout with, on this computer.">
      <select id="home-checkout-editor" value={editor} onChange={event => { setEditor(event.target.value); setHomeEditor(event.target.value); }}>{EDITORS.map(e => <option key={e.id} value={e.id}>{e.label}</option>)}</select>
    </SettingsRow>
  </SettingsGroup>;
}

/** The workspace's repositories dialog: each member's home, one click to a shell in it. */
export function WorkspaceHomes({ workspace, chooseFolder, onOpened }: { workspace: Collection; chooseFolder?(): Promise<string | null>; onOpened?(): void }) {
  const ids = [...new Set(workspace.members.map(member => member.spec?.repositoryId || '').filter(Boolean))];
  const { homes, error, replace } = useHomes(ids);
  if (!ids.length) return null;
  return <section className="workspace-homes" aria-label="Home checkouts">
    <h3>Home checkouts <small>your own clones, outside this workspace</small></h3>
    {error && <p className="error" role="alert">{error}</p>}
    {homes && <ul className="home-checkout-list">{homes.map(home => <HomeRow key={home.repositoryId} home={home} label={workspace.members.find(m => m.spec?.repositoryId === home.repositoryId)?.spec?.name || home.name} workspaceId={workspace.id} chooseFolder={chooseFolder} onChanged={replace} onOpened={onOpened}/>)}</ul>}
  </section>;
}
