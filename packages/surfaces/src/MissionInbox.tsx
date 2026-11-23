import { useRef, useState } from 'react';
import { ConnectError } from '@lazybox/client-core';
import { Icon } from './Icons';
import { DeliveryVerdict } from './DeliveryVerdict';
import { useDeliveryOutcome } from './useDeliveryOutcome';
import { awaitingRows, evidenceChip, flagLabel, headlineEvidence, trustLabel, trustMeaning, type AwaitingRow } from './missions-model';
import { useMissions } from './useMissions';
import { pendingPlans, usd } from './orchestration-model';
import { ReviewPlanButton } from './PlanSheet';
import type { ViewTarget } from './workspace-model';
import './missions.css';

/** How many tasks wait for the person's acceptance, across missions. */
export function useAwaitingTasks(): number {
  return awaitingRows(useMissions().missions).length;
}

/** What waits on the person in missions: tasks to accept and plans to approve. */
export function useMissionAttention(): number {
  const { missions } = useMissions();
  return awaitingRows(missions).length + pendingPlans(missions).length;
}

/**
 * The inbox's "Awaiting acceptance" group: a task reported done waits at its
 * human gate until the person accepts it or asks for changes, here or on the
 * mission's board. The trust badge says what backs the report.
 */
export function MissionInboxSection({ onOpen }: { onOpen(target: ViewTarget): void }) {
  const missions = useMissions();
  const rows = awaitingRows(missions.missions), plans = pendingPlans(missions.missions);
  return <>
    {plans.length > 0 && <section className="inbox-group inbox-missions group-plans" aria-labelledby="inbox-group-plans">
      <h3 id="inbox-group-plans"><Icon name="checklist" size={13}/>Plans to approve<span>{plans.length}</span></h3>
      <ul>{plans.map(({ mission, plan }) => <li key={plan.id} className="inbox-item inbox-task inbox-plan" data-inbox-row tabIndex={-1} aria-label={`Plan to approve: ${mission.title}`}>
        <div className="inbox-task-head"><strong>{mission.grants.find(g => g.id === plan.grantId)?.title || 'An orchestrator'} proposes {plan.tasks.length} {plan.tasks.length === 1 ? 'agent' : 'agents'}</strong></div>
        <span className="inbox-task-meta">{mission.title} · est. {usd(plan.totalUsd)}{plan.overlaps.length ? ` · ${plan.overlaps.length} shared ${plan.overlaps.length === 1 ? 'path' : 'paths'}` : ''}</span>
        <div className="inbox-task-actions"><ReviewPlanButton mission={mission} plan={plan}/></div>
      </li>)}</ul>
    </section>}
    {rows.length > 0 && <section className="inbox-group inbox-missions group-awaiting" aria-labelledby="inbox-group-awaiting">
      <h3 id="inbox-group-awaiting"><Icon name="target" size={13}/>Awaiting acceptance<span>{rows.length}</span></h3>
      <ul>{rows.map(row => <AwaitingTask key={row.task.id} row={row} onOpen={onOpen} update={missions.update}/>)}</ul>
    </section>}
  </>;
}

function AwaitingTask({ row: { mission, task }, onOpen, update }: { row: AwaitingRow; onOpen(target: ViewTarget): void; update: ReturnType<typeof useMissions>['update'] }) {
  const [changes, setChanges] = useState<string | null>(null);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const decide = async (accept: boolean) => {
    setBusy(true); setError('');
    try { await update(mission.id, accept ? { case: 'accept', value: { taskId: task.id, note: '' } } : { case: 'reject', value: { taskId: task.id, note: changes || '' } }); }
    catch (e) { setError(ConnectError.from(e).rawMessage || String(e)); setBusy(false); }
  };
  const attempt = task.attempt;
  const askButton = useRef<HTMLButtonElement>(null);
  // The same verdict the Mission card shows, so a person accepts knowing what is still open.
  const contracted = mission.plans.some(p => p.state === 'approved' && !!p.contract);
  const outcome = useDeliveryOutcome(contracted && attempt ? { missionId: mission.id, taskId: task.id } : undefined);
  // Back (or Escape in the note) returns to the choice, the focus on Ask for changes.
  const back = () => { setChanges(null); requestAnimationFrame(() => askButton.current?.focus()); };
  return <li className="inbox-item inbox-task" data-task={task.id} data-inbox-row tabIndex={-1} aria-label={`Awaiting acceptance: ${task.title}`}>
    <div className="inbox-task-head"><strong>{task.title}</strong><span className={`trust-badge trust-${task.trust}`} title={trustMeaning[task.trust]}>{trustLabel[task.trust] || task.trust}</span></div>
    <span className="inbox-task-meta">{mission.title}{task.report ? ` · “${task.report}”` : ''}</span>
    {outcome && <DeliveryVerdict outcome={outcome} compact/>}
    <p className="inbox-task-reason">{task.reason}{task.flags.filter(f => f !== 'no_evidence').map(f => ` · ${flagLabel[f] || f}`).join('')}</p>
    {headlineEvidence(task.evidence).length > 0 && <span className="inbox-task-meta">{headlineEvidence(task.evidence).map(e => { const chip = evidenceChip(e); return `${chip.mark} ${chip.label}`; }).join('   ')}</span>}
    {task.brief && <details className="mission-brief"><summary>Review task brief and acceptance criteria</summary><p className="mission-card-brief">{task.brief}</p></details>}
    {error && <p className="missions-error" role="alert">{error}</p>}
    <div className="inbox-task-actions">
      {changes === null ? <>
        <button type="button" className="primary" disabled={busy} onClick={() => void decide(true)}>Accept</button>
        <button ref={askButton} type="button" disabled={busy} onClick={() => setChanges('')}>Ask for changes…</button>
        {attempt?.sessionId && <button type="button" onClick={() => onOpen({ collectionId: attempt.collectionId, sessionId: attempt.sessionId, commandId: '' })}>Open conversation</button>}
      </> : <>
        <textarea autoFocus rows={2} aria-label={`What should change in ${task.title}`} placeholder="What should change" value={changes} onChange={e => setChanges(e.target.value)}
          onKeyDown={e => { if (e.key === 'Escape' && !e.nativeEvent.isComposing) { e.preventDefault(); e.stopPropagation(); back(); } }}/>
        <button type="button" className="primary" disabled={busy || !changes.trim()} onClick={() => void decide(false)}>Ask for changes</button>
        <button type="button" disabled={busy} onClick={back} title="Back (Esc)">Back</button>
      </>}
    </div>
  </li>;
}
