import { useEffect, useRef, type KeyboardEvent } from 'react';
import type { WorkspaceDirectoryEntry } from '@lazybox/protocol';
import { Icon } from './Icons';

/** The parent of a workspace-relative path; '.' is the workspace root. */
export const parentPath = (path: string) => path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '.';

// A colour per file family, so a folder reads at a glance without reading names.
const families: [RegExp, string][] = [
 [/\.(tsx?|jsx?|mjs|cjs)$/i, 'script'],
 [/\.(go|rs|py|rb|java|kt|swift|c|cc|cpp|h|hpp|cs|php|scala|ex|exs)$/i, 'code'],
 [/\.(json|ya?ml|toml|ini|env|lock|cfg|conf|xml|proto)$/i, 'config'],
 [/(\.(md|mdx|txt|rst|adoc)|^readme.*|^license.*|^changelog.*)$/i, 'doc'],
 [/\.(png|jpe?g|gif|svg|webp|ico|pdf)$/i, 'media'],
 [/\.(css|scss|less|html?)$/i, 'style'],
 [/(\.(sh|zsh|bash|fish)|^makefile|^dockerfile.*|^justfile)$/i, 'shell'],
];
export const fileFamily = (name: string) => families.find(([pattern]) => pattern.test(name))?.[1] || 'other';
const family = fileFamily;
/** A file's icon in its family colour, the same in folder listings, the file tree and tabs. */
export function FileIcon({ name, directory = false, size = 15 }: { name: string; directory?: boolean; size?: number }) {
 return <span className={'entry-icon file-icon ' + (directory ? 'is-folder' : 'file-' + fileFamily(name))}><Icon name={directory ? 'folder' : 'file'} size={size}/></span>;
}

/** Folder contents with a parent row, icons and keyboard navigation:
 * ↑/↓ or j/k move, Enter, → or l open, Backspace, ← or h go to the parent. */
export function DirectoryListing({ path, entries, truncated, onOpen }: { path: string; entries: WorkspaceDirectoryEntry[]; truncated: boolean; onOpen(path: string): void }) {
 const list = useRef<HTMLElement>(null);
 const rows = () => [...(list.current?.querySelectorAll<HTMLButtonElement>('button[data-entry]') || [])];
 useEffect(() => { rows()[path === '.' ? 0 : 1]?.focus({ preventScroll: true }); }, [path, entries]);
 const keydown = (event: KeyboardEvent<HTMLElement>) => {
  const all = rows(), index = all.indexOf(document.activeElement as HTMLButtonElement);
  const move = (to: number) => { event.preventDefault(); all[Math.max(0, Math.min(all.length - 1, to))]?.focus(); };
  if (event.key === 'ArrowDown' || event.key === 'j') move(index + 1);
  else if (event.key === 'ArrowUp' || event.key === 'k') move(index - 1);
  else if (event.key === 'Home') move(0);
  else if (event.key === 'End') move(all.length - 1);
  else if ((event.key === 'Backspace' || event.key === 'ArrowLeft' || event.key === 'h') && path !== '.') { event.preventDefault(); onOpen(parentPath(path)); }
  else if ((event.key === 'ArrowRight' || event.key === 'l') && index >= 0) { event.preventDefault(); all[index].click(); }
 };
 return <nav ref={list} className="workspace-directory" aria-label="Folder contents" onKeyDown={keydown}>
  {truncated && <p role="status">Showing the first 500 entries.</p>}
  {path !== '.' && <button data-entry className="directory-parent" title="Parent folder" onClick={() => onOpen(parentPath(path))}><span className="entry-icon"><Icon name="up" size={15}/></span><span>..</span></button>}
  {entries.map(entry => <button data-entry key={entry.path} className={entry.directory ? 'is-folder' : `is-file file-${family(entry.name)}`} onClick={() => onOpen(entry.path)}>
   <span className="entry-icon"><Icon name={entry.directory ? 'folder' : 'file'} size={15}/></span>
   <span className="entry-name">{entry.name}</span>
   {entry.directory && <span className="entry-open" aria-hidden="true"><Icon name="chevronRight" size={14}/></span>}
  </button>)}
  {!entries.length && <p className="module-empty">This folder is empty.</p>}
 </nav>;
}

/** The path as clickable segments; the last one is where you are. */
export function PathCrumbs({ path, disabled, onOpen }: { path: string; disabled?: boolean; onOpen(path: string): void }) {
 const parts = path === '.' ? [] : path.split('/');
 return <nav className="path-crumbs" aria-label="Path">
  <button disabled={disabled || !parts.length} onClick={() => onOpen('.')} title="Workspace root">Workspace</button>
  {parts.map((part, i) => <span key={i}><span className="crumb-separator" aria-hidden="true">/</span>
   {i === parts.length - 1 ? <strong aria-current="page">{part}</strong> : <button disabled={disabled} onClick={() => onOpen(parts.slice(0, i + 1).join('/'))}>{part}</button>}
  </span>)}
 </nav>;
}
