import { createContext, useContext, useSyncExternalStore } from 'react';
import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { WorkspaceDirectoryEntry, WorkspaceFile } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { activateTab, adjacentTab, canSplit, closeGroup as removeGroup, closePath, closeTab, editorGroups, emptyEditorLayout, findGroup, focusGroup, focusedGroup, moveTabToGroup, openPaths, openTab, parseEditorLayout, pinPath, pinTab, resizeSplit, shiftTab, splitGroup, visiblePaths, type EditorLayout } from './editor-model';

/** Markdown opens rendered; "Source" shows the text to edit. */
const renderedByDefault = (path: string) => /\.(md|markdown|mdown|mdx)$/i.test(path);

/** One open file, shared by every tab and group that shows it. */
export type DocState = {
 path: string;
 status: 'loading' | 'ready' | 'error';
 file?: WorkspaceFile;
 draft: string;
 error: string;
 saving: boolean;
 /** The last save was refused because the file changed on disk. */
 conflict: boolean;
 /** A newer version is on disk while this one has unsaved edits. */
 diskChanged: boolean;
 /** Put the cursor on a line (or just focus) in one group's editor. */
 reveal?: { group: string; line?: number; nonce: number };
 /** Markdown files show their source unless the preview is asked for. */
 rendered: boolean;
};
export type FolderState = { status: 'loading' | 'ready' | 'error'; entries: WorkspaceDirectoryEntry[]; truncated: boolean; error: string };
export type WorkbenchState = {
 layout: EditorLayout;
 docs: Readonly<Record<string, DocState>>;
 folders: Readonly<Record<string, FolderState>>;
 expanded: readonly string[];
 /** Whether the folders a person opened were restored; if not, the explorer opens the repositories. */
 restoredExpansion: boolean;
 selected: string;
 /** Bumped when the explorer should scroll its selection into view. */
 revealed: number;
 /** When the explorer was last asked to take focus, once its rows are there. */
 treeFocus: number;
 /** An unsaved file someone asked to close: the editor asks what to do with the edits. */
 closing?: { group: string; path: string };
};
export const isDirty = (doc?: DocState) => !!doc?.file && !doc.file.directory && doc.draft !== doc.file.text;
export const parentFolder = (path: string) => path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '.';
const ancestors = (path: string) => { const parts = path.split('/'), out = ['.']; for (let i = 1; i < parts.length; i++) out.push(parts.slice(0, i).join('/')); return out; };
const message = (error: unknown) => ConnectError.from(error).rawMessage || String(error);

export type WorkbenchHost = { showEditor(): void; showFiles(): void; quickOpen(): void; quickLook(path: string, line?: number): void };
export type OpenOptions = { line?: number; preview?: boolean; group?: string; /** Open in a new group beside ('row') or below ('column') the focused one. */ side?: 'row' | 'column'; focus?: boolean };

const instances = new Map<string, Workbench>();
let unloadGuard = false;
/** Unsaved edits in any workspace keep the page from closing without a warning. */
function guardUnload() {
 if (unloadGuard || typeof window === 'undefined') return;
 unloadGuard = true;
 window.addEventListener('beforeunload', event => { if ([...instances.values()].some(workbench => workbench.hasUnsaved())) event.preventDefault(); });
}

/** The files a workspace has open and the explorer's view of its folders. One
 * per workspace, kept while the window lives, so unsaved edits survive
 * switching workspaces, docking and closing the editor panel. */
export class Workbench {
 static for(api: RunnerClient, workspaceId: string, store?: ViewStateStore): Workbench {
  const scope = `${store?.scope || store?.key || 'local'}:workspace:${workspaceId}`;
  let workbench = instances.get(scope);
  if (!workbench) { workbench = new Workbench(api, workspaceId, store, scope); instances.set(scope, workbench); guardUnload(); }
  workbench.api = api;
  return workbench;
 }
 private state: WorkbenchState;
 private listeners = new Set<() => void>();
 private loads = new Map<string, AbortController>();
 private folderLoads = new Map<string, AbortController>();
 private nonce = 0;
 host: WorkbenchHost = { showEditor() {}, showFiles() {}, quickOpen() {}, quickLook() {} };
 /** Mounted explorers: while one is shown, the active file is revealed in it. */
 explorers = 0;
 private constructor(public api: RunnerClient, readonly workspaceId: string, private store: ViewStateStore | undefined, private scope: string) {
  let layout = emptyEditorLayout(), expanded: string[] = [], restoredExpansion = false;
  try { const saved = store?.getItem(`${scope}:editor`); if (saved) layout = parseEditorLayout(saved); } catch { /* a damaged layout starts empty */ }
  try { const saved = JSON.parse(store?.getItem(`${scope}:files-expanded`) || 'null'); if (Array.isArray(saved)) { expanded = saved.filter(value => typeof value === 'string').slice(0, 500); restoredExpansion = true; } } catch { /* start collapsed */ }
  this.state = { layout, docs: {}, folders: {}, expanded, restoredExpansion, selected: '', revealed: 0, treeFocus: 0 };
 }
 subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
 getState = () => this.state;
 hasUnsaved() { return Object.values(this.state.docs).some(isDirty); }
 private emit() { for (const listener of this.listeners) listener(); }
 private set(change: Partial<WorkbenchState>) {
  const previous = this.state;
  this.state = { ...previous, ...change };
  try {
   if (change.layout && change.layout !== previous.layout) this.store?.setItem(`${this.scope}:editor`, JSON.stringify(change.layout));
   if (change.expanded && change.expanded !== previous.expanded) this.store?.setItem(`${this.scope}:files-expanded`, JSON.stringify(change.expanded.slice(0, 500)));
  } catch { /* the arrangement is a convenience */ }
  this.emit();
 }
 private setDoc(path: string, change: Partial<DocState> | ((doc: DocState) => Partial<DocState>)) {
  const current = this.state.docs[path] || { path, status: 'loading', draft: '', error: '', saving: false, conflict: false, diskChanged: false, rendered: renderedByDefault(path) } satisfies DocState;
  this.set({ docs: { ...this.state.docs, [path]: { ...current, ...(typeof change === 'function' ? change(current) : change) } } });
 }
 private dropDoc(path: string) { this.loads.get(path)?.abort(); const { [path]: _, ...docs } = this.state.docs; this.set({ docs }); }
 private setLayout(layout: EditorLayout) {
  if (layout === this.state.layout) return;
  const open = openPaths(layout);
  const orphaned = Object.keys(this.state.docs).filter(path => !open.has(path));
  this.set({ layout });
  for (const path of orphaned) this.dropDoc(path);
 }

 /** Polls what is on screen, so files an agent changes do not go stale. Returns the detach. */
 attach() {
  const tick = () => { if (document.visibilityState === 'visible') this.poll(); };
  const timer = setInterval(tick, 4000);
  const folders = setInterval(() => { if (document.visibilityState === 'visible' && this.explorers) this.refreshFolders(true); }, 12000);
  const focus = () => { tick(); if (this.explorers) this.refreshFolders(true); };
  window.addEventListener('focus', focus);
  for (const path of openPaths(this.state.layout)) if (visiblePaths(this.state.layout).has(path)) this.ensureDoc(path);
  return () => { clearInterval(timer); clearInterval(folders); window.removeEventListener('focus', focus); };
 }
 private poll() {
  for (const path of visiblePaths(this.state.layout)) {
   const doc = this.state.docs[path];
   if (doc?.status === 'ready' && doc.file?.version && !doc.saving && !this.loads.has(path)) void this.load(path, 'poll');
  }
 }

 // Files -------------------------------------------------------------------
 ensureDoc(path: string) { const doc = this.state.docs[path]; if (!doc || (doc.status === 'error' && !this.loads.has(path))) void this.load(path, 'open'); }
 /** `open` shows a fresh file; `reload` replaces unsaved edits with the disk;
  * `poll` takes a newer disk version unless there are unsaved edits, which it flags instead. */
 private async load(path: string, mode: 'open' | 'reload' | 'poll') {
  this.loads.get(path)?.abort();
  const control = new AbortController(); this.loads.set(path, control);
  if (mode !== 'poll') this.setDoc(path, doc => ({ status: doc.file ? doc.status : 'loading', error: '' }));
  try {
   const file = await this.api.readWorkspaceFile({ collectionId: this.workspaceId, path }, { signal: control.signal, timeoutMs: 10000 });
   if (control.signal.aborted) return;
   if (file.directory) { this.dropDoc(path); this.setLayout(closePath(this.state.layout, path)); this.reveal(file.path === '.' ? '.' : file.path || path); return; }
   const doc = this.state.docs[path];
   if (!doc) return;
   if (mode === 'poll') {
    if (doc.file?.version === file.version) return;
    if (isDirty(doc)) { if (!doc.diskChanged) this.setDoc(path, { diskChanged: true }); return; }
   }
   this.setDoc(path, { status: 'ready', file, draft: file.text, error: '', conflict: false, diskChanged: false });
  } catch (error) {
   if (control.signal.aborted) return;
   // A poll that fails changes nothing: the file on screen stays as it was.
   if (mode !== 'poll') this.setDoc(path, { status: 'error', error: message(error) });
  } finally { if (this.loads.get(path) === control) this.loads.delete(path); }
 }
 reload(path: string) { void this.load(path, 'reload'); }
 setDraft(path: string, text: string) {
  const doc = this.state.docs[path];
  if (!doc || doc.draft === text) return;
  this.setDoc(path, { draft: text });
  // Editing a preview tab keeps it, as in any editor.
  if (isDirty(this.state.docs[path])) this.setLayout(pinPath(this.state.layout, path));
 }
 toggleRendered(path: string) { this.setDoc(path, doc => ({ rendered: !doc.rendered })); }
 /** Resolves to false when the file was not saved. */
 async save(path: string): Promise<boolean> {
  const doc = this.state.docs[path];
  if (!doc?.file || doc.saving || !doc.file.version || doc.file.truncated || doc.file.binary) return false;
  if (!isDirty(doc)) return true;
  this.setDoc(path, { saving: true, error: '' });
  try {
   const file = await this.api.saveWorkspaceFile({ collectionId: this.workspaceId, path: doc.file.path, text: doc.draft, expectedVersion: doc.file.version }, { timeoutMs: 15000 });
   // Typing during the save stays: the draft is only replaced when it matches.
   this.setDoc(path, current => ({ saving: false, file, conflict: false, diskChanged: false, draft: current.draft }));
   return true;
  } catch (error) {
   const refused = ConnectError.from(error).code === Code.Aborted;
   this.setDoc(path, { saving: false, conflict: refused, error: refused ? 'This file changed on disk after you opened it.' : message(error) });
   return false;
  }
 }
 /** Keep my edits over the newer disk version: take its version, then save. */
 async overwrite(path: string) {
  const doc = this.state.docs[path];
  if (!doc) return false;
  try {
   const latest = await this.api.readWorkspaceFile({ collectionId: this.workspaceId, path }, { timeoutMs: 10000 });
   this.setDoc(path, { file: latest, conflict: false, diskChanged: false, error: '' });
   return await this.save(path);
  } catch (error) { this.setDoc(path, { error: message(error) }); return false; }
 }

 // Editor groups and tabs ---------------------------------------------------
 open(path: string, options: OpenOptions = {}) {
  if (!path) return;
  let layout = this.state.layout, group = options.group;
  if (options.side) {
   const from = focusedGroup(layout);
   if (canSplit(layout)) { const id = this.newGroupId(); layout = splitGroup(layout, from.id, options.side, id, path); group = id; }
   else { const all = editorGroups(layout.root); group = all[(all.indexOf(from) + 1) % all.length].id; }
  }
  layout = openTab(layout, path, { group, preview: options.preview, keep: value => isDirty(this.state.docs[value]) });
  this.setLayout(layout);
  this.ensureDoc(path);
  // A link to a line opens the source at that line; otherwise Markdown opens rendered.
  if (options.line || options.focus) this.setDoc(path, { reveal: { group: layout.focused, line: options.line, nonce: ++this.nonce }, ...(options.line ? { rendered: false } : {}) });
  this.host.showEditor();
  if (this.explorers) this.reveal(path, { quiet: true });
 }
 activate(group: string, path: string) { this.setLayout(activateTab(this.state.layout, group, path)); this.ensureDoc(path); if (this.explorers) this.reveal(path, { quiet: true }); }
 focusGroup(group: string) { const layout = focusGroup(this.state.layout, group); if (layout !== this.state.layout) this.set({ layout }); }
 focusGroupAt(index: number) {
  const group = editorGroups(this.state.layout.root)[index];
  if (!group) return;
  this.focusGroup(group.id);
  if (group.active) this.setDoc(group.active, { reveal: { group: group.id, nonce: ++this.nonce } });
 }
 pin(group: string, path: string) { this.setLayout(pinTab(this.state.layout, group, path)); }
 /** Close a tab. An unsaved file shown nowhere else asks first; returns false then. */
 close(group: string, path: string, force = false) {
  const elsewhere = editorGroups(this.state.layout.root).some(value => value.id !== group && value.tabs.includes(path));
  if (!force && !elsewhere && isDirty(this.state.docs[path])) { this.set({ closing: { group, path } }); return false; }
  this.set({ closing: undefined });
  this.setLayout(closeTab(this.state.layout, group, path));
  return true;
 }
 cancelClose() { this.set({ closing: undefined }); }
 async saveAndClose(group: string, path: string) { if (await this.save(path)) this.close(group, path, true); else this.set({ closing: undefined }); }
 /** Close tabs without unsaved edits; unsaved ones stay open, marked. */
 closeTabs(group: string, which: 'all' | 'saved' | 'others') {
  const value = findGroup(this.state.layout, group);
  if (!value) return;
  let layout = this.state.layout;
  for (const path of value.tabs) {
   if (which === 'others' && path === value.active) continue;
   const elsewhere = editorGroups(layout.root).some(other => other.id !== group && other.tabs.includes(path));
   if (isDirty(this.state.docs[path]) && !elsewhere) continue;
   layout = closeTab(layout, group, path);
  }
  this.setLayout(layout);
 }
 closeGroup(group: string) {
  const value = findGroup(this.state.layout, group);
  if (!value) return;
  this.closeTabs(group, 'all');
  const left = findGroup(this.state.layout, group);
  if (left?.tabs.length) { this.set({ closing: { group, path: left.tabs.find(path => isDirty(this.state.docs[path])) || left.tabs[0] } }); return; }
  this.setLayout(removeGroup(this.state.layout, group));
 }
 split(direction: 'row' | 'column', group = this.state.layout.focused) {
  const layout = splitGroup(this.state.layout, group, direction, this.newGroupId());
  if (layout === this.state.layout) return;
  this.setLayout(layout);
  const created = focusedGroup(layout);
  if (created.active) this.setDoc(created.active, { reveal: { group: created.id, nonce: ++this.nonce } });
 }
 cycleTab(step: number, group = this.state.layout.focused) { const path = adjacentTab(this.state.layout, group, step); if (path) { this.activate(group, path); this.setDoc(path, { reveal: { group, nonce: ++this.nonce } }); } }
 shiftTab(step: -1 | 1, group = this.state.layout.focused) { const value = findGroup(this.state.layout, group); if (value?.active) this.setLayout(shiftTab(this.state.layout, group, value.active, step)); }
 moveTab(from: string, path: string, to: string, index?: number) { this.setLayout(moveTabToGroup(this.state.layout, from, path, to, index)); }
 resize(path: number[], ratio: number) { this.setLayout(resizeSplit(this.state.layout, path, ratio)); }
 private newGroupId() { const ids = new Set(editorGroups(this.state.layout.root).map(group => group.id)); let n = 2; while (ids.has(`group-${n}`)) n++; return `group-${n}`; }

 // Explorer ---------------------------------------------------------------
 async loadFolder(path: string, silent = false) {
  if (this.folderLoads.has(path)) return;
  const control = new AbortController(); this.folderLoads.set(path, control);
  const current = this.state.folders[path];
  if (!silent || !current) this.set({ folders: { ...this.state.folders, [path]: { entries: current?.entries || [], truncated: current?.truncated || false, status: current?.status === 'ready' ? 'ready' : 'loading', error: '' } } });
  try {
   const folder = await this.api.readWorkspaceFile({ collectionId: this.workspaceId, path }, { signal: control.signal, timeoutMs: 10000 });
   if (control.signal.aborted) return;
   const next: FolderState = folder.directory ? { status: 'ready', entries: folder.entries, truncated: folder.truncated, error: '' } : { status: 'error', entries: [], truncated: false, error: 'Not a folder' };
   const before = this.state.folders[path];
   // A refresh that finds nothing new does not re-render the tree.
   if (before?.status === next.status && before.truncated === next.truncated && JSON.stringify(before.entries.map(entry => [entry.path, entry.directory])) === JSON.stringify(next.entries.map(entry => [entry.path, entry.directory]))) return;
   this.set({ folders: { ...this.state.folders, [path]: next } });
  } catch (error) {
   if (control.signal.aborted) return;
   if (silent && current?.status === 'ready') {
    // A folder that disappeared closes; any other failure keeps what is shown.
    if (ConnectError.from(error).code === Code.NotFound) this.set({ expanded: this.state.expanded.filter(value => value !== path && !value.startsWith(path + '/')) });
    return;
   }
   this.set({ folders: { ...this.state.folders, [path]: { status: 'error', entries: [], truncated: false, error: message(error) } } });
  } finally { if (this.folderLoads.get(path) === control) this.folderLoads.delete(path); }
 }
 refreshFolders(silent = false) { for (const path of ['.', ...this.state.expanded]) void this.loadFolder(path, silent); }
 expand(path: string) {
  if (!this.state.expanded.includes(path)) this.set({ expanded: [...this.state.expanded, path] });
  if (this.state.folders[path]?.status !== 'ready') void this.loadFolder(path);
 }
 collapse(path: string) { if (this.state.expanded.includes(path)) this.set({ expanded: this.state.expanded.filter(value => value !== path) }); }
 toggle(path: string) { if (this.state.expanded.includes(path)) this.collapse(path); else this.expand(path); }
 collapseAll() { this.set({ expanded: [] }); }
 focusTree() { this.set({ treeFocus: Date.now() }); }
 select(path: string) { if (this.state.selected !== path) this.set({ selected: path }); }
 /** Show a file or folder in the explorer: its folders open, it selected and
  * scrolled into view. Unless quiet, the Files panel is shown for it. */
 reveal(path: string, { quiet = false }: { quiet?: boolean } = {}) {
  const folders = path === '.' ? ['.'] : ancestors(path);
  const expanded = [...this.state.expanded, ...folders.filter(folder => folder !== '.' && !this.state.expanded.includes(folder))];
  this.set({ expanded, selected: path, revealed: this.state.revealed + 1 });
  for (const folder of folders) if (this.state.folders[folder]?.status !== 'ready') void this.loadFolder(folder);
  if (!quiet) this.host.showFiles();
 }
}

export const WorkbenchContext = createContext<Workbench | undefined>(undefined);
/** The workspace's workbench, when this surface is part of a workspace window. */
export const useWorkbench = () => useContext(WorkbenchContext);
export function useWorkbenchState(workbench: Workbench): WorkbenchState { return useSyncExternalStore(workbench.subscribe, workbench.getState); }

/** How shortcuts read on this machine: ⌘ on a Mac, Ctrl elsewhere. */
export const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
export const shortcut = (keys: string) => isMac ? keys.replaceAll('Mod+', '⌘').replaceAll('Alt+', '⌥').replaceAll('Shift+', '⇧').replaceAll('Ctrl+', '⌃') : keys.replaceAll('Mod+', 'Ctrl+');
/** Whether an event carries the platform's command modifier. */
export const modKey = (event: { metaKey: boolean; ctrlKey: boolean }) => isMac ? event.metaKey : event.ctrlKey;
