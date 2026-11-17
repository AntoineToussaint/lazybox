import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { DialogHeader, DraggableDialog } from './DraggableDialog';
import './confirm-dialog.css';

/**
 * A choice to tick before confirming: one line, the box beside the first
 * line of its words, the whole line a click target.
 */
export function ConfirmCheckbox({ label, hint, checked, disabled, onChange }: { label: ReactNode; hint?: ReactNode; checked: boolean; disabled?: boolean; onChange(checked: boolean): void }) {
  return <label className="confirm-check">
    <input type="checkbox" checked={checked} disabled={disabled} onChange={event => onChange(event.target.checked)}/>
    <span className="confirm-check-text">{label}{hint && <small>{hint}</small>}</span>
  </label>;
}

/** What makes this confirmation riskier than usual, with what it asks of the person (a checkbox). */
export function ConfirmWarning({ title, children }: { title: ReactNode; children?: ReactNode }) {
  return <section className="confirm-warning">{title && <strong>{title}</strong>}{children}</section>;
}

/**
 * The confirmation every destructive or list-leaving action uses: a title
 * that asks, the thing it is about as a subtitle, what happens in one short
 * paragraph, what is kept on a muted line, and, when the action reaches
 * further than usual, a warning with a checkbox the person must tick first
 * (`check`). Cancel has the focus for a destructive choice, so Enter is
 * harmless; the confirm button is a deliberate danger (or primary) fill, and
 * a plain grey while it cannot be pressed. Confirming shows it working, and a
 * failure says why in the dialog without closing it.
 */
export function ConfirmDialog({ title, label, subject, subjectMeta, children, kept, details, warning, check, status, error, confirm, working, tone = 'danger', cancelLabel = 'Cancel', closeLabel, canConfirm = true, focus, className = '', onClose, onConfirm }: {
  title: string;
  /** The dialog's accessible name, when it is not the title ("Delete conversation"). */
  label?: string;
  /** What the action is about: its name, as a subtitle. */
  subject?: ReactNode;
  /** A fact beside the subject ("3 repositories"). */
  subjectMeta?: ReactNode;
  /** What happens, in a short paragraph. */
  children?: ReactNode;
  /** What is not touched: a muted line. */
  kept?: ReactNode;
  /** More than one warning, each with its own checkbox (ConfirmWarning, ConfirmCheckbox). */
  details?: ReactNode | ((busy: boolean) => ReactNode);
  /** The warning above `check` ("This conversation has an active session or command."). */
  warning?: ReactNode;
  /** A choice the person ticks; `required` keeps the confirm button off until they do. */
  check?: { label: ReactNode; hint?: ReactNode; checked: boolean; onChange(checked: boolean): void; required?: boolean };
  /** A line while something is being checked ("Checking running resources…"). */
  status?: ReactNode;
  /** A problem from before confirming (a check that failed); a failed confirmation shows its own. */
  error?: ReactNode;
  confirm: string;
  /** The confirm button while it works ("Deleting…"). */
  working: string;
  tone?: 'danger' | 'primary';
  cancelLabel?: string;
  /** The × button's name ("Cancel conversation deletion"). */
  closeLabel?: string;
  /** Off while something is unknown or blocked; a required `check` is added to it. */
  canConfirm?: boolean;
  /** Which button has the focus at first: Cancel for a destructive choice. */
  focus?: 'cancel' | 'confirm';
  className?: string;
  onClose(): void;
  /** Resolves when done (the caller closes the dialog); a rejection is shown in it. */
  onConfirm(): Promise<void> | void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [busy, setBusy] = useState(false), [failure, setFailure] = useState('');
  const pending = useRef(false);
  const describedBy = useId();
  const first = focus || (tone === 'danger' ? 'cancel' : 'confirm');
  // Focus as soon as it opens: a menu that opened it gives the focus back to its trigger as it closes.
  useEffect(() => { dialog.current?.showModal(); dialog.current?.querySelector<HTMLElement>('footer [data-autofocus]')?.focus(); }, []);
  const allowed = canConfirm && (!check?.required || check.checked);
  const run = async () => {
    if (pending.current || !allowed) return;
    pending.current = true; setBusy(true); setFailure('');
    try { await onConfirm(); }
    catch (e) { setFailure(String(e)); }
    finally { pending.current = false; setBusy(false); }
  };
  const shownError = failure || error;
  return <DraggableDialog ref={dialog} className={`confirm-dialog ${className}`.trim()} aria-label={label || title} aria-describedby={describedBy}
    onCancel={event => { event.preventDefault(); if (!busy) onClose(); }}>
    <DialogHeader><h2>{title}</h2><button type="button" className="confirm-close" aria-label={closeLabel || cancelLabel} disabled={busy} onClick={onClose}>×</button></DialogHeader>
    {subject && <p className="confirm-subject"><span>{subject}</span>{subjectMeta && <small>{subjectMeta}</small>}</p>}
    <div id={describedBy} className="confirm-body">
      {children && <p className="confirm-lead">{children}</p>}
      {kept && <p className="confirm-kept">{kept}</p>}
    </div>
    {status && <p className="confirm-status" role="status">{status}</p>}
    {typeof details === 'function' ? details(busy) : details}
    {(warning || check) && <ConfirmWarning title={warning}>
      {check && <ConfirmCheckbox label={check.label} hint={check.hint} checked={check.checked} disabled={busy} onChange={check.onChange}/>}
    </ConfirmWarning>}
    {shownError && <p className="confirm-error" role="alert">{shownError}</p>}
    <footer>
      <button type="button" data-autofocus={first === 'cancel' || undefined} disabled={busy} onClick={onClose}>{cancelLabel}</button>
      <button type="button" className={tone} data-autofocus={first === 'confirm' || undefined} disabled={busy || !allowed} onClick={() => void run()}>{busy ? working : confirm}</button>
    </footer>
  </DraggableDialog>;
}
