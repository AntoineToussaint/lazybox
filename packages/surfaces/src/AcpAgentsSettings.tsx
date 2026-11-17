import { useEffect, useId, useRef, useState, type FormEvent } from 'react';
import type { ProviderStatus } from '@lazybox/protocol';
import { Icon } from './Icons';
import { agentIcon, commandLine, isAcp, parseCommandLine, parseEnvLines } from './providers';
import { useRunner } from './RunnerContext';
import { SettingsGroup, SettingsHeader, settingsError } from './settings-parts';
import { useProviderAvailability } from './useProviderAvailability';
import './acp-agents.css';

type Draft = { id: string; title: string; command: string; env: string; editing: boolean };
const blank: Draft = { id: '', title: '', command: '', env: '', editing: false };

/** "Ready", "Not installed": an agent's state in a word or two. */
function stateLabel(status: ProviderStatus): { text: string; tone: string } {
  if (!status.installed) return { text: 'Not installed', tone: 'muted' };
  if (status.error) return { text: 'Cannot run', tone: 'warn' };
  return { text: 'Ready', tone: 'ok' };
}

/**
 * Settings → Agents: the ACP agents this Runner runs as conversations
 * (ACP.md). Installed ones are found on the Runner's PATH with their
 * version; missing ones say how to install them; the person adds any other
 * ACP agent by its command line, or changes how a built-in one starts.
 * Environment values are written once and never shown again.
 */
/** Asked for from elsewhere ("Add an agent…" in the picker): the section scrolls into view and its add form opens. */
let revealPending = false;
const REVEAL_EVENT = 'lazybox-reveal-acp-agents';
export function revealAcpAgents() { revealPending = true; window.dispatchEvent(new Event(REVEAL_EVENT)); }

export function AcpAgentsSettings() {
  const section = useRef<HTMLElement>(null);
  const { api, refresh: refreshInfo } = useRunner();
  const availability = useProviderAvailability();
  const [report, setReport] = useState(availability.report);
  useEffect(() => { if (availability.report) setReport(availability.report); }, [availability.report]);
  const [draft, setDraft] = useState<Draft | null>(null);
  useEffect(() => {
    const reveal = () => {
      if (!revealPending || !section.current) return;
      revealPending = false;
      section.current.scrollIntoView({ block: 'start', behavior: 'smooth' });
      setDraft(current => current || { ...blank });
    };
    reveal();
    window.addEventListener(REVEAL_EVENT, reveal);
    return () => window.removeEventListener(REVEAL_EVENT, reveal);
  }, []);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [note, setNote] = useState('');
  const [copied, setCopied] = useState('');
  const agents = (report?.providers || []).filter(p => isAcp(p.provider));
  const installed = agents.filter(a => a.installed || a.acp?.custom);
  const missing = agents.filter(a => !a.installed && !a.acp?.custom);
  const apply = (next: typeof report) => { setReport(next); refreshInfo(); };
  const copy = async (text: string) => { try { await navigator.clipboard.writeText(text); setCopied(text); setTimeout(() => setCopied(value => value === text ? '' : value), 1600); } catch { setCopied(''); } };
  const edit = (a: ProviderStatus) => setDraft({ id: a.acp?.id || '', title: a.title, command: commandLine(a), env: (a.acp?.envNames || []).map(n => `${n}=`).join('\n'), editing: true });
  const remove = async (a: ProviderStatus, restore: boolean) => {
    if (!restore && !window.confirm(`Remove ${a.title}? Conversations already running keep it.`)) return;
    setBusy(true); setError(''); setNote('');
    try { apply(await api.removeAcpAgent({ id: a.acp?.id || '' })); setNote(restore ? `${a.title} starts the default way again.` : `${a.title} removed.`); }
    catch (e) { setError(settingsError(e)); } finally { setBusy(false); }
  };
  return <section ref={section} className="acp-agents" aria-labelledby="acp-agents-title">
    <SettingsHeader id="acp-agents-title" title="Agent programs" description={<>Besides Claude and Codex, Lazybox runs any agent that speaks the <a href="https://agentclientprotocol.com" target="_blank" rel="noreferrer">Agent Client Protocol</a> as a conversation: its approvals on cards, its tool calls as lines, its plan, and its commands in the console. <span className="acp-preview-note"><span className="acp-preview">Preview</span> ACP agents are tested against Lazybox’s own fixtures; each real agent may differ.</span></>}
      actions={<button type="button" disabled={availability.checking} onClick={() => void availability.refresh()}><Icon name="refresh" size={13}/>{availability.checking ? 'Checking…' : 'Check again'}</button>}/>
    {!report && !availability.error && <p className="settings-empty" role="status">Looking for agents on the Runner…</p>}
    {availability.error && !report && <p className="settings-error" role="alert">{settingsError(availability.error, 'load')}</p>}
    {report && <SettingsGroup title="On this Runner" description={installed.length ? 'Each runs on its own sign-in: sign in once in your terminal the way that agent does.' : 'No ACP agent is installed on the Runner’s computer yet.'}>
      <ul className="acp-agent-list">
        {installed.map(a => <li key={a.provider} className="acp-agent">
          <span className="acp-agent-icon"><Icon name={agentIcon(a.provider, a)} size={18}/></span>
          <div className="acp-agent-text">
            <div className="acp-agent-name"><strong>{a.title}</strong>{a.version && <small className="mono">{a.version}</small>}{a.acp?.custom && <span className="acp-agent-tag">{a.acp.icon === 'agent' ? 'Added by you' : 'Your command'}</span>}<span className={`acp-agent-state tone-${stateLabel(a).tone}`}>{stateLabel(a).text}</span></div>
            <code className="acp-agent-command" title={a.path || undefined}>{commandLine(a)}</code>
            {a.acp?.envNames.length ? <small>Environment: {a.acp.envNames.join(', ')}</small> : null}
            {a.error && <small className="acp-agent-error">{a.error}</small>}
            {!a.error && a.acp?.launchProblem && <small className="acp-agent-error" role="note"><Icon name="alert" size={11}/> {a.acp.launchProblem}</small>}
          </div>
          <div className="acp-agent-actions">
            <button type="button" disabled={busy} onClick={() => edit(a)} aria-label={`Change how ${a.title} starts`}><Icon name="pencil" size={13}/>Edit</button>
            {a.acp?.custom && <button type="button" disabled={busy} onClick={() => void remove(a, a.acp?.icon !== 'agent')}>{a.acp?.icon !== 'agent' ? 'Restore default' : 'Remove'}</button>}
          </div>
        </li>)}
      </ul>
      {!draft && <button type="button" className="acp-agent-add" onClick={() => { setDraft({ ...blank }); setNote(''); setError(''); }}><Icon name="plus" size={13}/>Add an ACP agent…</button>}
      {draft && <AgentForm draft={draft} busy={busy} onCancel={() => setDraft(null)} onSave={async next => {
        setBusy(true); setError(''); setNote('');
        try {
          const { command, args } = parseCommandLine(next.command);
          const env = parseEnvLines(next.env);
          if (env.error) throw new Error(env.error);
          apply(await api.saveAcpAgent({ id: next.id, title: next.title, command, args, env: env.entries }));
          setDraft(null); setNote(`${next.title} saved. Pick it in a new conversation.`);
        } catch (e) { setError(e instanceof Error && !('code' in e) ? e.message : settingsError(e)); } finally { setBusy(false); }
      }}/>}
      {error && <p className="settings-inline-error" role="alert">{error}</p>}
      {note && <p className="settings-saved" role="status"><Icon name="check" size={14}/>{note}</p>}
    </SettingsGroup>}
    {missing.length > 0 && <SettingsGroup title="Not installed" description="Install one on the Runner’s computer, then Check again. It appears in New conversation.">
      <ul className="acp-agent-list is-missing">
        {missing.map(a => <li key={a.provider} className="acp-agent">
          <span className="acp-agent-icon"><Icon name={agentIcon(a.provider, a)} size={18}/></span>
          <div className="acp-agent-text">
            <div className="acp-agent-name"><strong>{a.title}</strong>{a.acp?.homepage && <a href={a.acp.homepage} target="_blank" rel="noreferrer" className="acp-agent-link">About</a>}</div>
            {a.installCommand && <code className="acp-agent-command">{a.installCommand}</code>}
          </div>
          <div className="acp-agent-actions">
            {a.installCommand && <button type="button" onClick={() => void copy(a.installCommand)} aria-label={`Copy the command that installs ${a.title}`}><Icon name={copied === a.installCommand ? 'check' : 'copy'} size={13}/>{copied === a.installCommand ? 'Copied' : 'Copy'}</button>}
          </div>
        </li>)}
      </ul>
    </SettingsGroup>}
  </section>;
}

/** Adding an agent, or changing how one starts: its name, command line and environment. */
function AgentForm({ draft, busy, onCancel, onSave }: { draft: Draft; busy: boolean; onCancel(): void; onSave(draft: Draft): void }) {
  const [value, setValue] = useState(draft);
  const id = useId();
  const first = useRef<HTMLInputElement>(null);
  useEffect(() => { first.current?.focus(); }, []);
  const parsed = parseCommandLine(value.command);
  const env = parseEnvLines(value.env);
  const valid = !!value.title.trim() && !!parsed.command && !env.error;
  const submit = (event: FormEvent) => { event.preventDefault(); if (valid && !busy) onSave(value); };
  return <form className="acp-agent-form" onSubmit={submit} onKeyDown={event => { if (event.key === 'Escape') { event.stopPropagation(); onCancel(); } }} aria-label={draft.editing ? `Change ${draft.title}` : 'Add an ACP agent'}>
    <h4>{draft.editing ? `How ${draft.title} starts` : 'Add an ACP agent'}</h4>
    <div className="acp-field"><label htmlFor={`${id}-title`}>Name</label><input ref={first} id={`${id}-title`} value={value.title} maxLength={64} disabled={busy} placeholder="My agent" onChange={e => setValue({ ...value, title: e.target.value })}/></div>
    <div className="acp-field"><label htmlFor={`${id}-command`}>Command line</label><input id={`${id}-command`} className="mono" value={value.command} disabled={busy} spellCheck={false} autoComplete="off" placeholder="npx @agentclientprotocol/claude-agent-acp" aria-describedby={`${id}-command-hint`} onChange={e => setValue({ ...value, command: e.target.value })}/>
      <small id={`${id}-command-hint`}>What starts the agent in ACP mode, often with <code>--acp</code> or <code>acp</code>. A program on the Runner’s PATH, or an absolute path.</small></div>
    <div className="acp-field"><label htmlFor={`${id}-env`}>Environment</label><textarea id={`${id}-env`} className="mono" rows={3} value={value.env} disabled={busy} spellCheck={false} placeholder="API_KEY=…" aria-describedby={`${id}-env-hint`} onChange={e => setValue({ ...value, env: e.target.value })}/>
      <small id={`${id}-env-hint`}>{draft.editing && draft.env ? 'One NAME=value per line. Leave a value empty to keep the saved one; remove a line to drop it.' : 'Optional. One NAME=value per line. Values stay on the Runner and are never shown again.'}</small></div>
    {env.error && <p className="settings-inline-error" role="alert">{env.error}</p>}
    <div className="acp-agent-form-actions">
      <button type="button" onClick={onCancel} disabled={busy}>Cancel</button>
      <button className="primary" disabled={!valid || busy}>{busy ? 'Saving…' : draft.editing ? 'Save' : 'Add agent'}</button>
    </div>
  </form>;
}
