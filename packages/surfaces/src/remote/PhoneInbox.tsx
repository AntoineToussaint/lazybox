import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ConnectError, type DeviceSession } from '@lazybox/client-core';
import { Decision, type Collection, type Session } from '@lazybox/protocol';
import { useRunner } from '../RunnerContext';
import { useInbox, useSeen } from '../useAttention';
import { ageLabel, attentionTitle, seenMark, type Attention } from '../attention';
import { AttentionIcon } from '../Inbox';
import { conversationDetails, conversationName } from '../conversation-model';
import { useLiveStatus } from '../LiveIndicator';
import { Icon } from '../Icons';
import { chipAnswer, rowPlan, type RowPlan } from './row-plan';
import { usePins, type PinStore } from './pins';
import { isArchivedAway } from '../agents-model';
import { ActionSheet, ConfirmSheet, DenySheet, useRowActions, type ActionState, type SheetChoice } from './PhoneSheets';
import { SwipeRow, type SwipeAction } from './SwipeRow';
import { PushNudge } from './PhoneSettings';
import { PullRequestInboxSection, usePullInboxCount } from '../PullRequestInbox';
import type { Route } from './route';

export function PinGlyph({ filled, size = 18 }: { filled?: boolean; size?: number }) {
  return <svg width={size} height={size} viewBox="0 0 24 24" fill={filled ? 'currentColor' : 'none'} stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M9 3h6l-1 6 4 4H6l4-4z"/><path d="M12 13v8"/></svg>;
}

const DONE_SHOWN = 5;
const agentNameOf = (session: Session) => session.provider === 'codex' ? 'Codex' : 'Claude';

/**
 * The phone's Inbox: what needs the person first, and the answer right on the
 * row — Allow or Deny an approval, tap an option of a question — with Undo
 * before it is sent. Rows swipe (right: allow or pin; left: mark read, pin,
 * stop), a long press or ⋯ opens every action, pulling down refreshes.
 */
export function PhoneInbox({ device, collections, pins: pinStore, go, onRefresh }: { device: DeviceSession['device']; collections: Collection[]; pins: PinStore; go(route: Route): void; onRefresh(): Promise<void> }) {
  const inbox = useInbox();
  const { seen, mark, restore } = useSeen();
  const { info, api, refresh } = useRunner();
  const respond = device.grants.includes('respond');
  const pins = usePins(pinStore);
  const actions = useRowActions();
  const [allDone, setAllDone] = useState(false);
  const [now, setNow] = useState(Date.now);
  const [sheet, setSheet] = useState<ReactNode>();
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 30_000); return () => clearInterval(timer); }, []);
  useEffect(() => setNow(Date.now()), [inbox.rows]);
  const needs = inbox.counts['needs-input'];
  useEffect(() => { document.title = needs ? `(${needs}) Lazybox` : 'Lazybox'; }, [needs]);
  const workspace = (row: Attention) => collections.find(value => value.id === row.session.collectionId)?.name || '';
  const pulls = usePullInboxCount();
  const summary = needs ? `${needs} waiting for you` : inbox.counts.failed || inbox.counts.review || pulls ? `${inbox.counts.failed + inbox.counts.review + pulls} to look at` : inbox.rows.length ? 'Nothing needs you' : '';
  const close = () => setSheet(undefined);
  const title = (row: Attention) => conversationName(row.session);

  const apply = (row: Attention, operation: Parameters<typeof api.apply>[0]['operation']) => async () => {
    try { await api.apply({ sessionId: row.session.id, operationId: crypto.randomUUID(), operation }, { timeoutMs: 30000 }); refresh(); }
    catch (error) { throw new Error(ConnectError.from(error).rawMessage || String(error)); }
  };
  const allow = (row: Attention, plan: Extract<RowPlan, { kind: 'approval' }>, confirmed = false) => {
    if (plan.confirm && !confirmed) {
      setSheet(<ConfirmSheet title="Allow this?" tone="primary" confirm="Allow once" onClose={close} onConfirm={() => allow(row, plan, true)}
        detail={<><code className="remote-sheet-code">{plan.what}</code>{plan.risks.length > 0 && <ul className="remote-risks">{plan.risks.map(risk => <li key={risk.kind}><Icon name="alert" size={12}/>{risk.text}</li>)}</ul>}</>}/>);
      return;
    }
    actions.schedule({ id: plan.request.requestId, label: `Allowed · ${title(row)}`, waiting: 'Allowing…', commit: apply(row, { case: 'approval', value: { requestId: plan.request.requestId, decision: Decision.ACCEPT, always: '', reason: '' } }) });
  };
  const deny = (row: Attention, plan: Extract<RowPlan, { kind: 'approval' }>) => setSheet(<DenySheet what={<code className="remote-sheet-code">{plan.what}</code>} agentName={agentNameOf(row.session)} onClose={close}
    onDeny={reason => actions.schedule({ id: plan.request.requestId, label: '', waiting: 'Denying…', commit: apply(row, { case: 'approval', value: { requestId: plan.request.requestId, decision: Decision.DECLINE, always: '', reason } }) }, 0)}/>);
  const answer = (row: Attention, plan: Extract<RowPlan, { kind: 'question' }>, labels: string[]) => actions.schedule({
    id: plan.request.requestId, label: `Answered “${labels.join(', ')}”`, waiting: 'Answering…',
    commit: apply(row, { case: 'answer', value: { requestId: plan.request.requestId, answers: chipAnswer(plan.question, labels) } }),
  });
  const togglePin = (row: Attention) => {
    const was = pins.pinned.includes(row.key);
    pins.toggle(row.key);
    actions.notify(was ? `Unpinned · ${title(row)}` : `Pinned · ${title(row)}`, () => pins.toggle(row.key));
  };
  const markRead = (row: Attention) => {
    const previous = seen.marks[row.key];
    mark([{ key: row.key, at: seenMark(row) }]);
    actions.notify(`Marked read · ${title(row)}`, () => restore(row.key, previous));
  };
  const stop = (row: Attention) => setSheet(<ConfirmSheet title="Stop this turn?" detail={`${agentNameOf(row.session)} stops what it is doing in “${title(row)}”. The conversation stays, and you can reply to continue.`} confirm="Stop" onClose={close}
    onConfirm={() => void apply(row, { case: 'interrupt', value: {} })().then(() => actions.notify(`Stopped · ${title(row)}`), error => actions.notify(`Not stopped: ${error instanceof Error ? error.message : String(error)}`, undefined, 'error'))}/>);

  const rowActions = (row: Attention, plan: RowPlan) => {
    const pinned = pins.pinned.includes(row.key);
    const canAllow = plan.kind === 'approval' && plan.allow && !actions.states[plan.request.requestId];
    const pin: SwipeAction = { id: 'pin', label: pinned ? 'Unpin' : 'Pin', glyph: <PinGlyph filled={!pinned}/>, tone: 'pin', run: () => togglePin(row) };
    const leading: SwipeAction | undefined = canAllow ? { id: 'allow', label: 'Allow', glyph: <Icon name="check" size={20}/>, tone: 'allow', run: () => allow(row, plan) } : pins.writable ? pin : undefined;
    const trailing: SwipeAction[] = [];
    if (row.unread && row.state !== 'needs-input') trailing.push({ id: 'read', label: 'Read', glyph: <Icon name="eye" size={20}/>, tone: 'read', run: () => markRead(row) });
    if (leading?.id === 'allow' && pins.writable) trailing.push(pin);
    if (respond && row.state === 'working' && !row.session.closed) trailing.push({ id: 'stop', label: 'Stop', glyph: <Icon name="stop" size={20}/>, tone: 'stop', run: () => stop(row) });
    return { leading, trailing };
  };
  const openSheet = (row: Attention, plan: RowPlan) => {
    const choices: SheetChoice[] = [{ id: 'open', label: 'Open conversation', icon: 'chat', run: () => go({ session: row.session.id }) }];
    if (plan.kind === 'approval' && plan.allow && !actions.states[plan.request.requestId]) choices.push({ id: 'allow', label: plan.confirm ? 'Allow once…' : 'Allow once', icon: 'check', tone: 'primary', run: () => allow(row, plan) });
    if (plan.kind === 'approval' && !actions.states[plan.request.requestId]) choices.push({ id: 'deny', label: 'Deny…', icon: 'close', run: () => deny(row, plan) });
    if (pins.writable) choices.push({ id: 'pin', label: pins.pinned.includes(row.key) ? 'Unpin' : 'Pin', glyph: <PinGlyph/>, run: () => togglePin(row) });
    if (row.unread && row.state !== 'needs-input') choices.push({ id: 'read', label: 'Mark as read', icon: 'eye', run: () => markRead(row) });
    if (respond && row.state === 'working' && !row.session.closed) choices.push({ id: 'stop', label: 'Stop this turn…', icon: 'stop', tone: 'danger', run: () => stop(row) });
    setSheet(<ActionSheet title={title(row)} detail={[workspace(row), row.reason].filter(Boolean).join(' · ')} choices={choices} onClose={close}/>);
  };

  // Pinned conversations stay together below what needs the person: a pin
  // never hides a request. Archived ones (on the computer) are not listed,
  // unless one waits for the person.
  const sections = useMemo(() => {
    const shown = (row: Attention) => !isArchivedAway(row.session);
    const pinnedRows = inbox.rows.filter(row => shown(row) && row.state !== 'needs-input' && pins.pinned.includes(row.key)).sort((a, b) => pins.pinned.indexOf(a.key) - pins.pinned.indexOf(b.key));
    const out: { id: string; title: string; state?: Attention['state']; rows: Attention[] }[] = [];
    for (const group of inbox.groups) {
      const rows = group.rows.filter(row => shown(row) && (group.state === 'needs-input' || !pins.pinned.includes(row.key)));
      if (rows.length) out.push({ id: group.state, title: attentionTitle[group.state], state: group.state, rows });
      if (group.state === 'needs-input' && pinnedRows.length) out.push({ id: 'pinned', title: 'Pinned', rows: pinnedRows });
    }
    if (pinnedRows.length && !out.some(section => section.id === 'pinned')) out.unshift({ id: 'pinned', title: 'Pinned', rows: pinnedRows });
    return out;
  }, [inbox, pins.pinned]);

  return <main className="remote-inbox" aria-labelledby="remote-inbox-title">
    <header className="remote-bar">
      <span className="remote-brand" aria-hidden="true">✳︎</span>
      <div className="remote-bar-title"><h1 id="remote-inbox-title">Inbox</h1>{summary && <span role="status">{summary}</span>}</div>
      <button className="remote-icon-button" aria-label="Settings" onClick={() => go({ screen: 'settings' })}><Icon name="sliders" size={19}/></button>
    </header>
    <PullToRefresh onRefresh={onRefresh}>
      <PushNudge onOpenSettings={() => go({ screen: 'settings' })}/>
      <PullRequestInboxSection collections={collections} phone canRespond={respond} defer={action => actions.schedule(action)} onOpen={target => go(target.sessionId ? { session: target.sessionId } : {})}/>
      {!info ? <p className="remote-empty" role="status">Loading conversations…</p>
        : !inbox.rows.length ? <div className="remote-empty"><Icon name="inbox" size={28}/><strong>No conversations yet</strong><p>Agents that need you will show up here.</p></div>
        : <>
          {!needs && !pulls && !inbox.counts.failed && !inbox.counts.review && <p className="remote-caught-up"><Icon name="check" size={14}/>Nothing needs you right now. Your agents keep working.</p>}
          {sections.map(section => {
            const rows = section.state === 'idle' && !allDone ? section.rows.slice(0, DONE_SHOWN) : section.rows;
            return <section key={section.id} className={`remote-group group-${section.id}`} aria-labelledby={`remote-group-${section.id}`}>
              <h2 id={`remote-group-${section.id}`}>{section.state ? <AttentionIcon state={section.state} size={13}/> : <PinGlyph size={13} filled/>}{section.title}<span>{section.rows.length}</span></h2>
              <ul>{rows.map(row => {
                const plan = rowPlan(row, respond);
                const { leading, trailing } = rowActions(row, plan);
                const state = 'request' in plan ? actions.states[plan.request.requestId] : undefined;
                return <SwipeRow key={row.key} leading={leading} trailing={trailing} onLongPress={() => openSheet(row, plan)} aria-label={title(row)}
                  data-state={row.state} data-unread={row.unread || undefined} data-pinned={pins.pinned.includes(row.key) || undefined}>
                  <RowBody row={row} plan={plan} workspace={workspace(row)} now={now} pinned={pins.pinned.includes(row.key)} state={state} profiles={info.profiles}
                    onOpen={() => go({ session: row.session.id })} onMore={() => openSheet(row, plan)}
                    onAllow={() => plan.kind === 'approval' && allow(row, plan)} onDeny={() => plan.kind === 'approval' && deny(row, plan)}
                    onAnswer={labels => plan.kind === 'question' && answer(row, plan, labels)} onUndo={() => 'request' in plan && actions.cancel(plan.request.requestId)}/>
                </SwipeRow>;
              })}</ul>
              {rows.length < section.rows.length && <button className="remote-more" onClick={() => setAllDone(true)}>Show {section.rows.length - rows.length} more</button>}
            </section>;
          })}
        </>}
    </PullToRefresh>
    {sheet}
    {actions.snackbar}
  </main>;
}

function RowBody({ row, plan, workspace, now, pinned, state, profiles, onOpen, onMore, onAllow, onDeny, onAnswer, onUndo }: {
  row: Attention; plan: RowPlan; workspace: string; now: number; pinned: boolean; state?: ActionState; profiles: Parameters<typeof conversationDetails>[1];
  onOpen(): void; onMore(): void; onAllow(): void; onDeny(): void; onAnswer(labels: string[]): void; onUndo(): void;
}) {
  const title = conversationName(row.session);
  return <>
    <div className="remote-row-head">
      <button className="remote-row-open" onClick={onOpen}>
        <AttentionIcon state={row.state} size={18}/>
        <span className="remote-row-main">
          <span className="remote-row-title">{row.unread && row.state !== 'needs-input' && <i className="unread-dot" aria-label="unread"/>}<strong>{title}</strong>{pinned && <span className="remote-pin-mark" aria-label="pinned"><PinGlyph size={12} filled/></span>}</span>
          <span className="remote-row-meta">{[workspace, conversationDetails(row.session, profiles, false)].filter(Boolean).join(' · ')}</span>
          {(row.line || row.state !== 'idle') && plan.kind === 'none' && <span className="remote-row-line">{row.state === 'working' ? <LiveText session={row.session}/> : <em>{row.reason}</em>}{row.line && <> · {row.line}</>}</span>}
          {plan.kind !== 'none' && <span className="remote-row-line"><em>{row.reason}</em>{plan.kind === 'question' && <> · {plan.question.text}</>}{plan.kind === 'elsewhere' && <> · {row.line}</>}</span>}
        </span>
      </button>
      <div className="remote-row-side">
        <time dateTime={row.at ? new Date(row.at).toISOString() : undefined}>{ageLabel(row.at, now)}</time>
        <button className="remote-row-more" aria-label={`Actions for ${title}`} onClick={onMore}><Icon name="more" size={18}/></button>
      </div>
    </div>
    {plan.kind !== 'none' && <div className="remote-row-act">
      {state ? <RowProgress state={state} onUndo={onUndo}/>
        : plan.kind === 'approval' ? <>
          <code className="remote-row-what" title={plan.what}>{plan.what}</code>
          {plan.risks.length > 0 && <ul className="remote-risks" aria-label="Worth a second look">{plan.risks.map(risk => <li key={risk.kind}><Icon name="alert" size={12}/>{risk.text}</li>)}</ul>}
          {plan.allow ? <div className="remote-row-buttons">
            <button type="button" onClick={onDeny}>Deny</button>
            <button type="button" className="primary" onClick={onAllow}>{plan.confirm ? 'Allow…' : 'Allow'}</button>
          </div> : <div className="remote-row-buttons"><small>{plan.openReason}</small><button type="button" onClick={onOpen}>Review</button></div>}
        </>
        : plan.kind === 'question' ? <QuestionChips plan={plan} onAnswer={onAnswer} onOpen={onOpen}/>
        : <p className="remote-row-elsewhere"><Icon name="phone" size={13}/>{plan.text}</p>}
    </div>}
  </>;
}

function RowProgress({ state, onUndo }: { state: ActionState; onUndo(): void }) {
  return <div className="remote-row-progress" data-phase={state.phase} role="status">
    {state.phase === 'sent' ? <><Icon name="check" size={14}/>Sent · waiting for the agent</> : <><span className="remote-spinner" aria-hidden="true"/>{state.text}</>}
    {state.phase === 'waiting' && <button type="button" onClick={onUndo}>Undo</button>}
  </div>;
}

function QuestionChips({ plan, onAnswer, onOpen }: { plan: Extract<RowPlan, { kind: 'question' }>; onAnswer(labels: string[]): void; onOpen(): void }) {
  const [picked, setPicked] = useState<string[]>([]);
  if (plan.mode === 'open') return <div className="remote-row-buttons"><small>{plan.count > 1 ? `${plan.count} questions` : plan.question.options.length ? `${plan.question.options.length} options` : 'Answer in your words'}</small><button type="button" className="primary" onClick={onOpen}>Answer</button></div>;
  const toggle = (label: string) => setPicked(all => all.includes(label) ? all.filter(value => value !== label) : [...all, label]);
  return <div className="remote-chips" role={plan.mode === 'multi' ? 'group' : 'radiogroup'} aria-label={plan.question.text}>
    {plan.question.options.map(option => plan.mode === 'multi'
      ? <button key={option.label} type="button" role="checkbox" aria-checked={picked.includes(option.label)} onClick={() => toggle(option.label)}>{picked.includes(option.label) && <Icon name="check" size={13}/>}{option.label}</button>
      : <button key={option.label} type="button" role="radio" aria-checked="false" title={option.description} onClick={() => onAnswer([option.label])}>{option.label}</button>)}
    {plan.question.other && <button type="button" className="remote-chip-other" onClick={onOpen}>Other…</button>}
    {plan.mode === 'multi' && <button type="button" className="primary remote-chip-send" disabled={!picked.length} onClick={() => onAnswer(picked)}>Send</button>}
  </div>;
}

function LiveText({ session }: { session: Session }) {
  const status = useLiveStatus(session);
  return <em className="remote-live">{status.text}</em>;
}

/**
 * Pull down at the top of the Inbox to ask the computer now. The list moves
 * with the finger, the indicator turns once it will refresh, and it stays
 * until the computer answered.
 */
function PullToRefresh({ onRefresh, children }: { onRefresh(): Promise<void>; children: ReactNode }) {
  const [pull, setPull] = useState(0);
  const [busy, setBusy] = useState(false);
  const start = useRef<{ y: number; x: number; active: boolean } | null>(null);
  const threshold = 72;
  const onTouchStart = (event: React.TouchEvent) => { if (window.scrollY <= 0 && !busy) start.current = { y: event.touches[0].clientY, x: event.touches[0].clientX, active: false }; };
  const onTouchMove = (event: React.TouchEvent) => {
    const s = start.current; if (!s) return;
    const dy = event.touches[0].clientY - s.y, dx = Math.abs(event.touches[0].clientX - s.x);
    if (!s.active) { if (dy > 8 && dy > dx * 1.5 && window.scrollY <= 0) s.active = true; else if (dy < 0 || dx > 12) { start.current = null; return; } else return; }
    const next = Math.min(120, dy * 0.5);
    if ((next >= threshold) !== (pull >= threshold) && next >= threshold) { try { navigator.vibrate?.(8); } catch { /* no haptics */ } }
    setPull(Math.max(0, next));
  };
  const onTouchEnd = () => {
    const s = start.current; start.current = null;
    if (!s?.active) return;
    if (pull >= threshold) { setBusy(true); setPull(48); void onRefresh().finally(() => { setBusy(false); setPull(0); }); }
    else setPull(0);
  };
  return <div className="remote-pull" onTouchStart={onTouchStart} onTouchMove={onTouchMove} onTouchEnd={onTouchEnd} onTouchCancel={onTouchEnd} data-pulling={pull > 0 || undefined}>
    <div className="remote-pull-indicator" aria-hidden={!busy} role={busy ? 'status' : undefined} style={{ height: pull, opacity: Math.min(1, pull / threshold) }}>
      <span className={busy ? 'remote-spinner' : 'remote-pull-arrow'} style={busy ? undefined : { transform: `rotate(${pull >= threshold ? 180 : 0}deg)` }}>{!busy && <Icon name="refresh" size={16}/>}</span>
      <small>{busy ? 'Refreshing…' : pull >= threshold ? 'Release to refresh' : 'Pull to refresh'}</small>
    </div>
    <div style={{ transform: pull && !busy ? `translateY(${pull * 0.15}px)` : undefined }}>{children}</div>
  </div>;
}
