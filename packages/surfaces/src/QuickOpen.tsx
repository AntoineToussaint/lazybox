import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { useBackdropDismiss } from './dismiss';
import { ConnectError } from '@lazybox/client-core';
import { FileIcon } from './DirectoryListing';
import { preloadEditor } from './editor-loader';
import { parseQuery } from './editor-model';
import { modKey, shortcut, useWorkbenchState, type Workbench } from './workbench';
import './files.css';

type Result = { path: string; positions: number[] };
/** Bold the matched characters of a path, split into its name and folder. */
export function highlight(text: string, positions: number[], offset = 0): ReactNode[] {
 const marked = new Set(positions), out: ReactNode[] = [];
 let run = '', bold = false;
 const flush = () => { if (run) out.push(bold ? <mark key={out.length}>{run}</mark> : run); run = ''; };
 for (let i = 0; i < text.length; i++) { const hit = marked.has(i + offset); if (hit !== bold) { flush(); bold = hit; } run += text[i]; }
 flush();
 return out;
}

/** Go to file: a fuzzy finder over every file in the workspace's repositories
 * (what Git would show; ignored files are left out). With no query it lists
 * the files opened most recently. */
export function QuickOpen({ workbench, onClose }: { workbench: Workbench; onClose(): void }) {
 const dialog = useRef<HTMLDialogElement>(null), input = useRef<HTMLInputElement>(null), list = useRef<HTMLDivElement>(null);
 useBackdropDismiss(dialog);
 const { layout } = useWorkbenchState(workbench);
 const [query, setQuery] = useState(''), [results, setResults] = useState<Result[]>([]), [selected, setSelected] = useState(0);
 const [status, setStatus] = useState<{ searching: boolean; error: string; truncated: boolean; indexed: number }>({ searching: false, error: '', truncated: false, indexed: 0 });
 const { term, line } = parseQuery(query);
 useEffect(() => { dialog.current?.showModal(); input.current?.focus(); void preloadEditor(); }, []);
 useEffect(() => {
  if (!term) { setResults(layout.recent.map(path => ({ path, positions: [] }))); setSelected(0); setStatus(value => ({ ...value, searching: false, error: '' })); return; }
  const control = new AbortController();
  setStatus(value => ({ ...value, searching: true }));
  // A short pause lets a fast typist's keystrokes share one search.
  const timer = setTimeout(() => {
   workbench.api.searchWorkspaceFiles({ collectionId: workbench.workspaceId, query: term, limit: 60 }, { signal: control.signal, timeoutMs: 15000 }).then(found => {
    if (control.signal.aborted) return;
    setResults(found.matches.map(match => ({ path: match.path, positions: match.positions })));
    setSelected(0);
    setStatus({ searching: false, error: '', truncated: found.truncated, indexed: found.indexed });
   }, error => { if (!control.signal.aborted) setStatus(value => ({ ...value, searching: false, error: ConnectError.from(error).rawMessage })); });
  }, 40);
  return () => { clearTimeout(timer); control.abort(); };
 }, [term, workbench, layout.recent]);
 useEffect(() => { list.current?.querySelector<HTMLElement>('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' }); }, [selected, results]);
 const choose = (result: Result | undefined, how: { side?: boolean; quick?: boolean } = {}) => {
  if (!result) return;
  onClose();
  if (how.quick) workbench.host.quickLook(result.path, line);
  else workbench.open(result.path, { line, side: how.side ? 'row' : undefined, focus: true });
 };
 const keydown = (event: KeyboardEvent) => {
  const move = (step: number) => { event.preventDefault(); setSelected(value => results.length ? (value + step + results.length) % results.length : 0); };
  // Pressing Go to file again steps down the list, as in other editors.
  if (event.key === 'ArrowDown' || (event.ctrlKey && event.key === 'n') || (modKey(event) && event.code === 'KeyP')) move(1);
  else if (event.key === 'ArrowUp' || (event.ctrlKey && event.key === 'p')) move(-1);
  else if (event.key === 'PageDown') move(8);
  else if (event.key === 'PageUp') move(-8);
  else if (event.key === 'Enter') { event.preventDefault(); choose(results[selected], { side: modKey(event), quick: event.altKey }); }
 };
 const empty = term ? (status.searching ? '' : status.error || 'No matching files.') : 'Type a file name. Recently opened files appear here.';
 return <dialog ref={dialog} className="quick-open" aria-label="Go to file" onCancel={event => { event.preventDefault(); onClose(); }}>
  <div className="quick-open-field">
   <input ref={input} value={query} onChange={event => setQuery(event.target.value)} onKeyDown={keydown} placeholder="Go to file — name, path, or name:line" aria-label="File name" role="combobox" aria-expanded="true" aria-controls="quick-open-results" aria-activedescendant={results[selected] ? `quick-open-${selected}` : undefined} spellCheck={false} autoComplete="off"/>
   {status.searching && <span className="quick-open-busy" aria-hidden="true"/>}
  </div>
  {!term && results.length > 0 && <small className="quick-open-heading">Recently opened</small>}
  <div ref={list} id="quick-open-results" className="quick-open-results" role="listbox" aria-label="Files">
   {results.map((result, i) => {
    const slash = result.path.lastIndexOf('/'), file = result.path.slice(slash + 1), folder = slash < 0 ? '' : result.path.slice(0, slash);
    return <div key={result.path} id={`quick-open-${i}`} role="option" aria-selected={i === selected} className="quick-open-row" title={result.path} onPointerMove={() => setSelected(i)} onClick={event => choose(result, { side: modKey(event), quick: event.altKey })}>
     <FileIcon name={file} size={14}/>
     <span className="quick-open-name">{highlight(file, result.positions, slash + 1)}</span>
     <span className="quick-open-folder">{highlight(folder, result.positions)}</span>
    </div>;
   })}
   {!results.length && empty && <p className="quick-open-empty" role="status">{empty}</p>}
  </div>
  <footer className="quick-open-footer"><span><kbd>↑</kbd><kbd>↓</kbd> choose</span><span><kbd>Enter</kbd> open</span><span><kbd>{shortcut('Mod+Enter')}</kbd> beside</span><span><kbd>{shortcut('Alt+Enter')}</kbd> quick look</span><span><kbd>:42</kbd> line</span>{status.truncated && <span className="quick-open-limit">Searched the first {status.indexed.toLocaleString()} files</span>}</footer>
 </dialog>;
}
