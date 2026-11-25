import { useMemo, useState } from 'react';
import type { Extension, ExtensionExample } from '@lazybox/protocol';

type Entry = Extension | ExtensionExample;
const categoryOf = (entry: Entry) => entry.listing?.category || 'Other';

export function useExtensionCatalog(installed: Extension[], examples: ExtensionExample[]) {
  const [query, setQuery] = useState('');
  const [category, setCategory] = useState('');
  const [installedOnly, setInstalledOnly] = useState(false);
  const normalized = query.trim().toLowerCase();
  const terms = normalized.split(/\s+/).filter(Boolean);
  const categories = [...new Set([...installed, ...examples].map(categoryOf))].sort();
  function filter<T extends Entry>(entries: T[]) {
    return entries.filter(entry => {
      if (category && categoryOf(entry) !== category) return false;
      const text = [entry.name, entry.id, entry.description, categoryOf(entry), ...(entry.listing?.keywords || [])].join(' ').toLowerCase();
      return terms.every(term => text.includes(term));
    }).sort((a, b) => Number(b.name.toLowerCase() === normalized) - Number(a.name.toLowerCase() === normalized));
  }
  const extensions = filter(installed);
  const available = installedOnly ? [] : filter(examples.filter(entry => !entry.installed));
  const controls = <div className="extension-search" role="search" aria-label="Extension catalog">
    <input type="search" aria-label="Search extensions" placeholder="Search extensions…" value={query} onChange={event => setQuery(event.target.value)}/>
    <select aria-label="Extension category" value={category} onChange={event => setCategory(event.target.value)}>
      <option value="">All categories</option>{categories.map(name => <option key={name}>{name}</option>)}
    </select>
    <label><input type="checkbox" checked={installedOnly} onChange={event => setInstalledOnly(event.target.checked)}/> Installed only</label>
    {!extensions.length && !available.length && (terms.length > 0 || category || installedOnly) && <p role="status">No matching extensions. <button onClick={() => { setQuery(''); setCategory(''); setInstalledOnly(false); }}>Clear filters</button></p>}
  </div>;
  return { extensions, available, controls };
}

export function ExtensionPreview({entry}: {entry: Entry}) {
  const bytes = entry.listing?.previewJpeg;
  const src = useMemo(() => bytes?.length ? 'data:image/jpeg;base64,' + btoa(String.fromCharCode(...bytes)) : '', [bytes]);
  return <div className="extension-listing">
    {entry.listing?.category && <small>{entry.listing.category}</small>}
    {src && <details><summary aria-label={'Preview ' + entry.name}>Preview</summary>
      <figure><img src={src} alt={entry.listing?.caption || entry.name}/>{entry.listing?.caption && <figcaption>{entry.listing.caption}</figcaption>}</figure>
    </details>}
  </div>;
}
