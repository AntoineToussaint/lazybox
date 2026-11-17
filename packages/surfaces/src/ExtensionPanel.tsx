import { Component, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ConnectError, createRunnerClient, type RunnerClient } from '@lazybox/client-core';
import type { Collection, Extension, ExtensionPanelSession } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { runAction } from './actions';
import { getAppearance, subscribeAppearance } from './appearance/apply';
import { BRIDGE_PROTOCOL, BridgeRefusal, bridgeLimits, createBridgeHost, stringParam, type BridgeHandlers, type BridgeRefused } from './extension-bridge';
import { projectAttention, projectCommands, projectConversations, projectMissions, projectUsage, projectWorkspaces } from './extension-data';
import { describeGrant, extensionStateLabel } from './extension-grants';
import { ExtensionApprovalSheet } from './ExtensionApprovalSheet';
import { Icon } from './Icons';
import type { ModuleProps } from './PanelRegistry';
import { useRunner } from './RunnerContext';
import { useSeen } from './useAttention';
import { useExtensions } from './useExtensions';
import './extensions.css';

/**
 * An extension panel (EXTENSIONS.md): one panel of one of the person's
 * extensions, in a sandboxed frame on the extension content origin, talking
 * to Lazybox only through the bridge. With nothing chosen it lists what can
 * be shown, what waits for approval, and the bundled examples.
 *
 * What the extension does never reaches Lazybox's own state: its frame is
 * opaque-origin (sandbox="allow-scripts" only), its messages are parsed and
 * answered by the bridge host (extension-bridge.ts) and the Runner checks the
 * panel's own credential again; a crash, a refusal or a page it tries to leave
 * for is a quiet notice here, never an error elsewhere.
 */
type Choice = { id: string; panel: string };
type Notice = { key: string; tone: 'info' | 'warn' | 'bad'; text: string; review?: boolean; reload?: boolean };

const readChoice = (store: ViewStateStore | undefined, key: string): Choice | undefined => {
  try { const value = JSON.parse(store?.getItem(key) || 'null'); return value && typeof value.id === 'string' ? { id: value.id, panel: typeof value.panel === 'string' ? value.panel : '' } : undefined; } catch { return undefined; }
};

function themePayload() {
  const { theme, typography } = getAppearance();
  return { scheme: theme.scheme, tokens: theme.tokens, fonts: { ui: getComputedStyle(document.documentElement).getPropertyValue('--font-ui').trim(), code: getComputedStyle(document.documentElement).getPropertyValue('--font-mono').trim() }, textScale: typography.uiFontSize / 13, density: typography.density };
}

/** A load that never finished: the window froze or was killed while an extension started. */
const loadingKey = (id: string) => `lazybox:extension-loading:${id}`;
function storage(): Storage | undefined { try { return globalThis.localStorage; } catch { return undefined; } }

export function ExtensionModule(props: ModuleProps) {
  return <ExtensionBoundary><ExtensionPanel {...props}/></ExtensionBoundary>;
}

/** A bug in this panel's own code is shown here, never thrown into the workspace. */
class ExtensionBoundary extends Component<{ children: ReactNode }, { error?: string }> {
  state: { error?: string } = {};
  static getDerivedStateFromError(error: unknown) { return { error: String((error as Error)?.message || error) }; }
  render() {
    if (this.state.error) return <div className="extension-state" role="alert"><Icon name="alert" size={18}/><p>This panel stopped: {this.state.error}</p><button type="button" onClick={() => this.setState({ error: undefined })}>Try again</button></div>;
    return this.props.children;
  }
}

function ExtensionPanel({ viewState, collections, onPanelTitle }: ModuleProps) {
  const key = viewState ? `${viewState.key}:extension` : '';
  const [choice, setChoiceState] = useState<Choice | undefined>(() => readChoice(viewState, key));
  const setChoice = (next?: Choice) => { setChoiceState(next); try { viewState?.setItem(key, next ? JSON.stringify(next) : ''); } catch { /* a convenience */ } };
  const { value, error } = useExtensions();
  const extension = choice ? value?.extensions.find(e => e.id === choice.id) : undefined;
  const panel = extension?.panels.find(p => p.id === choice?.panel) || extension?.panels[0];
  useEffect(() => {
    onPanelTitle?.(extension && panel ? { text: panel.title, tooltip: `${panel.title} · ${extension.name || extension.id} (extension)` } : undefined);
    return () => onPanelTitle?.(undefined);
  }, [extension?.id, panel?.id, panel?.title]);
  if (!value) return <div className="extension-state" role="status">{error ? <><Icon name="alert" size={18}/><p>Extensions are not available: {error}</p></> : <p>Loading extensions…</p>}</div>;
  if (!choice || !extension || !panel) return <ExtensionHome extensions={value.extensions} examples={value.examples} directory={value.directory} missing={choice && !extension ? choice.id : ''} onChoose={setChoice}/>;
  return <RunningExtension key={`${extension.id}/${panel.id}`} extension={extension} panelId={panel.id} collections={collections} onClose={() => setChoice(undefined)} onSwitch={setChoice}/>;
}

/** What can be shown, what waits for the person, and the examples. */
function ExtensionHome({ extensions, examples, directory, missing, onChoose }: { extensions: Extension[]; examples: { id: string; name: string; description: string; installed: boolean }[]; directory: string; missing: string; onChoose(choice: Choice): void }) {
  const { change } = useExtensions();
  const [reviewing, setReviewing] = useState<Extension>();
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  const running = extensions.filter(e => e.state === 'enabled' || e.state === 'review');
  const waiting = extensions.filter(e => e.state === 'new' || e.state === 'review' || e.state === 'disabled');
  const broken = extensions.filter(e => e.state === 'broken');
  const add = async (id: string) => {
    setBusy(id); setError('');
    let added: Extension | undefined;
    const failed = await change(async api => { const list = await api.installExampleExtension({ example: id }); added = list.extensions.find(e => e.id === id); return list; });
    setBusy('');
    if (failed) setError(failed); else if (added) setReviewing(added);
  };
  return <div className="extension-home">
    {missing && <p className="extension-notice tone-warn" role="status"><Icon name="alert" size={14}/> The extension this panel showed ({missing}) is no longer in your extensions folder.</p>}
    {running.length > 0 && <section aria-labelledby="extensions-running"><h3 id="extensions-running">Show</h3>
      <ul className="extension-tiles">{running.flatMap(ext => ext.panels.map(p => <li key={`${ext.id}/${p.id}`}>
        <button type="button" className="extension-tile" onClick={() => onChoose({ id: ext.id, panel: p.id })}>
          <span className="extension-mark" aria-hidden="true"><Icon name={p.icon || 'puzzle'} size={18}/></span>
          <span><strong>{p.title}</strong><small>{p.description || ext.description || ext.name}</small></span>
        </button>
      </li>))}</ul>
    </section>}
    {waiting.length > 0 && <section aria-labelledby="extensions-waiting"><h3 id="extensions-waiting">Waiting for you</h3>
      <ul className="extension-rows">{waiting.map(ext => <li key={ext.id}>
        <span className="extension-mark" aria-hidden="true"><Icon name={ext.panels[0]?.icon || 'puzzle'} size={16}/></span>
        <span className="extension-row-text"><strong>{ext.name || ext.id}</strong><small>{extensionStateLabel[ext.state]}{ext.grants.length ? ` · ${ext.grants.map(g => describeGrant(g).title.toLowerCase()).slice(0, 2).join(', ')}${ext.grants.length > 2 ? '…' : ''}` : ''}</small></span>
        {ext.state === 'disabled' ? <button type="button" onClick={() => runAction('settings.open', 'extensions')}>Settings…</button>
          : <button type="button" className="primary" onClick={() => setReviewing(ext)}>{ext.state === 'review' ? 'Review' : 'Review and turn on'}</button>}
      </li>)}</ul>
    </section>}
    {broken.length > 0 && <section aria-labelledby="extensions-broken"><h3 id="extensions-broken">Do not load</h3>
      <ul className="extension-rows">{broken.map(ext => <li key={ext.id} className="is-broken">
        <span className="extension-mark" aria-hidden="true"><Icon name="alert" size={16}/></span>
        <span className="extension-row-text"><strong>{ext.name || ext.id}</strong><small>{ext.error || ext.issues.filter(i => i.severity === 'error').map(i => `line ${i.line}: ${i.message}`)[0]}</small></span>
      </li>)}</ul>
    </section>}
    {examples.some(x => !x.installed) && <section aria-labelledby="extensions-examples"><h3 id="extensions-examples">Examples</h3>
      <ul className="extension-rows">{examples.filter(x => !x.installed).map(x => <li key={x.id}>
        <span className="extension-mark" aria-hidden="true"><Icon name="puzzle" size={16}/></span>
        <span className="extension-row-text"><strong>{x.name}</strong><small>{x.description}</small></span>
        <button type="button" disabled={!!busy} onClick={() => void add(x.id)}>{busy === x.id ? 'Adding…' : 'Add'}</button>
      </li>)}</ul>
    </section>}
    {!extensions.length && !examples.some(x => !x.installed) && <div className="extension-state"><Icon name="puzzle" size={22}/><p>No extensions yet.</p></div>}
    {error && <p className="settings-error" role="alert">{error}</p>}
    <footer className="extension-home-footer">
      <span>Make your own: <code>lazybox extension new my-panel</code></span>
      <button type="button" className="quiet" title={directory} onClick={() => runAction('settings.open', 'extensions')}><Icon name="sliders" size={13}/> Manage…</button>
    </footer>
    {reviewing && <ExtensionApprovalSheet extension={reviewing} onClose={() => setReviewing(undefined)} onApproved={ext => onChoose({ id: ext.id, panel: ext.panels[0]?.id || '' })}/>}
  </div>;
}

/** One running panel: its session, its frame and its bridge. */
function RunningExtension({ extension, panelId, collections, onClose, onSwitch }: { extension: Extension; panelId: string; collections: Collection[]; onClose(): void; onSwitch(choice: Choice): void }) {
  const { api, connection } = useRunner();
  const { value } = useExtensions();
  const { seen } = useSeen();
  const seenRef = useRef(seen); seenRef.current = seen;
  const collectionsRef = useRef(collections); collectionsRef.current = collections;
  const frame = useRef<HTMLIFrameElement>(null);
  const [session, setSession] = useState<ExtensionPanelSession>();
  const [opening, setOpening] = useState('');
  const [notices, setNotices] = useState<Notice[]>([]);
  const [reviewing, setReviewing] = useState(false);
  const [stalled, setStalled] = useState(() => { const at = Number(storage()?.getItem(loadingKey(extension.id)) || 0); return at > 0 && Date.now() - at > 15_000; });
  const [reopen, setReopen] = useState(0);
  const panel = extension.panels.find(p => p.id === panelId) || extension.panels[0];
  const runs = extension.state === 'enabled' || extension.state === 'review';
  const notify = (notice: Notice) => setNotices(list => [notice, ...list.filter(n => n.key !== notice.key)].slice(0, 3));
  // A session per panel, and a new one whenever its files or grants change (hot reload).
  useEffect(() => {
    if (!runs || stalled) { setSession(undefined); return; }
    const control = new AbortController();
    setOpening('');
    api.openExtensionPanel({ id: extension.id, panel: panel.id, origin: location.origin }, { signal: control.signal, timeoutMs: 8000 })
      .then(next => { if (!control.signal.aborted) setSession(next); })
      .catch(error => { if (!control.signal.aborted) setOpening(ConnectError.from(error).rawMessage); });
    return () => control.abort();
  }, [api, extension.id, panel.id, runs, extension.digest, extension.approvedGrants.join(), extension.grants.join(), stalled, reopen]);
  // The bridge: the Runner, called with this panel's credential alone.
  const client = useMemo<RunnerClient | undefined>(() => session ? createRunnerClient(connection.baseUrl, session.credential, connection.fetch) : undefined, [session?.credential, connection.baseUrl, connection.fetch]);
  const host = useMemo(() => {
    if (!session || !client) return undefined;
    const wait = async (id: string) => {
      const done = await client.waitUiRequest({ id, timeoutMs: 8000 });
      if (done.state === 'failed') throw new BridgeRefusal('failed', done.error || 'The window could not do that');
      return done;
    };
    const handlers: BridgeHandlers = {
      hello: async () => ({ protocol: BRIDGE_PROTOCOL, extension: { id: extension.id, name: extension.name || extension.id, version: extension.version }, panel: { id: panel.id, title: panel.title }, grants: session.grants, hosts: session.hosts, theme: themePayload() }),
      'workspaces.list': async () => projectWorkspaces((await client.listCollections({})).collections),
      'sessions.list': async params => {
        const limit = Math.min(500, Math.max(1, Number(params.limit) || 200));
        const workspaceId = typeof params.workspaceId === 'string' ? params.workspaceId : undefined;
        return projectConversations((await client.listHistory({ limit, ...(workspaceId ? { collectionId: workspaceId } : {}) })).sessions, collectionsRef.current, workspaceId);
      },
      'attention.list': async () => projectAttention((await client.listHistory({ limit: 300 })).sessions, collectionsRef.current, seenRef.current),
      'commands.list': async params => projectCommands((await client.listCommands({ sessionId: stringParam(params, 'sessionId') })).commands),
      'missions.list': async () => projectMissions((await client.listMissions({})).missions),
      'usage.read': async params => {
        const ids = typeof params.workspaceId === 'string' ? [params.workspaceId] : collectionsRef.current.filter(c => c.state !== 'deleted').slice(0, 20).map(c => c.id);
        const series = params.series === true;
        const reports = await Promise.all(ids.map(async workspaceId => ({ workspaceId, report: await client.getTelemetry({ workspace: workspaceId, series }) })));
        return projectUsage(reports, collectionsRef.current, { series });
      },
      'conversations.show': async params => {
        const sessionId = stringParam(params, 'sessionId');
        await wait((await client.postUiRequest({ kind: 'open', sessionId })).id);
        return { shown: true };
      },
      'messages.send': async params => {
        const sessionId = stringParam(params, 'sessionId'), text = stringParam(params, 'text', bridgeLimits.messageChars);
        await client.apply({ sessionId, operationId: crypto.randomUUID(), operation: { case: 'message', value: { text } } });
        return { sent: true };
      },
      do: async params => {
        const action = stringParam(params, 'action', 80), argument = typeof params.argument === 'string' ? params.argument.slice(0, 200) : '';
        const done = await wait((await client.postUiRequest({ kind: 'do', actionId: action, argument })).id);
        return { done: true, detail: done.detail };
      },
    };
    const refusedAt = new Map<string, number>();
    return createBridgeHost({ grants: session.grants, handlers, onRefused: (refused: BridgeRefused) => {
      // Said once a minute per kind: a polling extension does not flood the panel.
      const k = `${refused.code}:${refused.grant || refused.method}`;
      if (Date.now() - (refusedAt.get(k) || 0) < 60_000) return;
      refusedAt.set(k, Date.now());
      if (refused.code === 'not-granted') notify({ key: k, tone: 'warn', review: true, text: `${extension.name || extension.id} asked to ${describeGrant(refused.grant || '').title.toLowerCase()}; you have not allowed it.` });
      else if (refused.code === 'refused') { notify({ key: k, tone: 'warn', text: 'Lazybox refused a request from this extension; reconnecting it.' }); setReopen(n => n + 1); }
      else notify({ key: k, tone: refused.code === 'rate-limited' ? 'warn' : 'info', text: refused.message });
    } });
  }, [session, client]);
  // Messages from the frame: only from this panel's own shell.
  useEffect(() => {
    if (!host || !session) return;
    const receive = async (event: MessageEvent) => {
      if (!frame.current || event.source !== frame.current.contentWindow) return;
      const data = event.data as { lazybox?: unknown; from?: unknown; event?: unknown; data?: Record<string, unknown>; message?: { event?: unknown; data?: Record<string, unknown> } };
      if (!data || data.lazybox !== 1) return;
      const post = (message: unknown) => frame.current?.contentWindow?.postMessage({ lazybox: 1, to: 'extension', message }, '*');
      if (data.from === 'shell') {
        if (data.event === 'blocked') notify({ key: 'left', tone: 'bad', text: `${extension.name || extension.id} tried to leave its panel for ${String(data.data?.uri || 'another page')}; it stays on its own pages.`, reload: true });
        return;
      }
      if (data.from !== 'extension' || !data.message || typeof data.message !== 'object') return;
      const message = data.message;
      if (message.event === 'blocked') { notify({ key: `blocked:${String(message.data?.uri)}`, tone: 'warn', text: `Blocked ${String(message.data?.directive || '').replace(/-src(-elem|-attr)?$/, '')} to ${String(message.data?.uri || 'an inline source')}: ${extension.hosts.length ? 'not one of the hosts you allowed' : 'this extension may not use the network'}.` }); return; }
      if (message.event === 'error') { notify({ key: 'error', tone: 'info', text: `Script error in the extension: ${String(message.data?.message || '').slice(0, 200)}` }); return; }
      const reply = await host(message);
      if (!reply) return;
      if ((message as { method?: unknown }).method === 'hello' && !reply.error) {
        // Loaded: it got far enough to say hello.
        setTimeout(() => storage()?.removeItem(loadingKey(extension.id)), 3000);
      }
      post(reply);
    };
    window.addEventListener('message', receive);
    const theme = subscribeAppearance(() => frame.current?.contentWindow?.postMessage({ lazybox: 1, to: 'extension', message: { lazybox: 1, event: 'theme', data: themePayload() } }, '*'));
    return () => { window.removeEventListener('message', receive); theme(); };
  }, [host, session]);
  // A load in progress is recorded until it says hello, so a frozen window offers safe mode next time.
  useEffect(() => {
    if (!session) return;
    try { storage()?.setItem(loadingKey(extension.id), String(Date.now())); } catch { /* best effort */ }
    const clear = () => storage()?.removeItem(loadingKey(extension.id));
    window.addEventListener('pagehide', clear);
    return () => window.removeEventListener('pagehide', clear);
  }, [session?.url]);
  const others = (value?.extensions || []).filter(e => e.state === 'enabled' || e.state === 'review').flatMap(e => e.panels.map(p => ({ e, p })));
  const title = panel.title;
  const reload = () => frame.current?.contentWindow?.postMessage({ lazybox: 1, to: 'shell', command: 'reload' }, '*');
  return <div className="extension-panel">
    <div className="extension-bar">
      <span className="extension-mark is-small" aria-hidden="true"><Icon name={panel.icon || 'puzzle'} size={13}/></span>
      <select aria-label="Extension panel" title="Show another extension" value={`${extension.id}/${panel.id}`} onChange={event => { const [id, p] = event.target.value.split('/'); if (id === '') onClose(); else onSwitch({ id, panel: p }); }}>
        {others.map(({ e, p }) => <option key={`${e.id}/${p.id}`} value={`${e.id}/${p.id}`}>{p.title}{e.panels.length > 1 ? ` · ${e.name}` : ''}</option>)}
        <option value="/">All extensions…</option>
      </select>
      {extension.state === 'review' && <button type="button" className="extension-chip tone-warn" onClick={() => setReviewing(true)}>Asks for more · Review</button>}
      <span className="extension-spacer"/>
      <button type="button" className="quiet" aria-label={`What ${extension.name || extension.id} may do`} title="What it may read and do" onClick={() => setReviewing(true)}><Icon name="shield" size={13}/></button>
      <button type="button" className="quiet" aria-label="Reload the extension" title="Reload" disabled={!session} onClick={reload}><Icon name="refresh" size={13}/></button>
    </div>
    {!runs ? <div className="extension-state"><Icon name="lock" size={18}/><p>{extension.name || extension.id}: {extensionStateLabel[extension.state]?.toLowerCase()}.</p>
        {extension.state === 'new' ? <button type="button" className="primary" onClick={() => setReviewing(true)}>Review and turn on</button> : <button type="button" onClick={() => runAction('settings.open', 'extensions')}>Open Settings → Extensions</button>}</div>
      : stalled ? <div className="extension-state" role="alert"><Icon name="alert" size={18}/><p>{extension.name || extension.id} did not finish loading last time; the window may have frozen.</p>
        <div className="extension-state-actions"><button type="button" className="primary" onClick={() => { storage()?.removeItem(loadingKey(extension.id)); setStalled(false); }}>Load it</button><button type="button" onClick={onClose}>Show something else</button></div></div>
      : opening ? <div className="extension-state" role="alert"><Icon name="alert" size={18}/><p>{opening}</p><button type="button" onClick={() => setReopen(n => n + 1)}>Try again</button></div>
      : session ? <iframe ref={frame} key={session.url} className="extension-frame" title={`${title} (extension)`} src={session.url} sandbox="allow-scripts" referrerPolicy="no-referrer"/>
      : <div className="extension-state" role="status"><p>Starting {title}…</p></div>}
    {notices.length > 0 && <ul className="extension-notices" aria-label="Extension notices">{notices.map(n => <li key={n.key} className={`extension-notice tone-${n.tone}`} role="status">
      <Icon name={n.tone === 'info' ? 'alert' : 'shield'} size={13}/><span>{n.text}</span>
      {n.review && <button type="button" onClick={() => setReviewing(true)}>Review access</button>}
      {n.reload && <button type="button" onClick={() => { reload(); setNotices(list => list.filter(x => x.key !== n.key)); }}>Reload</button>}
      <button type="button" className="quiet" aria-label="Dismiss" onClick={() => setNotices(list => list.filter(x => x.key !== n.key))}>×</button>
    </li>)}</ul>}
    {reviewing && <ExtensionApprovalSheet extension={extension} onClose={() => setReviewing(false)}/>}
  </div>;
}

