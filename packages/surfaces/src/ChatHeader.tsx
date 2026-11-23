import { Fragment, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type MouseEventHandler, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import type { InfoResponse, JournalEvent, Session } from '@lazybox/protocol';
import { sessionStatus } from './agent-status';
import { agentModelText, type branchSummary } from './conversation-model';
import { fitHeader, HEADER_ORDER, isStat, MORE_WIDTH, reserve, STAT_WIDTH, type HeaderItem, type HeaderItemId, type HeaderPlan } from './header-layout';
import { Icon } from './Icons';
import { LiveIndicator } from './LiveIndicator';
import type { LiveStatus } from './live-status';
import { TelemetryPanel } from './TelemetryPanel';
import { ToolActivity } from './ToolActivity';
import { OrchestratorBadge } from './OrchestratorBadge';
import './chat-header.css';

type Branch = ReturnType<typeof branchSummary>;

/** What the agent is doing now, with the queue behind it. */
export function ChatStatus({ session, live }: { session?: Session; live: LiveStatus }) {
  return <span className="chat-status" data-state={session?.closed ? 'saved' : session?.status} data-tone={live.tone}>
    <LiveIndicator tone={live.tone}/>
    <span className="chat-status-text">
      <span role="status">{live.label}{session?.queued ? ` · ${session.queued} queued` : ''}</span>
      {live.text.length > live.label.length && <span aria-hidden="true">{live.text.slice(live.label.length)}</span>}
    </span>
  </span>;
}

/** Usage and tool activity of the execution on screen. */
export function SessionInspection({ sessionID, session, activity, activityTruncated }: { sessionID: string; session?: Session; activity: JournalEvent[]; activityTruncated?: boolean }) {
  return <>
    <TelemetryPanel executionId={sessionID} conversationId={session?.conversationId} compact/>
    <ToolActivity events={activity} truncated={activityTruncated}/>
  </>;
}

type HeaderProps = {
  agentName: string;
  session?: Session;
  sessionID: string;
  title: ReactNode;
  status: ReactNode;
  modelLabel: string;
  inspection: ReactNode;
};

/** The conversation's numbers, each a chip on the header's second line. */
export type HeaderStats = { usage?: ReactNode; background?: ReactNode; tools?: ReactNode; artifacts?: ReactNode };

/** The words for each item, in the details popover and for screen readers. */
const ITEM_LABEL: Record<HeaderItemId, string> = { status: 'Status', branch: 'Branch', model: 'Model', account: 'Subscription', prompts: 'Prompts', background: 'Background', artifacts: 'Artifacts', usage: 'Context and cost', tools: 'Tool calls' };

let canvas: CanvasRenderingContext2D | null | undefined;
/** A text's width in a font, without laying it out (0 where there is no canvas). */
function textWidth(text: string, font: string): number {
  if (canvas === undefined) canvas = typeof document === 'undefined' ? null : document.createElement('canvas').getContext('2d');
  if (!canvas || !text) return 0;
  canvas.font = font;
  return canvas.measureText(text).width;
}

/**
 * The header of a chat embedded in a workspace panel or a detached window.
 * The title has the first line (two, when it needs them) beside the window
 * and menu buttons; the second line says how the conversation is going, by
 * priority: the status, the branch, the agent and model, the subscription
 * when it is not the only one, then the numbers as compact chips. What does
 * not fit folds whole into "⋯" — nothing on the line is cut.
 */
export function ChatHeader({ agentName, session, sessionID, title, status, statusText, statusBusy, modelLabel, realTitle, branch, modelRequested, shownAccount, onOpenChat, menu, onContextMenu, onPickModel, onSwitchSubscription, stats, origin, review, detailButton }: Omit<HeaderProps, 'inspection'> & {
  /** Review this conversation's work (ReviewFromChat.tsx): first of the actions; folds into ⋯ in a slim panel. */
  review?: ReactNode;
  detailButton?: ReactNode;
  /** Where the workspace's work started (an issue), as a chip beside the actions. */
  origin?: ReactNode;
  /** The conversation's actions on a right-click, the same as its ⋯ menu. */
  onContextMenu?: MouseEventHandler<HTMLElement>;
  realTitle: string;
  branch: Branch;
  /** The status in words ("Turn done", "Running a command · 1m 02s"): its slot is sized from it. */
  statusText: string;
  /** The status ticks (a running turn): its slot keeps the room of a long one. */
  statusBusy: boolean;
  /** The model was asked for but the provider has not confirmed it yet. */
  modelRequested: boolean;
  /** The subscription, when it is worth saying (not the provider's only one); '' otherwise. */
  shownAccount: string;
  onOpenChat?(): void;
  menu: ReactNode;
  /** The model picker (`/model`): the model chip opens it. */
  onPickModel?(): void;
  /** Switch subscription… (`/subscription`): the subscription chip opens it. */
  onSwitchSubscription?(): void;
  stats: HeaderStats;
}) {
  const header = useRef<HTMLElement>(null), line = useRef<HTMLDivElement>(null), details = useRef<HTMLDivElement>(null), more = useRef<HTMLButtonElement>(null);
  const detailsId = useId();
  const modelText = agentModelText(agentName, modelLabel);
  const showBranch = !!realTitle && !!branch;
  // Which number chips have something to show: an empty one takes no room.
  // 'running': live work, which keeps its place on the line before the other numbers.
  const [present, setPresent] = useState<Record<string, false | 'shown' | 'running'>>({});
  const [width, setWidth] = useState(0);
  // Each number chip renders, once, into a node of its own that the line or "⋯" holds.
  const [chips] = useState(() => {
    const chip = (id: string) => { const node = document.createElement('span'); node.className = 'chat-stat'; node.dataset.stat = id; return node; };
    const prompts = chip('prompts'); prompts.classList.add('prompt-jump-slot'); prompts.dataset.promptJumpSlot = '';
    return { prompts, background: chip('background'), artifacts: chip('artifacts'), usage: chip('usage'), tools: chip('tools') };
  });
  useLayoutEffect(() => {
    const node = line.current;
    if (!node) return;
    const measure = () => setWidth(node.clientWidth);
    measure();
    const observer = new ResizeObserver(measure); observer.observe(node);
    return () => observer.disconnect();
  }, []);
  useLayoutEffect(() => {
    const root = header.current;
    if (!root) return;
    const read = () => {
      const next: Record<string, false | 'shown' | 'running'> = {};
      for (const slot of root.querySelectorAll<HTMLElement>('[data-stat]')) if (slot.dataset.stat) next[slot.dataset.stat] = slot.querySelector('.background-work-button[data-running]') ? 'running' : !!slot.querySelector('button:not([data-empty])') && 'shown';
      setPresent(previous => Object.keys(next).length === Object.keys(previous).length && Object.entries(next).every(([key, value]) => previous[key] === value) ? previous : next);
    };
    read();
    const observer = new MutationObserver(read);
    observer.observe(root, { childList: true, subtree: true, attributes: true, attributeFilter: ['data-empty', 'data-running'] });
    return () => observer.disconnect();
  }, []);
  const plan = useMemo<HeaderPlan>(() => {
    const node = line.current;
    if (!node || !width) return { inline: [...HEADER_ORDER], folded: [] };
    const style = getComputedStyle(node);
    const scale = Number(style.getPropertyValue('--text-scale')) || 1;
    const font = `${style.fontStyle} ${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
    const mono = `${10.5 * (Number(style.getPropertyValue('--code-scale')) || 1)}px ${style.getPropertyValue('--font-mono') || 'monospace'}`;
    // Each width includes what leads it on the line: the "·" before an item about the
    // conversation (16px), the 2px between number chips; the two groups are 12px apart.
    const dot = 16 * scale, chip = 2 * scale;
    const items: HeaderItem[] = [{ id: 'status', width: Math.max(reserve(textWidth(statusText, font), 12), statusBusy ? textWidth('Running a command · 10m 00s', font) : 0) + 14 * scale }];
    if (showBranch) items.push({ id: 'branch', width: dot + textWidth(branch!.name, mono) + 15 * scale });
    items.push({ id: 'model', width: dot + textWidth(modelText, font) + (onPickModel ? 17 : 0) * scale });
    if (shownAccount) items.push({ id: 'account', width: dot + textWidth(shownAccount, font) + (onSwitchSubscription ? 17 : 0) * scale });
    for (const id of ['prompts', 'artifacts', 'background', 'usage', 'tools'] as const) if (present[id]) items.push({ id, width: chip + STAT_WIDTH[id] * scale, urgent: present[id] === 'running' });
    // A little slack: an estimate never runs the line under the panel's edge.
    return fitHeader(items, width - 6 - 12 * scale, 0, MORE_WIDTH * scale);
  }, [width, present, statusText, statusBusy, showBranch, branch, modelText, onPickModel, shownAccount, !!onSwitchSubscription]);
  const folded = new Set(plan.folded);
  useEffect(() => {
    const close = () => details.current?.hidePopover?.();
    window.addEventListener('resize', close);
    return () => window.removeEventListener('resize', close);
  }, []);

  const requested = modelRequested ? ' · requested; the provider has not confirmed it yet' : '';
  const item = (id: HeaderItemId): ReactNode => {
    switch (id) {
      case 'branch': return showBranch && <span className="chat-meta-branch" title={branch!.title}><Icon name="branch" size={11}/><span className="chat-meta-branch-name">{branch!.name}</span></span>;
      case 'model': return onPickModel
        // tabIndex: in WebKit (the desktop app) a button is in the Tab order only when it asks.
        ? <button type="button" tabIndex={0} className="chat-model chat-model-button" data-requested={modelRequested || undefined} title={`${modelLabel || agentName}${requested}\nSwitch model (/model)`} aria-label={`Model ${modelText}: switch model`} onClick={onPickModel}>{modelText}<Icon name="chevron" size={10}/></button>
        : <span className="chat-model" data-requested={modelRequested || undefined} title={`${modelLabel || agentName}${requested}`}>{modelText}</span>;
      case 'account': return shownAccount && (onSwitchSubscription
        ? <button type="button" tabIndex={0} className="chat-account chat-model-button" title={`Subscription: ${shownAccount}\nSwitch subscription (/subscription)`} aria-label={`Subscription ${shownAccount}: switch subscription`} onClick={onSwitchSubscription}>{shownAccount}<Icon name="chevron" size={10}/></button>
        : <span className="chat-account" title="Subscription">{shownAccount}</span>);
      // A number chip moves between the line and "⋯" without remounting: its open list, its
      // polling and the "N prompts" slot (PromptNavigator renders into it) stay as they are.
      case 'prompts': case 'artifacts': case 'background': case 'usage': case 'tools': return <span className="chat-stat-host" ref={host => { if (host && chips[id].parentNode !== host) host.appendChild(chips[id]); }}/>;
      default: return null;
    }
  };
  const onLine = (stat: boolean) => HEADER_ORDER.filter(id => id !== 'status' && isStat(id) === stat && !folded.has(id)).map(id => <Fragment key={id}>{item(id)}</Fragment>);
  const shownFolded = plan.folded.filter(id => !isStat(id) || present[id]);
  const placeDetails = () => {
    const anchor = more.current?.getBoundingClientRect(), panel = details.current;
    if (!anchor || !panel) return;
    panel.style.left = `${Math.max(8, Math.min(anchor.right - 300, window.innerWidth - 308))}px`;
    panel.style.top = `${Math.max(8, Math.min(anchor.bottom + 6, window.innerHeight - 160))}px`;
  };

  return <header ref={header} className="chat-heading" onContextMenu={onContextMenu}>
    <span className={'agent-avatar ' + agentName.toLowerCase()} aria-hidden="true">{agentName === 'Claude' ? '✳︎' : '›'}</span>
    <div className="chat-heading-title">{title}</div>
    <div className="chat-heading-actions">
      {review}
      {origin}
      {/* Where this conversation sits in an agent tree: its Orchestrator grant, or the orchestrator that started it. */}
      <OrchestratorBadge session={session} sessionID={sessionID}/>
      {onOpenChat && <button className="icon-button" aria-label="Open chat window ↗" title="Open in a separate window" onClick={onOpenChat}><Icon name="external"/></button>}
      {menu}
    </div>
    <div className="chat-heading-detail-row"><div ref={line} className="chat-heading-meta">
      <div className="chat-heading-about">
        <span className="chat-heading-status">{status}</span>
        {onLine(false)}
      </div>
      {sessionID && <div className="chat-heading-stats" role="group" aria-label="Conversation numbers">
        {onLine(true)}
        {shownFolded.length > 0 && <button ref={more} type="button" className="icon-button chat-details-button" popoverTarget={detailsId}
          aria-label={`More details: ${shownFolded.map(id => ITEM_LABEL[id]).join(', ')}`} title={`More: ${shownFolded.map(id => ITEM_LABEL[id].toLowerCase()).join(', ')}`} onClick={placeDetails}><Icon name="more" size={14}/></button>}
      </div>}
    </div>
    {detailButton}</div>
    {createPortal(stats.artifacts, chips.artifacts)}{createPortal(stats.background, chips.background)}{createPortal(stats.usage, chips.usage)}{createPortal(stats.tools, chips.tools)}
    <div ref={details} id={detailsId} popover="auto" className="chat-details" role="group" aria-label="Conversation details">
      {plan.folded.map(id => <div key={id} className="chat-details-row" data-item={id} data-stat={isStat(id) || undefined}><span className="chat-details-label">{ITEM_LABEL[id]}</span><span className="chat-details-value">{item(id)}</span></div>)}
    </div>
  </header>;
}

/** The session heading of a standalone agent window. */
export function SessionHeader({ agentName, session, sessionID, title, status, modelLabel, inspection, accountName, lifecycle }: HeaderProps & { accountName: string; lifecycle: ReactNode }) {
  return <>
    {session && title}
    <div className="panel-title">
      <div><span className="eyebrow">Agent session</span><h2>{agentName}<span className="chat-model">{modelLabel}</span></h2></div>
      {lifecycle}
    </div>
    <div className="session-meta">
      {status}
      {accountName && <span>{accountName}</span>}
      {session?.gatewayId && session.gatewayId !== 'direct' && <span>Gateway: {session.gatewayId}</span>}
      <span>{session?.queued || 0} queued</span>
      <span className="mono">{sessionID.slice(0, 12)}</span>
    </div>
    {sessionID && <div className="agent-inspection-bar">{inspection}</div>}
  </>;
}

/** The brand bar of a standalone agent window, with the Runner's state. */
export function WindowTopbar({ connecting, info, sessionID, session }: { connecting: boolean; info?: InfoResponse; sessionID: string; session?: Session }) {
  return <header className="topbar">
    <span className="brand" aria-hidden="true">l<span>↗</span></span>
    <div><h1>lazybox <span>V2</span></h1><p>One window. Your agent.</p></div>
    <span className="pill" role="status">{connecting ? 'Connecting to Runner' : !info ? 'Runner unavailable' : sessionID ? sessionStatus(session) : 'Runner connected'}</span>
  </header>;
}
