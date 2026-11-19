import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { useBackdropDismiss } from './dismiss';
import { Code, ConnectError } from '@lazybox/client-core';
import type { WorkspacePathMatch } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { FileIcon } from './DirectoryListing';
import { FilePreview } from './FilePreview';
import { useRunner } from './RunnerContext';
import type { FileLink } from './file-reference';
import { modKey, useWorkbench } from './workbench';
import { contextMenuPlatform, copyText } from './ContextMenu';
import { absolutePath } from './context-menu-model';
import './files.css';

export type OpenModifiers = { altKey?: boolean; metaKey?: boolean; ctrlKey?: boolean;
 /** From a context menu: show it in the Files panel instead of opening it. */ reveal?: boolean;
 /** From a context menu: copy its path, absolute or relative to the workspace, instead of opening it. */ copy?: 'absolute' | 'relative' };
type Choice = { reference: FileLink; matches: { path: string; directory: boolean }[]; truncated: boolean; how: OpenModifiers };
export type PathChoice = Choice;

/** Opens file references the way a person means them. The Runner resolves
 * what an agent wrote (a bare name, a path without the repository folder, a
 * path from another checkout); several matches become a choice, and only a
 * reference that names nothing in the workspace is reported.
 *
 * In a workspace window a file opens in the Editor panel and a folder in the
 * Files panel; Alt (Option) opens the quick-look window instead, and Cmd/Ctrl
 * opens beside the current file. Without an editor (a detached chat window)
 * everything opens in the quick-look window. */
export function useFileOpener(collectionId: string | undefined, viewState?: ViewStateStore) {
 const { api } = useRunner();
 const workbench = useWorkbench();
 const [quick, setQuick] = useState<FileLink>();
 const [choice, setChoice] = useState<Choice>();
 const [notice, setNotice] = useState('');
 const integrated = !!workbench && !!collectionId && workbench.workspaceId === collectionId;
 useEffect(() => { if (!notice) return; const timer = setTimeout(() => setNotice(''), 8000); return () => clearTimeout(timer); }, [notice]);
 const show = (match: { path: string; directory: boolean }, reference: FileLink, how: OpenModifiers) => {
  setChoice(undefined);
  if (how.copy === 'relative') { void copyText(match.path, 'Copied relative path'); return; }
  if (how.copy === 'absolute') {
   void api.getCollection({ collectionId: collectionId! }, { timeoutMs: 8000 }).then(result => copyText(absolutePath(result.collection?.root || '', match.path), 'Copied path'), () => copyText(match.path, 'Copied relative path'));
   return;
  }
  if (how.reveal && integrated) { workbench.reveal(match.path); return; }
  if (!integrated || how.altKey) { if (integrated) workbench.host.quickLook(match.path, reference.line); else setQuick({ path: match.path, line: reference.line }); return; }
  const beside = modKey({ metaKey: !!how.metaKey, ctrlKey: !!how.ctrlKey });
  if (match.directory) workbench.reveal(match.path);
  else workbench.open(match.path, { line: reference.line, preview: !beside, side: beside ? 'row' : undefined });
 };
 /** `exact` skips resolution, for paths the Runner gave (a changed file, which may be deleted). */
 const open = async (reference: FileLink, how: OpenModifiers = {}, { exact = false } = {}) => {
  if (!collectionId) return;
  setNotice('');
  if (exact) { show({ path: reference.path, directory: false }, reference, how); return; }
  try {
   const resolved = await api.resolveWorkspacePath({ collectionId, path: reference.path }, { timeoutMs: 15000 });
   if (!resolved.matches.length) {
    const openLocalMedia = contextMenuPlatform().openLocalMedia;
    if (openLocalMedia && !how.copy && !how.reveal && /^(?:~\/|\/).*\.(?:png|jpe?g|gif|webp|heic|tiff?|bmp|mp4|mov|webm|m4v|mp3|m4a|wav|flac|ogg)$/i.test(reference.path)) {
     if (await openLocalMedia(reference.path)) return;
     setNotice(`Could not find “${reference.path}” in this workspace or on this computer.`);
    } else setNotice(`Nothing named “${reference.path}” in this workspace.`);
    return;
   }
   if (resolved.matches.length === 1) show(resolved.matches[0], reference, how);
   else setChoice({ reference, matches: resolved.matches.map((match: WorkspacePathMatch) => ({ path: match.path, directory: match.directory })), truncated: resolved.truncated, how });
  } catch (error) {
   const failure = ConnectError.from(error);
   // A Runner from before resolution existed: open the path as written.
   if (failure.code === Code.Unimplemented) show({ path: reference.path, directory: false }, reference, how);
   else setNotice(`Could not open “${reference.path}”: ${failure.rawMessage}`);
  }
 };
 const element = <>
  {choice && <PathChooser choice={choice} onChoose={match => show(match, choice.reference, choice.how)} onClose={() => setChoice(undefined)}/>}
  {quick && collectionId && <FilePreview workspaceId={collectionId} reference={quick} viewState={viewState} onClose={() => setQuick(undefined)}/>}
  {notice && <div className="file-link-notice" role="alert"><span>{notice}</span><button className="icon-button" aria-label="Dismiss" onClick={() => setNotice('')}>×</button></div>}
 </>;
 return { open, element };
}

/** Several files match a reference: show them, nearest first, and let the person pick (to open one, or to track it as an artifact). */
export function PathChooser({ choice, onChoose, onClose }: { choice: Choice; onChoose(match: Choice['matches'][number]): void; onClose(): void }) {
 const dialog = useRef<HTMLDialogElement>(null), list = useRef<HTMLDivElement>(null);
 useBackdropDismiss(dialog);
 useEffect(() => { dialog.current?.showModal(); list.current?.querySelector<HTMLButtonElement>('button')?.focus(); }, []);
 const keydown = (event: KeyboardEvent) => {
  const buttons = [...(list.current?.querySelectorAll<HTMLButtonElement>('button') || [])], index = buttons.indexOf(document.activeElement as HTMLButtonElement);
  const move = (to: number) => { event.preventDefault(); buttons[(to + buttons.length) % buttons.length]?.focus(); };
  if (event.key === 'ArrowDown' || event.key === 'j') move(index + 1);
  else if (event.key === 'ArrowUp' || event.key === 'k') move(index - 1);
  else if (event.key === 'Home') move(0);
  else if (event.key === 'End') move(buttons.length - 1);
 };
 const name = choice.reference.path.split('/').filter(Boolean).pop() || choice.reference.path;
 return <dialog ref={dialog} className="path-chooser" aria-labelledby="path-chooser-title" onCancel={event => { event.preventDefault(); onClose(); }}>
  <header><h2 id="path-chooser-title">Which “{name}”?</h2><button className="icon-button" aria-label="Close" onClick={onClose}>×</button></header>
  <p>{choice.matches.length}{choice.truncated ? '+' : ''} files in this workspace match <code>{choice.reference.path}</code>{choice.reference.line ? ` (line ${choice.reference.line})` : ''}.</p>
  <div ref={list} className="path-chooser-list" role="list" onKeyDown={keydown}>
   {choice.matches.map(match => {
    const slash = match.path.lastIndexOf('/');
    return <button key={match.path} role="listitem" onClick={() => onChoose(match)} title={match.path}>
     <FileIcon name={match.path.slice(slash + 1)} directory={match.directory}/>
     <span className="path-chooser-name">{match.path.slice(slash + 1)}</span>
     <span className="path-chooser-folder">{slash < 0 ? 'workspace root' : match.path.slice(0, slash)}</span>
    </button>;
   })}
  </div>
 </dialog>;
}
