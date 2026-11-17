import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent as ReactPointerEvent } from 'react';
import { Icon } from './Icons';
import { CodeBlock } from './RichContent';
import { useNow } from './LiveIndicator';
import { titleWhenTruncated } from './truncated-title';
import { activeSubagent, type Subagent, type SubagentStatus } from './subagent-model';
import { usePresentation, useSubagent, useSubagentView } from './subagent-view';
import { StopSubagent, SubagentMark, SubagentName, SubagentOutcome, SubagentPrompt, SubagentStats, SubagentTimeline, subagentTitle, useAgentStatus } from './SubagentCard';
import './subagents.css';

const WIDTH_KEY = 'lazybox.subagent-dock-width';
const readWidth = () => { try { const value = Number(localStorage.getItem(WIDTH_KEY)); return Number.isFinite(value) && value >= 280 ? value : 440; } catch { return 440; } };
const saveWidth = (value: number) => { try { localStorage.setItem(WIDTH_KEY, String(Math.round(value))); } catch { /* the width is a convenience */ } };
/** Below this chat width a panel covers the chat instead of sitting beside it. */
const NARROW = 720;

/**
 * One sub-agent in a panel of its own, following it live: docked beside the
 * conversation or floating over it, moved by its header and resized from its
 * edge. Read-only; closing it leaves the sub-agent running.
 */
export function SubagentPanel() {
  const view = useSubagentView();
  usePresentation(view);
  const panel = view?.panel ?? null;
  const agent = useSubagent(view, panel?.agentId ?? '');
  const status = useAgentStatus(view, agent);
  const active = activeSubagent(status);
  const now = useNow(!!panel && active);
  const host = useRef<HTMLElement>(null), body = useRef<HTMLDivElement>(null), header = useRef<HTMLElement>(null);
  const returnFocus = useRef<Element | null>(null);
  const [width, setWidth] = useState(readWidth);
  const [narrow, setNarrow] = useState(false);
  const [position, setPosition] = useState<{ left: number; top: number } | null>(null);
  const following = useRef(true);
  const chat = () => host.current?.closest<HTMLElement>('.single-session') ?? null;
  const docked = panel?.mode === 'dock' && !narrow;

  // A docked panel takes its width from the conversation beside it.
  useLayoutEffect(() => {
    const node = chat();
    if (!node) return;
    node.classList.toggle('subagent-docked', docked);
    node.style.setProperty('--subagent-dock', `${width}px`);
    return () => { node.classList.remove('subagent-docked'); };
  }, [docked, width, !!panel]);
  useLayoutEffect(() => {
    const node = chat();
    if (!node || !panel) return;
    const measure = () => setNarrow(node.clientWidth < NARROW);
    measure();
    const observer = new ResizeObserver(measure); observer.observe(node);
    return () => observer.disconnect();
  }, [!!panel]);
  // Opening takes focus to the panel; closing gives it back.
  const agentId = panel?.agentId;
  useEffect(() => {
    if (!agentId) return;
    if (!host.current?.contains(document.activeElement)) returnFocus.current = document.activeElement;
    following.current = true;
    header.current?.focus({ preventScroll: true });
  }, [agentId]);
  useLayoutEffect(() => { const node = body.current; if (node && following.current) node.scrollTop = node.scrollHeight; }, [agent?.items, agent?.status, agentId]);
  if (!view || !panel) return null;
  const close = () => { view.closePanel(); const back = returnFocus.current; if (back instanceof HTMLElement && back.isConnected) back.focus(); };
  if (!agent) return <aside ref={host} className="subagent-panel" data-mode="float" aria-label="Sub-agent"><header ref={header} tabIndex={-1}><span>This sub-agent is no longer in the conversation’s history.</span><button type="button" className="icon-button" aria-label="Close sub-agent panel" onClick={close}><Icon name="close" size={14}/></button></header></aside>;
  const title = subagentTitle(agent, view);
  const command = agent.kind === 'command', noun = command ? 'background command' : 'sub-agent';

  const clamp = (left: number, top: number) => {
    const bounds = chat()?.getBoundingClientRect(), own = host.current?.getBoundingClientRect();
    if (!bounds || !own) return { left, top };
    return { left: Math.max(8, Math.min(left, bounds.width - own.width - 8)), top: Math.max(8, Math.min(top, bounds.height - 48)) };
  };
  const startDrag = (event: ReactPointerEvent<HTMLElement>) => {
    if (docked || narrow || event.button !== 0 || (event.target as Element).closest('button')) return;
    const node = host.current, bounds = chat()?.getBoundingClientRect();
    if (!node || !bounds) return;
    const own = node.getBoundingClientRect(), start = { x: event.clientX, y: event.clientY, left: own.left - bounds.left, top: own.top - bounds.top };
    event.currentTarget.setPointerCapture(event.pointerId);
    const move = (next: PointerEvent) => setPosition(clamp(start.left + next.clientX - start.x, start.top + next.clientY - start.y));
    const end = () => { window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', end); };
    window.addEventListener('pointermove', move); window.addEventListener('pointerup', end);
  };
  const headerKeys = (event: KeyboardEvent<HTMLElement>) => {
    if (event.target !== event.currentTarget || docked || narrow) return;
    const step = event.shiftKey ? 40 : 10;
    const delta = ({ ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] } as Record<string, number[]>)[event.key];
    if (!delta) return;
    event.preventDefault();
    const node = host.current, bounds = chat()?.getBoundingClientRect();
    if (!node || !bounds) return;
    const own = node.getBoundingClientRect();
    setPosition(clamp(own.left - bounds.left + delta[0], own.top - bounds.top + delta[1]));
  };
  const resize = (event: ReactPointerEvent<HTMLElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    const startX = event.clientX, startWidth = width, max = (chat()?.clientWidth ?? 1200) - 320;
    event.currentTarget.setPointerCapture(event.pointerId);
    let last = startWidth;
    const move = (next: PointerEvent) => { last = Math.max(280, Math.min(max, startWidth - (next.clientX - startX))); setWidth(last); };
    const end = () => { saveWidth(last); window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', end); };
    window.addEventListener('pointermove', move); window.addEventListener('pointerup', end);
  };
  const resizeKeys = (event: KeyboardEvent<HTMLElement>) => {
    const delta = event.key === 'ArrowLeft' ? 24 : event.key === 'ArrowRight' ? -24 : 0;
    if (!delta) return;
    event.preventDefault();
    const next = Math.max(280, Math.min((chat()?.clientWidth ?? 1200) - 320, width + delta));
    setWidth(next); saveWidth(next);
  };
  return <aside ref={host} className="subagent-panel" data-mode={narrow ? 'sheet' : panel.mode} data-status={status} aria-label={`${command ? 'Background command' : 'Sub-agent'}: ${title}`} data-kind={agent.kind}
    style={!docked && !narrow && position ? { left: position.left, top: position.top, right: 'auto' } : undefined}
    onKeyDown={event => { if (event.key === 'Escape' && !event.defaultPrevented) { event.stopPropagation(); close(); } }}>
    {docked && <div className="subagent-panel-resize" role="separator" aria-orientation="vertical" aria-label={`Resize ${noun} panel`} aria-valuenow={width} aria-valuemin={280} tabIndex={0} onPointerDown={resize} onKeyDown={resizeKeys}/>}
    <header ref={header} tabIndex={-1} className="subagent-panel-head" data-movable={!docked && !narrow || undefined} title={!docked && !narrow ? 'Drag to move. Arrow keys move it.' : undefined} onPointerDown={startDrag} onKeyDown={headerKeys}>
      <SubagentMark status={status}/>
      <div className="subagent-panel-title">
        <span className="subagent-eyebrow">{command ? 'Background command' : agent.depth > 0 ? 'Nested sub-agent' : 'Sub-agent'}</span>
        <h3>{command ? <code onPointerEnter={titleWhenTruncated(agent.command || title)}>{title}</code> : <SubagentName agent={agent} view={view}/>}</h3>
      </div>
      <span className="subagent-actions">
        <StopSubagent view={view} agent={agent} status={status}/>
        {!command && <button type="button" className="icon-button" aria-label="Show in the conversation" title="Show its card in the conversation" onClick={() => view.reveal(agent.id)}><Icon name="target" size={14}/></button>}
        {!narrow && <button type="button" className="icon-button" aria-label={docked ? 'Float the panel' : 'Dock the panel'} title={docked ? 'Float over the conversation' : 'Dock beside the conversation'} onClick={() => { setPosition(null); view.setMode(docked ? 'float' : 'dock'); }}><Icon name={docked ? 'float' : 'dock-right'} size={14}/></button>}
        <button type="button" className="icon-button" aria-label={`Close ${noun} panel`} title={`Close (Esc). The ${noun} keeps running.`} onClick={close}><Icon name="close" size={14}/></button>
      </span>
    </header>
    <div className="subagent-panel-meta">
      <SubagentStats agent={agent} status={status} now={now}/>
    </div>
    {active && agent.progress && <p className="subagent-progress">{agent.progress}</p>}
    <div className="subagent-panel-body" ref={body} tabIndex={0} aria-label={command ? 'Command and output' : 'Sub-agent timeline'} onScroll={event => { const node = event.currentTarget; following.current = node.scrollHeight - node.scrollTop - node.clientHeight < 32; }}>
      {command ? <CommandDetails agent={agent} status={status}/> : <>
        <SubagentPrompt agent={agent}/>
        <SubagentTimeline agent={agent} status={status} scroller={body}/>
        {!active && <SubagentOutcome view={view} agent={agent} status={status}/>}
      </>}
    </div>
  </aside>;
}

/**
 * A background command: the whole script, and its output as the agent last
 * read it. Claude keeps a background command's output itself, in a file; the
 * Runner does not stream it, so the latest read is what can be shown.
 */
function CommandDetails({ agent, status }: { agent: Subagent; status: SubagentStatus }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => { try { await navigator.clipboard.writeText(agent.outputFile); setCopied(true); setTimeout(() => setCopied(false), 1500); } catch { /* the path stays selectable */ } };
  return <div className="background-command">
    <CodeBlock text={agent.command || agent.name} language="bash" label="Command"/>
    <h4>Output</h4>
    {agent.output ? <CodeBlock text={agent.output} label="Output"/> : <p className="subagent-empty">{activeSubagent(status) ? 'The agent has not read its output yet.' : 'The agent did not read its output.'}{agent.summary ? ` ${agent.summary}` : ''}</p>}
    {agent.outputFile && <p className="background-command-file"><span>Full output</span><code title={agent.outputFile}>{agent.outputFile}</code><button type="button" className="icon-button" aria-label="Copy the output file path" title={copied ? 'Copied' : 'Copy path'} onClick={() => void copy()}><Icon name={copied ? 'check' : 'snippets'} size={13}/></button></p>}
  </div>;
}
