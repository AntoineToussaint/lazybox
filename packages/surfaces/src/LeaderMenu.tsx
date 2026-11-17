import type { KeyHint } from './keyboard';

/** The which-key card shown after a leader such as ]]: every next key and what it does. */
export function LeaderMenu({ hint }: { hint: KeyHint | null }) {
 if (!hint) return null;
 return <div className="leader-menu" role="status" aria-label="Keyboard menu">
  <header><kbd>{hint.prefix}</kbd><small>Press a key · Esc to cancel</small></header>
  <ul>{hint.items.map(item => <li key={item.keys}><kbd>{item.keys}</kbd><span>{item.label}</span></li>)}</ul>
 </div>;
}
