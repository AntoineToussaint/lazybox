import { useEffect, useMemo, useRef, useState } from 'react';
import type { AppInfo, FeedbackDiagnostics, WorkspaceHost } from '@lazybox/platform';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { conversationName } from './conversation-model';
import { feedbackReport, issueUrl, LOG_LINES, type FeedbackConversation } from './feedback-model';
import './feedback.css';

/** What the window looked like when the person chose Send feedback: taken before the dialog opens. */
export type FeedbackShot = { png?: Uint8Array; error?: string };

/**
 * Help → Send feedback. The person writes what happened and chooses what to
 * attach; the preview below is exactly the report every destination gets.
 * Nothing is uploaded: Save to file writes it (and the screenshot) to a
 * folder, Copy puts it on the clipboard, and Open a GitHub issue opens a
 * pre-filled issue the person reads and posts themselves.
 */
export function FeedbackDialog({ host, shot, sessionId, onClose }: { host?: WorkspaceHost; shot?: FeedbackShot; sessionId?: string; onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const { api, info } = useRunner();
  const app = host?.app;
  const [what, setWhat] = useState('');
  const [appInfo, setAppInfo] = useState<AppInfo>();
  const [runnerVersion, setRunnerVersion] = useState('');
  const [diagnostics, setDiagnostics] = useState<FeedbackDiagnostics | 'loading' | 'none'>(app?.feedbackDiagnostics ? 'loading' : 'none');
  const [withShot, setWithShot] = useState(!!shot?.png);
  const [withDiagnostics, setWithDiagnostics] = useState(true);
  const [withConversation, setWithConversation] = useState(false);
  const [status, setStatus] = useState<{ text: string; path?: string; tone?: 'error' }>();
  const [saved, setSaved] = useState('');
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    const node = dialog.current;
    if (node && !node.open) node.showModal();
    node?.querySelector<HTMLElement>('[data-autofocus]')?.focus();
  }, []);
  useEffect(() => { void app?.info().then(setAppInfo, () => {}); }, [app]);
  useEffect(() => { void app?.feedbackDiagnostics?.().then(setDiagnostics, () => setDiagnostics('none')); }, [app]);
  useEffect(() => {
    const control = new AbortController();
    void api.version({}, { signal: control.signal, timeoutMs: 5000 }).then(v => setRunnerVersion(v.version), () => setRunnerVersion(''));
    return () => control.abort();
  }, [api]);
  const shotUrl = useMemo(() => shot?.png ? URL.createObjectURL(new Blob([shot.png as BlobPart], { type: 'image/png' })) : '', [shot]);
  useEffect(() => () => { if (shotUrl) URL.revokeObjectURL(shotUrl); }, [shotUrl]);
  const session = sessionId ? info?.sessions.find(s => s.id === sessionId) : undefined;
  const conversation: FeedbackConversation | undefined = session && {
    title: conversationName(session, ''), agent: session.provider || session.profileId, model: session.model,
    state: session.closed ? 'ended' : session.busy ? 'working' : session.fault ? 'failed' : 'waiting for you', error: session.fault,
  };
  const loaded = typeof diagnostics === 'object' ? diagnostics : undefined;
  const platform = loaded ? `macOS ${loaded.os} (${loaded.arch})` : `Browser: ${typeof navigator === 'undefined' ? 'unknown' : navigator.userAgent}`;
  const report = feedbackReport({
    what, app: { version: appInfo?.version || runnerVersion, channel: appInfo?.channel || (app ? '' : 'browser') }, runnerVersion, platform,
    screenshot: withShot && !!shot?.png, diagnostics: loaded, includeDiagnostics: withDiagnostics, conversation, includeConversation: withConversation,
  });
  const empty = !what.trim();
  // Anything changed after a save is no longer what the file holds, and an error about it is stale.
  useEffect(() => { setSaved(''); setStatus(current => current?.tone === 'error' ? undefined : current); }, [report]);
  const save = async (): Promise<string> => {
    if (saved) return saved;
    if (!app?.saveFeedback) {
      // In a browser: a download the browser keeps in its own folder.
      const link = document.createElement('a');
      link.href = URL.createObjectURL(new Blob([report], { type: 'text/markdown' }));
      link.download = 'lazybox-feedback.md';
      link.click();
      setTimeout(() => URL.revokeObjectURL(link.href), 1000);
      return '';
    }
    const path = await app.saveFeedback({ markdown: report, screenshot: withShot ? shot?.png : undefined });
    setSaved(path);
    return path;
  };
  const run = async (action: () => Promise<void>) => {
    if (empty) { setStatus({ text: 'Write what happened first.', tone: 'error' }); dialog.current?.querySelector<HTMLElement>('textarea')?.focus(); return; }
    setBusy(true);
    try { await action(); } catch (error) { setStatus({ text: String(error instanceof Error ? error.message : error), tone: 'error' }); } finally { setBusy(false); }
  };
  const saveToFile = () => run(async () => {
    const path = await save();
    setStatus(path ? { text: `Saved to ${tilde(path, loaded?.home)}.`, path } : { text: 'Downloaded lazybox-feedback.md.' });
  });
  const copy = () => run(async () => {
    await navigator.clipboard.writeText(report);
    setStatus({ text: withShot && shot?.png ? 'Copied the report. The screenshot is not on the clipboard: Save to file keeps it.' : 'Copied the report to the clipboard.' });
  });
  const openIssue = () => run(async () => {
    // The issue links to the saved report so the screenshot and the whole log can be attached.
    const path = app?.saveFeedback ? await save() : '';
    const url = issueUrl(report, what, { savedAt: path ? tilde(path, loaded?.home) : '', home: loaded?.home });
    if (host?.openLink) await host.openLink(url); else window.open(url, '_blank', 'noopener');
    setStatus({ text: path ? `Opened a new GitHub issue in your browser. Read it, attach the files from ${tilde(path, loaded?.home)} if you like, then post it.` : 'Opened a new GitHub issue in your browser. Read it, then post it.', path: path || undefined });
  });
  return <DraggableDialog ref={dialog} className="feedback-dialog" aria-labelledby="feedback-title" onCancel={event => { event.preventDefault(); onClose(); }}>
    <DialogHeader>
      <div className="feedback-heading"><span className="feedback-mark" aria-hidden="true"><Icon name="chat" size={16}/></span><h2 id="feedback-title">Send feedback</h2></div>
      <button className="onboarding-close" aria-label="Close" title="Close (Esc)" onClick={onClose}><Icon name="close" size={16}/></button>
    </DialogHeader>
    <p className="feedback-lede">Tell us what happened. Lazybox sends nothing by itself: you get a file, the clipboard, or a GitHub issue you read before posting.</p>
    <div className="feedback-layout">
      <div className="feedback-form">
        <label className="feedback-what">
          <span>What happened?</span>
          <textarea data-autofocus rows={6} value={what} onChange={event => setWhat(event.target.value)} placeholder="What did you do, what did you expect, and what happened instead?" aria-required="true"/>
        </label>
        <fieldset className="feedback-attach">
          <legend>Attach</legend>
          {shot?.png ? <label className="feedback-option">
            <input type="checkbox" checked={withShot} onChange={event => setWithShot(event.target.checked)}/>
            <span><strong>A screenshot of this window</strong><small>As it was when you chose Send feedback. Saved as screenshot.png next to the report.</small></span>
            {shotUrl && <img className={`feedback-shot${withShot ? '' : ' is-off'}`} src={shotUrl} alt="The screenshot to attach"/>}
          </label> : <p className="feedback-unavailable"><Icon name="image" size={13}/>{shot?.error ? `No screenshot: ${shot.error}` : 'Screenshots need the Lazybox app.'}</p>}
          <label className="feedback-option">
            <input type="checkbox" checked={withDiagnostics} onChange={event => setWithDiagnostics(event.target.checked)}/>
            <span><strong>Diagnostics</strong><small>{diagnostics === 'loading' ? 'Reading the Runner\'s log…' : `macOS and its version${loaded?.runnerLog ? `, and the last ${LOG_LINES} lines of the Runner's log` : ''}. Credentials are removed and your home folder reads ~.`}</small></span>
          </label>
          <label className={`feedback-option${conversation ? '' : ' is-disabled'}`}>
            <input type="checkbox" disabled={!conversation} checked={withConversation && !!conversation} onChange={event => setWithConversation(event.target.checked)}/>
            <span><strong>The open conversation</strong><small>{conversation ? 'Its title, agent, model, state and last error. Never its messages.' : 'Open a conversation to include which one this is about.'}</small></span>
          </label>
        </fieldset>
      </div>
      <section className="feedback-preview" aria-labelledby="feedback-preview-title">
        <h3 id="feedback-preview-title">Exactly what will be shared</h3>
        <pre tabIndex={0} aria-label="Report preview">{report}</pre>
      </section>
    </div>
    {status && <p className={`feedback-status${status.tone === 'error' ? ' is-error' : ''}`} role={status.tone === 'error' ? 'alert' : 'status'}>
      {status.text}
      {status.path && app?.revealPath && <button className="feedback-reveal" onClick={() => void app.revealPath!(status.path!).catch(error => setStatus({ text: String(error), tone: 'error' }))}>Show in Finder</button>}
    </p>}
    <footer className="feedback-actions">
      <small>No telemetry: nothing leaves this Mac unless you post it.</small>
      <button disabled={busy} onClick={() => void copy()}><Icon name="copy" size={13}/> Copy</button>
      <button disabled={busy} onClick={() => void saveToFile()}><Icon name="save" size={13}/> Save to file</button>
      <button className="primary" disabled={busy} onClick={() => void openIssue()}><Icon name="external" size={13}/> Open a GitHub issue…</button>
    </footer>
  </DraggableDialog>;
}

const tilde = (path: string, home = '') => home && path.startsWith(home + '/') ? `~${path.slice(home.length)}` : path;
