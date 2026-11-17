import { peekProps } from './peek-model';
import { focusWhenShown } from './focus';
import { useEffect, useMemo, useRef, type KeyboardEvent, type MouseEvent } from 'react';
import type { Collection } from '@lazybox/protocol';
import { FileIcon } from './DirectoryListing';
import { Icon } from './Icons';
import { preloadEditor } from './editor-loader';
import { activePath } from './editor-model';
import { isDirty, modKey, parentFolder, shortcut, useWorkbench, useWorkbenchState, type Workbench } from './workbench';
import { useFileContextMenu } from './FileArtifactActions';
import './files.css';
import { contextMenu } from './ContextMenu';
import { fileItems } from './workbench-context-menu';

type Row = { kind: 'entry'; path: string; name: string; directory: boolean; depth: number; repository: boolean } | { kind: 'status'; path: string; depth: number; text: string; error?: boolean };

/** The Files panel: the workspace as a tree, its repositories at the top. */
export function FileExplorer({ workspaceId, collections }: { workspaceId: string; collections: Collection[] }) {
 const workbench = useWorkbench();
 if (!workspaceId || !workbench || workbench.workspaceId !== workspaceId) return <p className="module-empty">Choose a workspace to browse its files.</p>;
 return <Explorer workbench={workbench} root={collections.find(value => value.id === workspaceId)?.root || ''} members={collections.find(value => value.id === workspaceId)?.members.map(member => member.spec?.name || '').filter(Boolean).join('\n') || ''}/>;
}

function Explorer({ workbench, members: names, root }: { workbench: Workbench; members: string; root: string }) {
 const state = useWorkbenchState(workbench);
 // Joined by the caller so the catalog's 3-second refresh does not rebuild the tree.
 const members = useMemo(() => names ? names.split('\n') : [], [names]);
 const tree = useRef<HTMLDivElement>(null);
 const expanded = useMemo(() => new Set(state.expanded), [state.expanded]);
 const opened = useRef(false);
 useEffect(() => {
  workbench.explorers++;
  void preloadEditor();
  workbench.refreshFolders();
  const active = activePath(workbench.getState().layout);
  if (active) workbench.reveal(active, { quiet: true });
  return () => { workbench.explorers--; };
 }, [workbench]);
 // The first time a workspace's files are shown, its repositories are open.
 useEffect(() => {
  if (opened.current || state.restoredExpansion || state.folders['.']?.status !== 'ready') return;
  opened.current = true;
  const repositories = state.folders['.'].entries.filter(entry => entry.directory && members.includes(entry.path));
  if (repositories.length <= 4) for (const entry of repositories) workbench.expand(entry.path);
 }, [state.folders, state.restoredExpansion, members, workbench]);
 const rows = useMemo(() => {
  const out: Row[] = [];
  const walk = (folder: string, depth: number) => {
   const value = state.folders[folder];
   if (!value || value.status === 'loading' && !value.entries.length) { out.push({ kind: 'status', path: folder, depth, text: 'Loading…' }); return; }
   if (value.status === 'error') { out.push({ kind: 'status', path: folder, depth, text: value.error || 'This folder could not be read.', error: true }); return; }
   for (const entry of value.entries) {
    out.push({ kind: 'entry', path: entry.path, name: entry.name, directory: entry.directory, depth, repository: depth === 0 && entry.directory && members.includes(entry.path) });
    if (entry.directory && expanded.has(entry.path)) walk(entry.path, depth + 1);
   }
   if (!value.entries.length) out.push({ kind: 'status', path: folder, depth, text: 'Empty folder' });
   if (value.truncated) out.push({ kind: 'status', path: folder, depth, text: 'Showing the first 500 entries' });
  };
  walk('.', 0);
  return out;
 }, [state.folders, expanded, members]);
 const entries = rows.filter((row): row is Extract<Row, { kind: 'entry' }> => row.kind === 'entry');
 const dirty = useMemo(() => new Set(Object.values(state.docs).filter(isDirty).map(doc => doc.path)), [state.docs]);
 const active = activePath(state.layout);
 const cursor = entries.some(row => row.path === state.selected) ? state.selected : entries[0]?.path;
 // A revealed file scrolls into view without taking focus from where you are.
 useEffect(() => {
  if (!state.selected) return;
  const frame = requestAnimationFrame(() => tree.current?.querySelector<HTMLElement>(`[data-path="${CSS.escape(state.selected)}"]`)?.scrollIntoView({ block: 'nearest' }));
  return () => cancelAnimationFrame(frame);
 }, [state.revealed, rows.length > 0]);
 // Asked to take focus (the Files button, ]] e): the current row does, as soon as there is one.
 // A request the panel mounts for (it was just shown) still counts; an old one does not.
 const focused = useRef(0);
 useEffect(() => {
  if (focused.current >= state.treeFocus || Date.now() - state.treeFocus > 3000 || !cursor) return;
  focused.current = state.treeFocus;
  focusWhenShown(tree.current?.querySelector<HTMLElement>(`[data-path="${CSS.escape(cursor)}"]`));
 }, [state.treeFocus, cursor]);
 const descend = useRef('');
 useEffect(() => {
  const folder = descend.current;
  if (!folder || state.folders[folder]?.status !== 'ready') return;
  descend.current = '';
  const child = state.folders[folder].entries[0];
  if (child && cursor === folder) focusRow(child.path);
 }, [state.folders]);
 const focusRow = (path: string) => { workbench.select(path); requestAnimationFrame(() => tree.current?.querySelector<HTMLElement>(`[data-path="${CSS.escape(path)}"]`)?.focus({ preventScroll: false })); };
 const activate = (row: Extract<Row, { kind: 'entry' }>, how: { preview?: boolean; side?: boolean; quick?: boolean; focus?: boolean }) => {
  workbench.select(row.path);
  if (row.directory) { workbench.toggle(row.path); return; }
  if (how.quick) { workbench.host.quickLook(row.path); return; }
  workbench.open(row.path, { preview: how.preview, side: how.side ? 'row' : undefined, focus: how.focus });
 };
 const click = (event: MouseEvent, row: Extract<Row, { kind: 'entry' }>) => {
  if (event.detail > 1) return;
  activate(row, { preview: true, side: modKey(event), quick: event.altKey });
 };
 const keydown = (event: KeyboardEvent<HTMLDivElement>) => {
  if (event.target !== event.currentTarget && !(event.target as HTMLElement).matches('[role="treeitem"]')) return;
  const index = entries.findIndex(row => row.path === cursor), row = entries[index];
  const move = (to: number) => { event.preventDefault(); const next = entries[Math.max(0, Math.min(entries.length - 1, to))]; if (next) focusRow(next.path); };
  const open = row?.directory && expanded.has(row.path);
  switch (event.key) {
   case 'ArrowDown': case 'j': move(index + 1); break;
   case 'ArrowUp': case 'k': move(index - 1); break;
   case 'Home': move(0); break;
   case 'End': move(entries.length - 1); break;
   case 'PageDown': move(index + 10); break;
   case 'PageUp': move(index - 10); break;
   case 'ArrowRight': case 'l':
    if (!row) return; event.preventDefault();
    if (row.directory && !open) workbench.expand(row.path);
    // Into a folder still loading: step in once its entries arrive.
    else if (open && state.folders[row.path]?.status !== 'ready') descend.current = row.path;
    else if (open) move(index + 1);
    else activate(row, { preview: true });
    break;
   case 'ArrowLeft': case 'h': {
    if (!row) return; event.preventDefault();
    if (open) { workbench.collapse(row.path); break; }
    const parent = parentFolder(row.path);
    if (parent !== '.') focusRow(parent);
    break;
   }
   case 'Enter': if (row) { event.preventDefault(); activate(row, { side: modKey(event), quick: event.altKey, focus: !row.directory }); } break;
   case ' ': if (row) { event.preventDefault(); activate(row, { preview: true }); } break;
   case '/': event.preventDefault(); workbench.host.quickOpen(); break;
   // As in other trees: * opens every folder beside this one.
   case '*': if (row) { event.preventDefault(); for (const sibling of state.folders[parentFolder(row.path)]?.entries || []) if (sibling.directory) workbench.expand(sibling.path); } break;
  }
 };
 const fileMenu = useFileContextMenu(workbench);
 return <div className="file-explorer">
  <div className="explorer-toolbar">
   <button className="explorer-find" onClick={() => workbench.host.quickOpen()} title="Find a file by name"><Icon name="search" size={14}/><span>Go to file…</span><kbd>{shortcut('Mod+P')}</kbd></button>
   <button className="icon-button" aria-label="Reveal active file" title="Reveal the active file" disabled={!active} onClick={() => active && workbench.reveal(active)}><Icon name="target" size={15}/></button>
   <button className="icon-button" aria-label="Collapse folders" title="Collapse all folders" disabled={!state.expanded.length} onClick={() => workbench.collapseAll()}><Icon name="collapse" size={15}/></button>
   <button className="icon-button" aria-label="Refresh files" title="Refresh files" onClick={() => workbench.refreshFolders()}><Icon name="refresh" size={15}/></button>
  </div>
  <div ref={tree} className="file-tree" role="tree" aria-label="Workspace files" onKeyDown={keydown}>
   {rows.map(row => row.kind === 'status'
    ? <div key={`status:${row.path}:${row.text}`} className={'tree-status' + (row.error ? ' is-error' : '')} style={{ paddingLeft: 30 + row.depth * 14 }} role={row.error ? 'alert' : undefined}>{row.text}{row.error && <button onClick={() => void workbench.loadFolder(row.path)}>Retry</button>}</div>
    : <div key={row.path} data-path={row.path} {...(row.directory ? {} : peekProps({ kind: 'file', workspaceId: workbench.workspaceId, path: row.path }))} role="treeitem" aria-level={row.depth + 1} aria-expanded={row.directory ? expanded.has(row.path) : undefined} aria-selected={row.path === state.selected} aria-current={!row.directory && row.path === active ? 'true' : undefined} tabIndex={row.path === cursor ? 0 : -1}
      className={'tree-row' + (row.directory ? ' is-folder' : '') + (row.repository ? ' is-repository' : '') + (row.path === active ? ' is-active' : '')} style={{ paddingLeft: 6 + row.depth * 14 }} title={row.path}
      draggable={!row.directory} onDragStart={event => { event.dataTransfer.setData('application/x-lazybox-path', row.path); event.dataTransfer.setData('text/plain', row.path); event.dataTransfer.effectAllowed = 'copy'; }}
      onContextMenu={event => { workbench.select(row.path); contextMenu('file', () => fileItems(workbench, row, root), { path: row.path, directory: row.directory, workspaceId: workbench.workspaceId }, `${row.path} actions`)(event); }}
      onClick={event => click(event, row)} onDoubleClick={() => { if (!row.directory) workbench.open(row.path, { focus: true }); }} onPointerEnter={row.directory ? undefined : preloadEditor}>
      <span className="tree-twisty" aria-hidden="true">{row.directory && <Icon name="chevronRight" size={13}/>}</span>
      {row.repository ? <span className="entry-icon file-icon is-repository" aria-hidden="true"><Icon name="repository" size={15}/></span> : <FileIcon name={row.name} directory={row.directory}/>}
      <span className="tree-name">{row.name}</span>
      {dirty.has(row.path) && <span className="tree-dirty" title="Unsaved changes" aria-label="Unsaved changes"/>}
     </div>)}
  </div>
  {fileMenu.element}
 </div>;
}
