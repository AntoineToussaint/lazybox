import { useEffect, useMemo, useRef, useState, type FormEvent, type KeyboardEvent } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { ActionMenu } from './ActionMenu';
import { ChatPanel } from './AgentWindow';
import { Icon } from './Icons';
import { actionEnabled, runAction } from './actions';
import { helpChoices, pickHelp, providerNeeds, reportFromProfiles, usingLine, type HelpChoice } from './help-availability';
import { openDocs } from './Onboarding';
import { useRunner } from './RunnerContext';
import { subscriptionName } from './subscription-model';
import { useProviderAvailability } from './useProviderAvailability';
import { readDraft, readLaunchChoices, writeDraft } from './view-state';
import './help-assistant.css';

/** Where the documentation explains what each agent needs (docs/troubleshooting.md). */
export const NO_AGENT_DOC = 'troubleshooting.md#ask-lazybox-says-no-agent-is-ready';

const suggestions = [
  'How do I add a second subscription?',
  'What are the keys I should know?',
  'How do I run my app from a workspace?',
  'Make new coding sessions ask before running tools',
];

// A preference is a convenience: an unavailable store never breaks the panel.
const get = (store: ViewStateStore | undefined, key: string) => { try { return store?.getItem(key) || ''; } catch { return ''; } };
const set = (store: ViewStateStore | undefined, key: string, value: string) => { try { store?.setItem(key, value); } catch { /* kept for this view */ } };

/**
 * Ask Lazybox, the help assistant. It picks the agent by itself from what the
 * Runner reports (the person's usual provider first, never a subscription at
 * a limit or signed out, else the other provider), says which in its header
 * with a menu to switch, and starts on the first question: no form to fill.
 * With nothing usable it says what each provider needs and how to fix it.
 * The assistant answers from the documentation and proposes settings changes
 * the person approves (SettingsProposal.tsx); it never runs commands.
 */
export function HelpAssistant({ question, setQuestion, workspaceId, workspaceName, viewState, sessionKey, focusToken, onPurpose }: {
  /** The workspace the panel is in: its usage is attributed there, and its name told to the assistant. */
  question?: string;
  setQuestion(value?: string): void;
  workspaceId: string;
  workspaceName?: string;
  viewState?: ViewStateStore;
  /** Where the panel keeps its conversation (shared with the other assistant purposes). */
  sessionKey: string;
  /** Changes when Ask Lazybox is asked for: the message box takes focus. */
  focusToken: number;
  /** Switches the panel to a planning or summary conversation. */
  onPurpose(purpose: 'planning' | 'summary'): void;
}) {
  const { api, info, refresh: refreshRunner } = useRunner();
  const availability = useProviderAvailability();
  const [sessionId, setSessionId] = useState(() => get(viewState, sessionKey));
  const [chosen, setChosen] = useState(() => get(viewState, `${sessionKey}:help-profile`));
  const [draft, setDraft] = useState(() => readDraft(viewState, 'help-start').text);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState('');
  const root = useRef<HTMLElement>(null);
  const preferred = readLaunchChoices(viewState).provider || 'claude';
  // A Runner older than the availability report: its profiles stand in.
  const report = availability.report || (availability.error && info ? reportFromProfiles(info.profiles) : undefined);
  const choices = useMemo(() => helpChoices(report, preferred), [report, preferred]);
  const pick = pickHelp(report, preferred, chosen);
  const session = info?.sessions.find(s => s.id === sessionId);
  // The header names what the conversation runs on, once there is one.
  const current: Pick<HelpChoice, 'agent' | 'name'> | undefined = session
    ? choices.find(c => c.profileId === session.profileId) || { agent: info?.profiles.find(p => p.id === session.profileId)?.provider === 'codex' ? 'Codex' : 'Claude', name: subscriptionName(info?.profiles.find(p => p.id === session.profileId), session.profileId) }
    : pick.kind === 'ready' ? pick.choice : undefined;

  const adopt = (id: string) => { setSessionId(id); set(viewState, sessionKey, id); };
  // The message box, or with nothing ready the first step that fixes it.
  const focus = () => requestAnimationFrame(() => root.current?.querySelector<HTMLElement>('textarea:not(:disabled), .help-empty button:not(:disabled)')?.focus({ preventScroll: true }));
  useEffect(() => { if (focusToken) focus(); }, [focusToken]);
  const choose = (profileId: string) => {
    setChosen(profileId); set(viewState, `${sessionKey}:help-profile`, profileId);
    // Another agent answers in a new conversation; the current one stays in history.
    if (sessionId && session?.profileId !== profileId) adopt('');
    focus();
  };
  const saveDraft = (text: string) => { setDraft(text); try { writeDraft(viewState, 'help-start', { text, files: [], operationId: '' }); } catch { /* kept while open */ } };

  const ask = async (text: string, coaching = false) => {
    const question = text.trim();
    if (!question || starting || pick.kind !== 'ready') return;
    setStarting(true); setError('');
    try {
      const opened = await api.openAssistant({ profileId: pick.choice.profileId, purpose: 'help', context: workspaceName ? `The person has the workspace "${workspaceName}" open in Lazybox.` : '', collectionId: workspaceId, options: { model: '', permission: '', workflow: 'execute', gatewayId: '', rotationProfileIds: [], noRotation: false, instructions: '' } });
      const id = opened.session?.id;
      if (!id) throw new Error('The Runner did not return the conversation.');
      try {
        await api.apply({ sessionId: id, operationId: crypto.randomUUID(), operation: { case: 'message', value: { text: question, attachments: [] } } }, { signal: AbortSignal.timeout(30_000) });
      } catch (e) {
        // The conversation is open: the question waits in its message box.
        try { writeDraft(viewState, id, { text: question, files: [], operationId: '' }); } catch { /* shown below */ }
        setError(`Your question is still in the message box; send it again. ${String(e)}`);
      }
      if (coaching) setQuestion(undefined); else saveDraft('');
      adopt(id); refreshRunner();
      focus();
    } catch (e) {
      setError(`${pick.choice.agent} could not start: ${String(e).replace(/^(Connect)?Error:\s*/, '')}`);
    } finally { setStarting(false); }
  };
  const submit = (event: FormEvent) => { event.preventDefault(); void ask(draft); };
  const onKey = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); void ask(draft); }
  };

  const header = <header className="help-assistant-header">
    <span className="help-assistant-title"><Icon name="ask" size={14}/>Help</span>
    <span className="help-assistant-dot" aria-hidden="true">·</span>
    {/* What answers is also the switch: one click shows every subscription and its state. */}
    <ActionMenu label="Choose the agent for Ask Lazybox" className="help-assistant-switch" summary={<>
      <span className="help-assistant-using" aria-live="polite">{current ? usingLine(current) : pick.kind === 'loading' ? 'checking agents…' : 'no agent ready'}</span>
      <Icon name="chevron" size={12}/>
    </>}>
      {choices.length === 0 && <button disabled>No subscriptions on this Runner</button>}
      {choices.map(choice => {
        const active = choice.profileId === (session?.profileId || (pick.kind === 'ready' ? pick.choice.profileId : ''));
        return <button key={choice.profileId} aria-current={active ? 'true' : undefined} className={choice.usable ? '' : 'help-choice-unready'} onClick={() => choose(choice.profileId)}>
          <Icon name={active ? 'check' : 'chat'} size={13}/><span><strong>{choice.agent} · {choice.name}</strong><small>{choice.detail}</small></span>
        </button>;
      })}
      <hr/>
      <button onClick={() => void availability.refresh()}><Icon name="refresh" size={13}/><span>Check again</span></button>
      {actionEnabled('settings.open') && <button onClick={() => runAction('settings.open', 'subscriptions')}><Icon name="card" size={13}/><span>Manage subscriptions…</span></button>}
      <button onClick={() => onPurpose('planning')}><Icon name="target" size={13}/><span>Discuss a plan instead</span></button>
      <button onClick={() => onPurpose('summary')}><Icon name="notes" size={13}/><span>Summarize evidence instead</span></button>
    </ActionMenu>
    {sessionId && <button className="help-assistant-icon" aria-label="New help conversation" title="New help conversation" onClick={() => { adopt(''); focus(); }}><Icon name="plus" size={14}/></button>}
    <button className="help-assistant-icon" aria-label="Open the documentation" title="Documentation" onClick={() => openDocs()}><Icon name="notes" size={14}/></button>
  </header>;

  return <section ref={root} className="help-assistant">
    {header}
    {question !== undefined && <form className="training-wheels" onSubmit={event => { event.preventDefault(); void ask(question, true); }}>
      <label>Question from Training Wheels<textarea value={question} disabled={starting} onChange={event => setQuestion(event.target.value)}/></label>
      <button disabled={starting || pick.kind !== 'ready' || !question.trim()}>Ask in a new conversation</button>
      <button type="button" onClick={() => setQuestion(undefined)}>Dismiss</button>
    </form>}
    {error && <div className="error help-assistant-error" role="alert">{error}<button onClick={() => setError('')}>Dismiss</button></div>}
    {sessionId
      ? <ChatPanel key={sessionId} collectionId={workspaceId} purpose="help" embedded showCommands={false} initialSessionId={sessionId} viewState={viewState}
        onSessionChange={id => { adopt(id); if (!id) focus(); }}/>
      : pick.kind === 'none'
        ? <NoAgent report={report} checking={availability.checking} onRefresh={() => void availability.refresh()}/>
        : <div className="help-start">
          <div className="help-start-intro">
            <h3>Ask anything about Lazybox</h3>
            <p>Answers come from the documentation. A change to your settings is shown to you first, and applies only if you allow it.</p>
            {pick.kind === 'ready' && pick.fallback && <p className="help-assistant-fallback" role="status"><Icon name="alert" size={12}/>{preferred === 'codex' ? 'Codex' : 'Claude'} is not ready, so {pick.choice.agent} answers.</p>}
          </div>
          <ul className="help-suggestions" aria-label="Suggested questions">
            {suggestions.map(text => <li key={text}><button type="button" disabled={starting || pick.kind !== 'ready'} onClick={() => void ask(text)}>{text}</button></li>)}
          </ul>
          <form className="help-ask" onSubmit={submit}>
            <textarea aria-label="Ask Lazybox" rows={3} maxLength={32768} value={draft} disabled={starting} placeholder={pick.kind === 'ready' ? `Ask ${pick.choice.agent} about Lazybox…  (Enter to send)` : 'Checking which agents are ready…'}
              onChange={e => saveDraft(e.target.value)} onKeyDown={onKey}/>
            <button className="primary" disabled={starting || pick.kind !== 'ready' || !draft.trim()}>{starting ? 'Asking…' : 'Ask'}<Icon name="send" size={13}/></button>
          </form>
        </div>}
  </section>;
}

/**
 * Nothing can answer: what each provider needs, with the step that fixes it.
 * Never a raw error, and the documentation is always one click away.
 */
function NoAgent({ report, checking, onRefresh }: { report: ReturnType<typeof reportFromProfiles> | undefined; checking: boolean; onRefresh(): void }) {
  const needs = providerNeeds(report);
  const [copied, setCopied] = useState('');
  const copy = async (provider: string, command: string) => {
    try { await navigator.clipboard.writeText(command); setCopied(provider); setTimeout(() => setCopied(''), 2000); }
    catch { setCopied(''); }
  };
  const canOpenSettings = actionEnabled('settings.open');
  return <section className="help-empty" aria-labelledby="help-empty-title">
    <span className="help-empty-icon" aria-hidden="true"><Icon name="ask" size={18}/></span>
    <h3 id="help-empty-title">No agent is ready to answer</h3>
    <p>Ask Lazybox answers with Claude Code or Codex, on your own subscription. Here is what each one needs:</p>
    <ul className="help-needs">
      {needs.map(need => <li key={need.provider} aria-label={need.title}>
        <div className="help-need-head"><strong>{need.title}</strong><span className={`help-need-state ${need.ready ? 'ready' : 'missing'}`}>{need.headline}</span></div>
        {need.detail && <p>{need.detail}</p>}
        {need.action === 'install' && need.command && <div className="help-command">
          <code>{need.command}</code>
          <button type="button" aria-label={`Copy the command that installs ${need.title}`} onClick={() => void copy(need.provider, need.command)}><Icon name={copied === need.provider ? 'check' : 'copy'} size={13}/>{copied === need.provider ? 'Copied' : 'Copy'}</button>
        </div>}
        <div className="help-need-actions">
          {need.action === 'install' && <button type="button" onClick={() => openDocs(NO_AGENT_DOC)}>How to install {need.title}</button>}
          {need.action === 'subscription' && (canOpenSettings
            ? <button type="button" onClick={() => runAction('settings.open', 'subscriptions')}>Add a {need.provider === 'codex' ? 'ChatGPT' : 'Claude'} subscription…</button>
            : <small>Add a subscription in Settings → Subscriptions, in the main window.</small>)}
        </div>
      </li>)}
    </ul>
    <footer>
      <button type="button" className="onboarding-docs" onClick={() => openDocs()}>Read the docs <Icon name="notes" size={12}/></button>
      <button type="button" onClick={onRefresh} disabled={checking}><Icon name="refresh" size={13}/>{checking ? 'Checking…' : 'Check again'}</button>
    </footer>
  </section>;
}
