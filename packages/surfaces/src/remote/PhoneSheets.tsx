import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { useBackdropDismiss } from '../dismiss';
import { Icon } from '../Icons';

/** How long a row's answer waits for Undo before it is sent. */
export const UNDO_MS = 4500;

/**
 * A bottom sheet: a modal dialog (focus stays inside, Esc and a tap outside
 * close it) that rises from the bottom edge, above the safe area, with
 * thumb-sized choices.
 */
export function Sheet({ title, detail, onClose, children, label }: { title: ReactNode; detail?: ReactNode; label: string; onClose(): void; children: ReactNode }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useBackdropDismiss(dialog);
  useEffect(() => { const node = dialog.current; if (node && !node.open) node.showModal(); }, []);
  return <dialog ref={dialog} className="remote-sheet" aria-label={label} onCancel={event => { event.preventDefault(); onClose(); }}>
    <div className="remote-sheet-body">
      <span className="remote-sheet-grip" aria-hidden="true"/>
      <header><strong>{title}</strong>{detail && <div className="remote-sheet-detail">{detail}</div>}</header>
      {children}
    </div>
  </dialog>;
}

export type SheetChoice = { id: string; label: string; icon?: string; glyph?: ReactNode; tone?: 'danger' | 'primary'; run(): void };

/** A list of actions in a sheet, and Cancel. */
export function ActionSheet({ title, detail, choices, onClose }: { title: ReactNode; detail?: ReactNode; choices: SheetChoice[]; onClose(): void }) {
  return <Sheet title={title} detail={detail} label="Actions" onClose={onClose}>
    <div className="remote-sheet-choices" role="group">
      {choices.map(choice => <button key={choice.id} type="button" className={choice.tone || undefined} onClick={() => { onClose(); choice.run(); }}>
        {choice.glyph || (choice.icon && <Icon name={choice.icon} size={18}/>)}<span>{choice.label}</span>
      </button>)}
    </div>
    <button type="button" className="remote-sheet-cancel" autoFocus onClick={onClose}>Cancel</button>
  </Sheet>;
}

/** Asks before something that cannot be taken back. Cancel has focus, so a stray tap or Enter does nothing. */
export function ConfirmSheet({ title, detail, confirm, tone = 'danger', onConfirm, onClose }: { title: ReactNode; detail?: ReactNode; confirm: string; tone?: 'danger' | 'primary'; onConfirm(): void; onClose(): void }) {
  return <Sheet title={title} detail={detail} label={typeof title === 'string' ? title : confirm} onClose={onClose}>
    <div className="remote-sheet-row">
      <button type="button" autoFocus onClick={onClose}>Cancel</button>
      <button type="button" className={tone} onClick={() => { onConfirm(); onClose(); }}>{confirm}</button>
    </div>
  </Sheet>;
}

/** Deny, with an optional reason the agent is told, as on the computer. */
export function DenySheet({ what, agentName, onDeny, onClose }: { what: ReactNode; agentName: string; onDeny(reason: string): void; onClose(): void }) {
  const [reason, setReason] = useState('');
  return <Sheet title="Deny this request?" detail={what} label="Deny this request?" onClose={onClose}>
    <form className="remote-sheet-form" onSubmit={event => { event.preventDefault(); onClose(); onDeny(reason.trim()); }}>
      <label>Why not? <small>Optional. {agentName} is told, and can try another way.</small>
        <textarea rows={2} value={reason} placeholder="e.g. Use make test instead" onChange={event => setReason(event.target.value)}/>
      </label>
      <div className="remote-sheet-row">
        <button type="button" onClick={onClose}>Back</button>
        <button type="submit" className="danger">Deny</button>
      </div>
    </form>
  </Sheet>;
}

type Snack = { id: string; text: string; undo?: () => void; action?: { label: string; run(): void }; tone?: 'error' | 'ok'; until: number };
export type RowAction = {
  /** One per request: scheduling it again replaces the earlier one. */
  id: string;
  /** What the snackbar says while Undo is possible ("Allowed · npm update"). */
  label: string;
  commit(): Promise<void>;
  /** What the row says while it waits ("Allowing…"). */
  waiting: string;
};
export type ActionState = { phase: 'waiting' | 'sending' | 'sent'; text: string };

/**
 * Answers sent from Inbox rows wait a moment for Undo, then go. Leaving the
 * page or the app sends them at once: an answer the person saw accepted is
 * never silently dropped. A failure says so, with Retry.
 */
export function useRowActions() {
  const [snack, setSnack] = useState<Snack>();
  const [states, setStates] = useState<Record<string, ActionState>>({});
  const timers = useRef(new Map<string, { timer: ReturnType<typeof setTimeout>; action: RowAction }>());
  const setState = (id: string, state: ActionState | undefined) => setStates(all => { const next = { ...all }; if (state) next[id] = state; else delete next[id]; return next; });
  const commit = useCallback(async (id: string) => {
    const entry = timers.current.get(id);
    if (!entry) return;
    clearTimeout(entry.timer); timers.current.delete(id);
    setState(id, { phase: 'sending', text: entry.action.waiting });
    setSnack(current => current?.id === id ? undefined : current);
    try {
      await entry.action.commit();
      setState(id, { phase: 'sent', text: 'Sent' });
      setTimeout(() => setState(id, undefined), 20_000);
    } catch (error) {
      setState(id, undefined);
      const message = error instanceof Error ? error.message.replace(/^\[\w+\]\s*/, '') : String(error);
      setSnack({ id: `error:${id}`, tone: 'error', text: `Not sent: ${message}`, action: { label: 'Retry', run: () => schedule(entry.action, 0) }, until: Date.now() + 10_000 });
    }
  }, []);
  const schedule = useCallback((action: RowAction, delay = UNDO_MS) => {
    const previous = timers.current.get(action.id);
    if (previous) clearTimeout(previous.timer);
    timers.current.set(action.id, { timer: setTimeout(() => void commit(action.id), delay), action });
    setState(action.id, { phase: 'waiting', text: action.waiting });
    if (delay > 0) setSnack({ id: action.id, text: action.label, undo: () => cancel(action.id), until: Date.now() + delay });
  }, [commit]);
  const cancel = useCallback((id: string) => {
    const entry = timers.current.get(id);
    if (entry) { clearTimeout(entry.timer); timers.current.delete(id); }
    setState(id, undefined); setSnack(undefined);
  }, []);
  const flush = useCallback(() => { for (const id of [...timers.current.keys()]) void commit(id); }, [commit]);
  /** A local change (pin, mark read) with its own Undo. */
  const notify = useCallback((text: string, undo?: () => void, tone?: 'ok' | 'error') => setSnack({ id: crypto.randomUUID(), text, undo, tone, until: Date.now() + (undo ? UNDO_MS : 3000) }), []);
  useEffect(() => {
    const hidden = () => { if (document.visibilityState === 'hidden') flush(); };
    document.addEventListener('visibilitychange', hidden); window.addEventListener('pagehide', flush);
    return () => { document.removeEventListener('visibilitychange', hidden); window.removeEventListener('pagehide', flush); flush(); };
  }, [flush]);
  useEffect(() => {
    if (!snack) return;
    const timer = setTimeout(() => setSnack(current => current === snack ? undefined : current), Math.max(0, snack.until - Date.now()));
    return () => clearTimeout(timer);
  }, [snack]);
  const snackbar = snack ? <div className="remote-snackbar" data-tone={snack.tone} role="status" aria-live="polite">
    <span>{snack.text}</span>
    {snack.undo && <button type="button" onClick={() => { snack.undo!(); setSnack(undefined); }}>Undo</button>}
    {snack.action && <button type="button" onClick={() => { setSnack(undefined); snack.action!.run(); }}>{snack.action.label}</button>}
    {!snack.undo && !snack.action && <button type="button" className="remote-snackbar-close" aria-label="Dismiss" onClick={() => setSnack(undefined)}><Icon name="close" size={14}/></button>}
  </div> : null;
  return { states, schedule, cancel, flush, notify, snackbar };
}
