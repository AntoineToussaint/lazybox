import { conversationName } from './conversation-model';
import { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Collection, TerminalStatus } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { getAppearance, subscribeAppearance } from './appearance/apply';
import { codeFont, terminalTheme } from './appearance/integrations';
import { Icon } from './Icons';
import './terminal.css';
import { contextMenu, copyText, showMenuNotice } from './ContextMenu';
import { fencedCode, type MenuItem } from './context-menu-model';
import { composeInConversation } from './compose';
import { useWorkConversation } from './PullRequestFlow';
import type { ViewTarget } from './workspace-model';
import { useHomeChoices } from './home-checkouts';
import { isMenuClick } from './context-menu-model';
import { isMac } from './workbench';
import { linkMenuItems, linkPreferences, openInBrowser, openInViewer } from './web-links';
import { webPage } from './web-address';
import { linksInRows, type TerminalRow } from './terminal-links';

/** Workspaces whose terminal panel was just asked for (from the header, or for one shell). */
const requested = new Map<string, string>();
const listeners = new Set<() => void>();
/** Ask the workspace's terminal panel (now or once it mounts and has listed its shells) to
 * take focus: on terminalId when given and running, else on a running shell when there is
 * one, otherwise (only when no particular shell was asked for) on a new one it starts. */
export function requestTerminal(collectionId: string, terminalId = '') {
  // Asking for one shell (a home's, just opened) wins over showing the panel.
  if (!terminalId && requested.get(collectionId)) return;
  requested.set(collectionId, terminalId); for (const listener of listeners) listener();
}
/** A tab's name: the workspace directory, or where a shell outside it runs (a repository's home, ~). */
export const terminalLabel = (terminal: TerminalStatus) => terminal.place ? terminal.title : terminal.directory || 'Workspace root';

const encoder = new TextEncoder();
const inputLimit = 32 << 10;

/** A press that moves farther than this before its release is a drag (a selection), not a click on a link. */
const dragDistance = 4;
/**
 * Opens a link clicked in a terminal as every link in the app opens: the
 * person's browser by default, the Web panel with Alt (or the other way round,
 * per Settings → Web & links). The exact address printed is what opens.
 */
function openTerminalLink(url: string, event: MouseEvent) {
  const page = webPage(url);
  if (!page) return;
  const report = (error: unknown) => console.error('Could not open link', error);
  if ((linkPreferences().target === 'viewer') !== event.altKey && openInViewer(page)) return;
  void openInBrowser(url).catch(report);
}
/** The rows of the logical line through buffer row `y` (0-based), the wrapped rows joined, and the first row's index. */
function logicalLine(view: Terminal, y: number): { first: number; rows: TerminalRow[] } {
  const buffer = view.buffer.active;
  let first = y;
  while (first > 0 && buffer.getLine(first)?.isWrapped) first--;
  const rows: TerminalRow[] = [];
  // A sign-in URL of a few hundred characters spans a few rows; a line of thousands is not searched past 64 rows.
  for (let row = first; row < buffer.length && rows.length < 64; row++) {
    const line = buffer.getLine(row);
    if (!line || (row > first && !line.isWrapped)) break;
    let text = ''; const cells: number[] = [];
    for (let x = 0; x < line.length; x++) {
      const cell = line.getCell(x);
      if (!cell || cell.getWidth() === 0) continue;
      const chars = cell.getChars() || ' ';
      text += chars; for (let i = 0; i < chars.length; i++) cells.push(x);
    }
    // The last row's blank end is not part of the line.
    if (!buffer.getLine(row + 1)?.isWrapped) { const kept = text.trimEnd().length; text = text.slice(0, kept); cells.length = kept; }
    rows.push({ text, cells });
  }
  return { first, rows };
}

function exitText(status: TerminalStatus) {
  const program = status.place === 'interactive-command' ? 'Command' : 'Shell';
  return status.error || (status.signal ? `${program} ended by ${status.signal}` : status.exitCode === undefined ? `${program} ended` : `${program} exited with code ${status.exitCode}`);
}

export function TerminalPanel({ workspaceId, collections, viewState, onClosePanel, initialTerminalId, initialInput, sessionId = '', onShowSession }: { workspaceId: string; collections: Collection[]; viewState?: ViewStateStore; initialTerminalId?: string; initialInput?: string; onClosePanel?(): void; /** The page's conversation: where Add selection to draft goes. */ sessionId?: string; onShowSession?(target: ViewTarget): void }) {
  const conversation = useWorkConversation(workspaceId, sessionId);
  // A terminal's selection goes to the work conversation's message box, as a block to review and send.
  const sendToAgent = (text: string) => {
    if (!conversation) { showMenuNotice('No conversation in this workspace yet'); return; }
    composeInConversation({ sessionId: conversation.id, text: fencedCode(text), api });
    showMenuNotice(`Added to the message box of “${conversationName(conversation, 'the conversation')}”`);
    onShowSession?.({ collectionId: workspaceId, sessionId: conversation.id, commandId: '' });
  };
  const { api } = useRunner();
  const collection = collections.find(value => value.id === workspaceId);
  const key = viewState ? `${viewState.key}:terminal` : '';
  const [terminals, setTerminals] = useState<TerminalStatus[]>([]);
  const [selected, setSelected] = useState(() => initialTerminalId || (key && viewState?.getItem(key)) || '');
  const initialCommand = useRef(initialInput);
  const [directory, setDirectory] = useState('');
  const homes = useHomeChoices(api, collection);
  const [error, setError] = useState(''), [listError, setListError] = useState(''), [opening, setOpening] = useState(false);
  const known = useRef<Set<string> | null>(null);
  const listed = useRef<TerminalStatus[]>([]), selectedRef = useRef(selected); selectedRef.current = selected;
  const focusView = useRef<(() => void) | null>(null);
  const size = useRef({ cols: 80, rows: 24 });
  const choose = (id: string) => { setSelected(id); if (key) try { viewState?.setItem(key, id); } catch { /* the choice is a convenience */ } };
  const refresh = async (signal?: AbortSignal) => {
    const result = await api.listTerminals({ collectionId: workspaceId }, { signal });
    if (signal?.aborted) return;
    setTerminals(result.terminals); listed.current = result.terminals;
    const ids = result.terminals.map(value => value.id);
    // A terminal that appeared since the last look was just opened: show it.
    const fresh = known.current ? ids.filter(id => !known.current!.has(id)) : [];
    if (fresh.length) choose(fresh.at(-1)!);
    else setSelected(current => ids.includes(current) ? current : ids.at(-1) || '');
    known.current = new Set(ids);
    return result.terminals;
  };
  const open = async () => {
    if (!workspaceId || opening) return;
    setOpening(true); setError('');
    try {
      // A home (`home:<repository>`) or the home folder (`~`) starts outside the workspace.
      const where = directory.startsWith('home:') ? { repositoryHome: directory.slice(5) } : directory === '~' ? { userHome: true } : { directory };
      const terminal = await api.openTerminal({ collectionId: workspaceId, ...where, cols: size.current.cols, rows: size.current.rows, requestKey: crypto.randomUUID() });
      known.current?.add(terminal.id); choose(terminal.id); await refresh();
    } catch (error) { setError(String(error)); }
    finally { setOpening(false); }
  };
  const close = async (id: string, cleanExit = false) => {
    try {
      await api.closeTerminal({ terminalId: id });
      const remaining = await refresh();
      if (remaining && (cleanExit ? remaining.every(terminal => terminal.done) : remaining.length === 0)) onClosePanel?.();
    } catch (error) { setError(String(error)); }
  };
  // Another workspace starts from an empty list. A Runner restart (a new
  // address or bearer, so a new `api`) does not: its terminals outlive it, so
  // the tabs and their views stay while the list is read again.
  useEffect(() => { known.current = null; setTerminals([]); setError(''); setListError(''); }, [workspaceId]);
  useEffect(() => {
    if (!workspaceId) return;
    const control = new AbortController(); let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      try { await refresh(control.signal); if (!control.signal.aborted) { setListError(''); consumeRef.current(); } }
      catch (error) { if (!control.signal.aborted) setListError(`Could not list terminals: ${String(error)}`); }
      finally { updating = false; }
    };
    void update(); const timer = setInterval(() => void update(), 2000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, workspaceId]);
  // A request waits until this workspace's shells are listed, so asking twice never starts a second one.
  const consume = () => {
    if (!workspaceId || known.current === null || !requested.has(workspaceId)) return;
    const wanted = requested.get(workspaceId) || '';
    // A shell opened a moment ago may not be listed yet: look again before choosing, and finish
    // as soon as it is listed. Listing it shows it and its view takes focus; finishing later took
    // focus a second time, a tenth of a second on, back from wherever the person had gone since.
    if (wanted && !listed.current.some(terminal => terminal.id === wanted) && (retries.current[wanted] = (retries.current[wanted] || 0) + 1) <= 5) {
      void refresh().then(() => { if (listed.current.some(terminal => terminal.id === wanted)) consumeRef.current(); else setTimeout(() => consumeRef.current(), 100); }, () => {});
      return;
    }
    requested.delete(workspaceId);
    const live = listed.current.filter(terminal => !terminal.done);
    let next = selectedRef.current;
    if (wanted && listed.current.some(terminal => terminal.id === wanted)) next = wanted;
    else if (!live.length) { if (!wanted) void open(); return; }
    else if (!live.some(terminal => terminal.id === selectedRef.current)) next = live.at(-1)!.id;
    // A shell newly chosen takes focus as its view mounts (focusView is still the one it replaces); the one shown takes it now.
    const switching = next !== selectedRef.current;
    choose(next);
    if (!switching) requestAnimationFrame(() => focusView.current?.());
  };
  const consumeRef = useRef(consume); consumeRef.current = consume;
  const retries = useRef<Record<string, number>>({});
  useEffect(() => {
    const listener = () => consumeRef.current();
    listeners.add(listener); listener();
    return () => { listeners.delete(listener); };
  }, [workspaceId]);
  const current = terminals.find(value => value.id === selected);
  const ready = !!collection && collection.state === 'ready';
  return <section className="terminal-panel">{(error || listError) && <div className="error" role="alert">{error || listError}</div>}
    {!workspaceId ? <p className="module-empty">Choose a workspace to open a terminal in it.</p> : <>
      <div className="terminal-tabs" role="tablist" aria-label="Terminals">
        {terminals.map(terminal => <span key={terminal.id} className={'terminal-tab' + (terminal.id === selected ? ' selected' : '') + (terminal.done ? ' exited' : '')}
          onContextMenu={contextMenu('terminal-tab', () => [{ label: 'New terminal', icon: 'plus', disabled: !ready || opening, run: () => void open() }, 'separator', { label: terminal.done ? 'Remove terminal' : 'Close terminal', icon: 'close', danger: !terminal.done, run: () => void close(terminal.id) }], { terminalId: terminal.id, workspaceId })}>
          <button role="tab" aria-selected={terminal.id === selected} title={terminal.place ? terminal.cwd : terminal.title} onClick={() => choose(terminal.id)}>{terminal.place && <Icon name={terminal.place === 'interactive-command' ? 'terminal' : 'home'} size={12}/>}{terminalLabel(terminal)}{terminal.done ? ' · exited' : ''}</button>
          <button className="terminal-close" aria-label={`Close terminal ${terminalLabel(terminal)}`} title={terminal.done ? 'Remove' : 'Close (hangs up the shell)'} onClick={() => void close(terminal.id)}>×</button>
        </span>)}
        <span className="terminal-new">
          <label><span className="sr-only">Terminal directory</span><select aria-label="Terminal directory" value={directory} onChange={event => setDirectory(event.target.value)}>
            <option value="">Workspace root</option>{collection?.members.map(member => member.spec?.name && <option key={member.spec.name} value={member.spec.name}>{member.spec.name}</option>)}
            {homes.length > 0 && <optgroup label="Home checkouts">{homes.map(home => <option key={home.repositoryId} value={'home:' + home.repositoryId}>{home.label}</option>)}</optgroup>}
            <option value="~">Home folder (~)</option>
          </select></label>
          <button aria-label="New terminal" title="New terminal" disabled={!ready || opening} onClick={() => void open()}><Icon name="plus" size={14}/><span>New terminal</span></button>
        </span>
      </div>
      {current ? <TerminalView key={current.id} api={api} terminal={current} initialCommand={current.id === initialTerminalId ? initialCommand : undefined} focusRef={focusView} workspaceId={workspaceId} canSend={!!conversation} onSendToAgent={sendToAgent} onNewTerminal={ready ? () => void open() : undefined} onCloseTerminal={() => void close(current.id)} onCleanExit={current.place === 'interactive-command' ? undefined : () => void close(current.id, true)} onSize={value => { size.current = value; }}/>
        : <p className="module-empty">{ready ? 'No terminal is open in this workspace. A terminal runs your login shell on the Runner, in the directory you choose, and keeps running when the Runner restarts.' : 'The workspace is not ready for a terminal.'}{ready && <><br/><button className="primary" disabled={opening} onClick={() => void open()}>Start terminal</button></>}</p>}
    </>}
  </section>;
}

function TerminalView({ api, terminal, initialCommand, focusRef, onSize, onCleanExit, workspaceId, canSend, onSendToAgent, onNewTerminal, onCloseTerminal }: { api: RunnerClient; terminal: TerminalStatus; initialCommand?: { current: string | undefined }; focusRef: { current: (() => void) | null }; onSize(size: { cols: number; rows: number }): void; onCleanExit?(): void; workspaceId: string; canSend: boolean; onSendToAgent(text: string): void; onNewTerminal?(): void; onCloseTerminal(): void }) {
  const xterm = useRef<Terminal | null>(null);
  const [hasSelection, setHasSelection] = useState(false);
  /** The web link under the pointer, for the right-click menu. */
  const hovered = useRef('');
  const cleanExit = useRef(onCleanExit); cleanExit.current = onCleanExit;
  const [error, onError] = useState('');
  const exited = useRef(terminal.done);
  if (terminal.done) exited.current = true;
  const startedLive = useRef(!terminal.done);
  const host = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<TerminalStatus>(terminal);
  const [connection, setConnection] = useState('Connecting');
  const id = terminal.id;
  // The shell outlives a Runner restart, so its view does too: requests use the
  // window's current client, and the stream picks up from its offset.
  const client = useRef(api); client.current = api;
  useEffect(() => {
    const element = host.current; if (!element) return;
    // A right-click keeps the selection for the Lazybox menu instead of selecting a word.
    const font = codeFont(getAppearance());
    const view = new Terminal({ theme: terminalTheme(getAppearance()), fontFamily: font.family, fontSize: font.size, lineHeight: 1.15, cursorBlink: true, scrollback: 5000, allowProposedApi: false, rightClickSelectsWord: false });
    xterm.current = view;
    const selectionListener = view.onSelectionChange(() => setHasSelection(view.hasSelection()));
    const fit = new FitAddon(); view.loadAddon(fit); view.open(element);
    // Web links: underlined with a pointer on hover, opened by a click, offered by the right-click menu.
    // A drag that selects text, a right-click and a Mac's Ctrl+click never open one.
    let pressedAt: { x: number; y: number } | null = null;
    const press = (event: MouseEvent) => { pressedAt = event.button === 0 ? { x: event.clientX, y: event.clientY } : null; };
    element.addEventListener('mousedown', press, true);
    const activate = (event: MouseEvent, url: string) => {
      const from = pressedAt; pressedAt = null;
      if (event.button !== 0 || isMenuClick(event, isMac) || view.hasSelection()) return;
      if (from && Math.hypot(event.clientX - from.x, event.clientY - from.y) > dragDistance) return;
      openTerminalLink(url, event);
    };
    const hoverLink = (_event: MouseEvent, url: string) => { hovered.current = url; };
    const leaveLink = (_event: MouseEvent, url: string) => { if (hovered.current === url) hovered.current = ''; };
    // OSC 8 hyperlinks a program prints open the same way.
    view.options.linkHandler = { activate, hover: hoverLink, leave: leaveLink, allowNonHttpProtocols: false };
    const links = view.registerLinkProvider({
      provideLinks(y, callback) {
        const { first, rows } = logicalLine(view, y - 1);
        const found = linksInRows(rows).filter(link => first + link.start.row <= y - 1 && y - 1 <= first + link.end.row);
        callback(found.length ? found.map(link => ({
          text: link.url,
          range: { start: { x: link.start.col + 1, y: first + link.start.row + 1 }, end: { x: link.end.col + 1, y: first + link.end.row + 1 } },
          decorations: { pointerCursor: true, underline: true },
          activate, hover: hoverLink, leave: leaveLink,
        })) : undefined);
      },
    });
    // The theme and code font follow Settings → Appearance live.
    const unfollow = subscribeAppearance(() => { const next = getAppearance(), f = codeFont(next); view.options.theme = terminalTheme(next); view.options.fontFamily = f.family; view.options.fontSize = f.size; try { fit.fit(); } catch { /* not laid out */ } });
    const focus = () => view.focus(); focusRef.current = focus;
    const control = new AbortController(), signal = control.signal;
    let done = false;
    // Input is sent in order, one request at a time; keys typed meanwhile are batched.
    let pending: Uint8Array[] = [], sending = false;
    const flush = async () => {
      if (sending) return; sending = true;
      try {
        while (pending.length && !signal.aborted) {
          const joined = new Uint8Array(pending.reduce((total, part) => total + part.length, 0));
          let at = 0; for (const part of pending) { joined.set(part, at); at += part.length; }
          pending = [];
          for (let start = 0; start < joined.length; start += inputLimit) await client.current.writeTerminal({ terminalId: id, data: joined.subarray(start, start + inputLimit) }, { signal });
        }
      } catch (error) { if (!signal.aborted && !done) onError(`Terminal input failed: ${String(error)}`); pending = []; }
      finally { sending = false; }
    };
    const send = (data: Uint8Array) => { if (done) return; pending.push(data); void flush(); };
    const input = view.onData(data => {
      // A person who started typing owns this prompt.
      if (initialCommand) initialCommand.current = undefined;
      send(encoder.encode(data));
    });
    const binary = view.onBinary(data => send(Uint8Array.from(data, character => character.charCodeAt(0) & 255)));
    let sized = '';
    const resize = () => {
      try { fit.fit(); } catch { return; }
      const value = `${view.cols}x${view.rows}`; if (value === sized || exited.current) return; sized = value;
      onSize({ cols: view.cols, rows: view.rows });
      void client.current.resizeTerminal({ terminalId: id, cols: view.cols, rows: view.rows }, { signal }).catch(error => {
        if (signal.aborted || exited.current) return;
        const failure = ConnectError.from(error);
        // The PTY can exit before either the list or the output stream reports it.
        if (failure.code === Code.FailedPrecondition && failure.rawMessage === 'terminal has exited') {
          exited.current = true;
          return;
        }
        onError(`Terminal resize failed: ${failure.rawMessage}`);
      });
    };
    const observer = new ResizeObserver(() => resize()); observer.observe(element);
    requestAnimationFrame(() => { resize(); view.focus(); });
    const stageAtPrompt = (status: TerminalStatus | undefined) => {
      if (!initialCommand) return;
      const text = initialCommand.current;
      if (!text || !status || signal.aborted || status.done) return;
      const bracketed = view.modes.bracketedPasteMode;
      if (!status.idle && (status.promptMarks || !bracketed)) return;
      initialCommand.current = undefined;
      if (/[\n\t]/.test(text) && !bracketed) {
        onError('This shell cannot hold a multi-line paste for review. Use Copy code to review the command.');
        return;
      }
      view.paste(text);
    };
    const stageTimeout = initialCommand ? setTimeout(() => {
      if (initialCommand.current) {
        initialCommand.current = undefined;
        onError('The shell prompt was not ready for a paste. Use Copy code to review the command.');
      }
    }, 8000) : undefined;
    void (async () => {
      let offset = 0n;
      while (!signal.aborted) {
        try {
          setConnection(offset ? 'Reconnecting' : 'Connecting');
          for await (const event of client.current.watchTerminal({ terminalId: id, after: offset }, { signal })) {
            if (event.data.length) view.write(event.data, () => stageAtPrompt(event.status));
            else stageAtPrompt(event.status);
            offset = event.offset + BigInt(event.data.length);
            if (event.status?.done) exited.current = true;
            if (event.status) setStatus(event.status);
            setConnection('Live');
            if (event.status?.done && !event.data.length) {
              done = true; view.options.disableStdin = true; view.options.cursorBlink = false;
              view.write(`\r\n\x1b[2m[${exitText(event.status)}]\x1b[0m\r\n`);
              // Seen ending live (not an old exited shell reopened): close after a beat.
              if (startedLive.current && event.status.exitCode === 0 && !event.status.signal && !event.status.error) setTimeout(() => cleanExit.current?.(), 400);
            }
          }
          if (done || signal.aborted) return;
        } catch (error) {
          if (signal.aborted) return;
          const code = ConnectError.from(error).code;
          if (code === Code.NotFound) { setConnection('Closed'); return; }
          setConnection(`Disconnected: ${String(error)}`);
        }
        await new Promise<void>(resolve => { const timer = setTimeout(resolve, 1000); signal.addEventListener('abort', () => { clearTimeout(timer); resolve(); }, { once: true }); });
      }
    })();
    return () => { unfollow(); if (focusRef.current === focus) focusRef.current = null; control.abort(); clearTimeout(stageTimeout); observer.disconnect(); selectionListener.dispose(); input.dispose(); binary.dispose(); links.dispose(); element.removeEventListener('mousedown', press, true); hovered.current = ''; view.dispose(); };
  }, [id]);
  return <div className="terminal-view">
    {error && <div className="error" role="alert">{error}</div>}
    <div ref={host} className="terminal-host" data-testid="terminal" data-terminal-id={id} onContextMenu={contextMenu('terminal', (): MenuItem[] => {
      const view = xterm.current, selection = view?.getSelection() || '';
      const live = !status.done;
      const link = hovered.current;
      return [
        ...(link ? [...linkMenuItems(link), 'separator' as const] : []),
        { label: 'Copy', icon: 'copy', shortcut: 'Mod+C', disabled: !selection && 'Select text in the terminal first', run: () => copyText(selection) },
        { label: 'Paste', icon: 'paste', shortcut: 'Mod+V', disabled: !live && 'The shell has ended', run: async () => {
          let text = '';
          try { text = await navigator.clipboard.readText(); } catch { showMenuNotice(`Paste with ${navigator.platform.includes('Mac') ? '⌘V' : 'Ctrl+V'}: the clipboard is not readable here`); return; }
          view?.paste(text); view?.focus();
        } },
        { label: 'Select all', icon: 'selectAll', run: () => { view?.selectAll(); view?.focus(); } },
        { label: 'Clear', icon: 'clear', run: () => { view?.clear(); view?.focus(); } },
        'separator',
        ...(terminal.place === 'interactive-command' ? [] : [{ label: 'Add selection to draft', icon: 'send' as const, disabled: !selection ? 'Select text in the terminal first' : !canSend && 'No conversation in this workspace yet', run: () => onSendToAgent(selection) }]),
        'separator',
        onNewTerminal && { label: 'New terminal', icon: 'plus', run: onNewTerminal },
        { label: live ? 'Close terminal' : 'Remove terminal', icon: 'close', danger: live, run: onCloseTerminal },
      ];
    }, { terminalId: id, workspaceId })}/>
    {hasSelection && terminal.place !== 'interactive-command' && <button type="button" className="terminal-send-selection"
      disabled={!canSend} title={canSend ? 'Add the selected terminal text to the conversation draft' : 'No conversation in this workspace yet'}
      onMouseDown={event => event.preventDefault()} onClick={() => {
        const view = xterm.current;
        const text = view?.getSelection();
        if (text) { onSendToAgent(text); view?.clearSelection(); }
      }}><Icon name="send" size={14}/>Add selection to draft</button>}
    <footer><span className="mono" title={status.shell}>{status.title}</span><span>{status.done ? exitText(status) : connection}</span></footer>
  </div>;
}
