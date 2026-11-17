import { useMemo, useRef, useState, type KeyboardEvent } from 'react';
import type { Mission, MissionEvent } from '@lazybox/protocol';
import { Icon } from './Icons';
import { useOpenConversation } from './AgentMessages';
import { lanesOf, messageThreads, type Lane } from './orchestration-model';
import { ago } from './missions-model';
import './orchestration.css';

export type MissionTab = 'board' | 'timeline' | 'messages' | 'leases';
const TABS: { key: MissionTab; label: string }[] = [{ key: 'board', label: 'Board' }, { key: 'timeline', label: 'Timeline' }, { key: 'messages', label: 'Messages' }, { key: 'leases', label: 'Leases' }];

/** The board's views, as tabs: ←/→ move between them, Home/End jump. */
export function MissionTabs({ mission, tab, onTab }: { mission: Mission; tab: MissionTab; onTab(tab: MissionTab): void }) {
  const refs = useRef<Record<string, HTMLButtonElement | null>>({});
  const counts: Partial<Record<MissionTab, number>> = { messages: mission.messages.length, leases: mission.leases.length + mission.overlaps.length };
  const key = (event: KeyboardEvent<HTMLDivElement>) => {
    const i = TABS.findIndex(t => t.key === tab);
    const next = event.key === 'ArrowRight' ? (i + 1) % TABS.length : event.key === 'ArrowLeft' ? (i + TABS.length - 1) % TABS.length : event.key === 'Home' ? 0 : event.key === 'End' ? TABS.length - 1 : -1;
    if (next < 0) return;
    event.preventDefault();
    onTab(TABS[next].key);
    refs.current[TABS[next].key]?.focus();
  };
  return <div className="mission-tabs" role="tablist" aria-label="Mission views" onKeyDown={key}>
    {TABS.map(t => <button key={t.key} ref={node => { refs.current[t.key] = node; }} type="button" role="tab" id={`mission-tab-${t.key}`} aria-selected={tab === t.key} aria-controls={`mission-view-${t.key}`} tabIndex={tab === t.key ? 0 : -1} onClick={() => onTab(t.key)}>
      {t.label}{counts[t.key] ? <span>{counts[t.key]}</span> : null}
    </button>)}
  </div>;
}

/**
 * Every event of the mission across every agent, oldest first: one lane per
 * actor (you, Lazybox and rules, each orchestrator and agent), a dot where
 * each acted, and what an event reacts to one click away. Nobody else shows
 * a causal timeline across parent, children and messages.
 */
export function MissionTimeline({ mission, now }: { mission: Mission; now: number }) {
  const events = useMemo(() => [...mission.events].sort((a, b) => Number(a.seq - b.seq)), [mission.events]);
  const lanes = useMemo(() => lanesOf(mission, events), [mission, events]);
  const [lit, setLit] = useState('');
  const byId = useMemo(() => new Map(events.map(e => [e.id, e])), [events]);
  const open = useOpenConversation();
  const show = (id: string) => {
    setLit(id);
    document.getElementById(`timeline-${id}`)?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  };
  if (!events.length) return <p className="mission-muted">Nothing has happened yet.</p>;
  const columns = `56px repeat(${lanes.length}, 22px) minmax(0, 1fr)`;
  return <div className="mission-timeline" role="table" aria-label="Mission timeline" style={{ ['--lanes' as string]: lanes.length }}>
    <div className="timeline-head" role="row" style={{ gridTemplateColumns: columns }}>
      <span role="columnheader" aria-label="When" className="timeline-when"/>
      {lanes.map(l => <span key={l.key} role="columnheader" className={`timeline-lane-head tone-${l.tone}`} title={l.title}>
        {l.conversation ? <button type="button" onClick={() => open(l.conversation!)} aria-label={`Open ${l.title}`}>{l.short}</button> : <span>{l.short}</span>}
      </span>)}
      <span role="columnheader" className="timeline-what">What happened</span>
    </div>
    <ol className="timeline-rows">{events.map(e => {
      const lane = laneOf(lanes, e);
      const cause = e.causedBy ? byId.get(e.causedBy) : undefined;
      return <li key={e.id || String(e.seq)} id={`timeline-${e.id}`} role="row" className={`timeline-row${lit === e.id ? ' lit' : ''}${e.known ? '' : ' unknown'}`} style={{ gridTemplateColumns: columns }}>
        <time role="cell" title={new Date(Number(e.atUnixMillis)).toLocaleString()}>{ago(e.atUnixMillis, now)}</time>
        {lanes.map((l, i) => <span key={l.key} role="cell" className={`timeline-cell${i === lane ? ` on tone-${l.tone}` : ''}`} aria-label={i === lane ? l.title : undefined}>{i === lane && <span className="timeline-dot" aria-hidden="true"/>}</span>)}
        <span role="cell" className="timeline-summary">
          <span title={`${e.type} v${e.version}`}>{e.summary}</span>
          {cause && <button type="button" className="timeline-cause" onClick={() => show(cause.id)} title={`Caused by: ${cause.summary}`}><Icon name="back" size={10}/> after #{String(cause.seq)}</button>}
        </span>
      </li>;
    })}</ol>
  </div>;
}

function laneOf(lanes: Lane[], e: MissionEvent): number {
  const i = lanes.findIndex(l => l.match(e));
  return i < 0 ? lanes.length - 1 : i;
}

/** Every message between the mission's agents, one thread per pair, with who sent what. */
export function MissionMessages({ mission, now }: { mission: Mission; now: number }) {
  const threads = useMemo(() => messageThreads(mission.messages), [mission.messages]);
  const open = useOpenConversation();
  if (!threads.length) return <p className="mission-muted">No messages between agents yet. An orchestrator and the agents it started talk with <code>lazybox agents send</code>; each message lands here and in both conversations.</p>;
  return <div className="mission-messages">{threads.map(t => <section key={t.key} className="message-thread" aria-label={`${t.a.label} and ${t.b.label}`}>
    <h5><button type="button" className="link-button" onClick={() => open(t.a.conversation)}>{t.a.label}</button><Icon name="send" size={11}/><button type="button" className="link-button" onClick={() => open(t.b.conversation)}>{t.b.label}</button><span>{t.messages.length}</span></h5>
    <ol>{t.messages.map(m => <li key={m.id} className={`thread-message kind-${m.kind}${m.fromConversationId === t.a.conversation ? ' side-a' : ' side-b'}`}>
      <span className="thread-from">{m.fromLabel || 'Agent'}{m.kind === 'report' && <b> reported</b>}<time>{ago(m.atUnixMillis, now)}</time></span>
      <p>{m.text}</p>
    </li>)}</ol>
  </section>)}</div>;
}

/** Who leased which paths, and where agents' changes met. */
export function MissionLeases({ mission }: { mission: Mission }) {
  const title = (id: string) => mission.tasks.find(t => t.id === id)?.title || id;
  return <div className="mission-leases">
    <h5>Leases <span>advisory: nothing is locked</span></h5>
    {!mission.leases.length ? <p className="mission-muted">No leases. An agent claims the paths its task will change with <code>lazybox lease claim &lt;path&gt; [--exclusive]</code>.</p>
      : <table><thead><tr><th>Path</th><th>Mode</th><th>Task</th><th>Until</th></tr></thead><tbody>{mission.leases.map(l => <tr key={l.id}>
        <td><code>{l.path}</code></td><td><span className={`lease-mode mode-${l.mode}`}>{l.mode}</span></td><td title={l.reason}>{l.taskTitle}</td><td>{new Date(Number(l.expiresAtUnixMillis)).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</td>
      </tr>)}</tbody></table>}
    <h5>Overlaps <span>seen after each turn, from what each agent changed</span></h5>
    {!mission.overlaps.length ? <p className="mission-muted">None so far.</p>
      : <ul className="overlap-list">{mission.overlaps.map((o, i) => <li key={i}><Icon name="alert" size={12}/><span><b>{o.taskIds.map(title).join(' and ')}</b> {o.kind === 'lease' ? 'met in an exclusive lease:' : 'both changed'} {o.paths.map(p => <code key={p}>{p}</code>)}</span></li>)}</ul>}
    <p className="mission-muted">Overlaps are flagged, never blocked: the orchestrator is told and decides to sequence, merge or hand off.</p>
  </div>;
}
