import { useEffect, useRef, useState } from 'react';
import type { AppHost, AvailableUpdate } from '@lazybox/platform';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { ReleaseSection } from './Onboarding';
import { useRunner } from './RunnerContext';
import { installUpdate, remindLater, skipVersion, useAppUpdate } from './app-update';
import { agentsNotice, progressFraction, progressText, releasesSince } from './update-model';
import './updates.css';

/**
 * Install an update, after reading what changed and what happens to running
 * agents. Nothing installs from anywhere else: the header's notice and
 * Settings → Updates open this. While it installs, the dialog shows each
 * step; Escape hides it (the header keeps the progress).
 */
export function UpdateDialog({ app, update, onClose }: { app?: AppHost; update: AvailableUpdate; onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const state = useAppUpdate(app);
  const { info } = useRunner();
  const agents = agentsNotice(info?.sessions || []);
  const { releases, complete } = releasesSince(update, update.current);
  const installing = state.installing;
  const failed = state.error?.phase === 'install' ? state.error : undefined;
  useEffect(() => {
    const node = dialog.current;
    if (node && !node.open) node.showModal();
    node?.querySelector<HTMLElement>('[data-autofocus]')?.focus();
  }, []);
  const fraction = progressFraction(installing);
  const later = () => { remindLater(update.version); onClose(); };
  const skip = () => { skipVersion(update.version); onClose(); };
  return <DraggableDialog ref={dialog} className="update-dialog" aria-labelledby="update-dialog-title" aria-describedby="update-dialog-summary"
    onCancel={event => { event.preventDefault(); onClose(); }}>
    <DialogHeader>
      <div className="update-dialog-title">
        <span className="update-dialog-mark" aria-hidden="true"><Icon name="download" size={16}/></span>
        <div><h2 id="update-dialog-title">Lazybox {update.version}</h2><small id="update-dialog-summary">You have {update.current}{update.date ? ` · released ${formatDate(update.date)}` : ''}</small></div>
      </div>
      <button className="onboarding-close" aria-label="Close" title="Close (Esc)" onClick={onClose}><Icon name="close" size={16}/></button>
    </DialogHeader>
    <section className="update-dialog-notes" aria-label={`What's new in ${update.version}`} tabIndex={0}>
      {releases.length ? releases.map(release => <ReleaseSection key={release.id} release={release}/>) : <p className="update-muted">No notes were published with this version.</p>}
      {!complete && releases.length > 0 && <p className="update-muted">Earlier changes since {update.current} are in the full release notes.</p>}
    </section>
    <div className={`update-callout${agents ? ' has-agents' : ''}`} role="note">
      <Icon name={agents ? 'activity' : 'refresh'} size={15}/>
      {agents ? <div><strong>{agents.title}</strong><p>{agents.body}</p><p className="update-muted">If the new version cannot take one over, that agent stops and its conversation resumes from where it was.</p></div>
        : <div><strong>Lazybox restarts for a few seconds.</strong><p>It replaces the app, moves its Runner to {update.version} and reopens your windows where they were.</p></div>}
    </div>
    <p className="update-signed"><Icon name="shield" size={13}/>The download is checked against the signing key built into this app before anything is replaced.</p>
    {failed && <p className="settings-error" role="alert">{failed.text}</p>}
    {installing ? <div className="update-progress" role="status" aria-live="polite">
      <span>{progressText(installing, update.version)}</span>
      <progress max={1} value={fraction} aria-label="Update progress"/>
    </div> : <footer className="update-dialog-actions">
      <button className="quiet" onClick={skip} title="Never offer this version again; a later one is offered as usual">Skip this version</button>
      <span/>
      <button onClick={later} title="Hide it for a day">Remind me later</button>
      <button className="primary" data-autofocus onClick={() => void installUpdate(app)}>{failed ? 'Try again' : 'Install and restart'}</button>
    </footer>}
  </DraggableDialog>;
}

const formatDate = (value: string) => {
  const date = new Date(value.includes('T') || value.includes(' ') ? value.replace(' ', 'T') : `${value}T12:00:00`);
  return Number.isNaN(date.getTime()) ? value.slice(0, 10) : date.toLocaleDateString(undefined, { month: 'long', day: 'numeric', year: 'numeric' });
};

/**
 * A quiet notice in the header while an update is waiting: what it is, and
 * one button that opens the install dialog. × is Remind me later (a day).
 * A skipped version, or one put off, shows nothing.
 */
export function UpdateNotice({ app }: { app?: AppHost }) {
  const update = useAppUpdate(app);
  const [open, setOpen] = useState(false);
  const available = update.available;
  if (!available || (update.offer !== 'offer' && !update.installing && !open)) return null;
  return <div className="update-notice" role="status">
    <Icon name="sparkle" size={14}/>
    <span>{update.installing ? progressText(update.installing, available.version) : `Lazybox ${available.version}`}</span>
    {!update.installing && <>
      <button className="update-notice-primary" onClick={() => setOpen(true)}>Update…</button>
      <button className="update-notice-close" aria-label="Remind me later" title="Remind me later (tomorrow)" onClick={() => remindLater(available.version)}><Icon name="close" size={12}/></button>
    </>}
    {update.installing && <button className="update-notice-link" onClick={() => setOpen(true)}>Details</button>}
    {update.error?.phase === 'install' && !update.installing && <button className="update-notice-error" title={update.error.text} onClick={() => setOpen(true)}>Update failed</button>}
    {open && <UpdateDialog app={app} update={available} onClose={() => setOpen(false)}/>}
  </div>;
}
