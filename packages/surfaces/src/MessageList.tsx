import { ActivityTime, exactTime, localDay, validActivityTime } from './ActivityTime';
import type { OpenModifiers } from './file-links';
import { Fragment, memo, type ReactNode, useContext, useMemo, useRef, useState } from 'react';
import type { HandoffMessage } from './ConversationHandoff';
import { contextMenu, type ContextMenuContext } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import { messageItems, messageRole, promptBefore, type MessageMenuDetail } from './chat-context-menu';
import { RichContent } from './RichContent';
import { SentAttachments } from './AttachmentThumbnail';
import './message-list.css';
import type { FileLink } from './file-reference';
import type { TranscriptMessage } from './transcript-model';
import type { ReplayProgress } from './transcript-cache';
import { switchedLine } from './subscription-model';
import { useCheckpointUI, type CheckpointScope } from './TurnCheckpoints';
import { MessageArtifactActions } from './MessageArtifactActions';
import { SubagentCard } from './SubagentCard';
import { ACTIVITY_LABELS } from './activity-detail';
import { ToolLineContext, ToolLines } from './ToolLine';
import { ACTIVITY_DETAILS, turnActivity, type ActivityDetail, type ToolEntry, type TurnCache } from './tool-lines';
import { AnsweredLine } from './RequestCards';
import { Icon } from './Icons';
import { ReviewAlert, ReviewCard, ReviewExchange } from './ReviewLauncher';
import { InlineRuns, RunMessageContext } from './code-runs';
import { TerminalHandoffCard } from './TerminalHandoffCard';
import { RestartMark, RunnerNoticeLine, SystemEvent } from './SystemEvents';
import { isSystemEvent } from './system-events';
import { LineageLine } from './AgentMessages';
import { splitItemContext } from './item-context';
import { QuotedItemChip } from './WorkItems';

const ownName = (id: string) => id;
/** The start of a message, for its accessible name. */
const ownSummary = (text: string) => { const line = text.trim().replace(/\s+/g, ' '); return line.length > 80 ? `${line.slice(0, 79)}…` : line; };

/**
 * A message's words; in one the person sent, an issue it carries (quoted by
 * the Runner as external content) shows as the issue's chip, which opens to
 * exactly what the agent received.
 */
function PersonText({ text, quoted, onOpenFile }: { text: string; quoted: boolean; onOpenFile?(reference: FileLink, how: OpenModifiers): void }) {
  const parts = quoted ? splitItemContext(text) : undefined;
  if (!parts) return <RichContent text={text} onOpenFile={onOpenFile}/>;
  return <>
    {parts.before && <RichContent text={parts.before} onOpenFile={onOpenFile}/>}
    <QuotedItemChip item={parts.item}/>
    {parts.after && <RichContent text={parts.after} onOpenFile={onOpenFile}/>}
  </>;
}

type MessageMenu = (context: ContextMenuContext<MessageMenuDetail>) => MenuItem[];
const MessageRow = memo(function MessageRow({ message: m, agentName, onOpenFile, nameFor = ownName, sessionId = '', menu, turn, turnControl, alwaysShowTime }: { message: TranscriptMessage; agentName: string; onOpenFile?(reference: FileLink, how: OpenModifiers): void; nameFor?(id: string): string; sessionId?: string; menu?: MessageMenu; turnControl?: ReactNode; alwaysShowTime?: boolean; /** Quiet: every tool call of the turn this run of calls opens. */ turn?: ToolEntry[] }) {
  if (m.type === 'switched' && m.switched) return <div className="subscription-switched" role="note"><span>{switchedLine(m.switched, nameFor)}</span></div>;
  if (m.type === 'subagent' && m.agentId) return <div className="subagent-anchor" data-message-id={m.id}><SubagentCard id={m.agentId}/></div>;
  if (m.type === 'tools') return m.tools ? <ToolLines entries={m.tools} turn={turn}/> : null;
  if (m.type === 'answered' && m.answered) return <AnsweredLine note={m.answered}/>;
  if (m.type === 'command') return <div className="command-line" role="note" data-message-id={m.id}><Icon name="check" size={12}/><span>{m.text}</span></div>;
  // A turn Send now stopped: one quiet line, not an error.
  if (m.type === 'stopped') return <div className="command-line stopped-line" role="note" data-message-id={m.id}><Icon name="stop" size={12}/><span>{m.text}</span></div>;
  if (m.origin) return <SystemEvent message={m} agentName={agentName} onOpenFile={onOpenFile} onContextMenu={menu && contextMenu<MessageMenuDetail>('message', menu, { message: m, role: messageRole(m), sessionId }, 'Message actions')}/>;
  if (m.terminalHandoff) return <TerminalHandoffCard key={m.terminalHandoff.id} handoff={m.terminalHandoff}/>;
  if (m.type === 'notice') return <RunnerNoticeLine message={m}/>;
  if (m.type === 'operation_accepted' && m.command) return <div className="command-line command-sent" role="note" data-message-id={m.id} title="You ran this command" onContextMenu={menu && contextMenu<MessageMenuDetail>('message', menu, { message: m, role: messageRole(m), sessionId }, 'Message actions')}><Icon name="chevronRight" size={12}/><code>{m.text.trim()}</code></div>;
  if (m.type === 'review' && m.review) return <div className="review-card-anchor" data-message-id={m.id}><ReviewCard data={m.review}/></div>;
  if (m.type === 'exchange') return <ReviewExchange text={m.text} id={m.id}/>;
  if (m.type === 'lineage' && m.lineage) return <LineageLine message={m} note={m.lineage}/>;
  if (m.type === 'review-alert') return <ReviewAlert text={m.text} id={m.id} note={m.review}/>;
  if (m.type === 'reattached') return <RestartMark message={m}/>;
  // The person's own message needs no name over it: it is theirs (screen readers still hear "You").
  // The conversation header identifies the main agent. Sub-agents and system events have their own cards.
  const own = m.type === 'operation_accepted';
  const reply = m.type === 'message_delta';
  const at = validActivityTime(m.at) ? m.at : undefined;
  const context = m.promptContext;
  const info = own ? [agentName, at && exactTime(at), 'Model: ' + (m.promptModel || 'Unreported'),
    'Last reported context: ' + (context ? context.used.toLocaleString() + (context.capacity ? ' / ' + context.capacity.toLocaleString() : '') + ' tokens' : 'Unreported')].filter(Boolean).join(' · ') : undefined;
  return <article title={info} className={'message ' + m.type} data-message-id={m.id} data-time-visible={alwaysShowTime || undefined} aria-label={own ? `You${m.steered ? ', steered' : ''}: ${ownSummary(m.text)}` : undefined}
    onContextMenu={menu && contextMenu<MessageMenuDetail>('message', menu, { message: m, role: messageRole(m), sessionId }, 'Message actions')}>
    {/* A message steered into a running turn says so, where it reached the agent. */}
    {own
      ? <>{m.steered && <span className="message-tag" title={`Given to ${agentName} inside its running turn`}>steered</span>}
          <div className="message-byline">{at !== undefined && <ActivityTime at={at} className="prompt-sent-time" description={info}/>} {turnControl}</div></>
      : (!reply || at !== undefined) && <div className="message-byline">
        {!reply && <span className="eyebrow">{m.type === 'failure' ? 'Stopped' : agentName}</span>}
        {reply && at !== undefined && <ActivityTime at={at} className="message-time"/>}
      </div>}
    {/* Only an agent's own words offer to run their commands. */}
    {m.type === 'message_delta' ? <RunMessageContext.Provider value={m}><RichContent text={m.text} onOpenFile={onOpenFile}/><InlineRuns messageId={m.id}/></RunMessageContext.Provider> : <PersonText text={m.text} quoted={m.type === 'operation_accepted'} onOpenFile={onOpenFile}/>}
    {m.files?.length ? <SentAttachments files={m.files}/> : null}
    {/* What the provider said, for whoever reports it: never the line itself. */}
    {m.type === 'failure' && m.detail ? <details className="failure-detail"><summary>Details</summary><pre>{m.detail}</pre></details> : null}
    <MessageArtifactActions message={m}/>
  </article>;
});

/**
 * The latest `visible` messages of a conversation. Only that tail is mounted:
 * opening a long conversation renders what is on screen, and earlier history
 * is added as the reader scrolls toward it. A render of the window that hosts
 * it (a keystroke in the composer) does not touch the rows.
 */
export const MessageList = memo(function MessageList({ messages, visible, agentName, onOpenFile, onShowEarlier, nameFor, checkpoints, sessionId, onReveal, onHandoff, onBranch, detail = 'verbose', loadingEarlier = false, alwaysShowTime }: { messages: TranscriptMessage[]; visible: number; agentName: string; onOpenFile?(reference: FileLink, how: OpenModifiers): void; onShowEarlier(): void; /** A subscription's name by profile ID, for switch lines. */ nameFor?(id: string): string; checkpoints?: CheckpointScope; /** The conversation, for Quote in reply. */ sessionId?: string; /** Scroll a message into view, mounting earlier history when needed. */ onReveal?(id: string): void; onHandoff?(message: HandoffMessage): void; onBranch?(message: TranscriptMessage): void; /** How much tool activity to show. */ detail?: ActivityDetail; /** Only the latest turns are in: the earlier history is being read (transcript `partial`). */ loadingEarlier?: boolean; alwaysShowTime?: boolean }) {
  const hidden = Math.max(0, messages.length - visible);
  const checkpoint = useCheckpointUI(messages, checkpoints);
  // One stable menu for every row, so a new message does not re-render the others.
  const latest = useRef({ messages, checkpoint, onReveal, onHandoff, onBranch });
  latest.current = { messages, checkpoint, onReveal, onHandoff, onBranch };
  const [menu] = useState<MessageMenu>(() => (context: ContextMenuContext<MessageMenuDetail>) => {
    const { messages, checkpoint, onReveal, onHandoff, onBranch } = latest.current;
    const { message, role } = context.detail;
    const id = message.id;
    const retry = role === 'user' && !!message.operationId && !message.command && !message.steered && !message.files?.length;
    const fork = role === 'agent' && !!message.turnId;
    const actions = checkpoint.items(id);
    if (onBranch && (retry || fork)) actions.push({
      label: retry ? 'Edit and restart here…' : 'Fork after this turn…',
      run: () => onBranch(message),
    });
    return messageItems(context, { prompt: promptBefore(messages, id), onReveal, onHandoff, turn: actions });
  });
  // Quiet: a turn's calls are one line, on its first run of calls.
  const turnCache = useRef<TurnCache>(new Map());
  const scope = useContext(ToolLineContext);
  const [overrides, setOverrides] = useState<Record<string, ActivityDetail | 'hidden'>>({});
  const responseModes = useMemo(() => {
    const result = new Map<string, ActivityDetail | 'hidden'>();
    if (!Object.keys(overrides).length) return result;
    let mode: ActivityDetail | 'hidden' | undefined;
    let turnId = '';
    for (const m of messages) {
      if (m.type === 'operation_accepted' && !m.steered) {
        mode = overrides[m.id];
        turnId = m.turnId;
      }
      if (!mode || (m.type !== 'tools' && m.type !== 'message_delta')) continue;
      if (turnId && m.turnId && turnId !== m.turnId) { mode = undefined; continue; }
      turnId ||= m.turnId;
      result.set(m.id, mode);
    }
    return result;
  }, [messages, overrides]);
  const quiet = detail === 'quiet' || Object.values(overrides).includes('quiet');
  const turns = useMemo(() => quiet ? turnActivity(messages, turnCache.current, hidden) : undefined, [messages, quiet, hidden]);
  const days = useMemo(() => messageDays(messages, hidden), [messages, hidden]);
  const row = (m: TranscriptMessage) => {
    const mode = responseModes.get(m.id);
    const control = m.type === 'operation_accepted' && !m.steered && !m.command && !m.origin && <select aria-label="Response detail for this prompt" value={overrides[m.id] || ''} onChange={e => {
      const value = e.currentTarget.value as ActivityDetail | 'hidden' | '';
      setOverrides(previous => {
        const next = { ...previous };
        if (value) next[m.id] = value;
        else delete next[m.id];
        return next;
      });
    }}>
      <option value="">Session · {ACTIVITY_LABELS[detail].label}</option>
      {ACTIVITY_DETAILS.map(level => <option key={level} value={level}>{ACTIVITY_LABELS[level].label}</option>)}
      <option value="hidden">Hide response</option>
    </select>;
    if (mode === 'hidden') return <Fragment key={m.id}>{checkpoint.marker(m.id)}</Fragment>;
    const content = <MessageRow turnControl={control} message={m} agentName={agentName} onOpenFile={onOpenFile} nameFor={nameFor} sessionId={sessionId} menu={menu} turn={turns?.get(m.id)} alwaysShowTime={alwaysShowTime}/>;
    return <Fragment key={m.id}>{mode && m.type === 'tools'
      ? <ToolLineContext.Provider value={{ ...scope, detail: mode }}>{content}</ToolLineContext.Provider>
      : content}{checkpoint.marker(m.id)}</Fragment>;
  };
  return <>
    {hidden > 0 && <button type="button" className="show-earlier" onClick={onShowEarlier}>Show earlier messages <small>{hidden.toLocaleString()} more</small></button>}
    {!hidden && loadingEarlier && messages.length > 0 && <div className="earlier-loading" role="status">Loading earlier messages…</div>}
    {days.flatMap(({ day, runs }) => {
      const first = runs[0];
      const firstId = Array.isArray(first) ? first[0].id : first.id;
      return [
        day && <div key={`day:${firstId}`} className="message-day" role="separator" aria-label={day}>{day}</div>,
        ...runs.map(run => Array.isArray(run)
          ? <div key={`events:${run[0].id}`} className="system-event-group" role="group" aria-label={`${run.length} events from Lazybox`}>{run.map(row)}</div>
          : row(run)),
      ];
    })}
    {checkpoint.overlay}
  </>;
});

/** Local calendar days, with consecutive Lazybox events grouped within each day. */
function messageDays(messages: readonly TranscriptMessage[], from: number) {
  type Run = TranscriptMessage | TranscriptMessage[];
  const out: { day: string; runs: Run[] }[] = [];
  let current = { day: '', runs: [] as Run[] };
  let run: TranscriptMessage[] = [];
  const flush = () => { if (run.length > 1) current.runs.push(run); else if (run.length) current.runs.push(run[0]); run = []; };
  for (let i = from; i < messages.length; i++) {
    const message = messages[i];
    const day = validActivityTime(message.at) ? localDay(message.at) : current.day;
    if (current.day !== day) {
      flush();
      if (current.runs.length) out.push(current);
      current = { day, runs: [] };
    }
    if (isSystemEvent(message)) { run.push(message); continue; }
    flush(); current.runs.push(message);
  }
  flush();
  if (current.runs.length) out.push(current);
  return out;
}

/** Opening a conversation: how much of its history has arrived, when the Runner says. */
export function ConversationLoading({ replay }: { replay?: ReplayProgress }) {
  const share = replay && replay.total > 0 ? Math.min(1, replay.loaded / replay.total) : undefined;
  return <div className="conversation-loading" role="status">
    <span>Loading conversation…{share !== undefined && ` ${Math.floor(share * 100)}%`}</span>
    {share !== undefined && <progress max={replay!.total} value={replay!.loaded} aria-label="Conversation history loaded"/>}
  </div>;
}
