/**
 * Lazybox's own events in a conversation (system-events.ts): a message it
 * wrote to the agent is one quiet line (an icon, what happened, a label
 * beside it) that opens to exactly what the agent received; what a Runner
 * restart left uncertain is one line too; a restart that interrupted nothing
 * is only a small mark, its count and time on hover.
 */
import { useEffect, useId, useRef, useState, type MouseEvent } from 'react';
import { Icon } from './Icons';
import { SentAttachments } from './AttachmentThumbnail';
import { copyText } from './ContextMenu';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';
import type { TranscriptMessage } from './transcript-model';
import { connectionLine, NOTICE_ICONS, restartTitle, systemEventView, type RunnerNotice } from './system-events';
import { OpenSender } from './AgentMessages';
import './system-events.css';

type Menu = ((event: MouseEvent<HTMLElement>) => void) | undefined;

/** A message Lazybox wrote to the agent: one line, opened to its text. */
export function SystemEvent({ message: m, agentName, onOpenFile, onContextMenu }: { message: TranscriptMessage; agentName: string; onOpenFile?(reference: FileLink, how: OpenModifiers): void; onContextMenu?: Menu }) {
  const [open, setOpen] = useState(false);
  const body = useId(), opened = useRef<HTMLDivElement>(null);
  // Opened, its line stays on screen with the text below it, even when the
  // conversation follows its latest message (which grows upward): once that
  // has settled, the line is brought back into view if it left.
  useEffect(() => {
    if (!open) return;
    let frame = requestAnimationFrame(() => { frame = requestAnimationFrame(() => opened.current?.parentElement?.querySelector('.system-event-line')?.scrollIntoView({ block: 'nearest' })); });
    return () => cancelAnimationFrame(frame);
  }, [open]);
  const view = systemEventView(m.origin!, agentName);
  const copyWhat = m.origin!.kind.startsWith('repository') ? (view.copy.includes('\n') ? 'paths' : 'path') : 'text';
  return <div className="system-event" data-kind={m.origin!.kind} data-tone={view.tone} data-open={open || undefined} data-message-id={m.id} role="note" aria-label={`${view.kind}: ${view.line}`} onContextMenu={onContextMenu}>
    <div className="system-event-line">
      <button type="button" className="system-event-toggle" aria-expanded={open} aria-controls={body}
        title={[view.line, view.copy, open ? 'Hide the message' : `Show exactly what ${agentName} received`].filter(Boolean).join('\n')} onClick={() => setOpen(value => !value)}>
        <span className="system-event-icon" aria-hidden="true"><Icon name={view.icon} size={12}/></span>
        <span className="system-event-text">{view.line}</span>
        {view.detail && <span className="system-event-detail">{view.detail}</span>}
        <span className="system-event-chevron" aria-hidden="true"><Icon name="chevronRight" size={11}/></span>
      </button>
      {m.origin!.conversation && <OpenSender origin={m.origin!}/>}
      {view.copy && <button type="button" className="icon-button system-event-copy" aria-label={`Copy ${copyWhat}`} title={`Copy ${view.copy}`} onClick={() => copyText(view.copy, `Copied ${copyWhat}`)}><Icon name="copy" size={11}/></button>}
    </div>
    {open && <div id={body} ref={opened} className="system-event-body">
      <p className="system-event-caption"><span>{view.sentBy}</span><button type="button" className="system-event-copy-text" onClick={() => copyText(m.text, 'Copied message')}><Icon name="copy" size={11}/>Copy</button></p>
      {/* Exactly the text the agent received, as written: not re-rendered. */}
      <pre className="system-event-text-sent">{m.text}</pre>
      {m.files?.length ? <SentAttachments files={m.files}/> : null}
    </div>}
  </div>;
}

const NOTICE_LINES: Record<RunnerNotice, string> = {
  'delivery-unknown': 'Runner restarted before a message was confirmed · resend it if it is still needed',
  'outcome-unknown': 'Runner restarted during a command · its outcome is unknown; the agent was told to check',
  'agent-lost': 'Lazybox restarted after this agent stopped · send a message to resume its session',
  reverted: '',
  cli: '',
  'pull-request': '',
  'connection-lost': '',
  storage: '',
  provider: '',
};

/** What a Runner restart (or a revert) left to know: one line, its whole sentence on hover. */
export function RunnerNoticeLine({ message: m }: { message: TranscriptMessage }) {
  const kind = m.notice || 'delivery-unknown', { icon } = NOTICE_ICONS[kind];
  // A lost connection the Runner got back is history, not an alarm.
  const tone = kind === 'connection-lost' && m.reconnected ? 'muted' : NOTICE_ICONS[kind].tone;
  const line = kind === 'connection-lost' ? connectionLine(m.text, m.at, m.reconnected?.at, !!m.reconnected) : NOTICE_LINES[kind] || m.text;
  return <div className="system-event notice" data-kind={kind} data-tone={tone} data-message-id={m.id} role="note" title={kind === 'connection-lost' && m.text ? `${line}\n${m.text}` : m.text}>
    <div className="system-event-line">
      <span className="system-event-static">
        <span className="system-event-icon" aria-hidden="true"><Icon name={icon} size={12}/></span>
        <span className="system-event-text">{line}</span>
        {(m.count || 1) > 1 && <span className="system-event-detail">{m.count}×</span>}
      </span>
    </div>
  </div>;
}

/** Restarts that interrupted nothing: a small mark, never a line of text. */
export function RestartMark({ message: m }: { message: TranscriptMessage }) {
  const title = restartTitle(m.count || 1, m.at);
  return <div className="runner-mark" role="note" tabIndex={0} aria-label={title} title={title} data-message-id={m.id}>
    <span><Icon name="refresh" size={9}/>{(m.count || 1) > 1 && <small>{m.count}</small>}</span>
  </div>;
}
