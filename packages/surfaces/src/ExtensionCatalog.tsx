import { useEffect, useMemo, useRef, useState } from 'react';
import { DialogHeader, DraggableDialog } from './DraggableDialog';
import type { Extension, ExtensionExample } from '@lazybox/protocol';

type Entry = Extension | ExtensionExample;
const categoryOf = (entry: Entry) => entry.listing?.category || 'Other';

export function useExtensionCatalog(installed: Extension[], examples: ExtensionExample[]) {
  const [query, setQuery] = useState('');
  const [category, setCategory] = useState('');
  const [status, setStatus] = useState('all');
  const normalized = query.trim().toLowerCase();
  const terms = normalized.split(/\s+/).filter(Boolean);
  const categories = [...new Set([...installed, ...examples].map(categoryOf))].sort();
  const entries: Entry[] = [...installed, ...examples.filter(entry => !entry.installed)];
  const filtered = entries.filter(entry => {
    if (status === 'installed' && !('state' in entry)) return false;
    if (status === 'available' && 'state' in entry) return false;
    if (category && categoryOf(entry) !== category) return false;
    const text = [entry.name, entry.id, entry.description, entry.listing?.caption, categoryOf(entry), ...(entry.listing?.keywords || [])].join(' ').toLowerCase();
    return terms.every(term => text.includes(term));
  }).sort((a, b) => Number(b.name.toLowerCase() === normalized) - Number(a.name.toLowerCase() === normalized) || (a.name || a.id).localeCompare(b.name || b.id));
  const controls = <div className="extension-search" role="search" aria-label="Extension catalog">
    <input type="search" aria-label="Search extensions" placeholder="Search extensions…" value={query} onChange={event => setQuery(event.target.value)}/>
    <select aria-label="Extension category" value={category} onChange={event => setCategory(event.target.value)}>
      <option value="">All categories</option>{categories.map(name => <option key={name}>{name}</option>)}
    </select>
    <select aria-label="Installation status" value={status} onChange={event => setStatus(event.target.value)}>
      <option value="all">All extensions</option><option value="installed">Installed</option><option value="available">Available to install</option>
    </select>
    {!filtered.length && entries.length > 0 && <p role="status">No matching extensions. <button onClick={() => { setQuery(''); setCategory(''); setStatus('all'); }}>Clear filters</button></p>}
  </div>;
  return { entries: filtered, controls };
}

export function ExtensionPreview({entry}: {entry: Entry}) {
  const bytes = entry.listing?.previewJpeg;
  const src = useMemo(() => bytes?.length ? 'data:image/jpeg;base64,' + btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join('')) : '', [bytes]);
  const [expanded, setExpanded] = useState(false);
  const thumbnail = useRef<HTMLButtonElement>(null);
  const label = 'Preview ' + entry.name;
  return <div className="extension-listing">
    {entry.listing?.category && <small className="extension-category">{entry.listing.category}</small>}
    {src && <button type="button" ref={thumbnail} className="extension-thumbnail" aria-label={label} onClick={() => setExpanded(true)}>
      <img loading="lazy" src={src} alt=""/>
    </button>}
    {entry.listing?.caption && <small>{entry.listing.caption}</small>}
    {expanded && <PreviewDialog label={label} src={src} caption={entry.listing?.caption || entry.name} onClose={() => { setExpanded(false); thumbnail.current?.focus({preventScroll: true}); }}/>}
  </div>;
}

function PreviewDialog({label, src, caption, onClose}: {label: string; src: string; caption: string; onClose(): void}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  return <DraggableDialog ref={dialog} className="extension-preview-dialog" aria-label={label} onClose={onClose} onDismiss={() => dialog.current?.close()}>
    <DialogHeader><h2>{label}</h2><button type="button" onClick={() => dialog.current?.close()}>Close preview</button></DialogHeader>
    <figure><img src={src} alt=""/><figcaption>{caption}</figcaption></figure>
  </DraggableDialog>;
}
