import { useEffect, useLayoutEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { subscriptionGroups } from './subscription-groups';
import { createPortal } from 'react-dom';
import { ConnectError } from '@lazybox/client-core';
import type { Mission, Session } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { useMissions } from './useMissions';
import { runAction } from './actions';
import { selectMission } from './navigate-requests';
import { useOpenConversation } from './AgentMessages';
import { budgetLine, ceilingLine, childrenOf, childState, defaultGrant, grantOf, grantProblem, permissionChoices, permissionLabel, profileLabel, usd, type GrantDraft } from './orchestration-model';
import { ReviewPlanButton } from './PlanSheet';
import './orchestration.css';

const message = (error: unknown) => ConnectError.from(error).rawMessage || String(error);
const ORCHESTRATE = 'lazybox-orchestrate';

/** Ask a conversation's header to open its "Let it run agents" sheet (the palette, `/orchestrate`). */
export function requestOrchestrate(sessionId: string) {
  window.dispatchEvent(new CustomEvent<string>(ORCHESTRATE, { detail: sessionId }));
}

/**
 * Where a conversation sits in an agent tree, in its header: the Orchestrator
 * badge (its grant, its agents, Revoke) when the person let it run agents, a
 * crumb to its parent when an orchestrator started it, and nothing otherwise.
 * It also hosts the grant sheet the palette and `/orchestrate` open.
 */
export function OrchestratorBadge({ session, sessionID }: { session?: Session; sessionID: string }) {
  const [granting, setGranting] = useState(false);
  useEffect(() => {
    const listener = (event: Event) => { if ((event as CustomEvent<string>).detail === sessionID) setGranting(true); };
    window.addEventListener(ORCHESTRATE, listener);
    return () => window.removeEventListener(ORCHESTRATE, listener);
  }, [sessionID]);
  const lineage = session?.lineage;
  const open = useOpenConversation();
  return <>
    {lineage?.grantId && session && <OrchestratorChip session={session}/>}
    {lineage?.parentConversationId && <button type="button" className="lineage-crumb" title={`Started by the orchestrator “${lineage.parentTitle || 'Orchestrator'}” for the mission “${lineage.missionTitle}”${lineage.role ? ` as its ${lineage.role}` : ''}. Its approvals and questions still come to you.`}
      aria-label={`Started by ${lineage.parentTitle || 'an orchestrator'}: open it`} onClick={() => open(lineage.parentConversationId)}>
      <Icon name="subagent" size={11}/><span>{lineage.parentTitle || 'Orchestrator'}</span>{lineage.role && <small>{lineage.role}</small>}
    </button>}
    {granting && session && <GrantDialog session={session} onClose={() => setGranting(false)}/>}
  </>;
}

/** The badge of a conversation that holds a grant, with its card. */
function OrchestratorChip({ session }: { session: Session }) {
  const missions = useMissions();
  const conversation = session.conversationId || session.id;
  const held = grantOf(missions.missions, conversation);
  const lineage = session.lineage!;
  const live = held ? held.grant.liveChildren : lineage.liveChildren, max = held?.grant.ceiling?.maxChildren ?? lineage.maxChildren;
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null), cardRef = useRef<HTMLDivElement>(null);
  // The card sits under the badge, above everything: the header clips what overflows it.
  const [at, setAt] = useState({ left: 0, top: 0 });
  useLayoutEffect(() => {
    if (!open || !ref.current) return;
    const place = () => { const r = ref.current!.getBoundingClientRect(); setAt({ left: Math.max(8, Math.min(r.left, window.innerWidth - 348)), top: r.bottom + 6 }); };
    place(); window.addEventListener('resize', place);
    return () => window.removeEventListener('resize', place);
  }, [open]);
  useEffect(() => {
    if (!open) return;
    const close = (event: PointerEvent) => { if (!ref.current?.contains(event.target as Node) && !cardRef.current?.contains(event.target as Node)) setOpen(false); };
    const key = (event: KeyboardEvent) => { if (event.key === 'Escape') setOpen(false); };
    window.addEventListener('pointerdown', close); window.addEventListener('keydown', key);
    return () => { window.removeEventListener('pointerdown', close); window.removeEventListener('keydown', key); };
  }, [open]);
  return <span className="orchestrator-chip-wrap" ref={ref}>
    <button type="button" className="orchestrator-chip" aria-expanded={open} aria-haspopup="dialog"
      aria-label={`Orchestrator of “${lineage.missionTitle}”: ${live} of ${max} agents running`} title={`This conversation may start and drive agents for “${lineage.missionTitle}”. Click for its grant and agents.`}
      onClick={() => setOpen(v => !v)}>
      <Icon name="subagent" size={11}/><span>Orchestrator</span><b>{live}/{max}</b>
    </button>
    {open && createPortal(<div className="workspace-shell orchestrator-layer"><OrchestratorCard cardRef={cardRef} at={at} mission={held?.mission} conversation={conversation} parent={{ collectionId: session.collectionId, sessionId: session.id }} missionTitle={lineage.missionTitle} grantId={lineage.grantId} onClose={() => { setOpen(false); ref.current?.querySelector('button')?.focus(); }}/></div>, document.body)}
  </span>;
}

function OrchestratorCard({ mission, conversation, parent, missionTitle, grantId, onClose, cardRef, at }: { mission?: Mission; conversation: string; /** The orchestrator's own conversation: its agents open as tabs next to it. */ parent: { collectionId: string; sessionId: string }; missionTitle: string; grantId: string; onClose(): void; cardRef: React.RefObject<HTMLDivElement | null>; at: { left: number; top: number } }) {
  const missions = useMissions();
  const { info } = useRunner();
  const open = useOpenConversation();
  const grant = mission?.grants.find(g => g.id === grantId);
  const children = childrenOf(mission, conversation);
  const [confirm, setConfirm] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState('');
  const revoke = async () => {
    if (!mission) return;
    setBusy(true); setError('');
    try { await missions.update(mission.id, { case: 'revokeGrant', value: { grantId, reason: '' } }); onClose(); }
    catch (e) { setError(message(e)); setBusy(false); }
  };
  useEffect(() => { cardRef.current?.querySelector<HTMLElement>('.orchestrator-children button, footer button')?.focus(); }, []);
  return <div className="orchestrator-card" role="dialog" aria-label="Orchestrator grant" ref={cardRef} style={{ left: at.left, top: at.top }}>
    <header>
      <strong>Runs agents for</strong>
      <button type="button" className="link-button" onClick={() => { selectMission(mission?.id || ''); runAction('missions.page'); onClose(); }}>{missionTitle}</button>
    </header>
    {grant && <p className="orchestrator-ceiling">{ceilingLine(grant)}</p>}
    {grant && (grant.spentUsd > 0 || (grant.ceiling?.budgetUsd || 0) > 0) && <p className={`orchestrator-budget${grant.budgetState ? ' budget-' + grant.budgetState : ''}`}>{budgetLine(grant)}{grant.budgetState === 'exhausted' ? ' · new agents paused' : ''}</p>}
    {mission && mission.plans.filter(p => p.state === 'proposed' && p.grantId === grantId).map(p => <div key={p.id} className="orchestrator-plan"><Icon name="checklist" size={12}/><span>Plan waiting: {p.tasks.length} tasks · est. {usd(p.totalUsd)}</span><ReviewPlanButton mission={mission} plan={p} label="Review…"/></div>)}
    {grant && <p className="orchestrator-subs">On {grant.ceiling?.profileIds.map(id => profileLabel(info?.profiles || [], id)).join(', ')}</p>}
    <p className="orchestrator-never"><Icon name="shield" size={11}/> It never approves, answers for you, passes a gate or accepts a task: those stay yours.</p>
    <h4>Its agents <span>{children.length}</span></h4>
    {!children.length && <p className="orchestrator-empty">None yet. It starts them with <code>lazybox agents spawn</code>.</p>}
    <ul className="orchestrator-children">{children.map(({ task, attempt }) => {
      const state = childState(task, attempt);
      return <li key={attempt.id}>
        <button type="button" onClick={() => { open(attempt.conversationId, attempt.sessionId, parent); onClose(); }} title="Open its conversation in a tab next to this one">
          <span className={`child-dot tone-${state.tone}`} aria-hidden="true"/>
          <span className="child-title">{attempt.title || task.title}</span>
          <small>{attempt.role || 'worker'} · {permissionLabel[attempt.permission] || attempt.permission}</small>
          <span className={`child-state tone-${state.tone}`}>{state.label}</span>
        </button>
      </li>;
    })}</ul>
    {error && <p className="missions-error" role="alert">{error}</p>}
    <footer>
      {confirm
        ? <><span>Its agents keep running; stop them from the board.</span><button type="button" onClick={() => setConfirm(false)} disabled={busy}>Keep</button><button type="button" className="danger" autoFocus disabled={busy} onClick={() => void revoke()}>{busy ? 'Revoking…' : 'Revoke'}</button></>
        : <button type="button" className="danger-quiet" onClick={() => setConfirm(true)}>Revoke grant…</button>}
    </footer>
  </div>;
}

/**
 * The person lets one conversation run agents for one mission: which
 * mission, and the most its agents may get. Every field defaults to the
 * conversation's own and only narrows; nothing here lets it approve, answer
 * or accept anything.
 */
export function GrantDialog({ session: given, onClose, missionId, candidates }: { session?: Session; onClose(): void; missionId?: string; /** Without a session: the conversations to choose from. */ candidates?: readonly Session[] }) {
  const { info } = useRunner();
  const [picked, setPicked] = useState(() => given?.id || candidates?.[0]?.id || '');
  const session = given || candidates?.find(s => s.id === picked) || candidates?.[0];
  const missions = useMissions();
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  const open = useMemo(() => missions.missions.filter(m => !m.canceled), [missions.missions]);
  const [choice, setChoice] = useState<string>(() => missionId || '');
  const chosen = choice || open[0]?.id || 'new';
  const [title, setTitle] = useState('');
  const [draft, setDraft] = useState<GrantDraft>(() => defaultGrant(session));
  useEffect(() => { if (!given && session) setDraft(defaultGrant(session)); }, [given, session?.id]);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const profiles = subscriptionGroups((info?.profiles || []).filter(p => p.provider === 'claude' || p.provider === 'codex'), { currentId: session?.profileId }).flatMap(group => group.items);
  const choices = permissionChoices(session?.permission);
  const problem = (!session ? 'Choose the conversation that orchestrates' : undefined) || grantProblem(draft) || (chosen === 'new' && !title.trim() ? 'Name the mission: the outcome its agents work toward' : undefined);
  const agentName = !session ? 'a conversation' : session.provider === 'codex' ? 'Codex' : 'Claude';
  const submit = async (event: FormEvent) => {
    event.preventDefault(); if (problem || busy || !session) return;
    setBusy(true); setError('');
    try {
      const mission = chosen === 'new' ? await missions.create(title.trim(), '') : open.find(m => m.id === chosen)!;
      if (chosen === 'new') setChoice(mission.id);
      await missions.update(mission.id, { case: 'grant', value: { sessionId: session.id, ceiling: { permission: draft.permission, profileIds: draft.profileIds, maxChildren: draft.maxChildren, depth: 1, budgetUsd: draft.budgetUsd, planRequired: draft.planRequired } } });
      onClose();
    } catch (e) { setError(message(e)); setBusy(false); }
  };
  const toggle = (id: string) => setDraft(d => ({ ...d, profileIds: d.profileIds.includes(id) ? d.profileIds.filter(p => p !== id) : [...d.profileIds, id] }));
  return <DraggableDialog ref={dialog} className="mission-dialog grant-dialog" aria-label="Let this conversation run agents" onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader><h2><Icon name="subagent" size={15}/> Let {agentName} run agents</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}>×</button></DialogHeader>
    <form onSubmit={submit}>
      {!given && <label>Conversation<select aria-label="Conversation" value={session?.id || ''} onChange={e => setPicked(e.target.value)}>{(candidates || []).map(s => <option key={s.id} value={s.id}>{s.title || 'Untitled conversation'}{s.lineage?.grantId ? ' · already an orchestrator' : ''}</option>)}</select></label>}
      <p className="mission-muted">This conversation becomes an <b>orchestrator</b>: it may start agents for one mission, message them, wait on them and stop them. Each one is a task on the mission's board.</p>
      <fieldset className="grant-missions"><legend>Mission</legend>
        {open.slice(0, 6).map(m => <label key={m.id} className={chosen === m.id ? 'chosen' : ''}><input type="radio" name="grant-mission" checked={chosen === m.id} onChange={() => setChoice(m.id)}/><span>{m.title}</span><small>{m.tasks.length} tasks</small></label>)}
        <label className={chosen === 'new' ? 'chosen grant-new' : 'grant-new'}><input type="radio" name="grant-mission" checked={chosen === 'new'} onChange={() => setChoice('new')}/>
          <input aria-label="New mission" placeholder="New mission: what should be true when it is done?" value={title} maxLength={200} onFocus={() => setChoice('new')} onChange={e => setTitle(e.target.value)}/></label>
      </fieldset>
      <fieldset className="grant-ceiling"><legend>Its agents get at most</legend>
        <div className="grant-row" role="radiogroup" aria-label="Permission">
          <span>Permission</span>
          {choices.map(c => <label key={c.mode} title={c.disabled} className={c.disabled ? 'disabled' : ''}><input type="radio" name="grant-permission" disabled={!!c.disabled} checked={draft.permission === c.mode} onChange={() => setDraft(d => ({ ...d, permission: c.mode }))}/>{permissionLabel[c.mode]}</label>)}
        </div>
        <div className="grant-row">
          <span>Subscriptions</span>
          {profiles.map(p => <label key={p.id}><input type="checkbox" checked={draft.profileIds.includes(p.id)} onChange={() => toggle(p.id)}/>{p.provider === 'codex' ? 'Codex' : 'Claude'}{p.label ? ` · ${p.label}` : p.id === session?.profileId ? ' · this one' : ''}</label>)}
        </div>
        <div className="grant-row">
          <span>Limits</span>
          <label className="grant-number">At once<input type="number" min={1} max={16} value={draft.maxChildren} aria-label="Agents at once" onChange={e => setDraft(d => ({ ...d, maxChildren: Math.max(1, Math.min(16, Number(e.target.value) || 1)) }))}/></label>
          <label className="grant-number">Budget $<input type="number" min={0} step={1} value={draft.budgetUsd || ''} placeholder="no cap" aria-label="Budget in dollars" onChange={e => setDraft(d => ({ ...d, budgetUsd: Math.max(0, Number(e.target.value) || 0) }))}/></label>
          {draft.budgetUsd > 0 && <small>{usd(draft.budgetUsd)} for the whole tree</small>}
        </div>
        <div className="grant-row">
          <span>Plan</span>
          <label><input type="checkbox" checked={draft.planRequired} onChange={e => setDraft(d => ({ ...d, planRequired: e.target.checked }))}/>Approve its plan first <small>before any agent starts</small></label>
        </div>
      </fieldset>
      <ul className="grant-rules">
        <li><Icon name="check" size={11}/> Starts and drives only the agents it started</li>
        <li><Icon name="check" size={11}/> Their approvals and questions come to you, labelled</li>
        <li><Icon name="cross" size={11}/> Never approves, answers for you, passes a gate or accepts a task</li>
        <li><Icon name="cross" size={11}/> Never gives an agent more than you allow here</li>
      </ul>
      {error && <p className="missions-error" role="alert">{error}</p>}
      <footer>
        {problem && <small className="grant-problem">{problem}</small>}
        <button type="button" disabled={busy} onClick={onClose}>Cancel</button>
        <button className="primary" disabled={!!problem || busy}>{busy ? 'Granting…' : 'Let it run agents'}</button>
      </footer>
    </form>
  </DraggableDialog>;
}
