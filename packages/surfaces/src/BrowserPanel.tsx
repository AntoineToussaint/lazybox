import { useEffect, useLayoutEffect, useMemo, useRef, useState, type FormEvent, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent, type WheelEvent as ReactWheelEvent } from 'react';
import type { BrowserAction, BrowserFrame, BrowserState, Session } from '@lazybox/protocol';
import { ActionMenu } from './ActionMenu';
import { runAction } from './actions';
import { actionIcon, ago, browserBadge, browsing, fitRect, hasBox, modifiersOf, mouseButton, pageOrigin, pointedAction, toDrawn, toPage, type BrowserBadge, type Rect } from './browser-model';
import { Icon } from './Icons';
import type { ModuleProps } from './PanelRegistry';
import { useRunner } from './RunnerContext';
import { useNow } from './settings-parts';
import { Resizer } from './Resizer';
import { SPLITS } from './resizable-model';
import { useBrowserWatch } from './useBrowser';
import { resolveAddress } from './web-address';
import { useLinkPreferences } from './web-links';
import './web-viewer.css';
import './browser.css';

/** One of the person's inputs (SendBrowserInput), in page CSS pixels. */
type Input = { type: 'mouse_move' | 'mouse_down' | 'mouse_up' | 'wheel' | 'key_down' | 'key_up' | 'text'; x?: number; y?: number; button?: string; clickCount?: number; deltaX?: number; deltaY?: number; key?: string; text?: string; modifiers?: number };

/**
 * The agent browser of the conversation in view (BROWSER.md): the page the
 * agent works in, live, with where it acts and what it did. Take over pauses
 * the agent and hands the page to you; Give back returns it; Stop ends the
 * agent's browsing until you let it browse again.
 */
export function BrowserPanel({ target, viewState, onPanelTitle }: ModuleProps) {
  const sessionId = target.sessionId;
  if (!sessionId) return <div className="browser-panel"><div className="browser-empty"><Icon name="globe" size={28}/><h3>Agent browser</h3><p>Open a conversation to watch its agent browse.</p></div></div>;
  return <BrowserSession key={sessionId} sessionId={sessionId} viewState={viewState} onPanelTitle={onPanelTitle}/>;
}

function BrowserSession({ sessionId, viewState, onPanelTitle }: { sessionId: string } & Pick<ModuleProps, 'viewState' | 'onPanelTitle'>) {
  const { api } = useRunner();
  const watch = useBrowserWatch(api, sessionId);
  const { state, actions } = watch;
  const agent = state?.agentName || 'The agent';
  const holding = state?.status === 'open' && state.controller === 'person';
  const badge = browserBadge(state, agent);
  const origin = pageOrigin(state?.url || '');
  const [problem, setProblem] = useState('');
  const [busy, setBusy] = useState('');
  const [flash, setFlash] = useState<{ action: BrowserAction; n: number }>();
  const root = useRef<HTMLDivElement>(null);
  const view = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    onPanelTitle?.(origin ? { text: origin.host, tooltip: `${state?.title ? state.title + '\n' : ''}${state?.url}` } : undefined);
  }, [origin?.host, state?.title, state?.url]);

  const control = async (action: string, url = '') => {
    if (busy) return;
    setBusy(action); setProblem('');
    try {
      watch.adopt(await api.controlBrowser({ sessionId, action, url }));
      if (action === 'take_over') requestAnimationFrame(() => view.current?.focus({ preventScroll: true }));
    } catch (error) { setProblem(message(error)); }
    finally { setBusy(''); }
  };
  // The person's input goes in order, one call after another.
  const queue = useRef(Promise.resolve());
  const send = (input: Input) => {
    queue.current = queue.current.then(() => api.sendBrowserInput({ sessionId, ...input }).then(() => undefined, error => setProblem(message(error))));
  };

  const keys = (event: ReactKeyboardEvent) => {
    if (event.target === view.current || event.metaKey || event.ctrlKey || event.altKey || event.defaultPrevented) return;
    if (event.target instanceof HTMLElement && /^(INPUT|TEXTAREA|SELECT)$/.test(event.target.tagName)) return;
    const key = event.key.toLowerCase();
    if (key === 't' && state?.status === 'open' && !holding) { event.preventDefault(); void control('take_over'); }
    else if (key === 'g' && holding) { event.preventDefault(); void control('give_back'); }
  };

  const live = browsing(state);
  const stopped = state?.status === 'stopped';
  return <div ref={root} className="browser-panel" data-holding={holding || undefined} onKeyDown={keys}>
    <div className="browser-toolbar" role="toolbar" aria-label="Agent browser">
      <BadgeChip badge={badge} closed={!badge}/>
      <span className="browser-toolbar-gap"/>
      {state?.status === 'open' && (holding
        ? <button type="button" className="primary browser-control" disabled={!!busy} aria-keyshortcuts="G" title="Give the browser back to the agent (G)" onClick={() => void control('give_back')}><Icon name="play" size={13}/><span>Give back</span></button>
        : <button type="button" className="browser-control" disabled={!!busy} aria-keyshortcuts="T" title={`Pause ${agent} and use the page yourself (T)`} onClick={() => void control('take_over')}><Icon name="target" size={13}/><span>Take over</span></button>)}
      {live && <button type="button" className="browser-control browser-stop" disabled={!!busy} title={`Close the page and stop ${agent} from browsing until you allow it again`} onClick={() => void control('stop')}><Icon name="stop" size={13}/><span>Stop</span></button>}
      <ActionMenu className="browser-menu" label="Agent browser actions">
        <button type="button" role="menuitemcheckbox" aria-checked={!!state?.browseFreely} disabled={!state || !!busy} onClick={() => void control(state?.browseFreely ? 'ask_sites' : 'browse_freely')}>
          <Icon name={state?.browseFreely ? 'check' : 'shield'}/><span>{state?.browseFreely ? 'Browsing freely · ask again for new sites' : 'Browse freely in this conversation'}</span>
        </button>
        <button type="button" disabled={!state?.url} onClick={() => void navigator.clipboard.writeText(state?.url || '').catch(() => setProblem('Could not copy the address'))}><Icon name="copy"/><span>Copy address</span></button>
        <button type="button" onClick={() => runAction('settings.open', 'browser')}><Icon name="sliders"/><span>Settings → Browser…</span></button>
      </ActionMenu>
    </div>
    {/* Where the page is, read like a browser's address bar: you type in it only while you hold the browser. */}
    {origin && !holding && <div className="browser-address" title={`${state?.title ? state.title + '\n' : ''}${state?.url}`}>
      <span className={`web-origin${origin.secure ? ' is-secure' : ' is-insecure'}`} title={origin.local ? 'A page on this machine' : origin.secure ? 'Secure connection (https)' : 'Not secure: this page is sent unencrypted (http)'}><Icon name={origin.local ? 'terminal' : origin.secure ? 'lock' : 'unlock'} size={12}/><span>{origin.host}</span></span>
      {origin.path && <span className="web-path mono">{origin.path}</span>}
      {state?.title && <span className="browser-title">{state.title}</span>}
      {state?.loading && <span className="browser-spinner" aria-label="Loading" role="img"/>}
    </div>}
    {holding && <TakeOver state={state!} agent={agent} viewState={viewState} busy={!!busy} onControl={control}/>}
    {state?.browseFreely && live && !holding && <p className="browser-note" role="note"><Icon name="alert" size={13}/><span>{agent} opens any site in this conversation without asking.</span><button type="button" onClick={() => void control('ask_sites')}>Ask again</button></p>}
    {stopped && <p className="browser-note is-stopped" role="status"><Icon name="stop" size={13}/><span>{badge?.detail || 'Stopped'}. {agent} can’t browse until you let it.</span><button type="button" className="primary" disabled={!!busy} onClick={() => void control('resume')}>Let {agent} browse again</button></p>}
    {state?.status === 'failed' && <p className="browser-note is-failed" role="alert"><Icon name="alert" size={13}/><span>{state.problem || 'The browser stopped unexpectedly.'} It reopens on {agent}’s next step.</span></p>}
    {problem && <p className="browser-note is-failed" role="alert"><Icon name="alert" size={13}/><span>{problem}</span><button type="button" className="icon-button" aria-label="Dismiss" onClick={() => setProblem('')}><Icon name="close" size={12}/></button></p>}
    {!state && watch.error && !watch.connected ? <div className="browser-empty"><Icon name="globe" size={28}/><h3>Agent browser</h3><p>Reconnecting to the Runner…</p></div>
      : !state || (state.status === 'closed' && !watch.frame) ? <Empty agent={agent} known={!!state}/>
      : <BrowserView canvas={view} frame={watch.frame} frames={watch.frames} state={state} actions={actions} flash={flash} holding={holding} send={send} agent={agent}/>}
    {actions.length > 0 && <ActionLog actions={actions} onPick={action => setFlash(current => ({ action, n: (current?.n || 0) + 1 }))}/>}
  </div>;
}

const message = (error: unknown) => { const text = error instanceof Error ? error.message : String(error); return text.replace(/^\[\w+\]\s*/, ''); };

function BadgeChip({ badge, closed }: { badge?: BrowserBadge; closed: boolean }) {
  if (closed || !badge) return <span className="browser-badge" data-tone="closed" role="status"><span className="browser-dot" aria-hidden="true"/>Closed</span>;
  return <span className="browser-badge" data-tone={badge.tone} role="status" title={badge.detail}><span className="browser-dot" aria-hidden="true"/>{badge.label}</span>;
}

function Empty({ agent, known }: { agent: string; known: boolean }) {
  const who = agent === 'The agent' ? 'an agent' : agent;
  return <div className="browser-empty">
    <Icon name="globe" size={28}/>
    <h3>Agent browser</h3>
    <p>When {who} browses the web for this conversation, you watch it here: every page, click and keystroke, as it happens. Take over at any time, or stop it.</p>
    <p className="browser-empty-small">{known ? 'No page is open.' : 'Nothing has been opened yet.'} Agents ask before opening a new site, and never type passwords.</p>
    <button type="button" className="quiet" onClick={() => runAction('settings.open', 'browser')}><Icon name="sliders" size={13}/> Browser settings</button>
  </div>;
}

/** While the person holds the browser: what that means, and a browser's own controls. */
function TakeOver({ state, agent, viewState, busy, onControl }: { state: BrowserState; agent: string; viewState: ModuleProps['viewState']; busy: boolean; onControl(action: string, url?: string): void }) {
  const { engine } = useLinkPreferences(viewState);
  const [address, setAddress] = useState(state.url);
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => { if (!editing) setAddress(state.url); }, [state.url, editing]);
  const go = (event: FormEvent) => {
    event.preventDefault();
    const resolved = resolveAddress(address, engine);
    if ('error' in resolved) { setError(resolved.error); return; }
    setError(''); setEditing(false); (document.activeElement as HTMLElement | null)?.blur();
    onControl('navigate', resolved.url);
  };
  return <>
    <p className="browser-banner" role="status"><Icon name="target" size={13}/><span><strong>You have the browser</strong> — {agent} is paused until you give it back (<kbd>G</kbd>). Keys go to the page; <kbd>⇧Esc</kbd> leaves it. {agent} only sees what you type if it reads the page afterwards.</span></p>
    <form className="web-toolbar browser-nav" role="toolbar" aria-label="Your browsing" onSubmit={go}>
      <button type="button" className="icon-button" aria-label="Back" title="Back" disabled={!state.canGoBack || busy} onClick={() => onControl('back')}><Icon name="back" size={14}/></button>
      <button type="button" className="icon-button web-forward" aria-label="Forward" title="Forward" disabled={!state.canGoForward || busy} onClick={() => onControl('forward')}><Icon name="back" size={14}/></button>
      <button type="button" className="icon-button" aria-label="Reload" title="Reload" disabled={busy} onClick={() => onControl('reload')}><Icon name="refresh" size={14}/></button>
      <div className={`web-address${editing ? ' is-editing' : ''}`}>
        <input className="mono" aria-label="Address" spellCheck={false} autoCapitalize="off" autoCorrect="off" value={address} placeholder={`Search ${engine.name} or type an address`}
          onChange={event => setAddress(event.target.value)} onFocus={event => { setEditing(true); event.currentTarget.select(); }} onBlur={() => setEditing(false)}
          onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); setAddress(state.url); setEditing(false); event.currentTarget.blur(); } }}/>
      </div>
    </form>
    {error && <p className="browser-note is-failed" role="alert"><Icon name="alert" size={13}/><span>{error}</span></p>}
  </>;
}

/** The page, drawn from the screencast, with where the agent acts; your input while you hold it. */
function BrowserView({ canvas, frame, frames, state, actions, flash, holding, send, agent }: {
  canvas: React.RefObject<HTMLCanvasElement | null>; frame?: BrowserFrame; frames: number; state: BrowserState; actions: BrowserAction[];
  flash?: { action: BrowserAction; n: number }; holding: boolean; send(input: Input): void; agent: string;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  // Read when a decode finishes, which may be renders after it started.
  const latestSize = useRef(size); latestSize.current = size;
  useLayoutEffect(() => {
    const node = box.current!;
    const measure = () => setSize(current => current.width === node.clientWidth && current.height === node.clientHeight ? current : { width: node.clientWidth, height: node.clientHeight });
    measure();
    const observer = new ResizeObserver(measure); observer.observe(node);
    return () => observer.disconnect();
  }, []);
  // The page's size in CSS pixels; the frame's when the Runner did not say.
  const viewport = { width: state.viewportWidth || frame?.width || 0, height: state.viewportHeight || frame?.height || 0 };
  const drawn = fitRect(size.width, size.height, viewport.width, viewport.height);
  const bitmap = useRef<ImageBitmap | undefined>(undefined);
  const decoding = useRef(false), wanted = useRef<BrowserFrame | undefined>(undefined);
  const draw = () => {
    const node = canvas.current, image = bitmap.current;
    if (!node) return;
    const ratio = window.devicePixelRatio || 1;
    const width = Math.max(1, Math.round(latestSize.current.width * ratio)), height = Math.max(1, Math.round(latestSize.current.height * ratio));
    if (node.width !== width || node.height !== height) { node.width = width; node.height = height; }
    const context = node.getContext('2d');
    if (!context) return;
    context.clearRect(0, 0, width, height);
    if (!image) return;
    const at = fitRect(width, height, image.width, image.height);
    context.imageSmoothingEnabled = true; context.imageSmoothingQuality = 'high';
    context.drawImage(image, at.x, at.y, at.width, at.height);
  };
  // Decode the latest frame only: frames that arrive while one decodes are skipped.
  const decode = () => {
    const next = wanted.current;
    if (!next || decoding.current) return;
    wanted.current = undefined; decoding.current = true;
    createImageBitmap(new Blob([next.jpeg as BlobPart], { type: 'image/jpeg' })).then(image => { bitmap.current?.close(); bitmap.current = image; draw(); }, () => { /* a damaged frame: the next one replaces it */ })
      .finally(() => { decoding.current = false; decode(); });
  };
  useEffect(() => { if (frame) { wanted.current = frame; decode(); } }, [frames]);
  useEffect(draw, [size.width, size.height]);
  useEffect(() => () => bitmap.current?.close(), []);

  // Where the agent acts: the latest action with a place, or one picked in the log.
  const pointed = flash?.action || pointedAction(actions);
  const mark = pointed && hasBox(pointed.box) && drawn.width > 0 ? toDrawn(pointed.box, drawn, viewport) : undefined;
  const markKey = pointed ? `${pointed.seq}:${pointed.status}:${flash?.action === pointed ? flash.n : 0}` : '';

  // The person's input, while they hold the browser.
  const point = (event: { clientX: number; clientY: number }) => {
    const bounds = box.current!.getBoundingClientRect();
    return toPage(event.clientX - bounds.left, event.clientY - bounds.top, drawn, viewport);
  };
  const lastMove = useRef(0), pressed = useRef<'left' | 'middle' | 'right' | ''>('');
  const wheel = useRef<{ x: number; y: number; dx: number; dy: number; timer?: ReturnType<typeof setTimeout> }>({ x: 0, y: 0, dx: 0, dy: 0 });
  const pointer = (type: 'mouse_down' | 'mouse_up' | 'mouse_move') => (event: ReactPointerEvent<HTMLCanvasElement>) => {
    if (!holding) return;
    const at = point(event);
    if (type === 'mouse_down') { event.currentTarget.focus({ preventScroll: true }); event.currentTarget.setPointerCapture(event.pointerId); }
    if (!at) return;
    if (type === 'mouse_move') {
      const now = performance.now();
      if (now - lastMove.current < 33) return;
      lastMove.current = now;
      send({ type, x: at.x, y: at.y, ...(pressed.current ? { button: pressed.current } : {}), modifiers: modifiersOf(event) });
      return;
    }
    event.preventDefault();
    const button = mouseButton(event.button);
    pressed.current = type === 'mouse_down' ? button : '';
    send({ type, x: at.x, y: at.y, button, clickCount: Math.max(1, event.detail || 1), modifiers: modifiersOf(event) });
  };
  const onWheel = (event: ReactWheelEvent<HTMLCanvasElement>) => {
    if (!holding) return;
    const at = point(event);
    if (!at) return;
    const w = wheel.current;
    w.x = at.x; w.y = at.y; w.dx += event.deltaX; w.dy += event.deltaY;
    if (w.timer) return;
    w.timer = setTimeout(() => { send({ type: 'wheel', x: w.x, y: w.y, deltaX: w.dx, deltaY: w.dy }); w.dx = 0; w.dy = 0; w.timer = undefined; }, 50);
  };
  const onKey = (type: 'key_down' | 'key_up') => (event: ReactKeyboardEvent<HTMLCanvasElement>) => {
    if (!holding || event.nativeEvent.isComposing) return;
    if (event.key === 'Escape' && event.shiftKey) { if (type === 'key_down') event.currentTarget.blur(); return; }
    // Copy and paste go through the clipboard events below.
    if ((event.metaKey || event.ctrlKey) && ['c', 'v', 'x'].includes(event.key.toLowerCase())) return;
    event.preventDefault(); event.stopPropagation();
    // Every key goes down and up, as the page would see it (letters too: shortcuts and key handlers
    // work); text arrives only from a paste.
    send({ type, key: event.key, modifiers: modifiersOf(event) });
  };
  const live = browsing(state);
  return <div ref={box} className="browser-view" data-live={live || undefined} data-holding={holding || undefined}>
    <canvas ref={canvas} className="browser-canvas" role="img" tabIndex={holding ? 0 : -1}
      aria-label={holding ? `The page, under your control: ${state.title || state.url}. Keys and clicks go to the page; Shift+Escape leaves it.` : `What ${agent} sees: ${state.title || state.url || 'no page yet'}`}
      data-frames={frames}
      onPointerDown={pointer('mouse_down')} onPointerUp={pointer('mouse_up')} onPointerMove={pointer('mouse_move')} onWheel={onWheel}
      onKeyDown={onKey('key_down')} onKeyUp={onKey('key_up')} onContextMenu={event => { if (holding) event.preventDefault(); }}
      onPaste={event => { if (!holding) return; const text = event.clipboardData.getData('text/plain'); if (text) { event.preventDefault(); send({ type: 'text', text }); } }}/>
    {!frame && <div className="browser-placeholder" role="status"><span className="browser-spinner" aria-hidden="true"/>{live ? 'Waiting for the first picture of the page…' : 'No picture of the page yet.'}</div>}
    {mark && !holding && <div className="browser-marks" aria-hidden="true">
      <span key={`h${markKey}`} className="browser-highlight" data-status={pointed!.status} style={{ left: mark.x, top: mark.y, width: mark.width, height: mark.height }}/>
      <span className="browser-cursor" style={{ transform: `translate(${mark.x + mark.width / 2}px, ${mark.y + mark.height / 2}px)` }}/>
    </div>}
    {!live && frame && state.status !== 'open' && <div className="browser-scrim" aria-hidden="true"/>}
  </div>;
}

/** The steps, newest at the bottom; it follows new ones unless you scrolled up to read. */
function ActionLog({ actions, onPick }: { actions: BrowserAction[]; onPick(action: BrowserAction): void }) {
  const list = useRef<HTMLOListElement>(null);
  const stick = useRef(true);
  const now = useNow(15_000);
  const [open, setOpen] = useState(true);
  useLayoutEffect(() => { const node = list.current; if (node && stick.current) node.scrollTop = node.scrollHeight; }, [actions, open]);
  const failed = useMemo(() => actions.filter(a => a.status === 'failed' || a.status === 'blocked').length, [actions]);
  return <>{open && <Resizer split={SPLITS.browserLog} label="browser steps"/>}<section className="browser-log" aria-label="Browser steps" data-open={open || undefined}>
    <button type="button" className="browser-log-toggle" aria-expanded={open} onClick={() => setOpen(!open)}>
      <Icon name="chevron" size={12}/><span>Steps</span><small>{actions.length}{failed ? ` · ${failed} refused or failed` : ''}</small>
    </button>
    {open && <ol ref={list} onScroll={event => { const node = event.currentTarget; stick.current = node.scrollTop + node.clientHeight >= node.scrollHeight - 8; }}>
      {actions.map(action => {
        const at = Number(action.atUnixMillis), running = action.status === 'running';
        const body = <>
          <span className="browser-step-icon" aria-hidden="true">{running ? <span className="browser-spinner"/> : <Icon name={action.status === 'failed' ? 'alert' : actionIcon(action.kind)} size={13}/>}</span>
          {action.actor === 'person' && <span className="browser-step-actor">You</span>}
          <span className="browser-step-text">{action.summary}{action.error && <small> · {action.error}</small>}</span>
          {action.status === 'blocked' && <span className="browser-step-tag">Refused</span>}
          <time dateTime={at ? new Date(at).toISOString() : undefined} title={at ? new Date(at).toLocaleString() : undefined}>{at ? ago(at, now) : ''}</time>
        </>;
        return <li key={String(action.seq)} data-status={action.status} data-actor={action.actor}>
          {hasBox(action.box) ? <button type="button" title="Show where on the page" onClick={() => onPick(action)}>{body}</button> : <div>{body}</div>}
        </li>;
      })}
    </ol>}
  </section></>;
}

/** The chat's "Claude is browsing" chip: opens the agent browser beside the conversation. */
export function BrowsingBadge({ session, agentName, onShow }: { session?: Session; agentName: string; onShow?(): void }) {
  const state = session?.browser;
  if (!state || session?.closed || !browsing(state)) return null;
  const badge = browserBadge(state, agentName);
  if (!badge) return null;
  const content = <><span className="browser-dot" aria-hidden="true"/><span>{badge.label}</span></>;
  return onShow
    ? <button type="button" className="browsing-badge" data-tone={badge.tone} title={`${badge.detail || badge.label} · Show the agent browser`} onClick={onShow}>{content}</button>
    : <span className="browsing-badge" data-tone={badge.tone} title={badge.detail}>{content}</span>;
}
