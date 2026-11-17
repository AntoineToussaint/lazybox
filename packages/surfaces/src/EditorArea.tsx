import { Suspense, useEffect, useMemo, useRef, useState, type DragEvent, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { ActionMenu } from './ActionMenu';
import { FileIcon } from './DirectoryListing';
import { Icon } from './Icons';
import { RichContent } from './RichContent';
import { CodeEditor, EditorSkeleton, preloadEditor } from './editor-loader';
import { canSplit, editorGroups, tabHints, type EditorGroup, type EditorNode } from './editor-model';
import { useVimPreferences } from './keyboard';
import { isDirty, modKey, parentFolder, shortcut, useWorkbench, useWorkbenchState, type Workbench, type WorkbenchState } from './workbench';
import { trackFile, trackableFile } from './FileArtifactActions';
import './files.css';
import { contextMenu } from './ContextMenu';
import { editorTabItems } from './workbench-context-menu';

const TAB = 'application/x-lazybox-tab', PATH = 'application/x-lazybox-path';
const name = (path: string) => path.split('/').pop() || path;
const markdown = (path: string) => /\.(md|markdown|mdown|mdx)$/i.test(path);

export const editorShortcuts: [string, string][] = [
 ['Go to file', 'Mod+P'], ['Split right', 'Mod+\\'], ['Split down', 'Mod+Alt+\\'], ['Next / previous tab', 'Mod+Alt+→ / ←'],
 ['Move tab right / left', 'Mod+Alt+Shift+→ / ←'], ['Focus group 1–4', 'Mod+Alt+1…4'], ['Close tab', 'Alt+W'], ['Save', 'Mod+S'],
];

/** The Editor panel: files in tabs, in up to four groups split side by side or stacked. */
export function EditorArea({ workspaceId, viewState }: { workspaceId: string; viewState?: ViewStateStore }) {
 const workbench = useWorkbench();
 if (!workspaceId || !workbench || workbench.workspaceId !== workspaceId) return <p className="module-empty">Choose a workspace to open its files.</p>;
 return <Editors workbench={workbench} viewState={viewState}/>;
}

function Editors({ workbench, viewState }: { workbench: Workbench; viewState?: ViewStateStore }) {
 const state = useWorkbenchState(workbench);
 const area = useRef<HTMLDivElement>(null);
 const vim = useVimPreferences(viewState).editor;
 useEffect(() => { void preloadEditor(); }, []);
 // Editor shortcuts are the editor's only while the focus is in it: elsewhere
 // ⌘\ splits the panel in front, and ⌥⌘←/→ belong to whatever has the focus.
 useEffect(() => {
  const keydown = (event: KeyboardEvent) => {
   if (event.defaultPrevented || event.isComposing || !area.current?.isConnected || document.querySelector('dialog[open]')) return;
   if (!area.current.contains(document.activeElement)) return;
   const mod = modKey(event), state = workbench.getState(), group = state.layout.focused;
   const act = (run: () => void) => { event.preventDefault(); event.stopPropagation(); run(); };
   if (mod && !event.shiftKey && event.code === 'Backslash') act(() => workbench.split(event.altKey ? 'column' : 'row'));
   else if (mod && event.altKey && (event.key === 'ArrowRight' || event.key === 'ArrowLeft')) act(() => event.shiftKey ? workbench.shiftTab(event.key === 'ArrowRight' ? 1 : -1) : workbench.cycleTab(event.key === 'ArrowRight' ? 1 : -1));
   else if (mod && event.altKey && /^Digit[1-4]$/.test(event.code)) act(() => workbench.focusGroupAt(Number(event.code.slice(5)) - 1));
   else if (event.altKey && !mod && !event.shiftKey && event.code === 'KeyW') act(() => { const active = editorGroups(state.layout.root).find(value => value.id === group)?.active; if (active) workbench.close(group, active); });
  };
  window.addEventListener('keydown', keydown, true);
  return () => window.removeEventListener('keydown', keydown, true);
 }, [workbench]);
 const groups = editorGroups(state.layout.root);
 const empty = groups.length === 1 && !groups[0].tabs.length;
 return <div ref={area} className="editor-area" data-testid="editor-area">
  {empty ? <EmptyEditor workbench={workbench}/> : <Node workbench={workbench} state={state} node={state.layout.root} path={[]} vim={vim} numbered={groups.length > 1}/>}
  {state.closing && <ClosePrompt workbench={workbench} closing={state.closing}/>}
 </div>;
}

function EmptyEditor({ workbench }: { workbench: Workbench }) {
 return <div className="editor-empty">
  <Icon name="file" size={28}/>
  <h3>No file open</h3>
  <p>Open a file from the Files panel, a link in the conversation, or by name.</p>
  <button onClick={() => workbench.host.quickOpen()}><Icon name="search" size={14}/>Go to file<kbd>{shortcut('Mod+P')}</kbd></button>
  <dl className="editor-shortcuts">{editorShortcuts.map(([label, keys]) => <div key={label}><dt>{label}</dt><dd><kbd>{shortcut(keys)}</kbd></dd></div>)}</dl>
 </div>;
}

function Node({ workbench, state, node, path, vim, numbered }: { workbench: Workbench; state: WorkbenchState; node: EditorNode; path: number[]; vim: boolean; numbered: boolean }) {
 if (node.type === 'group') return <Group workbench={workbench} state={state} group={node.group} vim={vim} index={editorGroups(state.layout.root).findIndex(group => group.id === node.group.id)} numbered={numbered}/>;
 return <div className={`editor-split split-${node.direction}`}>
  <div className="editor-cell" style={{ flex: `${node.ratio} 1 0` }}><Node workbench={workbench} state={state} node={node.children[0]} path={[...path, 0]} vim={vim} numbered={numbered}/></div>
  <SplitHandle node={node} onResize={ratio => workbench.resize(path, ratio)}/>
  <div className="editor-cell" style={{ flex: `${1 - node.ratio} 1 0` }}><Node workbench={workbench} state={state} node={node.children[1]} path={[...path, 1]} vim={vim} numbered={numbered}/></div>
 </div>;
}

/** Drag or arrow keys resize two groups; the ratio is saved once, on release. */
function SplitHandle({ node, onResize }: { node: Extract<EditorNode, { type: 'split' }>; onResize(ratio: number): void }) {
 const row = node.direction === 'row';
 const down = (event: ReactPointerEvent<HTMLDivElement>) => {
  if (event.button !== 0) return;
  const handle = event.currentTarget, first = handle.previousElementSibling as HTMLElement, second = handle.nextElementSibling as HTMLElement;
  const start = first.getBoundingClientRect(), end = second.getBoundingClientRect();
  const from = row ? start.left : start.top, span = (row ? end.right : end.bottom) - from;
  if (span <= 0) return;
  event.preventDefault(); handle.setPointerCapture(event.pointerId); handle.dataset.dragging = 'true';
  let ratio = node.ratio;
  const move = (next: PointerEvent) => { ratio = Math.min(0.85, Math.max(0.15, ((row ? next.clientX : next.clientY) - from) / span)); first.style.flex = `${ratio} 1 0`; second.style.flex = `${1 - ratio} 1 0`; };
  const stop = () => { handle.removeEventListener('pointermove', move); handle.removeEventListener('pointerup', stop); handle.removeEventListener('pointercancel', stop); delete handle.dataset.dragging; onResize(ratio); };
  handle.addEventListener('pointermove', move); handle.addEventListener('pointerup', stop); handle.addEventListener('pointercancel', stop);
 };
 const key = (event: ReactKeyboardEvent) => { const step = { ArrowLeft: -0.05, ArrowUp: -0.05, ArrowRight: 0.05, ArrowDown: 0.05 }[event.key]; if (step === undefined) return; event.preventDefault(); onResize(node.ratio + step); };
 return <div className="editor-split-handle" role="separator" tabIndex={0} aria-orientation={row ? 'vertical' : 'horizontal'} aria-label="Resize editor groups" aria-valuemin={15} aria-valuemax={85} aria-valuenow={Math.round(node.ratio * 100)} onPointerDown={down} onKeyDown={key}/>;
}

function Group({ workbench, state, group, vim, index, numbered }: { workbench: Workbench; state: WorkbenchState; group: EditorGroup; vim: boolean; index: number; numbered: boolean }) {
 const [dropAt, setDropAt] = useState<number | null>(null);
 const [trackNote, setTrackNote] = useState('');
 const strip = useRef<HTMLDivElement>(null);
 const hints = useMemo(() => tabHints(group.tabs), [group.tabs]);
 const focused = state.layout.focused === group.id;
 const label = numbered ? `Editor group ${index + 1}` : 'Editor';
 // Keep the shown tab in view as tabs are opened and cycled.
 useEffect(() => { strip.current?.querySelector<HTMLElement>('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest', inline: 'nearest' }); }, [group.active, group.tabs.length]);
 const dropIndex = (event: DragEvent) => {
  const tabs = [...(strip.current?.querySelectorAll<HTMLElement>('[role="tab"]') || [])];
  const at = tabs.findIndex(tab => { const box = tab.getBoundingClientRect(); return event.clientX < box.left + box.width / 2; });
  return at < 0 ? tabs.length : at;
 };
 const accepts = (event: DragEvent) => event.dataTransfer.types.includes(TAB) || event.dataTransfer.types.includes(PATH);
 const drop = (event: DragEvent) => {
  if (!accepts(event)) return;
  event.preventDefault(); event.stopPropagation(); setDropAt(null);
  const at = dropIndex(event), tab = event.dataTransfer.getData(TAB), path = event.dataTransfer.getData(PATH);
  if (tab) {
   const moved = JSON.parse(tab) as { group: string; path: string };
   const source = editorGroups(state.layout.root).find(value => value.id === moved.group);
   // Dropping after itself in its own group means the index shifts by one.
   const index = moved.group === group.id && source ? (source.tabs.indexOf(moved.path) < at ? at - 1 : at) : at;
   workbench.moveTab(moved.group, moved.path, group.id, index);
  } else if (path) workbench.open(path, { group: group.id });
 };
 const tabKey = (event: ReactKeyboardEvent, path: string) => {
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  const at = group.tabs.indexOf(path);
  const go = (to: number) => { event.preventDefault(); const next = group.tabs[(to + group.tabs.length) % group.tabs.length]; workbench.activate(group.id, next); requestAnimationFrame(() => strip.current?.querySelector<HTMLElement>(`[data-tab="${CSS.escape(next)}"]`)?.focus()); };
  if (event.key === 'ArrowRight') go(at + 1);
  else if (event.key === 'ArrowLeft') go(at - 1);
  else if (event.key === 'Home') go(0);
  else if (event.key === 'End') go(group.tabs.length - 1);
  else if (event.key === 'Delete' || event.key === 'Backspace') { event.preventDefault(); workbench.close(group.id, path); }
  else if (event.key === 'Enter') { event.preventDefault(); workbench.pin(group.id, path); workbench.focusGroupAt(index); }
 };
 const active = group.active;
 return <section className={'editor-group' + (focused ? ' is-focused' : '')} aria-label={label} data-group-id={group.id} onPointerDownCapture={() => workbench.focusGroup(group.id)}
  onDragOver={event => { if (accepts(event)) { event.preventDefault(); event.dataTransfer.dropEffect = event.dataTransfer.types.includes(TAB) ? 'move' : 'copy'; } }} onDrop={drop}>
  <div className="editor-tabbar">
   <div ref={strip} className="editor-tabs" role="tablist" aria-label={`${label} tabs`}
    onDragOver={event => { if (accepts(event)) { event.preventDefault(); setDropAt(dropIndex(event)); } }} onDragLeave={event => { if (!event.currentTarget.contains(event.relatedTarget as Node)) setDropAt(null); }} onDrop={drop}>
    {group.tabs.map((path, i) => {
     const doc = state.docs[path], dirty = isDirty(doc), selected = path === active;
     return <div key={path} role="tab" data-tab={path} aria-selected={selected} tabIndex={selected ? 0 : -1} title={path}
      className={'editor-tab' + (selected ? ' is-active' : '') + (group.preview === path ? ' is-preview' : '') + (dirty ? ' is-dirty' : '') + (dropAt === i ? ' drop-before' : '') + (dropAt === group.tabs.length && i === group.tabs.length - 1 ? ' drop-after' : '')}
      draggable onDragStart={event => { event.dataTransfer.setData(TAB, JSON.stringify({ group: group.id, path })); event.dataTransfer.setData('text/plain', path); event.dataTransfer.effectAllowed = 'move'; }} onDragEnd={() => setDropAt(null)}
      onClick={() => workbench.activate(group.id, path)} onDoubleClick={() => workbench.pin(group.id, path)} onAuxClick={event => { if (event.button === 1) { event.preventDefault(); workbench.close(group.id, path); } }} onKeyDown={event => tabKey(event, path)}
      onContextMenu={contextMenu('editor-tab', () => editorTabItems(workbench, group.id, path), { path, group: group.id, workspaceId: workbench.workspaceId }, `${name(path)} tab actions`)}>
      <FileIcon name={name(path)} size={14}/>
      <span className="editor-tab-name">{name(path)}</span>
      {hints[path] && <span className="editor-tab-hint">{hints[path]}</span>}
      <button className="editor-tab-close" tabIndex={-1} aria-label={`Close ${name(path)}`} title={dirty ? 'Unsaved changes · close' : `Close (${shortcut('Alt+W')})`} onClick={event => { event.stopPropagation(); workbench.close(group.id, path); }}>
       {dirty && <span className="dirty-dot" aria-hidden="true"/>}<Icon name="close" size={12}/>
      </button>
     </div>;
    })}
   </div>
   <div className="editor-group-actions">
    <button className="icon-button" aria-label="Split editor right" title={`Split right (${shortcut('Mod+\\')})`} disabled={!canSplit(state.layout)} onClick={() => workbench.split('row', group.id)}><Icon name="split-right" size={15}/></button>
    <button className="icon-button" aria-label="Split editor down" title={`Split down (${shortcut('Mod+Alt+\\')})`} disabled={!canSplit(state.layout)} onClick={() => workbench.split('column', group.id)}><Icon name="split-down" size={15}/></button>
    <ActionMenu className="editor-group-menu" label={`${label} actions`}>
     <button disabled={!active} onClick={() => active && workbench.shiftTab(-1, group.id)}><span>Move tab left</span><small>{shortcut('Mod+Alt+Shift+←')}</small></button>
     <button disabled={!active} onClick={() => active && workbench.shiftTab(1, group.id)}><span>Move tab right</span><small>{shortcut('Mod+Alt+Shift+→')}</small></button>
     <button disabled={!active} onClick={() => active && workbench.reveal(active)}><span>Reveal in Files</span></button>
     {active && trackableFile(active) && <button onClick={() => void trackFile(workbench.api, workbench.workspaceId, active).then(setTrackNote)}><span>Track as artifact</span></button>}
     <button disabled={!active} onClick={() => active && workbench.host.quickLook(active)}><span>Quick look</span></button>
     <button disabled={group.tabs.length < 2} onClick={() => workbench.closeTabs(group.id, 'others')}><span>Close other tabs</span></button>
     <button disabled={!group.tabs.length} onClick={() => workbench.closeTabs(group.id, 'saved')}><span>Close saved tabs</span></button>
     <button disabled={!group.tabs.length} onClick={() => workbench.closeTabs(group.id, 'all')}><span>Close all tabs</span></button>
     {editorGroups(state.layout.root).length > 1 && <button className="danger" onClick={() => workbench.closeGroup(group.id)}><span>Close group</span></button>}
    </ActionMenu>
   </div>
  </div>
  {trackNote && <p className="tree-status file-artifact-notice" role="status">{trackNote}<button type="button" className="link-button" onClick={() => setTrackNote('')}>Dismiss</button></p>}
  {active ? <Document key={active} workbench={workbench} state={state} group={group.id} path={active} vim={vim}/> : <div className="editor-empty is-group"><p>Drop a tab here, or open a file.</p><button onClick={() => { workbench.focusGroup(group.id); workbench.host.quickOpen(); }}>Go to file<kbd>{shortcut('Mod+P')}</kbd></button></div>}
 </section>;
}

function Document({ workbench, state, group, path, vim }: { workbench: Workbench; state: WorkbenchState; group: string; path: string; vim: boolean }) {
 const doc = state.docs[path];
 useEffect(() => { workbench.ensureDoc(path); }, [workbench, path]);
 if (!doc || (doc.status === 'loading' && !doc.file)) return <div className="editor-document"><EditorSkeleton label="Opening file"/></div>;
 if (doc.status === 'error' && !doc.file) return <div className="editor-document"><div className="editor-message" role="alert"><p>{doc.error || 'This file could not be opened.'}</p><button onClick={() => workbench.reload(path)}>Try again</button><button onClick={() => workbench.close(group, path, true)}>Close tab</button></div></div>;
 const file = doc.file!, dirty = isDirty(doc), editable = !!file.version && !file.truncated && !file.binary;
 const rendered = markdown(path) && doc.rendered;
 const folders = path.split('/').slice(0, -1);
 return <div className="editor-document">
  <div className="editor-breadcrumbs">
   <nav aria-label="File path" className="editor-crumbs">
    {folders.map((part, i) => <span key={i}><button title="Reveal in Files" onClick={() => workbench.reveal(folders.slice(0, i + 1).join('/'))}>{part}</button><span aria-hidden="true" className="crumb-separator">›</span></span>)}
    <button className="is-file" title="Reveal in Files" onClick={() => workbench.reveal(path)}>{name(path)}</button>
   </nav>
   <span className="editor-state" role="status">{doc.saving ? 'Saving…' : dirty ? 'Unsaved' : ''}</span>
   {markdown(path) && <button className="editor-chip" aria-pressed={rendered} onClick={() => workbench.toggleRendered(path)}>{rendered ? 'Source' : 'Preview'}</button>}
   {editable && <button className={'editor-chip' + (dirty ? ' primary' : '')} disabled={!dirty || doc.saving} title={`Save (${shortcut('Mod+S')})`} onClick={() => void workbench.save(path)}>Save</button>}
  </div>
  {(doc.conflict || doc.diskChanged) && <div className="editor-banner" role="alert"><span>{doc.conflict ? 'Not saved: this file changed on disk after you opened it.' : 'This file changed on disk.'} Your edits are still here.</span><button onClick={() => workbench.reload(path)}>Use disk version</button><button onClick={() => void workbench.overwrite(path)}>Keep mine</button></div>}
  {doc.error && !doc.conflict && <div className="editor-banner is-error" role="alert"><span>{doc.error}</span></div>}
  {file.truncated && <p className="editor-note" role="status">Showing the first 1 MiB. Editing is unavailable.</p>}
  {file.binary ? <div className="editor-message"><p>Binary file · no text preview.</p></div>
   : rendered ? <div className="editor-rendered"><RichContent text={doc.draft}/></div>
   : <div className="editor-code"><Suspense fallback={<EditorSkeleton/>}>
     <CodeEditor path={path} modelKey={`${workbench.workspaceId}:${path}`} viewKey={group} value={doc.draft} readOnly={!editable} vim={vim} reveal={doc.reveal?.group === group ? doc.reveal : undefined}
      onChange={text => workbench.setDraft(path, text)} onSave={() => workbench.save(path)} onQuit={force => workbench.close(group, path, force)} onFocus={() => workbench.focusGroup(group)}/>
    </Suspense></div>}
 </div>;
}

function ClosePrompt({ workbench, closing }: { workbench: Workbench; closing: { group: string; path: string } }) {
 const first = useRef<HTMLButtonElement>(null);
 useEffect(() => { first.current?.focus(); }, [closing.path]);
 return <div className="editor-close-prompt" role="alertdialog" aria-label="Unsaved changes" aria-describedby="editor-close-text" onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); workbench.cancelClose(); } }}>
  <p id="editor-close-text"><strong>{name(closing.path)}</strong> has unsaved changes.</p>
  <small>{parentFolder(closing.path) === '.' ? '' : parentFolder(closing.path)}</small>
  <div><button ref={first} className="primary" onClick={() => void workbench.saveAndClose(closing.group, closing.path)}>Save and close</button><button onClick={() => workbench.close(closing.group, closing.path, true)}>Discard changes</button><button onClick={() => workbench.cancelClose()}>Cancel</button></div>
 </div>;
}
