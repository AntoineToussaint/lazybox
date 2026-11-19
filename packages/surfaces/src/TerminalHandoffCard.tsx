import { useState } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import { runAction } from './actions';
import { useRunner } from './RunnerContext';
import { requestTerminal } from './TerminalPanel';
import type { TerminalHandoff } from './transcript-model';

export function TerminalHandoffCard({ handoff }: { handoff: TerminalHandoff }) {
  const { api } = useRunner();
  const [busy, setBusy] = useState(false);
  const [decision, setDecision] = useState('');
  const [terminalId, setTerminalId] = useState('');
  const [error, setError] = useState('');
  const status = handoff.status === 'pending' ? decision || 'pending' : handoff.status;
  const terminal = handoff.terminalId || terminalId;
  function showTerminal(id: string) {
    requestTerminal(handoff.workspaceId, id);
    runAction('terminal.open');
  }
  async function decide(accept: boolean) {
    setBusy(true);
    setError('');
    try {
      const started = await api.decideTerminalHandoff({ handoffId: handoff.id, accept });
      if (accept) {
        setDecision('running');
        setTerminalId(started.id);
        showTerminal(started.id);
      } else setDecision('declined');
    } catch (error) {
      const failure = ConnectError.from(error);
      if (failure.code === Code.NotFound) setDecision('expired');
      else setError(failure.rawMessage);
    } finally {
      setBusy(false);
    }
  }
  return <article className="message" aria-label="Interactive command">
    <strong>{status === 'pending' ? 'The agent needs you at the terminal' :
      status === 'running' ? 'Interactive command running' :
      status === 'finished' ? 'Interactive command finished' :
      status === 'declined' ? 'Interactive command declined' : 'Interactive command request expired'}</strong>
    <pre className="terminal-handoff-argv"><code>{JSON.stringify(handoff.argv)}</code></pre>
    <p>{handoff.cwd}</p>
    {status === 'pending' && <>
      <button className="primary" disabled={busy} onClick={() => void decide(true)}>Run and take over</button>{' '}
      <button disabled={busy} onClick={() => void decide(false)}>Decline</button>
    </>}
    {terminal && <button onClick={() => showTerminal(terminal)}>Show terminal</button>}
    {error && <p role="alert">{error}</p>}
  </article>;
}
