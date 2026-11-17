import { useEffect, useState, type ReactNode } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';

/**
 * The pieces every Settings tab is made of, so each one reads the same: a
 * section header (title, one sentence, at most one primary action), groups of
 * rows with the label on the left and the control on the right, and errors
 * written as sentences.
 */
export function SettingsHeader({ id, title, description, actions }: { id?: string; title: string; description?: ReactNode; actions?: ReactNode }) {
  return <header className="settings-section-header">
    <div><h2 id={id}>{title}</h2>{description && <p>{description}</p>}</div>
    {actions && <div className="settings-section-actions">{actions}</div>}
  </header>;
}

/** A titled group of rows on one surface. */
export function SettingsGroup({ id, title, description, children, className = "" }: { id?: string; title?: string; description?: ReactNode; children: ReactNode; className?: string }) {
  return <section className={`settings-group ${className}`.trim()}>
    {(title || description) && <div className="settings-group-heading">{title && <h3 id={id}>{title}</h3>}{description && <p>{description}</p>}</div>}
    <div className="settings-group-body">{children}</div>
  </section>;
}

/** One setting: what it is and what it does on the left, its control on the right (stacked when narrow). */
export function SettingsRow({ label, htmlFor, description, children, stacked }: { label: ReactNode; htmlFor?: string; description?: ReactNode; children: ReactNode; stacked?: boolean }) {
  return <div className={`settings-row${stacked ? ' is-stacked' : ''}`}>
    <div className="settings-row-text">{htmlFor ? <label htmlFor={htmlFor}>{label}</label> : <span className="settings-row-label">{label}</span>}{description && <small>{description}</small>}</div>
    <div className="settings-row-control">{children}</div>
  </div>;
}

/** A Runner refusal as a sentence a person can act on; never a code or a stack. */
export function settingsError(error: unknown, action = 'save'): string {
  const failure = ConnectError.from(error);
  switch (failure.code) {
    case Code.Aborted: case Code.FailedPrecondition:
      return failure.rawMessage && !/^[a-z_ ]+$/i.test(failure.rawMessage) && failure.code === Code.FailedPrecondition ? sentence(failure.rawMessage) : 'Settings changed in another window. Reload them, then try again.';
    case Code.Unavailable: case Code.DeadlineExceeded: case Code.Canceled:
      return `The Runner did not answer, so nothing was ${action === 'load' ? 'loaded' : 'saved'}. Check that it is running, then try again.`;
    case Code.InvalidArgument: return sentence(failure.rawMessage || 'That value is not accepted.');
    default: return failure.rawMessage ? sentence(failure.rawMessage) : `Could not ${action}. Try again.`;
  }
}
const sentence = (text: string) => { const t = text.trim(); return t ? `${t[0].toUpperCase()}${t.slice(1)}${/[.!?]$/.test(t) ? '' : '.'}` : t; };

/** The current time, ticking every `ms` while mounted, for labels like "in 2h 10m". */
export function useNow(ms = 30_000): number {
  const [now, setNow] = useState(Date.now);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), ms); return () => clearInterval(timer); }, [ms]);
  return now;
}
