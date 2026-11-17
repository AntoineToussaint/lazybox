import { Icon } from './Icons';

/** A panel of this page that is in a window of its own: a chat's window, or a group popped out. */
export type OtherWindow = {
  id: string;
  /** A chat's own window, or a group popped out of the page. */
  kind: 'chat' | 'popout';
  title: string;
  icon: string;
  /** Brings its window to the front. */
  show(): void;
  /** Back where it was in this window; its own window closes. */
  putBack(): void | Promise<void>;
};

/**
 * The quiet strip under the workspace header while panels of the page are in
 * windows of their own: what each one is, **Show** (its window to the front)
 * and **Put back** (where it was, here), and **Put all back** when there are
 * several. The palette offers the same, and ⇧⌘D in such a window.
 */
export function OtherWindows({ windows, putBackKeys, onPutAllBack }: { windows: OtherWindow[]; /** The keys of Put back in main window, as shown (⇧⌘D). */ putBackKeys?: string; onPutAllBack(): void }) {
  if (!windows.length) return null;
  return <section className="other-windows" aria-label="In other windows">
    <span className="other-windows-label"><Icon name="external" size={13}/>In other windows</span>
    <ul>
      {windows.map(item => <li key={item.id} className="other-window" data-kind={item.kind}>
        <Icon name={item.icon} size={13}/>
        <span className="other-window-title" title={item.title}>{item.title}</span>
        <button type="button" className="other-window-show" aria-label={`Show ${item.title}`} title="Bring its window to the front" onClick={item.show}>Show</button>
        <button type="button" className="other-window-put-back" aria-label={`Put ${item.title} back`} title={`Put back where it was in this window${putBackKeys ? ` · ${putBackKeys} in its window` : ''}`} onClick={item.putBack}><Icon name="put-back" size={12}/>Put back</button>
      </li>)}
    </ul>
    {windows.length > 1 && <button type="button" className="other-windows-all" onClick={onPutAllBack}><Icon name="put-back" size={12}/>Put all back</button>}
  </section>;
}
