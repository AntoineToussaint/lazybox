import { useState } from 'react';
import type { Session } from '@lazybox/protocol';
import { LiveIndicator } from './LiveIndicator';
import type { LiveStatus } from './live-status';
import { humanError } from './human-error';
import { OwnershipPanel } from './OwnershipPanel';
import { useRunner } from './RunnerContext';

/** A saved conversation: continue it by sending, or resume it explicitly when it needs recovery first. */
export function ResumeBar({ session, canResumeOnSend, onResume, onError }: {
  session: Session;
  canResumeOnSend: boolean;
  /** Resumes the conversation; true once it continues in a new execution. */
  onResume(processesStopped: boolean): Promise<boolean>;
  onError(message: string): void;
}) {
  const { refresh } = useRunner();
  const [resuming, setResuming] = useState(false);
  // What a previous Lazybox left unconfirmed, settled here (OwnershipPanel):
  // the session says so on its next refresh.
  const [settled, setSettled] = useState('');
  const recovery = session.recoveryRequired && settled !== session.id;
  const resume = async () => {
    if (resuming) return; setResuming(true); onError('');
    try { await onResume(false); }
    catch (error) { onError(humanError(error)); } finally { setResuming(false); }
  };
  return <div className="resume-bar">
    <span>{'Saved conversation'}</span>
    {session.resumeBlocked
      ? <p>{session.resumeBlocked}</p>
      : canResumeOnSend
        ? <><small>{session.status === 'interrupted' ? 'The previous agent stopped. Resume to reconnect before sending.' : 'Continue with this conversation’s saved account and permissions.'}</small>
          <button type="button" disabled={resuming} onClick={() => void resume()}>{resuming ? 'Resuming…' : 'Resume conversation'}</button></>
        : recovery
          ? <OwnershipPanel key={session.id} target={{ sessionId: session.id }} what="Resuming this conversation waits" autoFocus={false} onSettled={() => { setSettled(session.id); refresh(); }}/>
          : <button disabled={resuming || !session.providerReference || !session.capabilities?.resume} onClick={() => void resume()}>{resuming ? 'Resuming…' : 'Resume conversation'}</button>}
  </div>;
}

export function TranscriptNotice({ error, onRetry }: { error: string; onRetry(): void }) {
  return <div className="transcript-notice error" role="alert"><span>Conversation could not be loaded. {error}</span><button onClick={onRetry}>Retry conversation</button></div>;
}

/** The line above the composer while the agent works, or while the connection recovers. */
export function LiveActivity({ disconnected, live, session }: { disconnected: boolean; live: LiveStatus; session?: Session }) {
  if (disconnected) return <div key="reconnecting" className="live-activity" data-tone="busy" role="status"><LiveIndicator tone="busy"/>Connection interrupted · reconnecting…</div>;
  if (live.tone === 'busy' && session && !session.closed) return <div key="working" className="live-activity" data-tone="busy" aria-hidden="true"><LiveIndicator tone="busy"/>{live.text}</div>;
  return null;
}
