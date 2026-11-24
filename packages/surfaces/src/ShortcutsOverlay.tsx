import { useEffect, useMemo, useRef, useState } from 'react';
import { useBackdropDismiss } from './dismiss';
import { restoreFocus } from './pending-focus';
import { actionDefinitions, actionEnabled, actionGroups, bindings, displayBinding, runAction, type ActionDefinition } from './actions';
import { fuzzyMatch } from './fuzzy';
import { Icon } from './Icons';
import type { Keymap } from './keyboard';
import './command-center.css';

type Row = { id: string; title: string; group: string; keys: string[][]; /** Cannot be rebound (the reset chord). */ fixed?: boolean };

/** Every action with its keys, grouped; a numbered series ("Go to workspace 1…9") is one row. */
export function shortcutRows(keymap: Keymap): Row[] {
  const rows: Row[] = [];
  const series = new Map<string, Row>();
  for (const definition of actionDefinitions as readonly ActionDefinition[]) {
    const keys = bindings(keymap[definition.id as keyof Keymap]).map(binding => displayBinding(binding));
    if (definition.series) {
      const row = series.get(definition.series);
      const first = keys[0] ? [keys[0].join(' ')] : [];
      if (row) { row.keys[0].push(...first); continue; }
      const created = { id: definition.series, title: definition.title.replace(/\s*\d+$/, ' 1–9'), group: definition.group, keys: [first] };
      series.set(definition.series, created); rows.push(created); continue;
    }
    rows.push({ id: definition.id, title: definition.title, group: definition.group, keys, ...(definition.fixed ? { fixed: true } : {}) });
  }
  // A series reads as its first and last key: "⌘1 … ⌘9".
  for (const row of series.values()) row.keys = row.keys[0].length > 1 ? [[row.keys[0][0], '…', row.keys[0].at(-1)!]] : row.keys[0].length ? [row.keys[0]] : [];
  return rows;
}

/**
 * The shortcuts overlay (`?`, Mod+/): every action of the registry with its
 * keys, grouped and searchable, and a way to change them in Settings.
 */
export function ShortcutsOverlay({ keymap, onClose }: { keymap: Keymap; onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null), input = useRef<HTMLInputElement>(null);
  useBackdropDismiss(dialog);
  const returnFocus = useRef<HTMLElement | null>(document.activeElement as HTMLElement | null);
  const [query, setQuery] = useState('');
  const restore = useRef(true);
  useEffect(() => {
    dialog.current?.showModal(); input.current?.focus();
    const previous = returnFocus.current;
    // A focus the chosen action asked for (a panel, a conversation) wins over giving it back.
    return () => { if (restore.current) requestAnimationFrame(() => restoreFocus(previous)); };
  }, []);
  const rows = useMemo(() => shortcutRows(keymap), [keymap]);
  const shown = query.trim() ? rows.filter(row => fuzzyMatch(query, row.title) || row.keys.some(keys => keys.join(' ').toLowerCase().includes(query.trim().toLowerCase()))) : rows;
  const close = () => onClose();
  const customizable = actionEnabled('settings.open');
  const escape = rows.find(row => row.fixed && row.keys.length);
  return <dialog ref={dialog} className="shortcuts-overlay" aria-labelledby="shortcuts-title" onCancel={event => { event.preventDefault(); close(); }}>
    <header>
      <h2 id="shortcuts-title"><Icon name="keyboard" size={16}/>Keyboard shortcuts</h2>
      <input ref={input} type="search" value={query} onChange={event => setQuery(event.target.value)} placeholder="Filter shortcuts" aria-label="Filter shortcuts" spellCheck={false} autoComplete="off"/>
      <button type="button" className="icon-button" aria-label="Close keyboard shortcuts" onClick={close}><Icon name="close" size={14}/></button>
    </header>
    <div className="shortcuts-results"><div className="shortcuts-groups">
      {actionGroups.map(group => {
        const items = shown.filter(row => row.group === group);
        if (!items.length) return null;
        return <section key={group} aria-labelledby={`shortcuts-${group}`}>
          <h3 id={`shortcuts-${group}`}>{group}</h3>
          <dl>{items.map(row => <div key={row.id} className="shortcut-row" data-action={row.id}>
            <dt>{row.title}{row.fixed && <small className="shortcut-fixed" title="This shortcut cannot be changed, so it always works">always</small>}</dt>
            <dd>{row.keys.length ? row.keys.map((keys, i) => <span key={i} className="shortcut-keys">{i > 0 && <small>or</small>}{keys.map((key, k) => key === '…' ? <small key={k}>…</small> : <kbd key={k}>{key}</kbd>)}</span>) : <small className="shortcut-none">No shortcut</small>}</dd>
          </div>)}</dl>
        </section>;
      })}
      {!shown.length && <p className="command-palette-empty" role="status">No shortcut matches “{query}”.</p>}
    </div></div>
    <footer>
      <span>Keys separated by a space are pressed one after another. Every action is also in the command palette.
        {escape && <> Lost? <kbd>{escape.keys[0].join(' ')}</kbd> resets the layout and keys, with Undo.</>}</span>
      {customizable ? <button type="button" onClick={() => { restore.current = false; onClose(); runAction('settings.open', 'keyboard'); }}><Icon name="pencil" size={14}/>Customize…</button> : <small>Change shortcuts in Settings → Shortcuts, in the main window.</small>}
    </footer>
  </dialog>;
}
