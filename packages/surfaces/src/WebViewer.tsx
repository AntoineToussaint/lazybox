import { focusWhenShown } from './focus';
import { createContext, useContext, useEffect, useRef, useState, type Dispatch, type FormEvent, type KeyboardEvent as ReactKeyboardEvent, type ReactNode, type SetStateAction } from 'react';
import type { NativeWebViews, WebViewEvent } from '@lazybox/platform';
import { ActionMenu } from './ActionMenu';
import { Icon } from './Icons';
import { useNativeWebView } from './native-web-view';
import { current, isLoopback, nextZoom, originOf, resolveAddress, step, titled, visit, arrive, type SearchEngine, type WebHistory } from './web-address';
import { openInBrowser } from './web-links';
import { isMac } from './workbench';
import './web-viewer.css';

/** The host's native web views (the desktop app), for every viewer in the window. */
export const WebViewsContext = createContext<NativeWebViews | undefined>(undefined);

/** How long an iframe may take to load before the viewer says it may not show here. */
export const FRAME_STALL_MS = 12_000;
let framingHintDismissed = false;
const alt = isMac ? '⌥' : 'Alt';

export type WebViewerProps = {
  /** Unique in the window: names the native view. */
  id: string;
  /** The region's accessible name. */
  label: string;
  /** The page's accessible name (the iframe's title). */
  frameTitle: string;
  history: WebHistory;
  setHistory: Dispatch<SetStateAction<WebHistory>>;
  zoom: number;
  onZoom(zoom: number): void;
  engine: SearchEngine;
  addressLabel?: string;
  reloadLabel?: string;
  /** Before the navigation buttons (the Preview's action picker). */
  leading?: ReactNode;
  /** Under the start page's prompt, when no page is open. */
  start?: ReactNode;
  /** More items for the ⋯ menu. */
  menu?: ReactNode;
  /** Bumped to put the cursor in the address bar. */
  focusAddress?: number;
  onIcon?(icon: string): void;
};

/**
 * A small browser: address bar (an address or a search), back, forward,
 * reload or stop, zoom, copy the address, open in your browser. The page is a
 * native web view where the host has one (any site shows), else an iframe
 * (sites that refuse framing get a clear way out). The origin is always on
 * screen, with a padlock for a secure page.
 */
export function WebViewer(props: WebViewerProps) {
  const { id, label, frameTitle, history, setHistory, zoom, onZoom, engine, addressLabel = 'Address', reloadLabel = 'Reload', leading, start, menu, focusAddress, onIcon } = props;
  const views = useContext(WebViewsContext);
  const entry = current(history);
  const target = history.expect !== undefined ? history.entries[history.expect]?.url : undefined;
  const page = target ?? entry?.url ?? '';
  const [address, setAddress] = useState(page);
  const [editing, setEditing] = useState(false);
  const [loading, setLoading] = useState(!!page);
  const [notice, setNotice] = useState<{ text: string; url?: string; action?: string }>();
  const [status, setStatus] = useState('');
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => { if (!editing) setAddress(page); }, [page, editing]);
  // A panel just placed in the dock is drawn on its next frame: focus waits for it.
  useEffect(() => { if (focusAddress) { focusWhenShown(input.current); input.current?.select(); } }, [focusAddress]);
  useEffect(() => { if (!status) return; const timer = setTimeout(() => setStatus(''), 2500); return () => clearTimeout(timer); }, [status]);
  const origin = originOf(page);
  /** The address after its origin, shown beside the origin while nobody is typing. */
  const shownAddress = origin && !editing && address === page ? (() => { try { const url = new URL(page); return `${url.pathname}${url.search}${url.hash}`; } catch { return ''; } })() : '';
  const report = (error: unknown) => setNotice({ text: String(error instanceof Error ? error.message : error) });

  const go = (event: FormEvent) => {
    event.preventDefault();
    const resolved = resolveAddress(address, engine);
    if ('error' in resolved) { setNotice({ text: resolved.error }); return; }
    if (new URL(resolved.url).origin === location.origin) { setNotice({ text: 'That address is Lazybox itself; it is not shown inside itself.' }); return; }
    setNotice(undefined); setEditing(false); input.current?.blur();
    if (resolved.url === entry?.url && target === undefined) { reload(); return; }
    setHistory(h => visit(h, resolved.url)); setLoading(true);
  };
  const move = (by: -1 | 1) => { setHistory(h => step(h, by)); setLoading(true); };
  const native = !!views;
  const surface = useRef<HTMLDivElement>(null);
  const [nonce, setNonce] = useState(0);
  const reload = () => {
    setNotice(undefined); setLoading(true);
    if (native) void views!.action(id, 'reload').catch(report); else setNonce(n => n + 1);
  };
  const stop = () => {
    setLoading(false);
    if (native) void views!.action(id, 'stop').catch(report);
    else try { frame.current?.contentWindow?.stop(); } catch { /* another origin's page */ }
  };
  const copy = () => void navigator.clipboard.writeText(page).then(() => setStatus('Address copied'), () => setStatus('Could not copy the address'));
  const open = (url = page) => void openInBrowser(url).catch(report);
  const keys = (event: ReactKeyboardEvent) => {
    if (event.altKey && (event.key === 'ArrowLeft' || event.key === 'ArrowRight') && !(event.target instanceof HTMLInputElement)) { event.preventDefault(); move(event.key === 'ArrowLeft' ? -1 : 1); }
    if ((isMac ? event.metaKey : event.ctrlKey) && event.key.toLowerCase() === 'l') { event.preventDefault(); input.current?.focus(); input.current?.select(); }
  };

  // Native: the view reports its own loads, so links followed inside the page are history too.
  const [shown, setShown] = useState(false);
  const ready = useRef(false);
  useNativeWebView(views, id, surface, page, zoom, report, value => { setShown(value); if (value) ready.current = true; });
  useEffect(() => {
    if (!views) return;
    let host = origin?.host;
    return views.subscribe((event: WebViewEvent) => {
      if (event.id !== id) return;
      if (event.kind === 'loading') setLoading(true);
      else if (event.kind === 'loaded') {
        setLoading(false);
        const next = originOf(event.url)?.host;
        if (next !== host) { host = next; onIcon?.(''); }
        setHistory(h => { const landed = arrive(h, event.url); return event.title ? titled(landed, event.title) : landed; });
      }
      else if (event.kind === 'title') setHistory(h => titled(h, event.title));
      else if (event.kind === 'icon') onIcon?.(event.icon);
      else if (event.kind === 'blocked') setNotice({ text: `This page wanted to open a ${event.url.split(':')[0]}: link, which Lazybox does not open here.`, url: event.url, action: 'Open it' });
      else if (event.kind === 'download') setNotice({ text: 'This page started a download. The viewer never saves files; open the page in your browser to download it.', url: event.url, action: 'Open in browser' });
    });
  }, [views, id]);
  const asked = useRef('');
  useEffect(() => {
    if (!views || target === undefined) return;
    const key = `${history.expect}:${target}`;
    if (asked.current === key) return;
    asked.current = key;
    // A view not created yet opens on the latest address by itself.
    if (ready.current) void views.navigate(id, target).catch(report);
  }, [views, id, target, history.expect]);

  // Iframe: only the address it was given is known; a page from another origin keeps its own.
  const frame = useRef<HTMLIFrameElement>(null);
  const [framing, setFraming] = useState<'ok' | 'stalled' | 'refused'>('ok');
  const [loaded, setLoaded] = useState(false);
  const stall = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => {
    if (native || !page) return;
    setFraming('ok'); setLoaded(false); setLoading(true);
    stall.current = setTimeout(() => setFraming(state => state === 'ok' ? 'stalled' : state), FRAME_STALL_MS);
    return () => clearTimeout(stall.current);
  }, [native, page, nonce]);
  const frameLoaded = () => {
    clearTimeout(stall.current);
    setLoading(false); setLoaded(true); setFraming('ok');
    let href = '', title = '';
    try { href = frame.current?.contentWindow?.location.href || ''; title = frame.current?.contentDocument?.title || ''; } catch { /* another origin: its address stays its own */ }
    // A refused frame that a browser leaves blank is the one refusal a page can see.
    if (href === 'about:blank' && page !== 'about:blank') { setFraming('refused'); return; }
    setHistory(h => { const landed = arrive(h, href && href !== 'about:blank' ? href : h.entries[h.expect ?? h.index]?.url || page); return title ? titled(landed, title) : landed; });
  };
  const crossOrigin = !!origin && !native && !origin.local && (() => { try { return new URL(page).origin !== location.origin; } catch { return false; } })();

  return <section className="web-viewer" aria-label={label} onKeyDown={keys}>
    <form className="web-toolbar" onSubmit={go} role="toolbar" aria-label={`${label} navigation`}>
      {leading}
      <button type="button" className="icon-button" aria-label="Back" title={`Back (${alt}←)`} disabled={history.index <= 0} onClick={() => move(-1)}><Icon name="back" size={14}/></button>
      <button type="button" className="icon-button web-forward" aria-label="Forward" title={`Forward (${alt}→)`} disabled={history.index >= history.entries.length - 1} onClick={() => move(1)}><Icon name="back" size={14}/></button>
      {loading && page
        ? <button type="button" className="icon-button" aria-label="Stop loading" title="Stop" onClick={stop}><Icon name="close" size={14}/></button>
        : <button type="button" className="icon-button" aria-label={reloadLabel} title="Reload" disabled={!page} onClick={reload}><Icon name="refresh" size={14}/></button>}
      <div className={`web-address${editing ? ' is-editing' : ''}${shownAddress ? ' is-shown' : ''}`}>
        {/* Read like a browser's: where the page is, prominent; the rest of its address, quiet. Typing shows the whole address. */}
        {shownAddress && <span className="web-shown-address" aria-hidden="true">
          <span className={`web-origin${origin!.secure ? ' is-secure' : ' is-insecure'}`} onClick={() => input.current?.focus()} title={origin!.local ? 'A page on this machine' : origin!.secure ? 'Secure connection (https)' : 'Not secure: this page is sent unencrypted (http)'}>
            <Icon name={origin!.local ? 'terminal' : origin!.secure ? 'lock' : 'unlock'} size={12}/><span>{origin!.host}</span>
          </span>
          {shownAddress !== '/' && <span className="web-path mono">{shownAddress}</span>}
        </span>}
        <input ref={input} className="mono" aria-label={addressLabel} spellCheck={false} autoCapitalize="off" autoCorrect="off" placeholder={`Search ${engine.name} or type an address`} value={address}
          onChange={event => setAddress(event.target.value)}
          onFocus={event => { setEditing(true); event.currentTarget.select(); }}
          onBlur={() => setEditing(false)}
          onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); setAddress(page); setEditing(false); event.currentTarget.blur(); } }}/>
      </div>
      {zoom !== 1 && <button type="button" className="web-zoom" aria-label={`Zoom ${Math.round(zoom * 100)}%. Reset to 100%`} title="Reset zoom" onClick={() => onZoom(1)}>{Math.round(zoom * 100)}%</button>}
      <button type="button" className="icon-button web-open" aria-label="Open in browser" title="Open in your browser" disabled={!page} onClick={() => open()}><Icon name="external" size={14}/></button>
      <ActionMenu className="web-menu" label={`${label} actions`}>
        <button type="button" disabled={!page} onClick={() => open()}><Icon name="external"/><span>Open in your browser</span></button>
        <button type="button" className="web-narrow-only" disabled={history.index >= history.entries.length - 1} onClick={() => move(1)}><Icon name="chevronRight"/><span>Forward</span></button>
        <button type="button" disabled={!page} onClick={copy}><Icon name="copy"/><span>Copy address</span></button>
        <button type="button" disabled={zoom >= 2} onClick={() => onZoom(nextZoom(zoom, 1))}><Icon name="zoomIn"/><span>Zoom in</span></button>
        <button type="button" disabled={zoom <= 0.5} onClick={() => onZoom(nextZoom(zoom, -1))}><Icon name="zoomOut"/><span>Zoom out</span></button>
        <button type="button" disabled={zoom === 1} onClick={() => onZoom(1)}><Icon name="fit"/><span>Actual size</span></button>
        {menu}
      </ActionMenu>
    </form>
    <div className="web-progress-track" aria-hidden="true">{loading && page && <span className="web-progress"/>}</div>
    {status && <p className="web-status" role="status">{status}</p>}
    {notice && <p className="web-notice" role="alert">
      <Icon name="alert" size={14}/><span>{notice.text}</span>
      {notice.url && <button type="button" onClick={() => { open(notice.url); setNotice(undefined); }}>{notice.action || 'Open in browser'}</button>}
      <button type="button" className="icon-button" aria-label="Dismiss" onClick={() => setNotice(undefined)}><Icon name="close" size={12}/></button>
    </p>}
    <div className={`web-page${native ? ' is-native' : ''}`}>
      {!page ? <div className="web-start">
        <Icon name="globe" size={28}/>
        <h3>Open a web page</h3>
        <p>Type an address above, or words to search with {engine.name}.</p>
        {start}
        <p className="web-tip">{alt}-click a link anywhere in Lazybox to open it here, or right-click it for more.</p>
      </div> : native ? <div ref={surface} className="web-surface" data-shown={shown || undefined}>
        {/* Under the native view; seen only while a menu or dialog is in front of it. */}
        <div className="web-placeholder" aria-hidden={shown || undefined}><Icon name="globe" size={20}/><span>{entry?.title || origin?.host || page}</span></div>
      </div> : <>
        {framing === 'ok'
          ? <iframe ref={frame} key={`${page}#${nonce}`} title={frameTitle} src={page} referrerPolicy="no-referrer" style={zoom !== 1 ? { width: `${100 / zoom}%`, height: `${100 / zoom}%`, transform: `scale(${zoom})`, transformOrigin: '0 0' } : undefined}
            sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-modals allow-downloads" onLoad={frameLoaded}/>
          : <div className="web-refused" role="status">
            <Icon name="globe" size={28}/>
            <h3>{framing === 'refused' ? 'This site can’t be shown here' : 'This page isn’t loading here'}</h3>
            <p>{framing === 'refused' ? `${origin?.host || 'This site'} doesn’t allow other apps to show its pages.` : `${origin?.host || 'This page'} has not loaded after ${FRAME_STALL_MS / 1000} seconds. Some sites refuse to be shown inside other apps.`}</p>
            <div className="web-refused-actions">
              <button type="button" className="primary" onClick={() => open()}><Icon name="external" size={14}/> Open in browser</button>
              {framing === 'stalled' && <button type="button" onClick={() => { setFraming('ok'); setNonce(n => n + 1); }}>Try again</button>}
            </div>
          </div>}
        {framing === 'ok' && loaded && crossOrigin && !framingHintDismissed && <FramingHint onOpen={() => open()}/>}
      </>}
    </div>
  </section>;
}

/** A browser does not tell a page that a frame was refused; this says what a blank page means. */
function FramingHint({ onOpen }: { onOpen(): void }) {
  const [hidden, setHidden] = useState(false);
  if (hidden) return null;
  return <p className="web-framing-hint">
    <span>Blank page? Some sites refuse to be shown inside other apps.</span>
    <button type="button" onClick={onOpen}>Open in browser</button>
    <button type="button" className="icon-button" aria-label="Dismiss" onClick={() => { framingHintDismissed = true; setHidden(true); }}><Icon name="close" size={12}/></button>
  </p>;
}
