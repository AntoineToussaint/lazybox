import { useEffect, useRef, useState } from 'react';
import type { RepositoryIdentityRefresh, RepositoryReplacement as Replacement } from '@lazybox/protocol';
import { humanError } from './human-error';
import { useRunner } from './RunnerContext';
import './repository-replacement.css';

/**
 * The way forward when a repository's history changed on GitHub since
 * Lazybox copied it (WORKSPACES.md, "When a repository's history changes"):
 * a force-push with a new root, or a new repository that took the name.
 * Making a workspace there would mix two histories, so the Runner refused;
 * replacing Lazybox's copy clones what GitHub has now, after the person sees
 * what that changes. `onReplaced` runs once it is done (New task retries).
 */
export function ReplaceRepositoryCopy({ replacement, lead = true, autoFocus = false, onReplaced, onCancel }: {
  replacement: Pick<Replacement, 'identity' | 'name' | 'hasCopy'>;
  /** Say what happened first (New task's error card); Settings states it itself. */
  lead?: boolean;
  autoFocus?: boolean;
  onReplaced(refresh: RepositoryIdentityRefresh): void;
  onCancel?(): void;
}) {
  const { api } = useRunner();
  const [step, setStep] = useState<'offer' | 'checking' | 'confirm' | 'replacing'>('offer');
  const [preview, setPreview] = useState<RepositoryIdentityRefresh>();
  const [error, setError] = useState('');
  const offer = useRef<HTMLButtonElement>(null), confirm = useRef<HTMLButtonElement>(null);
  useEffect(() => { if (autoFocus) offer.current?.focus(); }, [autoFocus]);
  useEffect(() => { if (step === 'confirm') confirm.current?.focus(); }, [step]);
  const name = preview?.name || replacement.name.replace(/^github:/, '');
  const check = async () => {
    setStep('checking'); setError('');
    try { setPreview(await api.refreshRepositoryIdentity({ identity: replacement.identity, confirm: false }, { timeoutMs: 30_000 })); setStep('confirm'); }
    catch (failure) { setError(humanError(failure)); setStep('offer'); }
  };
  const replace = async () => {
    setStep('replacing'); setError('');
    try {
      const done = await api.refreshRepositoryIdentity({ identity: replacement.identity, confirm: true }, { timeoutMs: 600_000 });
      onReplaced(done);
    } catch (failure) { setError(humanError(failure)); setStep('confirm'); }
  };
  const copied = preview ? preview.hasCopy : replacement.hasCopy;
  const workspaces = preview?.workspaces || [];
  return <section className="repository-replacement" aria-label={`${name} changed on GitHub`}>
    {lead && <p className="repository-replacement-lead">
      <strong>{copied ? 'The repository’s history changed on GitHub since Lazybox copied it.' : `${name} on GitHub is a different repository now: a new one took its name since Lazybox listed it.`}</strong>{' '}
      Lazybox won’t mix the two histories in one workspace.
    </p>}
    {step === 'confirm' && preview && <div className="repository-replacement-effects" role="group" aria-label="What replacing the copy changes">
      <p>Lazybox will copy {name} as GitHub has it now{preview.reason === 'replaced' ? ' (the new repository under that name)' : ''}. New workspaces start from that copy.</p>
      {copied && (workspaces.length
        ? <p>{workspaces.length === 1 ? 'One workspace was' : `${workspaces.length} workspaces were`} made from the old copy: <span className="repository-replacement-names">{workspaces.join(', ')}</span>. {workspaces.length === 1 ? 'It keeps' : 'They keep'} working on {workspaces.length === 1 ? 'its' : 'their'} own worktrees and branches; the old copy stays on disk for {workspaces.length === 1 ? 'it' : 'them'}. Pushing from {workspaces.length === 1 ? 'it' : 'them'} still goes to github.com/{name}, which now has the new history.</p>
        : <p>No workspace uses the old copy. It stays on disk, unused.</p>)}
    </div>}
    {step === 'replacing' && <p className="repository-replacement-status" role="status">Copying {name} from GitHub… A large repository takes a while.</p>}
    {step === 'checking' && <p className="repository-replacement-status" role="status">Checking {name} on GitHub…</p>}
    {error && <p className="repository-replacement-error" role="alert">{error}</p>}
    <div className="repository-replacement-actions">
      {step === 'confirm' || step === 'replacing'
        ? <>
          <button type="button" disabled={step === 'replacing'} onClick={() => { setStep('offer'); onCancel?.(); }}>Cancel</button>
          <button type="button" className="primary" ref={confirm} disabled={step === 'replacing'} onClick={() => void replace()}>{step === 'replacing' ? 'Replacing…' : copied ? 'Replace and continue' : 'Copy it and continue'}</button>
        </>
        : <>
          {onCancel && <button type="button" onClick={onCancel}>Cancel</button>}
          <button type="button" className="primary" ref={offer} disabled={step === 'checking'} onClick={() => void check()}>{copied ? 'Replace Lazybox’s copy…' : 'Use the new repository…'}</button>
        </>}
    </div>
  </section>;
}
