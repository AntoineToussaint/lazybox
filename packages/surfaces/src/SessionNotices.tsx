import { useState } from 'react';
import type { Session } from '@lazybox/protocol';
import { LiveIndicator } from './LiveIndicator';
import type { LiveStatus } from './live-status';
import { humanError } from './human-error';

/** A saved conversation: continue it by sending, or resume it explicitly when it needs recovery first. */
export function ResumeBar({ session, canResumeOnSend, onResume, onError }: {
  session: Session;
  canResumeOnSend: boolean;
  /** Resumes the conversation; true once it continues in a new execution. */
  onResume(processesStopped: boolean): Promise<boolean>;
  onError(message: string): void;
}) {
  const [resuming, setResuming] = useState(false);
  const [processesStopped, setProcessesStopped] = useState(false);
  const resume = async () => {
    if (resuming) return; setResuming(true); onError('');
    try { if (await onResume(processesStopped)) setProcessesStopped(false); }
    catch (error) { onError(humanError(error)); } finally { setResuming(false); }
  };
  return <div className="resume-bar">
    <span>{'Saved conversation'}</span>
    {session.resumeBlocked
      ? <p>{session.resumeBlocked}</p>
      : canResumeOnSend
        ? <small>Send a message to continue with this conversation’s saved account and permissions.</small>
        : <>
          {session.recoveryRequired && <label><input type="checkbox" checked={processesStopped} onChange={event => setProcessesStopped(event.target.checked)}/>I have stopped or reconciled the previous agent and its commands.</label>}
          <button disabled={resuming || !session.providerReference || !session.capabilities?.resume || (session.recoveryRequired && !processesStopped)} onClick={() => void resume()}>{resuming ? 'Resuming…' : 'Resume conversation'}</button>
        </>}
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
