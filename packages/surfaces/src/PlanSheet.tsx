import { useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { ConnectError } from '@lazybox/client-core';
import type { Mission, MissionPlan } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { useMissions } from './useMissions';
import { defaultCap, overlapsOf, permissionLabel, planOrder, usd } from './orchestration-model';
import './orchestration.css';

const message = (error: unknown) => ConnectError.from(error).rawMessage || String(error);

/**
 * The person's one look at an orchestrator's plan before any of its agents
 * starts (Warp's pattern, sota-agents.md §6.11): every task with who does it,
 * at what permission, where and on which paths — paths two tasks share are
 * flagged, since parallel writers collide there — the estimate, and the $ cap
 * and fan-out the person approves it with. ⌘↵ approves; Esc closes.
 */
export function PlanSheet({ mission, plan, onClose }: { mission: Mission; plan: MissionPlan; onClose(): void }) {
  const missions = useMissions();
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  const grant = mission.grants.find(g => g.id === plan.grantId);
  const [cap, setCap] = useState(() => defaultCap(plan.totalUsd, grant?.ceiling?.budgetUsd || 0));
  const [wip, setWip] = useState(() => Math.min(grant?.ceiling?.maxChildren || 3, Math.max(1, plan.tasks.length)));
  const [note, setNote] = useState(''), [rejecting, setRejecting] = useState(false);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const order = useMemo(() => planOrder(plan.tasks), [plan.tasks]);
  const decided = plan.state !== 'proposed';
  const decide = async (approve: boolean) => {
    if (busy || decided) return;
    setBusy(true); setError('');
    try {
      await missions.update(mission.id, approve
        ? { case: 'approvePlan', value: { planId: plan.id, capUsd: cap, maxChildren: wip, note } }
        : { case: 'rejectPlan', value: { planId: plan.id, capUsd: 0, maxChildren: 0, note } });
      onClose();
    } catch (e) { setError(message(e)); setBusy(false); }
  };
  const submit = (event: FormEvent) => { event.preventDefault(); void decide(!rejecting); };
  const overBudget = cap > 0 && plan.totalUsd > cap;
  return <DraggableDialog ref={dialog} className="mission-dialog plan-sheet" aria-label={`Plan for ${mission.title}`} onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader><h2><Icon name="checklist" size={15}/> {grant?.title || 'The orchestrator'} proposes {plan.tasks.length} {plan.tasks.length === 1 ? 'agent' : 'agents'}</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}>×</button></DialogHeader>
    <form onSubmit={submit} onKeyDown={event => { if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) { event.preventDefault(); void decide(!rejecting); } }}>
      <p className="plan-for">For <b>{mission.title}</b>{plan.summary ? <> · {plan.summary}</> : null}</p>
      {decided && <p className="missions-notice">This plan was {plan.state}.</p>}
      <ol className="plan-tasks">{order.map(({ task, step }) => <li key={task.key} data-key={task.key}>
        <span className="plan-step" title={task.after.length ? `Starts after ${task.after.join(', ')}` : 'Starts right away'}>{step}</span>
        <div className="plan-task-main">
          <div className="plan-task-head"><strong>{task.title}</strong><span className="plan-estimate">{task.estimateUsd > 0 ? usd(task.estimateUsd) : '—'}</span></div>
          <div className="plan-task-meta">
            <span>{task.role || 'worker'}</span>
            <span>{permissionLabel[task.permission] || (task.permission ? task.permission : `${permissionLabel[grant?.ceiling?.permission || 'ask'] || 'Ask'} (its default)`)}</span>
            <span>{task.workspace === 'new' ? 'its own workspace' : 'this workspace'}</span>
            {task.after.length > 0 && <span>after {task.after.join(', ')}</span>}
          </div>
          {task.paths.length > 0 && <ul className="plan-paths" aria-label="Paths">{task.paths.map(path => {
            const others = overlapsOf(plan.overlaps, task.key, path), with_ = others.length ? others : undefined;
            return <li key={path} className={with_ ? 'overlap' : ''} title={with_ ? `Also in ${with_.join(', ')}: they would edit the same files` : undefined}>{with_ && <Icon name="alert" size={10}/>}<code>{path}</code></li>;
          })}</ul>}
          {task.brief && <details className="plan-brief"><summary>Brief</summary><p>{task.brief}</p></details>}
        </div>
      </li>)}</ol>
      {plan.overlaps.length > 0 && <p className="plan-overlaps" role="note"><Icon name="alert" size={12}/> {plan.overlaps.length === 1 ? 'One path is' : `${plan.overlaps.length} paths are`} in more than one task: {plan.overlaps.map(o => `${o.path} (${o.keys.join(', ')})`).join('; ')}. Order them, or give them their own workspaces.</p>}
      <div className="plan-totals">
        <span>Estimate <b>{usd(plan.totalUsd)}</b></span>
        {grant && grant.spentUsd > 0 && <span>Spent so far {usd(grant.spentUsd)}</span>}
      </div>
      {!decided && !rejecting && <div className="grant-row plan-limits">
        <label className="grant-number">Budget cap $<input type="number" min={0} step={1} value={cap || ''} placeholder="no cap" aria-label="Budget cap in dollars" onChange={e => setCap(Math.max(0, Number(e.target.value) || 0))}/></label>
        <label className="grant-number">At once<input type="number" min={1} max={16} value={wip} aria-label="Agents at once" onChange={e => setWip(Math.max(1, Math.min(16, Number(e.target.value) || 1)))}/></label>
        <small className={overBudget ? 'plan-warn' : ''}>{overBudget ? `Below the estimate: the last tasks will pause for you` : cap > 0 ? 'New agents pause and ask you when the tree would pass it' : 'No cap: agents start as the plan says'}</small>
      </div>}
      {!decided && <label className="plan-note"><span>{rejecting ? 'Why (the orchestrator is told)' : 'Note to the orchestrator'} {!rejecting && <small>optional</small>}</span><textarea rows={2} value={note} maxLength={2048} onChange={e => setNote(e.target.value)} placeholder={rejecting ? 'What should change' : 'Anything it should know'}/></label>}
      {error && <p className="missions-error" role="alert">{error}</p>}
      <footer>
        {decided ? <button type="button" onClick={onClose}>Close</button> : rejecting ? <>
          <button type="button" disabled={busy} onClick={() => setRejecting(false)}>Back</button>
          <button className="danger" disabled={busy}>{busy ? 'Rejecting…' : 'Reject plan'}</button>
        </> : <>
          <button type="button" className="plan-reject" disabled={busy} onClick={() => setRejecting(true)}>Reject…</button>
          <span className="plan-shortcut">⌘↵</span>
          <button className="primary" disabled={busy} autoFocus>{busy ? 'Approving…' : cap > 0 ? `Approve with ${usd(cap)} cap` : 'Approve'}</button>
        </>}
      </footer>
    </form>
  </DraggableDialog>;
}

/** "Review plan…": a button that opens the sheet for a mission's plan. */
export function ReviewPlanButton({ mission, plan, label = 'Review plan…', className = 'primary' }: { mission: Mission; plan: MissionPlan; label?: string; className?: string }) {
  const [open, setOpen] = useState(false);
  return <>
    <button type="button" className={className} onClick={() => setOpen(true)}>{label}</button>
    {open && <PlanSheet mission={mission} plan={plan} onClose={() => setOpen(false)}/>}
  </>;
}
