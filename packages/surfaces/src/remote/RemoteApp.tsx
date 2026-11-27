import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent, type KeyboardEvent } from 'react';
import { agentTitle } from '../providers';
import { ConnectError, DeviceRequestError, DeviceRevoked, indexedDBDeviceStore, openDeviceSession, pairDevice, parsePairingFragment, type DeviceSession, type DeviceStore, type PairingOffer, type StoredDevice } from '@lazybox/client-core';
import type { Collection, Session } from '@lazybox/protocol';
import { RunnerProvider, useRunner } from '../RunnerContext';
import { AttentionScope, useInbox, useMarkViewed } from '../useAttention';
import { conversationKey, requestSummary } from '../attention';
import { conversationName } from '../conversation-model';
import type { Approval } from '../request-model';
import { sessionStatus } from '../agent-status';
import { useTranscript } from '../useTranscript';
import { ConversationLoading, MessageList } from '../MessageList';
import { RequestCard } from '../session-parts';
import { QueuedMessages, useQueuedDelivery } from '../QueuedMessages';
import { confirmMessage, conversationReader } from '../transcript-cache';
import { queuedMessages } from '../transcript-model';
import { LiveIndicator, useLiveStatus } from '../LiveIndicator';
import { Icon } from '../Icons';
import { useComposerDictation } from '../voice/ComposerDictation';
import { VoicePill } from '../voice/VoiceLayer';
import { readAloud, stopReading, synthesizer, useVoiceState } from '../voice/voice-session';
import { speakableText } from '../voice/voice-model';
import { latestAgentReply } from '../voice/useReadAloudReplies';
import { grantLabels } from '../DevicesSettings';
import { parseRoute, useRoute, type Route } from './route';
import { localPinStore, runnerPinStore, type PinStore } from './pins';
import { closeStaleNotifications, registerWorker, resyncPush } from './push';
import { PhoneInbox } from './PhoneInbox';
import { PhoneSettings } from './PhoneSettings';
import { CommandStrip, PhoneCommand, useConversationCommands } from './PhoneCommand';
import { ConfirmSheet } from './PhoneSheets';
import '../style.css';
import '../inbox.css';
import './remote.css';

type Phase =
  | { kind: 'loading' }
  | { kind: 'pair'; offer: PairingOffer; replacing?: StoredDevice }
  | { kind: 'unpaired' }
  | { kind: 'revoked' }
  | { kind: 'offline'; stored: StoredDevice; message: string }
  | { kind: 'ready'; stored: StoredDevice; session: DeviceSession };

/** Renew the credential this long before it ends, so no request meets an expired one. */
const RENEW_BEFORE_MS = 90_000;

function defaultDeviceName(): string {
  const agent = navigator.userAgent;
  if (/iPhone/.test(agent)) return 'iPhone';
  if (/iPad/.test(agent)) return 'iPad';
  if (/Android/.test(agent)) return /Mobile/.test(agent) ? 'Android phone' : 'Android tablet';
  return 'Phone';
}

/**
 * Lazybox on a phone (REMOTE.md): pairing from the QR code, then the inbox,
 * a conversation, its requests, and a composer. It talks only to the device
 * gateway, with a credential its own key obtains; the owner's bearer never
 * reaches the phone.
 */
export function RemoteApp({ baseUrl = location.origin, store: given, fetch }: { baseUrl?: string; store?: DeviceStore; fetch?: typeof globalThis.fetch }) {
  const store = useMemo(() => given || indexedDBDeviceStore(), [given]);
  const [phase, setPhase] = useState<Phase>({ kind: 'loading' });
  const signIn = useCallback(async (stored: StoredDevice) => {
    try { setPhase({ kind: 'ready', stored, session: await openDeviceSession({ baseUrl, stored, fetch }) }); }
    catch (error) {
      if (error instanceof DeviceRevoked) { await store.clear().catch(() => {}); setPhase({ kind: 'revoked' }); return; }
      setPhase({ kind: 'offline', stored, message: error instanceof Error ? error.message : String(error) });
    }
  }, [baseUrl, fetch, store]);
  // The QR link's secret lives in the fragment; take it once, then drop it from the address bar and history.
  const [offer] = useState(() => parsePairingFragment(location.hash));
  useEffect(() => {
    let cancelled = false;
    if (location.hash) { const url = new URL(location.href); url.hash = ''; history.replaceState(history.state, '', url); }
    void store.load().catch(() => undefined).then(stored => {
      if (cancelled) return;
      if (offer) setPhase({ kind: 'pair', offer, replacing: stored });
      else if (stored) void signIn(stored);
      else setPhase({ kind: 'unpaired' });
    });
    return () => { cancelled = true; };
  }, [store, signIn, offer]);
  const revoked = useCallback(() => { void store.clear().catch(() => {}); setPhase({ kind: 'revoked' }); }, [store]);
  const forget = useCallback(async () => { await store.clear().catch(() => {}); setPhase({ kind: 'unpaired' }); }, [store]);
  return <div className="remote-app">
    {phase.kind === 'loading' && <div className="remote-center" role="status"><Brand/><p>Opening Lazybox…</p></div>}
    {phase.kind === 'pair' && <PairScreen baseUrl={baseUrl} offer={phase.offer} replacing={phase.replacing} store={store} fetch={fetch} onPaired={stored => void signIn(stored)}/>}
    {phase.kind === 'unpaired' && <Explain title="Pair this phone with Lazybox" lines={['On your computer, open Lazybox → Settings → Devices.', 'Choose Pair a phone.', 'Scan the code with this phone’s camera.']}/>}
    {phase.kind === 'revoked' && <Explain title="This phone was signed out" lead="It was revoked on your computer, so it can no longer see or answer your agents." lines={['To use it again, open Lazybox → Settings → Devices on your computer.', 'Choose Pair a phone and scan the new code.']}/>}
    {phase.kind === 'offline' && <div className="remote-center"><Brand/><h1>Can’t reach your computer</h1><p>{phase.message}</p><p className="remote-hint">Lazybox keeps working there. Check that the computer is awake and that Tailscale is connected on both devices.</p><button className="primary remote-wide" onClick={() => void signIn(phase.stored)}>Try again</button></div>}
    {phase.kind === 'ready' && <Signed baseUrl={baseUrl} stored={phase.stored} initial={phase.session} fetch={fetch} onRevoked={revoked} onForget={forget}/>}
  </div>;
}

function Brand() { return <span className="remote-brand" aria-hidden="true">✳︎</span>; }

function Explain({ title, lead, lines }: { title: string; lead?: string; lines: string[] }) {
  return <div className="remote-center"><Brand/><h1>{title}</h1>{lead && <p>{lead}</p>}<ol className="remote-steps">{lines.map(line => <li key={line}>{line}</li>)}</ol></div>;
}

function PairScreen({ baseUrl, offer, replacing, store, fetch, onPaired }: { baseUrl: string; offer: PairingOffer; replacing?: StoredDevice; store: DeviceStore; fetch?: typeof globalThis.fetch; onPaired(stored: StoredDevice): void }) {
  const [name, setName] = useState(defaultDeviceName);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [paired, setPaired] = useState<StoredDevice>();
  const submit = async (event: FormEvent) => {
    event.preventDefault(); if (busy) return;
    setBusy(true); setError('');
    try { const stored = await pairDevice({ baseUrl, offer, name: name.trim(), store, fetch }); setPaired(stored); }
    catch (e) { setError(e instanceof DeviceRequestError || e instanceof Error ? e.message : String(e)); }
    finally { setBusy(false); }
  };
  if (paired) return <div className="remote-center" role="status">
    <span className="remote-check" aria-hidden="true"><Icon name="check" size={30}/></span>
    <h1>Paired</h1>
    <p>Your computer shows this phone as <strong>{paired.device.name}</strong> with key <span className="mono">{paired.device.fingerprint}</span>.</p>
    <button className="primary remote-wide" autoFocus onClick={() => onPaired(paired)}>Open inbox</button>
  </div>;
  return <form className="remote-center remote-pair" onSubmit={submit}>
    <Brand/>
    <h1>Pair this phone</h1>
    <p>Follow and answer the agents running on your computer.</p>
    <div className="remote-grants" aria-label="This phone will be able to">
      {offer.grants.map(grant => <span key={grant}><Icon name="check" size={14}/>{grantLabels[grant] || grant}</span>)}
    </div>
    <label className="remote-name">Name this phone<input value={name} maxLength={64} autoComplete="off" onChange={event => setName(event.target.value)} required/></label>
    {replacing && <p className="remote-hint">This phone is already paired as {replacing.device.name}. Pairing again replaces it here; revoke the old one on your computer.</p>}
    {error && <p className="remote-error" role="alert">{error}</p>}
    <button className="primary remote-wide" disabled={busy || !name.trim()}>{busy ? 'Pairing…' : 'Pair this phone'}</button>
    <small className="remote-hint">A key is created on this phone and never leaves it. You can revoke this phone at any time in Settings → Devices on your computer.</small>
  </form>;
}

function Signed({ baseUrl, stored, initial, fetch, onRevoked, onForget }: { baseUrl: string; stored: StoredDevice; initial: DeviceSession; fetch?: typeof globalThis.fetch; onRevoked(): void; onForget(): void }) {
  const [session, setSession] = useState(initial);
  const renewing = useRef<Promise<DeviceSession | undefined> | undefined>(undefined);
  const renew = useCallback(() => {
    renewing.current ??= openDeviceSession({ baseUrl, stored, fetch }).then(next => { setSession(next); return next; }, error => { if (error instanceof DeviceRevoked) onRevoked(); return undefined; }).finally(() => { renewing.current = undefined; });
    return renewing.current.then(next => next && { baseUrl, token: next.token });
  }, [baseUrl, stored, fetch, onRevoked]);
  // Renew ahead of expiry; a refused credential also renews through the link.
  useEffect(() => {
    const timer = setTimeout(() => void renew(), Math.max(5_000, session.expiresAt - Date.now() - RENEW_BEFORE_MS));
    return () => clearTimeout(timer);
  }, [session.expiresAt, renew]);
  const connection = useMemo(() => ({ baseUrl, token: session.token, fetch, renew }), [baseUrl, session.token, fetch, renew]);
  return <RunnerProvider connection={connection}><AttentionScope root={`remote:${session.device.id}`}><PhoneShell device={session.device} onForget={onForget}/></AttentionScope></RunnerProvider>;
}

/** Ask the computer again when the phone comes back: online, or the app brought to the front. */
function useWakeRefresh(refresh: () => void) {
  useEffect(() => {
    const wake = () => { if (document.visibilityState === 'visible') refresh(); };
    window.addEventListener('online', wake); document.addEventListener('visibilitychange', wake);
    return () => { window.removeEventListener('online', wake); document.removeEventListener('visibilitychange', wake); };
  }, [refresh]);
}

function useOnline(): boolean {
  const [online, setOnline] = useState(() => navigator.onLine !== false);
  useEffect(() => {
    const update = () => setOnline(navigator.onLine !== false);
    window.addEventListener('online', update); window.addEventListener('offline', update);
    return () => { window.removeEventListener('online', update); window.removeEventListener('offline', update); };
  }, []);
  return online;
}

/**
 * The computer's pins, which the Runner keeps on each conversation. The page
 * is served by the Runner it talks to, so the Runner always has them. A phone
 * that may respond pins too, and the pins it kept on itself before move to the
 * Runner once; a read-only phone keeps showing those beside the computer's.
 */
function usePhonePins(device: DeviceSession['device']): PinStore {
  const { api, info, refresh } = useRunner();
  const writable = device.grants.includes('respond');
  const local = useMemo(() => localPinStore(`remote:${device.id}`), [device.id]);
  const store = useMemo(() => runnerPinStore({ pin: (sessionId, pinned) => api.pinConversation({ sessionId, pinned }, { timeoutMs: 15000 }), refresh },
    { writable, legacy: writable ? [] : local.list() }), [api, refresh, writable, local]);
  useEffect(() => { store.update((info?.sessions || []).map(session => ({ key: conversationKey(session), sessionId: session.id, pinnedAt: Number(session.pinnedAtUnixMillis || 0) }))); }, [store, info?.sessions]);
  const migrated = useRef(false);
  useEffect(() => {
    if (!writable || migrated.current || !info) return;
    migrated.current = true;
    const legacy = local.list();
    if (!legacy.length) return;
    const known = new Set(info.sessions.map(conversationKey));
    store.set([...new Set([...store.list(), ...legacy.filter(key => known.has(key))])]);
    // What the Runner no longer lists cannot be pinned there; it is dropped with the rest.
    local.set([]);
  }, [writable, info, store, local]);
  return store;
}

function PhoneShell({ device, onForget }: { device: DeviceSession['device']; onForget(): void }) {
  const { api, info, error, refresh } = useRunner();
  const [route, go] = useRoute();
  const [collections, setCollections] = useState<Collection[]>([]);
  const pins = usePhonePins(device);
  const loadCollections = useCallback((signal?: AbortSignal) => api.listCollections({}, { signal, timeoutMs: 10000 }).then(result => { if (!signal?.aborted) setCollections(result.collections); }, () => {}), [api]);
  useEffect(() => {
    const control = new AbortController();
    void loadCollections(control.signal); const timer = setInterval(() => void loadCollections(control.signal), 20000);
    return () => { control.abort(); clearInterval(timer); };
  }, [loadCollections]);
  useWakeRefresh(refresh);
  const online = useOnline();
  // Once per page, not once per client: a renewed credential gives a new
  // client, and a re-sync then could race a preference just saved in Settings.
  // One that could not reach the Runner is tried again with the next client.
  const resynced = useRef(false);
  useEffect(() => {
    if (resynced.current) return; resynced.current = true;
    void registerWorker().then(() => api.getPushSettings({}, { timeoutMs: 15000 })).then(settings => resyncPush(api, settings)).catch(() => { resynced.current = false; });
  }, [api]);
  // The service worker shows pushes and opens their conversation here, without a reload.
  useEffect(() => {
    const message = (event: MessageEvent) => {
      if (event.data?.type !== 'lazybox-open' || typeof event.data.url !== 'string') return;
      const target = new URL(event.data.url, location.origin);
      if (target.origin === location.origin) go(parseRoute(target.search));
    };
    navigator.serviceWorker?.addEventListener('message', message);
    return () => navigator.serviceWorker?.removeEventListener('message', message);
  }, [go]);
  // A banner whose conversation no longer waits (answered here or on the computer) leaves the lock screen.
  const inbox = useInbox();
  const waiting = useMemo(() => new Set(inbox.rows.filter(row => row.state === 'needs-input' || (row.state === 'failed' && row.unread)).map(row => row.key)), [inbox.rows]);
  useEffect(() => { if (info && document.visibilityState === 'visible') void closeStaleNotifications(tag => tag === 'test' || (tag === 'summary' ? waiting.size > 0 : waiting.has(tag))); }, [info, waiting]);
  const onRefresh = useCallback(async () => {
    const before = info;
    refresh(); await loadCollections();
    // Until the computer answered (or gave up), so the spinner means something.
    const started = Date.now();
    while (Date.now() - started < 6000 && latestInfo.current === before) await new Promise(resolve => setTimeout(resolve, 120));
  }, [info, refresh, loadCollections]);
  const latestInfo = useRef(info); latestInfo.current = info;
  const offline = !!error && !info;
  return <>
    {route.screen === 'settings' ? <PhoneSettings device={device} pins={pins} onBack={() => go({})} onForget={onForget}/>
      : route.session ? <PhoneConversation key={route.session} sessionId={route.session} commandId={route.command} device={device} collections={collections} go={go}/>
      : <PhoneInbox device={device} collections={collections} pins={pins} go={go} onRefresh={onRefresh}/>}
    {!online ? <div className="remote-offline" role="status"><Icon name="cloud" size={15}/>This phone is offline. What you see was loaded earlier.</div>
      : error && <div className="remote-offline" role="status"><span className="remote-spinner" aria-hidden="true"/>{offline ? 'Connecting to your computer…' : 'Can’t reach your computer · retrying'}<button type="button" onClick={refresh}>Retry now</button></div>}
  </>;
}

function PhoneConversation({ sessionId, commandId, device, collections, go }: { sessionId: string; commandId?: string; device: DeviceSession['device']; collections: Collection[]; go(route: Route): void }) {
  const { api, info } = useRunner();
  const transcript = useTranscript(api, sessionId, !!info);
  const [fetched, setFetched] = useState<Session>();
  const [error, setError] = useState('');
  const [confirming, setConfirming] = useState<{ approval: Approval; resolve(ok: boolean): void }>();
  const live = info?.sessions.find(value => value.id === sessionId);
  // An ended conversation is not in the live list; read it, and keep reading while it is shown.
  useEffect(() => {
    const control = new AbortController();
    const load = () => void api.getSession({ sessionId }, { signal: control.signal, timeoutMs: 10000 }).then(result => { if (!control.signal.aborted && result.session?.id === sessionId) setFetched(result.session); }, failure => { if (!control.signal.aborted && !live) setError(ConnectError.from(failure).rawMessage); });
    load(); const timer = setInterval(load, 4000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, sessionId, !!live]);
  const session = live || fetched;
  const commands = useConversationCommands(sessionId);
  useMarkViewed(commandId ? undefined : session);
  const [visible, setVisible] = useState(60);
  const end = useRef<HTMLDivElement>(null);
  const count = transcript.messages.length + (session?.pending.length || 0);
  useEffect(() => { if (!commandId) end.current?.scrollIntoView({ block: 'end' }); }, [count, commandId]);
  const agentName = agentTitle(session?.provider || info?.profiles.find(p => p.id === session?.profileId)?.provider, info?.profiles);
  const respond = device.grants.includes('respond'), start = device.grants.includes('start');
  const workspace = collections.find(value => value.id === session?.collectionId)?.name || '';
  const status = useLiveStatus(session, transcript.recent);
  const queued = queuedMessages(session?.queuedMessages, transcript);
  const queueDelivery = useQueuedDelivery(api, sessionId, queued, transcript.turnBoundary);
  // The phone asks before allowing what the computer flags; the computer shows the same flags.
  const confirmAllow = useCallback((approval: Approval) => new Promise<boolean>(resolve => setConfirming({ approval, resolve })), []);
  const voice = useVoiceState();
  const latestReply = latestAgentReply(transcript.messages);
  const readingAvailable = !!latestReply && !!synthesizer();
  const recording = voice.phase === 'starting' || voice.phase === 'listening' || voice.phase === 'stopping';
  if (commandId) return <PhoneCommand commandId={commandId} commands={commands} onBack={() => go({ session: sessionId })}/>;
  return <main className="remote-conversation" aria-label={conversationName(session, 'Conversation')}>
    <header className="remote-bar">
      <button className="remote-icon-button" aria-label="Back to inbox" onClick={() => go({})}><Icon name="back" size={20}/></button>
      <div className="remote-bar-title"><h1>{conversationName(session, 'Conversation')}</h1><span>{[workspace, sessionStatus(session)].filter(Boolean).join(' · ')}</span></div>
      {readingAvailable && <button type="button" className="remote-icon-button" aria-label={voice.reading ? 'Stop reading reply' : 'Read latest reply'} disabled={recording}
        onClick={() => { if (voice.reading) void stopReading(); else void readAloud(speakableText(latestReply, 'full')); }}><Icon name={voice.reading ? 'stop' : 'speaker'} size={20}/></button>}
    </header>
    <CommandStrip commands={commands} onOpen={command => go({ session: sessionId, command })}/>
    <div className="remote-transcript">
      {transcript.error && <p className="remote-error" role="alert">Conversation could not be loaded. {transcript.error} <button onClick={transcript.retry}>Retry</button></p>}
      {error && !session && <p className="remote-error" role="alert">{error}</p>}
      {transcript.loading && !transcript.messages.length && <ConversationLoading replay={transcript.replay}/>}
      <MessageList conversationId={`remote:${device.id}:${session?.conversationId || sessionId}`} key={sessionId} messages={transcript.messages} visible={visible} agentName={agentName} onShowEarlier={() => setVisible(value => value + 60)} loadingEarlier={transcript.partial}/>
      {session?.fault && <p className="remote-error" role="alert">{session.fault}</p>}
      {session?.pending.map(request => request.type === 'terminal_handoff'
        ? <p key={request.requestId} className="remote-hint">Review and run the interactive command on your computer.</p>
        : request.type === 'repository_request'
        ? <p key={request.requestId} className="remote-hint">{requestSummary(request)}? The agent waits for this repository. Add it or decline on your computer.</p>
        : request.type === 'browser_request'
        ? <p key={request.requestId} className="remote-hint">Browser · {requestSummary(request)}? The agent's browsing waits for you. Answer on your computer.</p>
        : respond
        ? <RequestCard key={request.requestId} request={request} api={api} session={session.id} onError={setError} agentName={agentName} confirmAllow={confirmAllow} remote/>
        : <p key={request.requestId} className="remote-hint">The agent is waiting for an answer. This phone can only read; answer on your computer.</p>)}
      {session && !session.closed && status.tone === 'busy' && <div className="remote-activity" aria-hidden="true"><LiveIndicator tone="busy"/>{status.text}</div>}
      <div ref={end}/>
    </div>
    {error && session && <p className="remote-error remote-floating" role="alert">{error}<button aria-label="Dismiss" onClick={() => setError('')}>×</button></p>}
    {respond && !transcript.error && <QueuedMessages session={session} queued={queued} turnRunning={transcript.turnRunning}
      live={status} agentName={agentName} activity={transcript.activity} delivery={queueDelivery}/>}
    {session && <PhoneComposer key={session.id} session={session} respond={respond} start={start} onError={setError}/>}
    {confirming && <ConfirmSheet title="Allow this?" tone="primary" confirm="Allow"
      detail={<><code className="remote-sheet-code">{confirming.approval.command || confirming.approval.file || confirming.approval.description || confirming.approval.tool}</code><ul className="remote-risks">{confirming.approval.risks.map(risk => <li key={risk.kind}><Icon name="alert" size={12}/>{risk.text}</li>)}{confirming.approval.defaultToNo && !confirming.approval.risks.length && <li><Icon name="alert" size={12}/>{agentName} suggests saying no unless you are sure</li>}</ul></>}
      onConfirm={() => confirming.resolve(true)} onClose={() => { confirming.resolve(false); setConfirming(undefined); }}/>}
    <PhoneVoicePill/>
  </main>;
}

/** The recording light and the live words' controls, on the phone too. */
function PhoneVoicePill() {
  return <VoicePill state={useVoiceState()}/>;
}

function PhoneComposer({ session, respond, start, onError }: { session: Session; respond: boolean; start: boolean; onError(message: string): void }) {
  const { api, refresh } = useRunner();
  const key = `lazybox-remote-draft:${session.id}`;
  const [text, setText] = useState(() => { try { return sessionStorage.getItem(key) || ''; } catch { return ''; } });
  const [sending, setSending] = useState(false);
  const attemptKey = key + ':operation';
  const attempt = useRef<{id: string; message: string; target?: string} | undefined>(undefined);
  const inFlight = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  const saveAttempt = (value: typeof attempt.current) => {
    attempt.current = value;
    try {
      if (value) sessionStorage.setItem(attemptKey, JSON.stringify(value));
      else sessionStorage.removeItem(attemptKey);
    } catch { /* The operation remains available in this window. */ }
  };
  const box = useRef<HTMLTextAreaElement>(null);
  const commit = (value: string) => {
    setText(value);
    saveAttempt(undefined);
    try { if (value) sessionStorage.setItem(key, value); else sessionStorage.removeItem(key); } catch { /* The draft lives in memory. */ }
  };
  useEffect(() => { const node = box.current; if (node) { node.style.height = 'auto'; node.style.height = `${Math.min(node.scrollHeight, 160)}px`; } }, [text]);
  // Dictation with the browser's speech recognition (Web Speech). Where the
  // browser has none (iOS Safari without Siri), the keyboard's own microphone
  // dictates into the box instead, so no button is shown.
  const formRef = useRef<HTMLFormElement>(null);
  const dictation = useComposerDictation({ textarea: box, destination: conversationName(session), value: text, enabled: respond && !sending, onPreview: setText, onCommit: commit, onSend: () => formRef.current?.requestSubmit() });
  if (!respond) return <footer className="remote-composer remote-readonly">This phone can read conversations only. Pair it again to reply.</footer>;
  if (session.closed && !start) return <footer className="remote-composer remote-readonly">This conversation ended. Continue it on your computer.</footer>;
  const send = async (event?: FormEvent) => {
    event?.preventDefault();
    const message = text.trim();
    if (!message || inFlight.current) return;
    inFlight.current = true;
    setSending(true); onError('');
    let delivery = attempt.current;
    if (!delivery) {
      try {
        const saved = JSON.parse(sessionStorage.getItem(attemptKey) || 'null');
        if (saved?.message === message && typeof saved.id === 'string' && (!saved.target || typeof saved.target === 'string')) delivery = saved;
      } catch { /* Older drafts have no operation record. */ }
    }
    if (!delivery || delivery.message !== message) delivery = {id: crypto.randomUUID(), message};
    saveAttempt(delivery);
    try {
      let target = delivery.target || session.id;
      if (!delivery.target && session.closed) {
        const resumed = await api.resumeSession({ sessionId: session.id }, { timeoutMs: 60000 });
        if (!resumed.session) throw new Error('The conversation could not be continued');
        target = resumed.session.id;
      }
      delivery.target = target;
      if (mounted.current && attempt.current === delivery) saveAttempt(delivery);
      const request = api.apply({ sessionId: target, operationId: delivery.id, operation: { case: 'message', value: { text: message, attachments: [] } } }, { timeoutMs: 30000 });
      const receipt = await confirmMessage(conversationReader(api, target), delivery.id, request);
      if (receipt?.error || receipt?.delivery === 'failed') throw Error(receipt.error || 'The Runner rejected this message.');
      if (mounted.current && attempt.current === delivery) commit('');
      refresh();
    } catch (error) { if (mounted.current) onError(`Could not confirm delivery: ${ConnectError.from(error).rawMessage || String(error)}`); }
    finally { inFlight.current = false; if (mounted.current) setSending(false); }
  };
  const interrupt = async () => {
    try { await api.apply({ sessionId: session.id, operationId: crypto.randomUUID(), operation: { case: 'interrupt', value: {} } }, { timeoutMs: 15000 }); }
    catch (error) { onError(ConnectError.from(error).rawMessage || String(error)); }
  };
  const keyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== 'Enter' || (!event.metaKey && !event.ctrlKey)) return;
    event.preventDefault();
    formRef.current?.requestSubmit();
  };
  return <form ref={formRef} className="remote-composer" onSubmit={event => { if (dictation.finishAndSend()) { event.preventDefault(); return; } void send(event); }}>
    {session.busy && !session.closed && <button type="button" className="remote-interrupt" onClick={() => void interrupt()}>Interrupt</button>}
    <textarea ref={box} rows={1} aria-label="Message" placeholder={session.closed ? 'Continue this conversation…' : session.busy ? 'Queue a follow-up…' : 'Reply…'} value={text} onChange={event => { dictation.takeOver(); commit(event.target.value); }} onKeyDown={keyDown} enterKeyHint="send"/>
    {dictation.supported && <button type="button" className={'remote-mic' + (dictation.active ? ' is-active' : '')} aria-label={dictation.active ? 'Stop dictation' : 'Dictate'} disabled={sending} onClick={dictation.active ? dictation.stop : dictation.start}><Icon name={dictation.active ? 'stop' : 'mic'} size={18}/></button>}
    <button className="primary remote-send" aria-label={sending ? 'Sending…' : 'Send message'} disabled={sending || !text.trim()}><Icon name="send" size={18}/></button>
  </form>;
}
