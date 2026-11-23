import { peekProps } from './peek-model';
import { SubscriptionOptions } from './SubscriptionGroups';
import { useDismiss } from './dismiss';
import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { ConnectError } from '@lazybox/client-core';
import type { Collection, Mission, MissionEvidence, MissionTask, Session } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import { conversationName } from './conversation-model';
import { readPreference, writePreference } from './view-state';
import type { ModuleProps } from './PanelRegistry';
import type { ViewTarget } from './workspace-model';
import { ago, arrange, briefTemplate, columns, evidenceChip, flagLabel, headlineEvidence, liveLabel, missionSummary, statusLabel, taskActions, trustLabel, trustMeaning } from './missions-model';
import { useMissionDetail, useMissions, type ChangeInput } from './useMissions';
import { agentTitle } from './providers';
import { useMissionSelection } from './navigate-requests';
import { GrantDialog } from './OrchestratorBadge';
import { useOpenConversation } from './AgentMessages';
import { budgetLine, overlapsOfTask, permissionLabel, reviewHeld, usd } from './orchestration-model';
import { MissionLeases, MissionMessages, MissionTabs, MissionTimeline, type MissionTab } from './MissionViews';
import { ReviewPlanButton } from './PlanSheet';
import './missions.css';

const message = (error: unknown) => ConnectError.from(error).rawMessage || String(error);
const selectedKey = 'missions:selected', profileKey = 'missions:assign-profile', tabKey = 'missions:tab';

/**
 * The Missions panel: a mission's tasks on a board whose every status the
 * Runner derives from the mission's event log, with the trust behind it and
 * the evidence that backs it. A person creates missions and tasks, assigns a
 * task to a conversation, reports it done, and accepts it.
 */
export function MissionsPanel({ collections, viewState, onShowSession, onShowPanel, onReturn, openMission, pinned }: Pick<ModuleProps, 'collections' | 'viewState' | 'onShowSession' | 'onShowPanel' | 'openMission'> & { pinned?: string; onReturn?(): void }) {
  const missions = useMissions();
  const [selected, setSelected] = useState(() => readPreference(viewState, selectedKey) || '');
  const [creating, setCreating] = useState(false);
  const choose = (id: string) => { setSelected(id); try { writePreference(viewState, selectedKey, id); } catch { /* a convenience */ } };
  // A mission chosen in the command palette.
  useMissionSelection(id => { setCreating(false); choose(id); });
  const [detachError, setDetachError] = useState('');
  const current = pinned !== undefined ? missions.missions.find(m => m.id === pinned) : missions.missions.find(m => m.id === selected) || missions.missions.find(m => !m.canceled) || missions.missions[0];
  useEffect(() => { if (pinned && current) document.title = current.title + ' · Missions'; }, [pinned, current?.title]);
  const open = missions.missions.filter(m => !m.canceled), closed = missions.missions.filter(m => m.canceled);
  return <section className="missions-panel" aria-label="Missions and tasks">
    <header className="missions-header">
      {pinned !== undefined && <strong className="missions-heading">{current?.title || "Mission"}</strong>}
      {pinned === undefined && missions.missions.length > 0 && !creating && current && <ActionMenu className="mission-picker" align="start" label={`Mission: ${current.title}`} title="Choose the mission"
        summary={<><span className="mission-picker-title">{current.title}</span><small>{current.canceled ? 'Canceled' : missionSummary(current)}</small><Icon name="chevron" size={11}/></>}>
        <ActionMenuItems items={[
          ...open.map(m => ({ label: m.title, note: missionSummary(m), checked: m.id === current.id, run: () => choose(m.id) })),
          closed.length > 0 && { label: 'Canceled', submenu: closed.map(m => ({ label: m.title, checked: m.id === current.id, run: () => choose(m.id) })) },
        ]}/>
      </ActionMenu>}
      {creating && <strong className="missions-heading">New mission</strong>}
      <div className="missions-tools">
        {pinned === undefined && !creating && missions.missions.length > 0 && <button type="button" className="mission-new" onClick={() => setCreating(true)}><Icon name="plus" size={13}/> New mission</button>}
        {openMission && current && !creating && <button type="button" className="quiet" onClick={() => {
          setDetachError('');
          void openMission(current.id).then(() => onReturn?.()).catch(error => setDetachError(message(error)));
        }}>Detach mission</button>}
        <button type="button" className="icon-button" aria-label="Refresh missions" title="Refresh" onClick={() => void missions.refresh()}><Icon name="refresh" size={14}/></button>
      </div>
    </header>
    <div className="missions-body">
      {detachError && <p role="alert">{detachError}</p>}
      {pinned !== undefined && missions.loaded && !current && <p role="status">This Mission is unavailable on this Runner.</p>}
      {missions.error && <p className="missions-notice tone-warn" role="status">Missions unavailable; retrying. {missions.error}</p>}
      {!missions.loaded && !missions.error && <p className="missions-empty" role="status">Loading missions…</p>}
      {creating && <NewMission onCancel={missions.missions.length ? () => setCreating(false) : undefined} onCreate={async (title, brief) => { const m = await missions.create(title, brief); choose(m.id); setCreating(false); }}/>}
      {pinned === undefined && !creating && missions.loaded && !missions.missions.length && <div className="missions-intro">
        <Icon name="target" size={26}/>
        <h3>Create your first mission</h3>
        <p>Describe the outcome. You can assign tasks to conversations in different workspaces on this Runner.</p>
        <NewMission onCreate={async (title, brief) => { const m = await missions.create(title, brief); choose(m.id); }}/>
      </div>}
      {!creating && current && <MissionBoard key={current.id} missionId={current.id} collections={collections} viewState={viewState} onShowSession={onShowSession} onShowPanel={onShowPanel}/>}
    </div>
  </section>;
}

function NewMission({ onCreate, onCancel }: { onCreate(title: string, brief: string): Promise<void>; onCancel?(): void }) {
  const [title, setTitle] = useState(''), [brief, setBrief] = useState('');
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const submit = async (event: FormEvent) => {
    event.preventDefault(); if (!title.trim() || busy) return;
    setBusy(true); setError('');
    try { await onCreate(title, brief); setTitle(''); setBrief(''); } catch (e) { setError(message(e)); } finally { setBusy(false); }
  };
  return <form className="mission-form" onSubmit={submit} aria-label="New mission">
    <label>Outcome<input autoFocus value={title} maxLength={200} placeholder="What should be true when this is done?" onChange={e => setTitle(e.target.value)}/></label>
    <label>Brief <small>optional</small><textarea value={brief} rows={3} maxLength={4096} placeholder="Context, constraints, links. Agents receive each task's own brief." onChange={e => setBrief(e.target.value)}/></label>
    {error && <p className="missions-error" role="alert">{error}</p>}
    <div className="mission-form-actions">{onCancel && <button type="button" onClick={onCancel}>Cancel</button>}<button className="primary" disabled={!title.trim() || busy}>{busy ? 'Creating…' : 'Create mission'}</button></div>
  </form>;
}

type BoardProps = { missionId: string; collections: Collection[]; viewState?: ViewStateStore; onShowSession?(target: ViewTarget): void; onShowPanel?(kind: string, target?: ViewTarget): void };

function MissionBoard({ missionId, collections, viewState, onShowSession, onShowPanel }: BoardProps) {
  const missions = useMissions();
  const mission = useMissionDetail(missionId, missions.put);
  const boardRef = useRef<HTMLDivElement>(null);
  const [adding, setAdding] = useState(false);
  const [tab, setTab] = useState<MissionTab>(() => (readPreference(viewState, tabKey) as MissionTab) || 'board');
  const chooseTab = (next: MissionTab) => { setTab(next); try { writePreference(viewState, tabKey, next); } catch { /* a convenience */ } };
  const [notice, setNotice] = useState<ReactNode>('');
  const [error, setError] = useState('');
  const [dialog, setDialog] = useState<{ kind: 'assign' | 'report' | 'reject' | 'cancel'; task: MissionTask }>();
  const [now, setNow] = useState(Date.now);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 30_000); return () => clearInterval(timer); }, []);
  if (!mission) return <p className="missions-empty" role="status">Loading the mission…</p>;
  const change = async (input: ChangeInput, done?: string) => {
    setError('');
    try { const result = await missions.update(mission.id, input); if (done) setNotice(done); return result; }
    catch (e) { setError(message(e)); throw e; }
  };
  const firstTask = !mission.tasks.length && !mission.error && !mission.canceled;
  const addTask = async (title: string, brief: string, blockedBy: string[]) => {
    await change({ case: 'addTask', value: { title, brief, blockedBy, goalId: '' } });
  };
  const board = arrange(mission.tasks);
  const openConversation = (task: MissionTask) => {
    const a = task.attempt; if (!a?.sessionId) return;
    onShowSession?.({ collectionId: a.collectionId, sessionId: a.sessionId, commandId: '' });
  };
  const openEvidence = (task: MissionTask, e: MissionEvidence) => {
    if (e.kind === 'run' && e.commandId) onShowPanel?.('console', { collectionId: e.collectionId || task.attempt?.collectionId || '', sessionId: e.sessionId || task.attempt?.sessionId || '', commandId: e.commandId });
    else if (e.url) window.open(e.url, '_blank', 'noopener');
  };
  const attention = mission.tasks.find(t => t.gate && t.status === 'needs_you')
    || mission.tasks.find(t => ['failed', 'needs_you', 'changes_requested', 'blocked'].includes(t.status));
  const goals = new Map(mission.goals.map(g => [g.id, g.title]));
  return <div ref={boardRef} className="mission-board" data-mission={mission.id}>
    <div className="mission-head">
      <div className="mission-title"><h3>{mission.title}</h3>
        <span className="mission-process" title={`Process ${mission.process}, pinned when the mission started (${mission.processDigest}). A report asks you to accept the task; only you can.`}>{mission.process === 'solo-pair' ? 'One agent per task' : mission.process} · you accept each task</span>
      </div>
      {!firstTask && <p className="mission-summary" role="status">{missionSummary(mission)}</p>}
      {tab === 'board' && attention && !mission.canceled && !mission.error && <div className="mission-next-decision">
        <button type="button" className="link-button" onClick={() => boardRef.current?.querySelector<HTMLElement>(`[data-task="${CSS.escape(attention.id)}"]`)?.focus()}>
          {statusLabel[attention.status]} · {attention.title}
        </button>
        <span>{attention.reason}</span>
      </div>}
      {mission.plans.filter(p => p.state === 'approved' && p.contract?.acceptanceCriteria.length).map(p =>
        <details className="mission-brief" key={p.id}><summary>Acceptance criteria · {p.summary || 'Approved plan'}</summary>
          <ul>{p.contract!.acceptanceCriteria.map((line, i) => <li key={i}>{line}</li>)}</ul>
        </details>)}
      {mission.brief && <details className="mission-brief"><summary>Brief</summary><p>{mission.brief}</p></details>}
      {mission.error && <p className="missions-notice tone-warn">{mission.error}</p>}
    </div>
    {firstTask && <section className="mission-first-task">
      <h4>Start with one task</h4>
      <p>Choose a piece of this outcome that one conversation can deliver. You can pick its workspace when you assign it.</p>
      {!adding && <button type="button" className="primary" onClick={() => { chooseTab('board'); setAdding(true); }}>Add first task</button>}
      {adding && <AddTask mission={mission} onClose={() => setAdding(false)} onAdd={addTask}/>}
    </section>}
    <OrchestratorStrip collections={collections} mission={mission} onNotice={setNotice} onError={setError} change={change}/>
    {notice && <p className="missions-notice" role="status">{notice}<button type="button" className="link-button" onClick={() => setNotice('')}>Dismiss</button></p>}
    {error && <p className="missions-error" role="alert">{error}<button type="button" className="link-button" onClick={() => setError('')}>Dismiss</button></p>}
    <MissionTabs mission={mission} tab={tab} onTab={chooseTab}/>
    {tab === 'timeline' && <div role="tabpanel" id="mission-view-timeline" aria-labelledby="mission-tab-timeline"><MissionTimeline mission={mission} now={now}/></div>}
    {tab === 'messages' && <div role="tabpanel" id="mission-view-messages" aria-labelledby="mission-tab-messages"><MissionMessages mission={mission} now={now}/></div>}
    {tab === 'leases' && <div role="tabpanel" id="mission-view-leases" aria-labelledby="mission-tab-leases"><MissionLeases mission={mission}/></div>}
    {tab === 'board' && <div role="tabpanel" id="mission-view-board" aria-labelledby="mission-tab-board">
    {!firstTask && !mission.canceled && !mission.error && (adding
      ? <AddTask mission={mission} onClose={() => setAdding(false)} onAdd={addTask}/>
      : <button type="button" className="mission-add-task" onClick={() => setAdding(true)}><Icon name="plus" size={13}/> Add task</button>)}
    {mission.tasks.length > 0 && <div className="mission-columns">
      {columns.map(column => <section key={column.key} className={`mission-column column-${column.key}`} aria-labelledby={`column-${mission.id}-${column.key}`}>
        <h4 id={`column-${mission.id}-${column.key}`}>{column.title}<span>{board[column.key].length}</span></h4>
        {!board[column.key].length && <p className="mission-column-empty">{column.empty}</p>}
        <ul>{board[column.key].map(task => <TaskCard key={task.id} task={task} goal={goals.get(task.goalId)} now={now} collections={collections} titles={mission.tasks} orchestrators={mission.grants} overlaps={overlapsOfTask(mission, task.id)}
          onOpen={() => openConversation(task)} onEvidence={e => openEvidence(task, e)}
          onAssign={() => setDialog({ kind: 'assign', task })} onReport={() => setDialog({ kind: 'report', task })}
          onAccept={() => void change({ case: 'accept', value: { taskId: task.id, note: '' } }, `Accepted “${task.title}”.`).catch(() => {})}
          onReject={() => setDialog({ kind: 'reject', task })} onCancel={() => setDialog({ kind: 'cancel', task })}
          onNotice={setNotice} onError={setError}/>)}</ul>
      </section>)}
    </div>}
    <MissionHistory mission={mission} now={now}/>
    </div>}
    {dialog?.kind === 'assign' && <AssignDialog task={dialog.task} mission={mission} collections={collections} viewState={viewState} onClose={() => setDialog(undefined)}
      onAssign={async (input, newConversation) => {
        const result = await change({ case: 'assign', value: input });
        setDialog(undefined);
        const sessionId = result?.sessionId;
        const where = result?.tasks.find(t => t.id === dialog.task.id)?.attempt;
        setNotice(<>{newConversation ? 'Started a conversation for' : 'Assigned'} “{dialog.task.title}”. {sessionId && <button type="button" className="link-button" onClick={() => onShowSession?.({ collectionId: where?.collectionId || '', sessionId, commandId: '' })}>Open it</button>}</>);
      }}/>}
    {dialog?.kind === 'report' && <NoteDialog title={`Report “${dialog.task.title}” done`} label="What was done" placeholder="One or two lines: what changed, what you checked." confirm="Report done"
      body="The report asks you to accept the task. Whatever it says, its status is only as trusted as the evidence the Runner recorded: lazybox runs, commits and checks."
      onClose={() => setDialog(undefined)} onConfirm={async note => { await change({ case: 'report', value: { taskId: dialog.task.id, summary: note } }); setDialog(undefined); }}/>}
    {dialog?.kind === 'reject' && <NoteDialog title={`Ask for changes on “${dialog.task.title}”`} label="What should change" placeholder="Shown on the task; send it to the agent too if you want it to act." confirm="Ask for changes" required
      onClose={() => setDialog(undefined)} onConfirm={async note => { await change({ case: 'reject', value: { taskId: dialog.task.id, note } }, `Asked for changes on “${dialog.task.title}”.`); setDialog(undefined); }}/>}
    {dialog?.kind === 'cancel' && <NoteDialog title={`Cancel “${dialog.task.title}”?`} label="Why" placeholder="Optional" confirm="Cancel task" danger
      body="A canceled task takes no more reports or evidence. Its conversation keeps running until you stop it."
      onClose={() => setDialog(undefined)} onConfirm={async note => { await change({ case: 'cancelTask', value: { taskId: dialog.task.id, reason: note } }, `Canceled “${dialog.task.title}”.`); setDialog(undefined); }}/>}
  </div>;
}

function AddTask({ onAdd, onClose, mission }: { mission: Mission; onAdd(title: string, brief: string, blockedBy: string[]): Promise<void>; onClose(): void }) {
  const [title, setTitle] = useState(''), [brief, setBrief] = useState(''), [after, setAfter] = useState('');
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const submit = async (event: FormEvent) => {
    event.preventDefault(); if (!title.trim() || busy) return;
    setBusy(true); setError('');
    try { await onAdd(title, brief === briefTemplate ? '' : brief, after ? [after] : []); setTitle(''); setBrief(''); setAfter(''); }
    catch (e) { setError(message(e)); } finally { setBusy(false); }
  };
  const open = mission.tasks.filter(t => t.status !== 'done' && t.status !== 'canceled');
  return <form className="mission-form task-form" onSubmit={submit} aria-label="New task" onKeyDown={event => { if (event.key === 'Escape') onClose(); }}>
    <label>Task<input autoFocus value={title} maxLength={200} placeholder="One assignment, one owner" onChange={e => setTitle(e.target.value)}/></label>
    <label>Brief <small>sent to the agent</small><textarea value={brief} rows={4} maxLength={4096} placeholder={briefTemplate} onFocus={() => { if (!brief) setBrief(briefTemplate); }} onChange={e => setBrief(e.target.value)}/></label>
    {open.length > 0 && <label>Starts after <small>optional</small><select value={after} onChange={e => setAfter(e.target.value)}><option value="">Any time</option>{open.map(t => <option key={t.id} value={t.id}>{t.title}</option>)}</select></label>}
    {error && <p className="missions-error" role="alert">{error}</p>}
    <div className="mission-form-actions"><button type="button" onClick={onClose}>Done</button><button className="primary" disabled={!title.trim() || busy}>{busy ? 'Adding…' : 'Add task'}</button></div>
  </form>;
}

type CardProps = { task: MissionTask; goal?: string; now: number; collections: Collection[]; titles: MissionTask[]; orchestrators: Mission['grants']; overlaps: ReturnType<typeof overlapsOfTask>; onOpen(): void; onEvidence(e: MissionEvidence): void; onAssign(): void; onReport(): void; onAccept(): void; onReject(): void; onCancel(): void; onNotice(text: string): void; onError(text: string): void };

function TaskCard({ task, goal, now, collections, titles, orchestrators, overlaps, onOpen, onEvidence, onAssign, onReport, onAccept, onReject, onCancel, onNotice, onError }: CardProps) {
  const [expanded, setExpanded] = useState(false);
  const actions = taskActions(task);
  const attempt = task.attempt;
  const workspace = collections.find(c => c.id === attempt?.collectionId)?.name;
  const chips = expanded ? task.evidence : headlineEvidence(task.evidence);
  const label: Record<string, string> = { assign: task.attempts.length ? 'Assign again…' : 'Assign…', report: 'Report done…', accept: 'Accept', reject: 'Ask for changes…', open: 'Open conversation' };
  const run: Record<string, () => void> = { assign: onAssign, report: onReport, accept: onAccept, reject: onReject, open: onOpen };
  const blockedBy = task.blockedBy.map(id => titles.find(t => t.id === id)?.title || id);
  return <li className={`mission-card status-${task.status}`} data-task={task.id} tabIndex={0} {...peekProps({ kind: 'mission-task', taskId: task.id, title: task.title })} aria-label={`${task.title}, ${statusLabel[task.status] || task.status}${task.trust ? ', ' + trustLabel[task.trust] : ''}`}>
    <div className="mission-card-head">
      <button type="button" className="mission-card-title" aria-expanded={expanded} onClick={() => setExpanded(v => !v)}><strong>{task.title}</strong></button>
      <TaskMenu task={task} onCancel={onCancel} onAssign={onAssign}/>
    </div>
    <div className="mission-card-status">
      <span className={`mission-status status-${task.status}`}>{statusLabel[task.status] || task.status}</span>
      {task.trust && <TrustBadge trust={task.trust}/>}
      {task.trust !== task.evidenceTrust && task.status !== 'ready' && task.evidence.length > 0 && <span className="mission-evidence-trust" title={trustMeaning[task.evidenceTrust]}>evidence: {trustLabel[task.evidenceTrust]?.toLowerCase()}</span>}
      {task.flags.filter(f => f !== 'no_evidence' || task.trust === 'claimed').map(f => <span key={f} className={`mission-flag flag-${f}`}>{flagLabel[f] || f}</span>)}
      {goal && <span className="mission-goal">{goal}</span>}
      {overlaps.map((o, i) => <span key={i} className="mission-flag flag-overlap" title={`${o.kind === 'lease' ? 'Changed inside an exclusive lease of' : 'Changed the same files as'} “${o.other}”: ${o.paths.join(', ')}`}><Icon name="alert" size={10}/> Overlaps “{o.other}”</span>)}
      {task.costUsd > 0 || task.estimateUsd > 0 ? <span className={`mission-cost${task.estimateUsd > 0 && task.costUsd > task.estimateUsd ? ' over' : ''}`} title={task.estimateUsd > 0 ? `Cost so far against the plan's estimate of ${usd(task.estimateUsd)}` : 'Cost so far'}>{usd(task.costUsd)}{task.estimateUsd > 0 ? ` / ${usd(task.estimateUsd)}` : ''}</span> : null}
      {attempt?.parentConversationId && <span className="mission-lineage" title={`Started by the orchestrator “${orchestrators.find(g => g.conversationId === attempt.parentConversationId)?.title || 'Orchestrator'}” as its ${attempt.role || 'worker'}, at ${permissionLabel[attempt.permission] || attempt.permission} on ${attempt.profileId}`}>
        <Icon name="subagent" size={11}/>{orchestrators.find(g => g.conversationId === attempt.parentConversationId)?.title || 'Orchestrator'}<small>{attempt.role || 'worker'}</small></span>}
    </div>
    <p className="mission-reason">{task.reason}</p>
    {attempt && <button type="button" className="mission-conversation" disabled={!attempt.sessionId || attempt.live === 'missing' || attempt.live === 'reserved'} onClick={onOpen} title={attempt.live === 'reserved' ? 'The worker conversation is being prepared' : attempt.sessionId ? 'Open the conversation working on this task' : 'This conversation was deleted'}>
      <Icon name="chat" size={13}/><span className="mission-conversation-title">{attempt.title || 'Conversation'}</span>
      {workspace && <span className="mission-conversation-where">{workspace}</span>}
      <span className={`mission-live live-${attempt.live}`}>{liveLabel(attempt.live, attempt.liveDetail)}</span>
    </button>}
    {chips.length > 0 && <ul className="mission-evidence" aria-label="Evidence">{chips.map(e => {
      const chip = evidenceChip(e);
      const actionable = (e.kind === 'run' && !!e.commandId) || !!e.url;
      return <li key={e.ref + e.atUnixMillis}><button type="button" className={`evidence-chip tone-${chip.tone}`} title={chip.label + '\n' + chip.title + (actionable ? '\n\n' + (e.kind === 'run' ? 'Open its output' : 'Open on GitHub') : '')} disabled={!actionable} onClick={() => onEvidence(e)}><span aria-hidden="true">{chip.mark}</span><span className="evidence-label">{e.historical ? 'History · ' : ''}{chip.label}</span><time>{ago(e.atUnixMillis, now)}</time></button></li>;
    })}</ul>}
    {expanded && <div className="mission-card-detail">
      {task.brief && <><h5>Brief</h5><p className="mission-card-brief">{task.brief}</p></>}
      {task.report && <><h5>Report</h5><p>{task.report}</p></>}
      {task.decisionNote && <><h5>Your note</h5><p>{task.decisionNote}</p></>}
      {blockedBy.length > 0 && <><h5>Starts after</h5><p>{blockedBy.join(', ')}</p></>}
      {task.attempts.length > 1 && <><h5>Attempts</h5><ol className="mission-attempts">{task.attempts.map(a => <li key={a.id}>{a.title || 'Conversation'} · {a.endedAtUnixMillis ? `ended (${a.outcome || 'stopped'})` : liveLabel(a.live, a.liveDetail)}</li>)}</ol></>}
      {!task.evidence.length && <p className="mission-muted">No evidence yet. Runs through <code>lazybox run</code> in its conversation or workspace, commits and pull request checks attach here automatically.</p>}
    </div>}
    {task.nextSnippets.length > 0 && attempt?.sessionId && !attempt.endedAtUnixMillis && <NextSnippets task={task} onNotice={onNotice} onError={onError}/>}
    {actions.length > 0 && <div className="mission-actions">{actions.map((action, i) => <button key={action} type="button" className={i === 0 ? 'primary' : ''} onClick={run[action]}>{label[action]}</button>)}</div>}
  </li>;
}

/**
 * The mission's orchestrators: the conversations the person let run its
 * agents, what each may give them, and Revoke; or the way to name one.
 */
function OrchestratorStrip({ collections, mission, onNotice, onError, change }: { collections: Collection[]; mission: Mission; onNotice(text: string): void; onError(text: string): void; change(input: ChangeInput, done?: string): Promise<Mission | undefined> }) {
  const { info } = useRunner();
  const open = useOpenConversation();
  const [granting, setGranting] = useState(false);
  const [revoking, setRevoking] = useState('');
  const live = mission.grants.filter(g => g.live);
  const candidates = useMemo(() => latestConversations(info?.sessions || []).filter(s => !s.closed && !s.lineage?.parentConversationId && !s.lineage?.grantId), [info?.sessions]);
  if (mission.canceled || mission.error) return null;
  return <div className="mission-orchestrators" aria-label="Orchestrators">
    {live.map(g => <div key={g.id} className="mission-orchestrator">
      <div className="mission-orchestrator-heading">
        <Icon name="subagent" size={13}/>
        <button type="button" className="link-button" title="Open the orchestrator's conversation" onClick={() => open(g.conversationId, g.sessionId)}>{g.title || 'Orchestrator'}</button>
        <span className="mission-orchestrator-count">{g.liveChildren}/{g.ceiling?.maxChildren} agents</span>
      </div>
      {g.ceiling && <span className="mission-orchestrator-ceiling">{[
        `${permissionLabel[g.ceiling.permission] || g.ceiling.permission} or narrower`,
        g.ceiling.depth > 1 && `${g.ceiling.depth} levels deep`,
        g.ceiling.planRequired && 'plan first',
      ].filter(Boolean).join(' · ')}</span>}
      <div className="mission-orchestrator-accounting">
        <span className={`mission-orchestrator-budget${g.budgetState ? ' budget-' + g.budgetState : ''}`} title="Spent is reported usage. Reserved is the remaining estimate for running agents. Both count toward the cap.">{budgetLine(g)}</span>
      {revoking === g.id
        ? <span className="mission-orchestrator-confirm">Its agents keep running.<button type="button" onClick={() => setRevoking('')}>Keep</button><button type="button" className="danger" autoFocus onClick={() => { setRevoking(''); void change({ case: 'revokeGrant', value: { grantId: g.id, reason: '' } }, `“${g.title || 'The orchestrator'}” can no longer run agents.`).catch(() => {}); }}>Revoke</button></span>
        : <button type="button" className="mission-orchestrator-revoke" onClick={() => setRevoking(g.id)}>Revoke…</button>}
      </div>
      {mission.plans.filter(p => p.state === 'proposed' && p.grantId === g.id).map(p => <div key={p.id} className="mission-plan-waiting" role="status">
        <Icon name="checklist" size={13}/><span><b>Plan waiting for you:</b> {p.tasks.length} {p.tasks.length === 1 ? 'agent' : 'agents'} · est. {usd(p.totalUsd)}{p.overlaps.length ? ` · ${p.overlaps.length} shared ${p.overlaps.length === 1 ? 'path' : 'paths'}` : ''}</span>
        <ReviewPlanButton mission={mission} plan={p}/>
      </div>)}
      {g.budgetState === 'exhausted' && <BudgetStop mission={mission} grantId={g.id} cap={g.ceiling?.budgetUsd || 0} spent={g.spentUsd} reserved={g.reservedUsd} change={change}/>}
    </div>)}
    {(() => { const held = reviewHeld(mission); return held && live.length > 0 && <p className="missions-notice tone-warn mission-review-held" role="status"><Icon name="pending" size={13}/> {held.waiting === 1 ? '1 task waits' : `${held.waiting} tasks wait`} for your review; new agents are held until you accept or send back work.</p>; })()}
    {!live.length && <button type="button" className="mission-orchestrate" disabled={!candidates.length} title={candidates.length ? 'Let one of your conversations start and drive agents for this mission' : 'Open a conversation first'} onClick={() => setGranting(true)}><Icon name="subagent" size={13}/> Let a conversation run agents…</button>}
    {granting && <GrantDialog collections={collections} missionId={mission.id} candidates={candidates} onClose={() => setGranting(false)}/>}
  </div>;
}

/** New agents paused at the cap: the person raises it, here. */
function BudgetStop({ mission, grantId, cap, spent, reserved, change }: { mission: Mission; grantId: string; cap: number; spent: number; reserved: number; change(input: ChangeInput, done?: string): Promise<Mission | undefined> }) {
  const [value, setValue] = useState(() => Math.ceil(Math.max(cap * 1.5, spent + reserved + 5)));
  const [busy, setBusy] = useState(false);
  return <form className="mission-budget-stop" role="alert" onSubmit={event => { event.preventDefault(); setBusy(true); void change({ case: 'raiseBudget', value: { grantId, capUsd: value } }, `The tree of “${mission.title}” may spend ${usd(value)}; new agents may start.`).catch(() => {}).finally(() => setBusy(false)); }}>
    <Icon name="alert" size={13}/><span><b>New agents paused.</b> {usd(spent)} spent and {usd(reserved)} reserved of the {usd(cap)} budget.</span>
    <label className="grant-number">New cap $<input type="number" min={0} step={1} value={value} aria-label="New budget cap in dollars" onChange={e => setValue(Math.max(0, Number(e.target.value) || 0))}/></label>
    <button className="primary" disabled={busy || value <= cap}>Raise budget</button>
  </form>;
}

function TrustBadge({ trust }: { trust: string }) {
  return <span className={`trust-badge trust-${trust}`} title={trustMeaning[trust]}>{trustLabel[trust] || trust}</span>;
}

function TaskMenu({ task, onCancel, onAssign }: { task: MissionTask; onCancel(): void; onAssign(): void }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(open, () => setOpen(false), [ref]);
  if (task.status === 'done' || task.status === 'canceled') return null;
  return <div className="mission-menu" ref={ref}>
    <button type="button" className="icon-button" aria-label={`More actions for ${task.title}`} aria-expanded={open} onClick={() => setOpen(v => !v)}><Icon name="more" size={14}/></button>
    {open && <div className="mission-menu-list" role="menu">
      {task.attempt && <button role="menuitem" type="button" onClick={() => { setOpen(false); onAssign(); }}>Assign to another conversation…</button>}
      <button role="menuitem" type="button" className="danger" onClick={() => { setOpen(false); onCancel(); }}>Cancel task…</button>
    </div>}
  </div>;
}

/** V1's `]]n`: the snippet chain the conversation's last snippet offers, one click away. */
function NextSnippets({ task, onNotice, onError }: { task: MissionTask; onNotice(text: string): void; onError(text: string): void }) {
  const { api } = useRunner();
  const [busy, setBusy] = useState('');
  const send = async (key: string) => {
    const sessionId = task.attempt?.sessionId; if (!sessionId) return;
    setBusy(key);
    try {
      const catalog = await api.listSnippets({}, { timeoutMs: 10000 });
      const snippet = catalog.snippets.find(s => s.key === key);
      if (!snippet) throw new Error(`Snippet “${key}” is no longer in your catalog.`);
      await api.apply({ sessionId, operationId: crypto.randomUUID(), operation: { case: 'message', value: { text: snippet.body, images: [], attachments: [], source: { kind: 'snippet', snippetKey: key, commandId: '', missionId: '', taskId: '' } } } });
      onNotice(`Sent “${key}” to the conversation.`);
    } catch (e) { onError(message(e)); } finally { setBusy(''); }
  };
  return <div className="mission-next" aria-label="Next snippets">
    <span>After <code>{task.lastSnippet}</code>, next:</span>
    {task.nextSnippets.map(key => <button key={key} type="button" disabled={!!busy} onClick={() => void send(key)} title={`Send the “${key}” snippet to the conversation`}>{busy === key ? 'Sending…' : key}</button>)}
  </div>;
}

function MissionHistory({ mission, now }: { mission: Mission; now: number }) {
  const [all, setAll] = useState(false);
  if (!mission.events.length) return null;
  const shown = all ? mission.events : mission.events.slice(0, 12);
  return <details className="mission-history">
    <summary>History <span>{Number(mission.seq)} events{mission.ignored ? ` · ${mission.ignored} kept but not understood` : ''}</span></summary>
    <ol>{shown.map(e => <li key={String(e.seq)} className={e.known ? '' : 'unknown'} title={e.known ? `${e.type} v${e.version}` : e.rawJson}>
      <time>{ago(e.atUnixMillis, now)}</time><span className={`actor actor-${e.actorKind}`}>{actorLabel(e.actorKind)}</span><span>{e.summary}</span>
    </li>)}</ol>
    {!all && mission.events.length > shown.length && <button type="button" className="link-button" onClick={() => setAll(true)}>Show {mission.events.length - shown.length} more</button>}
  </details>;
}
const actorLabel = (kind: string) => ({ human: 'You', agent: 'Agent', rule: 'Rule', check: 'Checks', lazybox: 'Lazybox' } as Record<string, string>)[kind] || kind;

type AssignInput = { taskId: string; sessionId: string; open?: { profileId: string; collectionId: string; cwd: string; resumeReference: string; emptyWorkspace: boolean }; sendBrief: boolean };

function AssignDialog({ task, mission, collections, viewState, onClose, onAssign }: { task: MissionTask; mission: Mission; collections: Collection[]; viewState?: ViewStateStore; onClose(): void; onAssign(input: AssignInput, newConversation: boolean): Promise<void> }) {
  const { info } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  const ready = collections.filter(c => c.state === 'ready' && !c.ownershipUncertain);
  const conversations = useMemo(() => latestConversations(info?.sessions || []), [info?.sessions]);
  const profiles = info?.profiles || [];
  const [mode, setMode] = useState<'new' | 'existing'>(conversations.length && task.attempt ? 'existing' : 'new');
  const [collectionId, setCollectionId] = useState('');
  const [profileId, setProfileId] = useState(() => { const saved = readPreference(viewState, profileKey); return profiles.find(p => p.id === saved)?.id || profiles[0]?.id || ''; });
  const [sessionId, setSessionId] = useState('');
  const [sendBrief, setSendBrief] = useState(true);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const workspaceName = (id: string) => collections.find(c => c.id === id)?.name || 'No workspace';
  const submit = async (event: FormEvent) => {
    event.preventDefault(); if (!canSubmit) return; setBusy(true); setError('');
    try {
      if (mode === 'new') { try { writePreference(viewState, profileKey, profileId); } catch { /* a convenience */ } }
      await onAssign(mode === 'new' ? { taskId: task.id, sessionId: '', open: { profileId, collectionId, cwd: '', resumeReference: '', emptyWorkspace: false }, sendBrief } : { taskId: task.id, sessionId, sendBrief }, mode === 'new');
    } catch (e) { setError(message(e)); setBusy(false); }
  };
  const canSubmit = !busy && (mode === 'new' ? ready.some(c => c.id === collectionId) && profiles.some(p => p.id === profileId) : conversations.some(s => s.id === sessionId));
  return <DraggableDialog ref={dialog} className="mission-dialog" aria-label={`Assign ${task.title}`} onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader><h2>Assign “{task.title}”</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}>×</button></DialogHeader>
    <form onSubmit={submit}>
      <fieldset className="mission-assign-mode"><legend className="sr-only">Where the work happens</legend>
        <label><input type="radio" name="assign-mode" autoFocus={mode === 'new'} checked={mode === 'new'} onChange={() => setMode('new')}/> A new conversation</label>
        <label><input type="radio" name="assign-mode" autoFocus={mode === 'existing'} checked={mode === 'existing'} disabled={!conversations.length} onChange={() => setMode('existing')}/> A conversation you have</label>
      </fieldset>
      {mode === 'new' ? <>
        {!ready.length && <p className="missions-notice tone-warn">Create a workspace with the task's repositories first: a task works in a workspace's own branches.</p>}
        <label>Workspace<select aria-label="Workspace" value={collectionId} onChange={e => setCollectionId(e.target.value)}>{!collectionId && <option value="">Choose a workspace…</option>}{ready.map(c => <option key={c.id} value={c.id}>{c.name}</option>)}</select></label>
        <label>Agent<select aria-label="Agent" value={profileId} onChange={e => setProfileId(e.target.value)}><SubscriptionOptions profiles={profiles} label={(p, _, grouped) => { const name = agentTitle(p.provider, profiles); return grouped ? p.label || name : `${name}${p.label && p.label !== name ? ' · ' + p.label : ''}`; }}/></select></label>
        <p className="mission-muted">Every conversation in a workspace shares its files. Give parallel tasks their own workspaces.</p>
      </> : <label>Conversation<select aria-label="Conversation" value={sessionId} onChange={e => setSessionId(e.target.value)}><option value="">Choose a conversation…</option>{conversations.map(s => <option key={s.id} value={s.id}>{conversationName(s)} · {workspaceName(s.collectionId)}</option>)}</select></label>}
      <label className="mission-check"><input type="checkbox" checked={sendBrief} onChange={e => setSendBrief(e.target.checked)}/> Send the task's brief to the agent now</label>
      {sendBrief && <pre className="mission-brief-preview" aria-label="Brief to send">{task.title}{task.brief ? '\n\n' + task.brief : ''}{`\n\n(Task of the Lazybox mission “${mission.title}”. …)`}</pre>}
      {error && <p className="missions-error" role="alert">{error}</p>}
      <footer><button type="button" disabled={busy} onClick={onClose}>Cancel</button><button className="primary" disabled={!canSubmit}>{busy ? 'Assigning…' : mode === 'new' ? 'Start conversation' : 'Assign'}</button></footer>
    </form>
  </DraggableDialog>;
}

/** One row per conversation, its latest session; assistants cannot work on tasks. */
function latestConversations(sessions: readonly Session[]): Session[] {
  const rows = new Map<string, Session>();
  for (const s of sessions) {
    if (s.purpose && s.purpose !== 'coding') continue;
    const key = s.conversationId || s.id, seen = rows.get(key);
    if (!seen || s.createdAtUnixMillis > seen.createdAtUnixMillis || (!s.successorExecutionId && seen.successorExecutionId)) rows.set(key, s);
  }
  return [...rows.values()].filter(s => !s.successorExecutionId).sort((a, b) => Number(b.createdAtUnixMillis - a.createdAtUnixMillis));
}

function NoteDialog({ title, label, placeholder, confirm, body, required, danger, onClose, onConfirm }: { title: string; label: string; placeholder: string; confirm: string; body?: string; required?: boolean; danger?: boolean; onClose(): void; onConfirm(note: string): Promise<void> }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  const [note, setNote] = useState(''), [busy, setBusy] = useState(false), [error, setError] = useState('');
  const submit = async (event: FormEvent) => {
    event.preventDefault(); if (busy || (required && !note.trim())) return;
    setBusy(true); setError('');
    try { await onConfirm(note); } catch (e) { setError(message(e)); setBusy(false); }
  };
  return <DraggableDialog ref={dialog} className="mission-dialog" aria-label={title} onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader><h2>{title}</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}>×</button></DialogHeader>
    <form onSubmit={submit}>
      {body && <p className="mission-muted">{body}</p>}
      <label>{label}<textarea autoFocus rows={3} value={note} maxLength={2048} placeholder={placeholder} onChange={e => setNote(e.target.value)} onKeyDown={event => { if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) event.currentTarget.form?.requestSubmit(); }}/></label>
      {error && <p className="missions-error" role="alert">{error}</p>}
      <footer><button type="button" disabled={busy} onClick={onClose}>Back</button><button className={danger ? 'danger' : 'primary'} disabled={busy || (required && !note.trim())}>{busy ? 'Saving…' : confirm}</button></footer>
    </form>
  </DraggableDialog>;
}
