import { useEffect, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { Snippet } from '@lazybox/protocol';
import { readKeymap, readMenu, saveMenu, type MenuEntry } from './keyboard';
import { useRunner } from './RunnerContext';
import { Icon } from './Icons';
import { SettingsGroup } from './settings-parts';

/** Edits the user's own ]] menu entries: one key sends or inserts one snippet. */
export function LeaderMenuSettings({ store }: { store?: ViewStateStore }) {
 const { api } = useRunner();
 const [entries, setEntries] = useState<MenuEntry[]>(() => readMenu(store));
 const [snippets, setSnippets] = useState<Snippet[]>([]), [status, setStatus] = useState('');
 useEffect(() => {
  const control = new AbortController();
  void api.listSnippets({}, { signal: control.signal }).then(result => setSnippets(result.snippets), error => { if (!control.signal.aborted) setStatus('Snippets could not be loaded. Entries you saved still work.'); });
  return () => control.abort();
 }, [api]);
 const leader = readKeymap(store)['navigation.exit'];
 const change = (index: number, patch: Partial<MenuEntry>) => { setEntries(entries.map((entry, i) => i === index ? { ...entry, ...patch } : entry)); setStatus(''); };
 return <section className="leader-menu-settings">
  <SettingsGroup title={`${leader} menu`} description="Your own entries next to the built-in ones. Each key sends a snippet, or inserts it when you already have a draft.">
  <form onSubmit={event => { event.preventDefault(); try { void saveMenu(entries, store).then(result => setStatus(result.saved || !result.message ? `Menu saved.${result.message ? ` ${result.message.replace(/^Saved to (.*)\.$/, '$1 updated.')}` : ''}` : result.message)); } catch (error) { setStatus(String(error).replace(/^Error:\s*/, '')); } }}>
   {entries.length === 0 && <p className="leader-menu-empty">No entries of your own yet.</p>}
   {entries.length > 0 && <div className="leader-menu-rows">{entries.map((entry, index) => <div className="leader-menu-row" key={index}>
    <label>Key<input aria-label={`Key for entry ${index + 1}`} maxLength={7} value={entry.key} onChange={event => change(index, { key: event.target.value.trim() })}/></label>
    <label>Snippet<select aria-label={`Snippet for entry ${index + 1}`} value={entry.snippet} onChange={event => { const snippet = snippets.find(s => s.key === event.target.value); change(index, { snippet: event.target.value, label: entry.label || snippet?.description || event.target.value }); }}>
     <option value="">Choose…</option>
     {entry.snippet && !snippets.some(s => s.key === entry.snippet) && <option value={entry.snippet}>{entry.snippet} (missing)</option>}
     {snippets.map(s => <option key={s.key} value={s.key}>{s.key}{s.description ? ` · ${s.description}` : ''}</option>)}
    </select></label>
    <label>Label<input aria-label={`Label for entry ${index + 1}`} maxLength={60} value={entry.label} onChange={event => change(index, { label: event.target.value })}/></label>
    <label>Action<select aria-label={`Action for entry ${index + 1}`} value={entry.mode} onChange={event => change(index, { mode: event.target.value === 'insert' ? 'insert' : 'send' })}><option value="send">Send</option><option value="insert">Insert</option></select></label>
    <button type="button" className="icon-button" title="Remove entry" aria-label={`Remove entry ${index + 1}`} onClick={() => setEntries(entries.filter((_, i) => i !== index))}><Icon name="close" size={14}/></button>
   </div>)}</div>}
   <div className="leader-menu-actions"><button type="button" onClick={() => setEntries([...entries, { key: '', label: '', snippet: '', mode: 'send' }])}><Icon name="plus" size={14}/>Add entry</button><p role="status">{status}</p><button className="primary">Save menu</button></div>
  </form>
  </SettingsGroup>
 </section>;
}
