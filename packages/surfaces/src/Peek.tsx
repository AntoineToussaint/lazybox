import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import { agentTitle } from './providers';
import { ConnectError } from '@lazybox/client-core';
import type { Artifact, HostedProcess, Session, WorkspaceFile } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { RunnerBadge, RunnerScope } from './RunnerDirectory';
import { useTranscript } from './useTranscript';
import { MessageList } from './MessageList';
import { RichContent, CodeBlock } from './RichContent';
import { LiveStatusLabel } from './LiveIndicator';
import { Console } from './session-parts';
import { conversationDetails, conversationName, conversationTitle } from './conversation-model';
import { useMissions } from './useMissions';
import { liveLabel, statusLabel as missionStatus } from './missions-model';
import { backendLabel, formatMemory, formatUptime, processState, stopConsequence } from './processes-model';
import { onArtifactsChanged } from './artifact-client';
import { languageOf } from './review-model';
import { modKey, shortcut } from './workbench';
import { parsePeek, peekIcons, peekKey, peekList, peekNouns, peekTitle, stepPeek, typingTarget, type PeekItem } from './peek-model';
import type { ViewTarget } from './workspace-model';
import { registerContextMenu, type ContextMenuContext } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import './peek.css';

export { peekProps, type PeekItem } from './peek-model';

/**
 * Peek: the live item over the current page, without leaving it. Alt-click a
 * peekable row (a conversation, a process, a command, an artifact, a mission
 * task, a changed file), press Space on it like Quick Look, or choose "Peek"
 * in its menu. In the overlay ← and → step through the rows around it, ⌘↵
 * opens it here for real, Space or Esc closes it and focus returns to the row
 * last shown. The item stays live (a conversation streams, a command's output
 * follows) with its key action: send a message, stop a turn, stop a command.
 *
 * A sub-agent card peeks in its conversation's own sub-agent panel, floating;
 * that card handles its Alt-click and Space itself.
 */

type PeekState = { item: PeekItem; list: PeekItem[] };
let state: PeekState | null = null;
const listeners = new Set<() => void>();
const emit = () => { for (const listener of listeners) listener(); };
const subscribe = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; };

/** Open a peek of `item`; ← and → move through `list`. */
export function openPeek(item: PeekItem, list: PeekItem[] = [item]) {
  const items = peekList(list);
  state = { item, list: items.some(value => peekKey(value) === peekKey(item)) ? items : [item, ...items] };
  emit();
}
export function closePeek() { state = null; emit(); }

/** The peekable rows around `element`: the nearest enclosing list that holds more than it, in the order shown. */
function rowsAround(element: Element): PeekItem[] {
  const scope = element.closest('[data-peek-list]') || (() => {
    for (let node = element.parentElement; node && node !== document.body; node = node.parentElement)
      if (node.querySelectorAll('[data-peek]').length > 1) return node;
    return null;
  })();
  if (!scope) return [];
  return peekList([...scope.querySelectorAll('[data-peek]')].map(node => parsePeek(node.getAttribute('data-peek'))));
}

/** Peek what `element` (or the peekable row around it) is. Returns whether it was peekable. */
export function peekElement(element: Element | null): boolean {
  const host = element?.closest('[data-peek]');
  const item = parsePeek(host?.getAttribute('data-peek'));
  if (!host || !item || item.kind === 'subagent') return false;
  openPeek(item, rowsAround(host));
  return true;
}

const interactive = 'button, a[href], input, select, textarea, summary, [contenteditable="true"], [role="button"], [role="menuitem"]';

/**
 * "Peek" in the context menus of what can be peeked: the row's own data-peek
 * when it has one (so ← and → follow the list), else the item its menu's
 * detail names.
 */
const peekFromDetail: Record<string, (detail: any) => PeekItem | undefined> = {
  conversation: d => d?.sessionId ? { kind: 'conversation', sessionId: d.sessionId, workspaceId: d.workspaceId } : undefined,
  'inbox-item': d => d?.sessionId ? { kind: 'conversation', sessionId: d.sessionId, workspaceId: d.workspaceId } : undefined,
  process: d => d?.process?.id ? { kind: 'process', processId: d.process.id, title: d.process.label } : undefined,
  command: d => d?.commandId ? { kind: 'command', commandId: d.commandId } : undefined,
  file: d => d?.path && !d.directory && d.workspaceId ? { kind: 'file', workspaceId: d.workspaceId, path: d.path } : undefined,
};
function peekMenu(context: ContextMenuContext): MenuItem[] {
  const host = context.target.closest('[data-peek]') || context.element.closest('[data-peek]') || context.element.querySelector('[data-peek]');
  const marked = parsePeek(host?.getAttribute('data-peek'));
  const item = marked && marked.kind !== 'subagent' ? marked : peekFromDetail[context.scope]?.(context.detail);
  if (!item) return [];
  return [{ label: 'Peek', icon: 'eye', shortcut: 'Space', run: () => { if (host && marked) peekElement(host); else openPeek(item); } }];
}
let menusRegistered = false;
function registerPeekMenus() {
  if (menusRegistered) return;
  menusRegistered = true;
  for (const scope of Object.keys(peekFromDetail)) registerContextMenu(scope, peekMenu);
}

/** Alt-click and Space on peekable rows, for the whole window. */
function usePeekGestures(enabled: boolean) {
  useEffect(() => {
    if (!enabled) return;
    // Capture, so a row's own click (navigation) does not also run.
    const click = (event: MouseEvent) => {
      if (!event.altKey || event.button !== 0 || event.defaultPrevented || event.metaKey || event.ctrlKey) return;
      const target = event.target instanceof Element ? event.target : null;
      const host = target?.closest('[data-peek]');
      if (!host || parsePeek(host.getAttribute('data-peek'))?.kind === 'subagent') return;
      // A control inside the row (Stop, a menu) keeps its own click.
      const control = target!.closest(interactive);
      if (control && control !== host && host.contains(control)) return;
      event.preventDefault(); event.stopPropagation();
      peekElement(host);
    };
    const key = (event: KeyboardEvent) => {
      if (event.key !== ' ' || event.repeat || event.defaultPrevented || event.altKey || event.metaKey || event.ctrlKey || event.shiftKey) return;
      const target = event.target instanceof HTMLElement ? event.target : null;
      if (!target || typingTarget(target)) return;
      // Space on the row itself (its link, or a focusable row); a control inside it keeps Space.
      const host = target.closest('[data-peek]');
      if (!host || (host !== target && target.matches(interactive))) return;
      if (parsePeek(host.getAttribute('data-peek'))?.kind === 'subagent') return;
      event.preventDefault(); event.stopPropagation();
      peekElement(host);
    };
    window.addEventListener('click', click, true);
    window.addEventListener('keydown', key, true);
    return () => { window.removeEventListener('click', click, true); window.removeEventListener('keydown', key, true); };
  }, [enabled]);
}

/** Where "Open here" goes: the window's own navigation (Workspace provides it). */
export type PeekNavigation = {
  showSession(target: ViewTarget): void;
  showModule(kind: string): void;
  showTerminal?(): void;
  openFile?(workspaceId: string, path: string, line?: number): boolean;
  openArtifact?(workspaceId: string, artifactId: string): void;
};
/** What a peek learned about its item that "Open here" needs (a command's conversation, a process's). */
type Resolved = { workspaceId?: string; sessionId?: string; commandId?: string; terminalId?: string };

function openHere(item: PeekItem, resolved: Resolved, nav: PeekNavigation) {
  const workspaceId = resolved.workspaceId || ('workspaceId' in item ? item.workspaceId || '' : '');
  switch (item.kind) {
    case 'conversation': nav.showSession({ collectionId: workspaceId, sessionId: item.sessionId, commandId: '', ...(item.runner ? { runner: item.runner } : {}) }); return;
    case 'command':
      if (resolved.sessionId) nav.showSession({ collectionId: workspaceId, sessionId: resolved.sessionId, commandId: item.commandId });
      else nav.showModule('processes');
      return;
    case 'process':
      if (resolved.commandId && resolved.sessionId) nav.showSession({ collectionId: workspaceId, sessionId: resolved.sessionId, commandId: resolved.commandId });
      else if (resolved.sessionId) nav.showSession({ collectionId: workspaceId, sessionId: resolved.sessionId, commandId: '' });
      else if (resolved.terminalId && nav.showTerminal) nav.showTerminal();
      else nav.showModule('processes');
      return;
    case 'artifact': nav.openArtifact?.(item.workspaceId, item.artifactId); return;
    case 'mission-task':
      if (resolved.sessionId) nav.showSession({ collectionId: workspaceId, sessionId: resolved.sessionId, commandId: '' });
      else nav.showModule('missions');
      return;
    case 'file': if (!nav.openFile?.(item.workspaceId, item.path, item.line)) nav.showModule('files'); return;
  }
}

/** The window's peek overlay; mount once per window. */
export function PeekHost({ navigation }: { navigation: PeekNavigation }) {
  usePeekGestures(true);
  useEffect(registerPeekMenus, []);
  const current = useSyncExternalStore(subscribe, () => state);
  // Closing hands focus back to the row of the item last shown, else where it was.
  const opener = useRef<Element | null>(null);
  const wasOpen = useRef(false);
  useLayoutEffect(() => {
    if (current && !wasOpen.current) opener.current = document.activeElement;
    if (!current && wasOpen.current) {
      const last = lastShown.current;
      const row = last ? [...document.querySelectorAll<HTMLElement>('[data-peek]')].find(node => { const item = parsePeek(node.getAttribute('data-peek')); return item && peekKey(item) === last; }) : undefined;
      const back = row?.matches('a,button,[tabindex]') ? row : row?.querySelector<HTMLElement>('a,button,[tabindex]') || (opener.current instanceof HTMLElement ? opener.current : null);
      back?.focus({ preventScroll: false });
    }
    wasOpen.current = !!current;
  }, [current]);
  const lastShown = useRef('');
  if (current) lastShown.current = peekKey(current.item);
  if (!current) return null;
  return <PeekOverlay key="peek" state={current} navigation={navigation}/>;
}

function PeekOverlay({ state: { item, list }, navigation }: { state: PeekState; navigation: PeekNavigation }) {
  const dialog = useRef<HTMLDialogElement>(null), body = useRef<HTMLDivElement>(null);
  // What the content reported about its item, tagged with that item's key. A
  // reset effect here would run after the content's own effects in the same
  // commit and erase a report made on mount (a task whose conversation is
  // already known), so "Open here" went to the wrong place. A report for an
  // item no longer shown is dropped; one for another item is never read.
  const key = peekKey(item);
  const shownKey = useRef(key); shownKey.current = key;
  const [reported, setReported] = useState<{ key: string; resolved?: Resolved; title?: string }>({ key });
  const report = (change: { resolved?: Resolved; title?: string }) => { if (key === shownKey.current) setReported(last => ({ ...(last.key === key ? last : {}), key, ...change })); };
  const resolved = reported.key === key ? reported.resolved || {} : {};
  const title = reported.key === key ? reported.title || '' : '';
  useEffect(() => { dialog.current?.showModal(); body.current?.focus({ preventScroll: true }); }, []);
  const index = list.findIndex(value => peekKey(value) === peekKey(item));
  const previous = stepPeek(list, item, -1), next = stepPeek(list, item, 1);
  const go = (to?: PeekItem) => { if (to) { openPeek(to, list); requestAnimationFrame(() => body.current?.focus({ preventScroll: true })); } };
  const here = () => { closePeek(); openHere(item, resolved, navigation); };
  const keys = (event: ReactKeyboardEvent) => {
    if (event.defaultPrevented) return;
    const typing = typingTarget(event.target as HTMLElement);
    if (event.key === 'Enter' && modKey(event.nativeEvent)) { event.preventDefault(); here(); return; }
    if (typing) return;
    if (event.key === 'ArrowLeft' || event.key === 'ArrowUp' && event.altKey) { event.preventDefault(); go(previous); return; }
    if (event.key === 'ArrowRight' || event.key === 'ArrowDown' && event.altKey) { event.preventDefault(); go(next); return; }
    if (event.key === ' ' && !(event.target as HTMLElement).matches(interactive)) { event.preventDefault(); closePeek(); }
  };
  const noun = peekNouns[item.kind];
  return <DraggableDialog ref={dialog} className="peek-dialog" aria-label={`Peek: ${title || peekTitle(item)}`} aria-roledescription="peek"
    onCancel={event => { event.preventDefault(); closePeek(); }} onKeyDown={keys}>
    <DialogHeader className="peek-header">
      <span className="peek-kind" aria-hidden="true"><Icon name={peekIcons[item.kind]} size={15}/></span>
      <div className="peek-heading">
        <small>{noun}{list.length > 1 && index >= 0 ? ` · ${index + 1} of ${list.length}` : ''}</small>
        <h2 title={title || peekTitle(item)}>{title || peekTitle(item)}</h2>
      </div>
      <div className="peek-nav">
        <button type="button" className="icon-button" aria-label="Previous" title="Previous (←)" disabled={!previous} onClick={() => go(previous)}><Icon name="chevron" size={14}/></button>
        <button type="button" className="icon-button peek-next" aria-label="Next" title="Next (→)" disabled={!next} onClick={() => go(next)}><Icon name="chevron" size={14}/></button>
      </div>
      <button type="button" className="primary peek-open" onClick={here} title={`Open here (${shortcut('Mod+Enter')})`}>Open here</button>
      <button type="button" className="icon-button" aria-label="Close peek" title="Close (Esc or Space)" onClick={closePeek}><Icon name="close" size={14}/></button>
    </DialogHeader>
    <div ref={body} className="peek-body" tabIndex={-1} data-peek-body="">
      <PeekContent key={key} item={item} onResolved={value => report({ resolved: value })} onTitle={value => report({ title: value })} onPeek={to => go(to)}/>
    </div>
    <footer className="peek-keys" aria-hidden="true">
      <span><kbd>←</kbd><kbd>→</kbd> previous · next</span>
      <span><kbd>{shortcut('Mod+Enter')}</kbd> open here</span>
      <span><kbd>Space</kbd> or <kbd>Esc</kbd> close</span>
    </footer>
  </DraggableDialog>;
}

type ContentProps = { onResolved(resolved: Resolved): void; onTitle(title: string): void; onPeek(item: PeekItem): void };
function PeekContent({ item, ...props }: { item: PeekItem } & ContentProps) {
  switch (item.kind) {
    // Another Runner's conversation is read from that Runner.
    case 'conversation': return item.runner
      ? <RunnerScope runner={item.runner} fallback={<Problem>That Runner is not reachable.</Problem>}><RunnerBadge runner={item.runner} className="peek-runner"/><ConversationPeek sessionId={item.sessionId} {...props}/></RunnerScope>
      : <ConversationPeek sessionId={item.sessionId} {...props}/>;
    case 'command': return <CommandPeek commandId={item.commandId} {...props}/>;
    case 'process': return <ProcessPeek processId={item.processId} {...props}/>;
    case 'artifact': return <ArtifactPeek workspaceId={item.workspaceId} artifactId={item.artifactId} {...props}/>;
    case 'mission-task': return <TaskPeek taskId={item.taskId} {...props}/>;
    case 'file': return <FilePeek workspaceId={item.workspaceId} path={item.path} {...props}/>;
    default: return null;
  }
}

const errorText = (error: unknown) => ConnectError.from(error).rawMessage || String(error);
function Problem({ children }: { children: ReactNode }) { return <p className="peek-problem" role="alert">{children}</p>; }
function Loading({ what }: { what: string }) { return <p className="peek-loading" role="status">Loading {what}…</p>; }

/** A conversation, live: its latest messages and status, a message box and Stop. */
function ConversationPeek({ sessionId, onResolved, onTitle }: { sessionId: string } & ContentProps) {
  const { api, info } = useRunner();
  const live = info?.sessions.find(value => value.id === sessionId);
  const [saved, setSaved] = useState<Session>(), [error, setError] = useState('');
  useEffect(() => {
    if (live) return;
    const control = new AbortController();
    api.getSession({ sessionId }, { signal: control.signal, timeoutMs: 8000 }).then(value => { if (!control.signal.aborted) setSaved(value.session); }, failure => { if (!control.signal.aborted) setError(errorText(failure)); });
    return () => control.abort();
  }, [api, sessionId, !!live]);
  const session = live || saved;
  const transcript = useTranscript(api, sessionId, true);
  const [visible, setVisible] = useState(40);
  const scroller = useRef<HTMLDivElement>(null), stuck = useRef(true);
  useEffect(() => { onResolved({ workspaceId: session?.collectionId, sessionId }); onTitle(session ? conversationName(session) : ''); }, [session?.collectionId, session?.title]);
  useLayoutEffect(() => { const node = scroller.current; if (node && stuck.current) node.scrollTop = node.scrollHeight; }, [transcript.messages.length, transcript.messages.at(-1)?.text]);
  const agentName = agentTitle(session?.provider, info?.profiles, 'Claude');
  return <div className="peek-conversation">
    <div className="peek-meta">
      {session && <LiveStatusLabel session={session}/>}
      {session && <span>{conversationDetails(session, info?.profiles || [], false)}</span>}
      {session?.busy && !session.closed && <StopTurn sessionId={sessionId}/>}
    </div>
    {error && !session && <Problem>Could not read this conversation: {error}</Problem>}
    <div ref={scroller} className="peek-transcript" onScroll={event => { const node = event.currentTarget; stuck.current = node.scrollHeight - node.scrollTop - node.clientHeight < 32; }}>
      {transcript.messages.length
        ? <MessageList key={sessionId} messages={transcript.messages} visible={visible} agentName={agentName} onShowEarlier={() => setVisible(value => value + 40)} loadingEarlier={transcript.partial}/>
        : <p className="peek-empty">{session ? 'No messages yet.' : 'Loading the conversation…'}</p>}
    </div>
    {session && <PeekComposer session={session}/>}
  </div>;
}

function StopTurn({ sessionId }: { sessionId: string }) {
  const { api } = useRunner();
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  return <>
    <button type="button" className="peek-stop" disabled={busy} title="Stop the agent's current turn" onClick={async () => {
      setBusy(true); setError('');
      try { await api.apply({ sessionId, operationId: crypto.randomUUID(), operation: { case: 'interrupt', value: {} } }); }
      catch (failure) { setError(errorText(failure)); }
      finally { setBusy(false); }
    }}><Icon name="stop" size={12}/>{busy ? 'Stopping…' : 'Stop turn'}</button>
    {error && <span className="peek-problem" role="alert">{error}</span>}
  </>;
}

/** A short reply without leaving the page; the full composer is one ⌘↵ away. */
function PeekComposer({ session }: { session: Session }) {
  const { api } = useRunner();
  const [text, setText] = useState(''), [sending, setSending] = useState(false), [notice, setNotice] = useState(''), [error, setError] = useState('');
  if (session.closed) return <p className="peek-composer-closed">This conversation has ended. Open it here to continue it.</p>;
  const send = async () => {
    const message = text.trim();
    if (!message || sending) return;
    setSending(true); setError(''); setNotice('');
    try {
      await api.apply({ sessionId: session.id, operationId: crypto.randomUUID(), operation: { case: 'message', value: { text: message, attachments: [] } } }, { timeoutMs: 30000 });
      setText(''); setNotice(session.busy ? 'Sent. The agent reads it after its current step.' : 'Sent.');
    } catch (failure) { setError(errorText(failure)); }
    finally { setSending(false); }
  };
  return <form className="peek-composer" onSubmit={event => { event.preventDefault(); void send(); }}>
    <textarea aria-label="Message" placeholder={`Message ${conversationTitle(session) ? `“${conversationTitle(session)}”` : 'this conversation'}… (Enter to send)`} rows={2} value={text} disabled={sending}
      onChange={event => setText(event.target.value)}
      onKeyDown={event => { if (event.key === 'Enter' && !event.shiftKey && !modKey(event.nativeEvent) && !event.nativeEvent.isComposing) { event.preventDefault(); void send(); } }}/>
    <button type="submit" className="primary" disabled={sending || !text.trim()} aria-label="Send message"><Icon name="send" size={14}/></button>
    {(notice || error) && <small role={error ? 'alert' : 'status'} className={error ? 'peek-problem' : 'peek-notice'}>{error || notice}</small>}
  </form>;
}

/** A command's live output and Stop, the console it has everywhere. */
function CommandPeek({ commandId, onResolved, onTitle }: { commandId: string } & ContentProps) {
  const { api } = useRunner();
  const [error, setError] = useState('');
  useEffect(() => {
    const control = new AbortController();
    api.readCommand({ commandId, offset: 0n, limit: 1 }, { signal: control.signal, timeoutMs: 8000 }).then(({ status }) => {
      if (!control.signal.aborted && status) { onResolved({ sessionId: status.sessionId, workspaceId: status.collectionId }); onTitle(status.title); }
    }, () => {});
    return () => control.abort();
  }, [api, commandId]);
  return <div className="peek-command">
    {error && <Problem>{error}</Problem>}
    <Console api={api} command={commandId} onError={setError}/>
  </div>;
}

/** A hosted process: what it is, what it uses, its output or conversation, and Stop. */
function ProcessPeek({ processId, onResolved, onTitle, onPeek }: { processId: string } & ContentProps) {
  const { api } = useRunner();
  const [process, setProcess] = useState<HostedProcess>(), [gone, setGone] = useState(false), [error, setError] = useState('');
  const [confirming, setConfirming] = useState(false), [stopping, setStopping] = useState(false);
  useEffect(() => {
    const control = new AbortController();
    const read = () => api.listProcesses({}, { signal: control.signal, timeoutMs: 8000 }).then(value => {
      if (control.signal.aborted) return;
      const found = value.processes.find(p => p.id === processId);
      setProcess(found); setGone(!found); setError('');
    }, failure => { if (!control.signal.aborted) setError(errorText(failure)); });
    void read(); const timer = setInterval(() => void read(), 3000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, processId]);
  useEffect(() => { if (process) { onResolved({ workspaceId: process.workspaceId, sessionId: process.sessionId, commandId: process.commandId, terminalId: process.terminalId }); onTitle(process.label); } }, [process?.id, process?.label]);
  if (error && !process) return <Problem>Could not read processes: {error}</Problem>;
  if (gone) return <p className="peek-empty">This process is no longer running.</p>;
  if (!process) return <Loading what="the process"/>;
  const state = processState(process);
  const facts = [
    ['State', state.label], ['Kind', process.kind], ['Workspace', process.workspaceName], ['Conversation', process.conversationTitle],
    ['PID', String(process.programPid || process.hostPid)], ['Up', formatUptime(process.uptimeSeconds)],
    ['CPU', process.usage ? `${process.usage.cpuPercent.toFixed(1)}%` : ''], ['Memory', process.usage ? formatMemory(process.usage.memoryBytes) : ''],
    ['Hosted by', backendLabel(process.backend)],
  ].filter(([, value]) => value);
  return <div className="peek-process">
    <dl className="peek-facts">{facts.map(([name, value]) => <div key={name}><dt>{name}</dt><dd>{value}</dd></div>)}</dl>
    <div className="peek-actions">
      {process.sessionId && process.kind === 'agent' && <button type="button" onClick={() => onPeek({ kind: 'conversation', sessionId: process.sessionId, workspaceId: process.workspaceId, title: process.conversationTitle })}><Icon name="chat" size={13}/>Peek its conversation</button>}
      {confirming
        ? <span className="peek-confirm" role="group" aria-label="Confirm stop"><span>{stopConsequence(process)}</span>
          <button type="button" onClick={() => setConfirming(false)} disabled={stopping}>Cancel</button>
          <button type="button" className="danger" disabled={stopping} onClick={async () => {
            setStopping(true); setError('');
            try { await api.stopProcess({ id: process.id }); setConfirming(false); setGone(true); }
            catch (failure) { setError(errorText(failure)); }
            finally { setStopping(false); }
          }}>{stopping ? 'Stopping…' : 'Stop'}</button></span>
        : <button type="button" className="danger" onClick={() => setConfirming(true)}><Icon name="stop" size={12}/>Stop…</button>}
    </div>
    {error && <Problem>{error}</Problem>}
    {process.commandId && <Console api={api} command={process.commandId} onError={setError}/>}
  </div>;
}

/** An artifact's current version, as its page shows it. */
function ArtifactPeek({ workspaceId, artifactId, onResolved, onTitle }: { workspaceId: string; artifactId: string } & ContentProps) {
  const { api } = useRunner();
  const [artifact, setArtifact] = useState<Artifact>(), [error, setError] = useState(''), [revision, setRevision] = useState(0);
  useEffect(() => onArtifactsChanged(changed => { if (changed === workspaceId) setRevision(value => value + 1); }), [workspaceId]);
  useEffect(() => {
    const control = new AbortController();
    api.getArtifact({ collectionId: workspaceId, artifactId }, { signal: control.signal, timeoutMs: 10000 }).then(value => { if (!control.signal.aborted) { setArtifact(value); setError(''); } }, failure => { if (!control.signal.aborted) setError(errorText(failure)); });
    return () => control.abort();
  }, [api, workspaceId, artifactId, revision]);
  useEffect(() => { onResolved({ workspaceId }); if (artifact) onTitle(artifact.title); }, [artifact?.title]);
  if (error && !artifact) return <Problem>Could not read this artifact: {error}</Problem>;
  if (!artifact) return <Loading what="the artifact"/>;
  return <div className="peek-artifact">
    <div className="peek-meta"><span className="peek-chip">{artifact.kind || 'notes'}</span><span>Version {artifact.contentVersion} of {artifact.versions.length || 1}</span>{artifact.trackedPath && <span title="The workspace file it follows">{artifact.trackedPath}</span>}</div>
    <article className="peek-document"><RichContent text={artifact.content}/></article>
  </div>;
}

/** A mission task: its status and why, its brief, its report, and its conversation. */
function TaskPeek({ taskId, onResolved, onTitle, onPeek }: { taskId: string } & ContentProps) {
  const missions = useMissions();
  const mission = missions.missions.find(m => m.tasks.some(t => t.id === taskId));
  const task = mission?.tasks.find(t => t.id === taskId);
  const attempt = task?.attempt;
  useEffect(() => { if (task) { onResolved({ sessionId: attempt?.sessionId, workspaceId: attempt?.collectionId }); onTitle(task.title); } }, [task?.title, attempt?.sessionId]);
  if (!task) return missions.loaded ? <p className="peek-empty">This task is no longer in any mission.</p> : <Loading what="the task"/>;
  return <div className="peek-task">
    <div className="peek-meta"><span className={`peek-chip status-${task.status}`}>{missionStatus[task.status] || task.status}</span>{mission && <span>{mission.title}</span>}{attempt && <span>{attempt.endedAtUnixMillis ? `ended (${attempt.outcome || 'stopped'})` : liveLabel(attempt.live, attempt.liveDetail)}</span>}</div>
    {task.reason && <p className="peek-reason">{task.reason}</p>}
    {task.brief && <section><h3>Brief</h3><RichContent text={task.brief}/></section>}
    {task.report && <section><h3>Report</h3><RichContent text={task.report}/></section>}
    {attempt?.sessionId && attempt.live !== 'missing' && <div className="peek-actions"><button type="button" onClick={() => onPeek({ kind: 'conversation', sessionId: attempt.sessionId, workspaceId: attempt.collectionId, title: attempt.title })}><Icon name="chat" size={13}/>Peek its conversation</button></div>}
  </div>;
}

/** A workspace file, read-only: Markdown rendered, code highlighted, a folder listed. */
function FilePeek({ workspaceId, path, onResolved, onTitle }: { workspaceId: string; path: string } & ContentProps) {
  const { api } = useRunner();
  const [file, setFile] = useState<WorkspaceFile>(), [error, setError] = useState('');
  useEffect(() => {
    const control = new AbortController();
    api.readWorkspaceFile({ collectionId: workspaceId, path }, { signal: control.signal, timeoutMs: 10000 }).then(value => { if (!control.signal.aborted) setFile(value); }, failure => { if (!control.signal.aborted) setError(errorText(failure)); });
    return () => control.abort();
  }, [api, workspaceId, path]);
  useEffect(() => { onResolved({ workspaceId }); onTitle(path); }, [path]);
  if (error) return <Problem>Could not read {path}: {error}</Problem>;
  if (!file) return <Loading what="the file"/>;
  if (file.directory) return <ul className="peek-folder">{file.entries.map(entry => <li key={entry.path}><Icon name={entry.directory ? 'folder' : 'file'} size={13}/>{entry.name}</li>)}</ul>;
  if (file.binary) return <p className="peek-empty">Binary file · no text preview.</p>;
  return <div className="peek-file">
    {file.truncated && <p className="peek-notice" role="status">Preview limited to 1 MiB.</p>}
    {/\.(md|markdown|mdown)$/i.test(path) ? <article className="peek-document"><RichContent text={file.text}/></article> : <CodeBlock text={file.text} language={languageOf(path)} label={path}/>}
  </div>;
}

