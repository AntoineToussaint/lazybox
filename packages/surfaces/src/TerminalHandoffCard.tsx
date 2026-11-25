import { useEffect, useState } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import { reviewArgument, workspaceFolder } from './handoff-model';
import { Icon } from './Icons';
import { runAction } from './actions';
import { useRunner } from './RunnerContext';
import { requestTerminal } from './TerminalPanel';
import type { TerminalHandoff } from './transcript-model';

export function TerminalHandoffCard({ handoff, messageId }: { handoff: TerminalHandoff; messageId: string }) {
  const { api } = useRunner();
  const [busy, setBusy] = useState(false);
  const [decision, setDecision] = useState('');
  const [terminalId, setTerminalId] = useState('');
  const [error, setError] = useState('');
  // The workspace's root names the folder from it: `lazybox/scripts`, never a bare `scripts`.
  const [root, setRoot] = useState('');
  useEffect(() => {
    setRoot('');
    if (!handoff.cwd || !handoff.workspaceId) return;
    const control = new AbortController();
    api.getCollection({ collectionId: handoff.workspaceId }, { signal: control.signal, timeoutMs: 8000 })
      .then(result => { if (!control.signal.aborted) setRoot(result.collection?.root || ''); }, () => {});
    return () => control.abort();
  }, [api, handoff.cwd, handoff.workspaceId]);
  const recorded = handoff.status === 'pending' ? decision || 'pending' : handoff.status;
  const terminal = handoff.terminalId || terminalId;
  const [ended, setEnded] = useState('');
  const status = terminal && ended === terminal ? 'finished' : recorded;
  useEffect(() => {
    if (!terminal || status !== 'running') return;
    const control = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    async function reconcile() {
      try {
        const list = await api.listTerminals({ collectionId: handoff.workspaceId }, { signal: control.signal });
        const live = list.terminals.find(item => item.id === terminal);
        if (!control.signal.aborted && (!live || live.done)) { setEnded(terminal); return; }
      } catch { /* Reconnect on the next poll. */ }
      if (!control.signal.aborted) timer = setTimeout(reconcile, 5000);
    }
    void reconcile();
    return () => { control.abort(); clearTimeout(timer); };
  }, [api, terminal, status, handoff.workspaceId]);
  function showTerminal(id: string) {
    requestTerminal(handoff.workspaceId, id);
    runAction('terminal.open');
  }
  async function decide(accept: boolean) {
    setBusy(true);
    setError('');
    try {
      const started = await api.decideTerminalHandoff({ handoffId: handoff.id, accept, allowAgentInput: accept && handoff.agentInput });
      if (accept) {
        setDecision('running');
        setTerminalId(started.id);
        showTerminal(started.id);
      } else setDecision('declined');
    } catch (error) {
      const failure = ConnectError.from(error);
      if (failure.code === Code.NotFound) setDecision('unavailable');
      else setError(failure.rawMessage);
    } finally {
      setBusy(false);
    }
  }
  const pending = status === 'pending';
  const command = <><pre className="terminal-handoff-argv"><code>{handoff.argv.map(reviewArgument).join(' ')}</code></pre>
    {handoff.cwd && <details className="terminal-handoff-directory">
      <summary>Working directory · {workspaceFolder(root, handoff.cwd)}</summary>
      <code>{handoff.cwd}</code>
    </details>}</>;
  return <article data-message-id={messageId} className="message terminal-handoff" data-pending={pending || undefined} aria-label="Interactive command">
    <header><Icon name="terminal" size={16}/><strong>{pending ? (handoff.agentInput ? 'Allow the agent to read and type in this terminal?' : 'Review interactive command') :
      status === 'running' ? 'Interactive command running' :
      status === 'finished' ? 'Interactive command finished' :
      status === 'unavailable' ? 'Interactive command request unavailable' :
      status === 'declined' ? 'Interactive command declined' : 'Interactive command request expired'}</strong></header>
    {status === 'unavailable' && <p>Check the terminal and conversation history for the outcome.</p>}
    {status === 'expired' && <p>The command did not run. Ask the agent to request approval again.</p>}
    {pending ? command : <details className="terminal-handoff-details"><summary>View command</summary>{command}</details>}
    {status === 'pending' && <>
      {handoff.agentInput && <p>The agent can read output and type freely until this command exits. In a shell or interpreter it can run additional commands without asking again.</p>}
      <button data-terminal-decision={messageId} className="primary" disabled={busy} onClick={() => void decide(true)}>{handoff.agentInput ? 'Run with agent input' : 'Run and take over'}</button>{' '}
      <button disabled={busy} onClick={() => void decide(false)}>Decline</button>
    </>}
    {terminal && (status === 'running' || status === 'finished') && <button onClick={() => showTerminal(terminal)}>Show terminal</button>}
    {error && <p role="alert">{error}</p>}
  </article>;
}
