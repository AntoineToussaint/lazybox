import { CodeBlock, RichContent } from './RichContent';
import { useEffect, useRef, useState, type FormEvent } from 'react';
import { follow, providerData, type RunnerClient } from '@lazybox/client-core';
import { Decision, type AgentEvent, type CommandStatus } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { readConsoleState, writeConsoleState } from './view-state';

export function RequestCard({ request, api, session, onError }: { request: AgentEvent; api: RunnerClient; session: string; onError: (s: string) => void }) {
  const [answers, setAnswers] = useState<Record<string, string>>({});
  const [waiting, setWaiting] = useState(false);
  const data = providerData(request.providerDataJson);
  const input = data.input || data.arguments || data;
  const description = typeof input.description === 'string' ? input.description : typeof data.reason === 'string' ? data.reason : '';
  const command = typeof input.command === 'string' ? input.command : Array.isArray(input.command) ? input.command.join(' ') : Array.isArray(input.argv) ? input.argv.join(' ') : '';
  const questions: any[] = data.questions || data.input?.questions || [];
  const decide = async (decision: Decision) => {
    setWaiting(true);
    try { await api.apply({ sessionId: session, operationId: crypto.randomUUID(), operation: { case: 'approval', value: { requestId: request.requestId, decision } } }); }
    catch (e) { onError(String(e)); setWaiting(false); }
  };
  const answer = async (event: FormEvent) => {
    event.preventDefault(); setWaiting(true);
    try { await api.apply({ sessionId: session, operationId: crypto.randomUUID(), operation: { case: 'answer', value: { requestId: request.requestId, answers: Object.fromEntries(Object.entries(answers).map(([key, value]) => [key, { values: [value] }])) } } }); }
    catch (e) { onError(String(e)); setWaiting(false); }
  };
  return <article className="request"><div className="eyebrow">{request.type === 'question' ? 'Agent question' : 'Approval required'}</div><h3>{request.text || 'Your response is needed'}</h3>
    {description && <div className="approval-description"><RichContent text={description}/></div>}
    {command ? <CodeBlock text={command} language="bash" label="Command"/> : request.type!=='question' && <CodeBlock text={JSON.stringify(input,null,2)} language="json" label="Requested input"/>}
    <details className="request-metadata"><summary>Request details</summary><CodeBlock text={JSON.stringify(data,null,2)} language="json"/><small className="mono">{request.requestId}</small></details>
    {request.type === 'question' ? <form onSubmit={answer}>{questions.map((q, i) => {
      const key = q.id || q.question; return <label key={key || i}>{q.question || q.header}
        {q.options?.length > 0 && <small>{q.options.map((o: any) => o.label).join(' / ')}</small>}
        <input required value={answers[key] || ''} onChange={e => setAnswers({ ...answers, [key]: e.target.value })}/></label>;
    })}<button disabled={waiting || !questions.length}>Send answer</button></form> : <div className="row"><button disabled={waiting} onClick={() => void decide(Decision.DECLINE)}>Decline</button><button className="primary" disabled={waiting || request.text === 'item/permissions/requestApproval'} onClick={() => void decide(Decision.ACCEPT)}>Allow once</button></div>}
  </article>;
}

export function Console({ api, command, onOpenViewer, viewState, onError }: { api: RunnerClient; command: string; onOpenViewer?: () => void; viewState?: ViewStateStore; onError?: (error: string) => void }) {
  const [text, setText] = useState('');
  const [status, setStatus] = useState('Connecting');
  const [metadata, setMetadata] = useState<CommandStatus>();
  const [followOutput, setFollowOutput] = useState(true);
  const [search, setSearch] = useState('');
  const [offset, setOffset] = useState(0n);
  const pane = useRef<HTMLPreElement>(null);
  const restoreScroll = useRef<number | null>(null);
  const saveView = (search: string, follow: boolean) => {
    try { writeConsoleState(viewState, command, { search, follow, scrollTop: pane.current?.scrollTop || 0 }); }
    catch (error) { onError?.(`Could not save console view: ${String(error)}`); }
  };
  useEffect(() => {
    setText(''); setMetadata(undefined); setOffset(0n);
    try { const saved = readConsoleState(viewState, command); setSearch(saved.search); setFollowOutput(saved.follow); restoreScroll.current = saved.scrollTop; }
    catch (error) { onError?.(`Could not restore console view: ${String(error)}`); }
    const control = new AbortController(); const decoders = new Map<string, TextDecoder>();
    void follow((after, signal) => api.watchOutput({ commandId: command, after }, { signal }), control.signal, event => {
      if (event.data.length) {
        let decoder = decoders.get(event.channel); if (!decoder) { decoder = new TextDecoder(); decoders.set(event.channel, decoder); }
        const chunk = decoder.decode(event.data, { stream: true });
        setText(value => (value + chunk).slice(-524288)); setOffset(event.offset + BigInt(event.data.length));
      }
      if (event.status) setMetadata(event.status);
    }, setStatus);
    return () => control.abort();
  }, [api, command, viewState]);
  useEffect(() => {
    const element = pane.current; if (!element) return;
    if (followOutput) element.scrollTo({ top: element.scrollHeight });
    else if (restoreScroll.current !== null && (element.scrollHeight - element.clientHeight >= restoreScroll.current || status === 'Complete')) {
      element.scrollTo({ top: restoreScroll.current }); restoreScroll.current = null;
    }
  }, [text, followOutput, status]);
  return <section className="console"><div className="panel-title"><div><span className="eyebrow">Shared console</span><h2>{metadata?.title || 'Command output'}</h2></div><span className="pill">{status}</span></div>
    <div className="console-tools"><label><input type="checkbox" checked={followOutput} onChange={e => { setFollowOutput(e.target.checked); restoreScroll.current = null; saveView(search, e.target.checked); }}/> Follow</label><input aria-label="Search output" placeholder="Find in output…" value={search} onChange={e => { setSearch(e.target.value); restoreScroll.current = null; saveView(e.target.value, followOutput); }}/><button onClick={() => navigator.clipboard.writeText(text).catch(error => onError?.(String(error)))}>Copy</button>{onOpenViewer && <button onClick={onOpenViewer}>Open viewer ↗</button>}</div>
    <pre ref={pane} tabIndex={0} className="output" data-testid="console-output" onWheel={() => { restoreScroll.current = null; }} onScroll={() => { if (!followOutput && restoreScroll.current === null) saveView(search, false); }}>{search ? text.split('\n').filter(line => line.toLocaleLowerCase().includes(search.toLocaleLowerCase())).join('\n') : text || 'Waiting for output…'}</pre>
    <footer><span className="mono">{command.slice(0, 12)} · {offset.toString()} bytes</span><span>{metadata?.captureError || (metadata?.done ? metadata.exitCode === undefined ? 'Capture EOF · upstream exit unknown' : `Exit ${metadata.exitCode}${metadata.signal ? ' · ' + metadata.signal : ''}` : 'Running')}</span></footer>
    <small className="retention">Showing up to the last 512 KiB. Full retained output is available through command reads until the configured spool budget is exhausted.</small>
  </section>;
}

