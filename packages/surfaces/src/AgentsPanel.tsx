import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import type { Collection, Session } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { Icon } from './Icons';
import { AttentionIcon, LiveText } from './Inbox';
import { conversationDetails, conversationName } from './conversation-model';
import { ageLabel, attentionTitle, conversationKey, seenMark, type Attention } from './attention';
import { agentSections, isPinned, lineageLabel, liveSubagents, metaLine, nestByLineage, visibleRows, type Grouping } from './agents-model';
import { useMissionAttention } from './MissionInbox';
import { useRunner } from './RunnerContext';
import { RunnerBadge, RunnerScope, useMergedInbox, useRunnerActionHosts, useRunnerDirectory } from './RunnerDirectory';
import { healthLabel, rowTarget, type RunnerRow } from './runner-directory';
import { readPreference, writePreference } from './view-state';
import { contextMenu } from './ContextMenu';
import { peekProps } from './peek-model';
import { InlineConversationRename, useConversationActions } from './ConversationActions';
import { conversationRowGestures, useAgentsFocusRequests } from './conversation-requests';
import { LiveIndicator } from './LiveIndicator';
import { MissionInboxSection } from './MissionInbox';
import { PullRequestInboxSection, usePullInboxCount } from './PullRequestInbox';
import type { ViewTarget } from './workspace-model';
import './agents-panel.css';

const read = <T,>(store: ViewStateStore | undefined, name: string, fallback: T): T => { try { const value = JSON.parse(readPreference(store, name) || 'null'); return value ?? fallback; } catch { return fallback; } };
const write = (store: ViewStateStore | undefined, name: string, value: unknown) => { try { writePreference(store, name, JSON.stringify(value)); } catch { /* a convenience */ } };
const strings = (value: unknown) => Array.isArray(value) ? value.filter((key): key is string => typeof key === 'string') : [];

/**
 * Agents: every conversation across workspaces, most urgent first (needs you,
 * failed, ready for review, working, done), always on screen under
 * Workspaces. Pins are the Runner's, so the phone shows them too; a
 * conversation's running sub-agents nest under it. Type to filter; j/k move,
 * Enter opens, Space peeks, p pins, m marks read, F2 renames, → shows
 * sub-agents, / filters.
 */
export function AgentsPanel({ collections, target, viewState, onShowSession }: { collections: Collection[]; target: ViewTarget; viewState?: ViewStateStore; onShowSession?(target: ViewTarget): void }) {
  // Every Runner the window shows, most urgent first across them; each row acts on its own Runner.
  const inbox = useMergedInbox();
  const directory = useRunnerDirectory();
  const { api, info, refresh } = useRunner();
  const [savedGrouping, setGrouping] = useState<Grouping>(() => { const value = read<string>(viewState, 'agents.grouping', 'priority'); return value === 'workspace' || value === 'runner' ? value : 'priority'; });
  const grouping: Grouping = savedGrouping === 'runner' && !inbox.multi ? 'priority' : savedGrouping;
  const [folded, setFolded] = useState<string[]>(() => strings(read<unknown>(viewState, 'agents.folded', ['idle'])));
  const [expanded, setExpanded] = useState<string[]>([]);
  const [filter, setFilter] = useState('');
  const [renaming, setRenaming] = useState('');
  const panel = useRef<HTMLDivElement>(null), filterInput = useRef<HTMLInputElement>(null);
  const open = (row: RunnerRow) => onShowSession?.(row.runner === inbox.here ? { collectionId: row.session.collectionId, sessionId: row.session.id, commandId: '' } : rowTarget(row));
  const actions = useConversationActions({ onOpen: next => onShowSession?.(next) });
  // Another Runner's rows act through that Runner (its pins, renames, deletes).
  const others = useRunnerActionHosts(() => useConversationActions({ onOpen: next => onShowSession?.(next) }), value => value.dialogs);
  const actionsFor = (row: RunnerRow) => row.runner === inbox.here ? actions : others.get(row.runner);
  // Ages read in minutes: a tick every 30 s, and whenever the rows change.
  const [now, setNow] = useState(Date.now);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 30_000); return () => clearInterval(timer); }, []);
  useEffect(() => { setNow(Date.now()); }, [inbox.rows]);
  const collectionsOf = (row: Attention) => !row.runner || row.runner === inbox.here ? collections : directory?.runners.find(value => value.id === row.runner)?.collections || [];
  const runnerName = (row: Attention) => directory?.runners.find(value => value.id === row.runner)?.label || 'Runner';
  const workspace = (row: Attention) => collectionsOf(row).find(value => value.id === row.session.collectionId)?.name || (row.session.collectionId ? 'Unavailable workspace' : 'No workspace');
  const sections = useMemo(() => agentSections(inbox, grouping, workspace, filter, runnerName), [inbox, grouping, collections, filter, directory?.runners]);
  const filtering = !!filter.trim();
  // While filtering every match shows: a folded section would hide what was asked for.
  const closed = filtering ? [] : folded;

  // Pins kept in this window before the Runner kept them move to the Runner, once.
  const migrated = useRef(false);
  useEffect(() => {
    if (migrated.current || !info) return;
    migrated.current = true;
    const local = strings(read<unknown>(viewState, 'agents.pinned', []));
    if (!local.length) return;
    const sessions = info.sessions.filter(session => local.includes(conversationKey(session)) && !isPinned(session));
    void Promise.allSettled(sessions.map(session => api.pinConversation({ sessionId: session.id, pinned: true }))).then(results => {
      if (results.every(result => result.status === 'fulfilled')) write(viewState, 'agents.pinned', []);
      refresh();
    });
  }, [info, api, refresh, viewState]);

  const toggleFold = (id: string) => { const next = folded.includes(id) ? folded.filter(value => value !== id) : [...folded, id]; setFolded(next); write(viewState, 'agents.folded', next); };
  const toggleExpanded = (key: string, show = !expanded.includes(key)) => setExpanded(list => show ? [...new Set([...list, key])] : list.filter(value => value !== key));
  const choose = (next: Grouping) => { setGrouping(next); write(viewState, 'agents.grouping', next); };
  const unread = inbox.rows.filter(row => row.unread && row.state !== 'needs-input');
  const markAll = () => inbox.mark(unread.map(row => ({ key: row.key, at: seenMark(row) })));
  const rowOf = (key?: string) => inbox.rows.find(row => row.key === key);
  const rowsByKey = useMemo(() => new Map(inbox.rows.map(row => [row.key, row])), [inbox.rows]);
  const runnerRow = (row: Attention) => rowsByKey.get(row.key) || { ...row, runner: inbox.here };
  const focusable = () => [...panel.current?.querySelectorAll<HTMLElement>('.agent-row, .agent-subagent') || []];
  // A plan to approve or a task to accept comes first: it is what waits on the person.
  const focusFirst = () => { const first = panel.current?.querySelector<HTMLElement>('.inbox-missions .inbox-task-actions button, .agent-row'); (first || filterInput.current)?.focus({ preventScroll: false }); first?.scrollIntoView({ block: 'nearest' }); };
  useAgentsFocusRequests(focusFirst);

  const keyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const element = event.target as HTMLElement;
    if (element === filterInput.current) {
      if (event.key === 'ArrowDown') { event.preventDefault(); focusFirst(); }
      else if (event.key === 'Enter') { event.preventDefault(); const first = visibleRows(sections, closed)[0]; if (first) open(runnerRow(first)); }
      else if (event.key === 'Escape' && filter) { event.preventDefault(); event.stopPropagation(); setFilter(''); }
      return;
    }
    if (element.matches('input,textarea,select')) return;
    const rows = focusable(), index = rows.indexOf(document.activeElement as HTMLElement);
    const step = ({ j: 1, ArrowDown: 1, k: -1, ArrowUp: -1 } as Record<string, number>)[event.key];
    if (step) { event.preventDefault(); const next = rows[Math.max(0, Math.min(rows.length - 1, index < 0 ? 0 : index + step))]; next?.focus(); next?.scrollIntoView({ block: 'nearest' }); return; }
    if (event.key === 'Home' || event.key === 'End') { event.preventDefault(); (event.key === 'Home' ? rows[0] : rows.at(-1))?.focus(); return; }
    if (event.key === '/') { event.preventDefault(); filterInput.current?.focus(); return; }
    if (event.key === 'Escape' && filter) { event.preventDefault(); setFilter(''); return; }
    const key = element.closest<HTMLElement>('[data-key]')?.dataset.key, row = rowOf(key), acts = row && actionsFor(row);
    if (!row || !acts) return;
    if (event.key === 'p') { event.preventDefault(); void acts.pin(row.session); }
    else if (event.key === 'm') { if (acts.unread(row.session)) { event.preventDefault(); acts.markRead(row.session); } }
    else if (event.key === 'F2') { event.preventDefault(); setRenaming(row.key); }
    else if (event.key === 'ArrowRight' && liveSubagents(row.session).length) { event.preventDefault(); toggleExpanded(row.key, true); }
    else if (event.key === 'ArrowLeft') { event.preventDefault(); toggleExpanded(row.key, false); element.closest<HTMLElement>('[data-key]')?.querySelector<HTMLElement>('.agent-row')?.focus(); }
  };

  const missionAttention = useMissionAttention();
  const waiting = inbox.counts['needs-input'] + missionAttention;
  const visibleCount = sections.reduce((sum, section) => sum + section.rows.length, 0);
  // Three grouping choices leave less room: the short form says the same.
  const pulls = usePullInboxCount();
  const summary = filtering ? `${visibleCount} of ${inbox.rows.length}` : waiting ? `${waiting} waiting${inbox.multi ? '' : ' for you'}` : unread.length ? `${unread.length} to review` : pulls ? `${pulls} ${pulls === 1 ? 'pull request' : 'pull requests'} to look at` : inbox.rows.length ? 'Nothing needs you' : '';
  return <div ref={panel} className="agents-panel" onKeyDown={keyDown} data-peek-list>
    <header className="agents-heading">
      <label className="agents-filter">
        <Icon name="search" size={13}/>
        <input ref={filterInput} type="search" aria-label="Filter agents" placeholder="Filter agents" autoComplete="off" spellCheck={false} value={filter} onChange={event => setFilter(event.target.value)}/>
        {filter && <button type="button" className="icon-button agents-filter-clear" aria-label="Clear filter" title="Clear (Esc)" onClick={() => { setFilter(''); filterInput.current?.focus(); }}><Icon name="close" size={11}/></button>}
      </label>
      <button className="icon-button agents-mark-all" aria-label="Mark all as read" title={unread.length ? `Mark ${unread.length} as read` : 'Nothing to mark as read'} disabled={!unread.length} onClick={markAll}><Icon name="checkAll" size={14}/></button>
    </header>
    <div className="agents-toolbar">
      <div className="agents-grouping" role="group" aria-label="Group agents">
        <button aria-pressed={grouping === 'priority'} onClick={() => choose('priority')}>Priority</button>
        <button aria-pressed={grouping === 'workspace'} onClick={() => choose('workspace')}>{inbox.multi ? 'Workspace' : 'By workspace'}</button>
        {inbox.multi && <button aria-pressed={grouping === 'runner'} onClick={() => choose('runner')}>Runner</button>}
      </div>
      <span className="agents-summary" role="status" data-urgent={waiting > 0 || undefined}>{summary}</span>
    </div>
    <div className="agents-list">
    {onShowSession && <MissionInboxSection onOpen={onShowSession}/>}
    {onShowSession && <PullRequestInboxSection collections={collections} onOpen={onShowSession}/>}
    {inbox.offline.map(row => <p key={row.id} className="agents-runner-offline" role="status" data-health={row.health}><RunnerBadge runner={row.id}/>{healthLabel[row.health]}. Its agents show here again once it answers.</p>)}
    {!inbox.rows.length && <div className="agents-empty"><Icon name="activity" size={20}/><p>Agents from every workspace show up here, the ones that need you first.</p></div>}
    {inbox.rows.length > 0 && filtering && !visibleCount && <div className="agents-empty"><p>No agent matches “{filter.trim()}”.</p><button onClick={() => setFilter('')}>Clear filter</button></div>}
    {sections.map(section => {
      const shut = closed.includes(section.id);
      return <section key={section.id} className="agents-group" data-group={section.id} aria-label={section.title}>
        <button className="agents-group-title" aria-expanded={!shut} title={shut ? `Show ${section.title}` : `Fold ${section.title}`} onClick={() => toggleFold(section.id)}>
          <Icon name={shut ? 'chevronRight' : 'chevron'} size={11}/>
          {section.state ? <AttentionIcon state={section.state} size={12}/> : section.id === 'pinned' ? <span className="agents-group-pin" aria-hidden="true"><Icon name="pin" size={11}/></span> : null}
          <span className="agents-group-name">{section.title}</span><b>{section.rows.length}</b>
        </button>
        {!shut && <ul>{nestByLineage(section.rows).map(({ row: listed, depth }) => {
          const row = runnerRow(listed), acts = actionsFor(row), elsewhere = row.runner !== inbox.here;
          const title = conversationName(row.session), pinned = isPinned(row.session), dot = row.unread && row.state !== 'needs-input';
          const subagents = liveSubagents(row.session), open_ = expanded.includes(row.key);
          const lineage = row.session.lineage, from = depth ? '' : lineageLabel(row.session);
          return <li key={row.key} data-key={row.key} data-state={row.state} data-unread={dot || undefined} data-pinned={pinned || undefined} data-depth={depth || undefined} data-orchestrator={lineage?.grantId ? '' : undefined}
            data-runner={inbox.multi ? row.runner : undefined}
            onContextMenu={contextMenu('conversation', () => acts?.items(row.session, { rename: () => setRenaming(row.key) }) || [], { sessionId: row.session.id, workspaceId: row.session.collectionId }, `Conversation ${title}`)}>
            {renaming === row.key
              ? <div className="agent-renaming"><AttentionIcon state={row.state} size={14}/><RunnerScope runner={row.runner} here={!elsewhere}><InlineConversationRename session={row.session} onDone={() => { setRenaming(''); requestAnimationFrame(() => panel.current?.querySelector<HTMLElement>(`[data-key="${CSS.escape(row.key)}"] .agent-row`)?.focus()); }}/></RunnerScope></div>
              : <button className="agent-row" {...peekProps({ kind: 'conversation', sessionId: row.session.id, workspaceId: row.session.collectionId, title: row.session.title, ...(inbox.multi ? { runner: row.runner } : {}) })}
                  aria-current={!elsewhere && row.session.id === target.sessionId ? 'true' : undefined} title={[title, conversationDetails(row.session, info?.profiles || []), row.line].filter(Boolean).join('\n')}
                  {...(elsewhere ? { onClick: () => open(row) } : conversationRowGestures({ collectionId: row.session.collectionId, sessionId: row.session.id, commandId: '' }, () => open(row)))}>
                  <span className="agent-mark"><AttentionIcon state={row.state} size={14}/>{dot && <i className="unread-dot"/>}</span>
                  <span className="sr-only">{attentionTitle[row.state]}{dot ? ', unread' : ''}{pinned ? ', pinned' : ''}: </span>
                  <span className="agent-main">
                    <span className="agent-title"><strong>{title}</strong>{pinned && section.id !== 'pinned' && <span className="agent-pinned" aria-hidden="true"><Icon name="pin" size={10}/></span>}
                      {depth > 0 && <span className="agent-role agent-lineage" title={lineageLabel(row.session)}>{lineage?.role || 'agent'}</span>}
                      {lineage?.grantId && <span className="agent-orchestrator" title={`Orchestrator of “${lineage.missionTitle}”: ${lineage.liveChildren} of ${lineage.maxChildren} agents running`}><Icon name="subagent" size={10}/>{lineage.liveChildren}/{lineage.maxChildren}</span>}</span>
                    {from && <span className="agent-lineage" title={lineageLabel(row.session)}><Icon name="subagent" size={10}/>{from}</span>}
                    <span className="agent-meta">{inbox.multi && grouping !== 'runner' && <RunnerBadge runner={row.runner}/>}{row.state === 'working' ? <>{grouping !== 'workspace' && <span className="agent-workspace">{workspace(row)}</span>}<LiveText session={row.session} subagentsShown={subagents.length > 0}/></> : <span>{metaLine(row, workspace(row), grouping !== 'workspace' || section.id === 'pinned')}</span>}</span>
                  </span>
                  <time className="agent-age" dateTime={row.at ? new Date(row.at).toISOString() : undefined}>{ageLabel(row.at, now)}</time>
                </button>}
            {renaming !== row.key && <span className="agent-tools">
              {dot && <button className="icon-button agent-seen" tabIndex={-1} aria-label={`Mark ${title} as read`} title="Mark as read (m)" onClick={() => acts?.markRead(row.session)}><Icon name="check" size={12}/></button>}
              <button className="icon-button agent-pin" tabIndex={-1} aria-pressed={pinned} aria-label={`${pinned ? 'Unpin' : 'Pin'} ${title}`} title={pinned ? 'Unpin (p)' : 'Pin to the top (p)'} disabled={!acts} onClick={() => void acts?.pin(row.session)}><Icon name="pin" size={12}/></button>
            </span>}
            {subagents.length > 0 && <button className="agent-subagents-toggle" aria-expanded={open_} onClick={() => toggleExpanded(row.key)} title={open_ ? 'Hide sub-agents (←)' : 'Show sub-agents (→)'}>
              <Icon name={open_ ? 'chevron' : 'chevronRight'} size={10}/><Icon name="subagent" size={11}/>{subagents.length} {subagents.length === 1 ? 'sub-agent' : 'sub-agents'} working
            </button>}
            {open_ && subagents.length > 0 && <ul className="agent-subagents" aria-label={`Sub-agents of ${title}`}>{subagents.map(agent =>
              <li key={agent.id}><button className="agent-subagent" title={`${agent.name}\n${agent.detail}`} onClick={() => open(row)}>
                <LiveIndicator tone={agent.tone}/><span className="agent-subagent-name">{agent.name}</span><span className="agent-subagent-detail">{agent.detail}</span>{agent.since > 0 && <time>{ageLabel(agent.since, now)}</time>}
              </button></li>)}</ul>}
          </li>;
        })}</ul>}
      </section>;
    })}
    </div>
    {actions.dialogs}
    {others.hosts}
  </div>;
}
