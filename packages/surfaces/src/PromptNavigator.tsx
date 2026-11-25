import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type RefObject } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from './Icons';
import { useDismiss } from './dismiss';
import type { TranscriptMessage } from './transcript-model';
import './prompt-navigator.css';

const firstLine = (text: string) => text.trim().split('\n', 1)[0].slice(0, 120);

/**
 * Jump between your prompts without scrolling: a rail of markers on the
 * conversation's edge, a searchable list, and Alt+↑ / Alt+↓.
 */
export const PromptNavigator = memo(function PromptNavigator({ messages, paneRef, onReveal }: { messages: TranscriptMessage[]; paneRef: RefObject<HTMLDivElement | null>; onReveal(id: string): void }) {
 const prompts = useMemo(() => messages.map((message, index) => ({ message, index })).filter(({ message }) => message.type === 'operation_accepted' && !message.origin && message.text.trim()), [messages]);
 const [open, setOpen] = useState(false), [query, setQuery] = useState(''), [active, setActive] = useState(0);
 const search = useRef<HTMLInputElement>(null), panel = useRef<HTMLDivElement>(null), toggle = useRef<HTMLButtonElement>(null);
 // The chip belongs to the chat's header, where it covers nothing; floating over the transcript it sat on
 // whatever scrolled under it. A window without that header keeps it in the transcript's corner.
 const [slot, setSlot] = useState<Element | null>(null);
 useLayoutEffect(() => { setSlot(paneRef.current?.closest('.agent-window')?.querySelector('[data-prompt-jump-slot]') || null); });
 // Keep markers inside the content edge: classic scrollbars own their gutter.
 // ResizeObserver also notices a scrollbar appearing as the transcript grows.
 const [gutter, setGutter] = useState(0);
 // The pane is a later sibling; wait until its ref has attached.
 useEffect(() => {
  const pane = paneRef.current; if (!pane) return;
  const measure = () => setGutter(Math.max(0, pane.offsetWidth - pane.clientWidth));
  measure();
  const resize = new ResizeObserver(measure); resize.observe(pane);
  return () => resize.disconnect();
 }, [paneRef]);
 const latest = useRef({ prompts, onReveal }); latest.current = { prompts, onReveal };

 // The prompt at or above the top of the view. Unmounted earlier history is above it.
 const current = () => {
  const pane = paneRef.current; if (!pane) return -1;
  const top = pane.getBoundingClientRect().top + 48; let found = -1;
  latest.current.prompts.forEach(({ message }, k) => {
   const node = pane.querySelector(`[data-message-id="${CSS.escape(message.id)}"]`);
   if (!node || node.getBoundingClientRect().top <= top) found = k;
  });
  return found;
 };
 const step = (direction: -1 | 1) => {
  const list = latest.current.prompts; if (!list.length) return;
  const pane = paneRef.current, at = current();
  let target = direction > 0 ? at + 1 : at;
  if (direction < 0 && at >= 0 && pane) {
   const node = pane.querySelector(`[data-message-id="${CSS.escape(list[at].message.id)}"]`);
   // Already at this prompt: go to the one before it.
   if (node && Math.abs(node.getBoundingClientRect().top - pane.getBoundingClientRect().top) < 60) target = at - 1;
  }
  target = Math.max(0, Math.min(list.length - 1, target));
  latest.current.onReveal(list[target].message.id);
 };

 useEffect(() => {
  const keydown = (event: KeyboardEvent) => {
   if (!event.altKey || event.metaKey || event.ctrlKey || (event.key !== 'ArrowUp' && event.key !== 'ArrowDown')) return;
   const scope = paneRef.current?.closest('.embedded-chat, .agent-window, .chat-panel, section') || paneRef.current?.parentElement;
   const target = event.target as HTMLElement | null;
   if (!scope || !target || !scope.contains(target)) return;
   // In a draft, Alt+arrows move the caret; only an empty composer jumps.
   if (target.matches('textarea, input') && (target as HTMLTextAreaElement).value.trim()) return;
   event.preventDefault(); step(event.key === 'ArrowUp' ? -1 : 1);
  };
  window.addEventListener('keydown', keydown);
  return () => window.removeEventListener('keydown', keydown);
 }, [paneRef]);

 const filtered = useMemo(() => {
  const needle = query.trim().toLowerCase();
  return prompts.map((p, k) => ({ ...p, k })).reverse().filter(({ message }) => !needle || message.text.toLowerCase().includes(needle));
 }, [prompts, query]);
 useEffect(() => { if (open) { setActive(0); requestAnimationFrame(() => search.current?.focus()); } }, [open]);
 useEffect(() => setActive(0), [query]);
 useEffect(() => {
  if (open) panel.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
 }, [active, filtered, open]);
 // A press outside or Escape closes the list; focus goes back where it was (the message box).
 useDismiss(open, () => setOpen(false), [panel, toggle]);
 if (prompts.length < 2) return null;

 const choose = (id: string) => { setOpen(false); setQuery(''); onReveal(id); };
 const last = Math.max(1, messages.length - 1);
 return <>
  <nav className="prompt-rail" aria-label="Your prompts" style={{ right: gutter + 3 }}>
   {prompts.map(({ message, index }, k) => <button key={message.id} type="button" style={{ top: `${(index / last) * 100}%` }} title={`#${k + 1} · ${firstLine(message.text)}`} aria-label={`Prompt ${k + 1}: ${firstLine(message.text)}`} onClick={() => onReveal(message.id)}/>)}
  </nav>
  {(() => { const jump = <div className={`prompt-jump${slot ? ' in-header' : ''}`}>
   {/* A press keeps the focus where it is, so closing the list can give it back. */}
   <button ref={toggle} type="button" className="prompt-jump-toggle" aria-haspopup="dialog" aria-expanded={open} onMouseDown={event => event.preventDefault()} aria-label="Your prompts" title="Your prompts · Alt+↑ / Alt+↓" onClick={() => setOpen(value => !value)}><Icon name="chat" size={14}/><span>{prompts.length}<span className="prompt-jump-noun"> prompts</span></span></button>
   {open && <div ref={panel} className="prompt-jump-list" role="dialog" aria-label="Jump to a prompt" onKeyDown={event => {
    if (event.key === 'ArrowDown') { event.preventDefault(); setActive(value => Math.min(filtered.length - 1, value + 1)); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); setActive(value => Math.max(0, value - 1)); }
    else if (event.key === 'Enter' && filtered[active]) { event.preventDefault(); choose(filtered[active].message.id); }
   }}>
    <input ref={search} type="search" placeholder="Find a prompt…" aria-label="Find a prompt" value={query} onChange={event => setQuery(event.target.value)}/>
    <ul role="listbox" aria-label="Prompts, newest first">
     {filtered.map(({ message, k }, row) => <li key={message.id} role="option" aria-selected={row === active}>
      <button type="button" onMouseEnter={() => setActive(row)} onClick={() => choose(message.id)}><small>#{k + 1}</small><span>{firstLine(message.text)}</span></button>
     </li>)}
     {!filtered.length && <li className="prompt-jump-empty">No prompt matches.</li>}
    </ul>
    <footer>↑↓ choose · Enter jump · Alt+↑/↓ anywhere in the chat</footer>
   </div>}
  </div>;
   return slot ? createPortal(jump, slot) : jump; })()}
 </>;
});
