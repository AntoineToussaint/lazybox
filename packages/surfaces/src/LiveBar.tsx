import { useContext, useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import { agentTitle } from './providers';
import { follow, type RunnerClient } from '@lazybox/client-core';
import type { CommandStatus, JournalEvent, Session } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { Icon } from './Icons';
import { LiveIndicator, useNow } from './LiveIndicator';
import { elapsedLabel, type LiveStatus } from './live-status';
import { isManagedCommandTool, mayNeverEnd, offersBackground, outputLines, rootStep, runningCommand, stepDetail, commandParts, QUIET_MS } from './live-step';
import { LiveActivity } from './SessionNotices';
import { SubagentTray, useBackgroundRunning } from './SubagentTray';
import './live-bar.css';
import { PanelShownContext } from './useAttention';
import { contextMenu, copyText } from './ContextMenu';
import { stripAnsi } from './ansi';
import { LogTail, type LogActions } from './LogView';
import { openInBrowser, openInViewer, openPreview } from './web-links';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';
import { readPreference, writePreference } from './view-state';
import { humanError } from './human-error';

/** Whether the bar shows a command's latest lines: a choice that holds for every conversation until changed. */
const EXPANDED_PREFERENCE = 'chat.live-output-expanded';
const OUTPUT_PREFERENCE = 'chat.live-output';
const outputHidden = (store?: ViewStateStore) => { try { return readPreference(store, OUTPUT_PREFERENCE) === 'hidden'; } catch { return false; } };

const TAIL_BYTES = 32768;
const agentLabel = (session?: Session) => agentTitle(session?.provider, [], 'the agent');
const sentence = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);

/** The newest output of a Lazybox-managed command, as it streams. */
function useCommandTail(api: RunnerClient, commandId: string): string {
  const [text, setText] = useState('');
  useEffect(() => {
    setText('');
    if (!commandId) return;
    const control = new AbortController(), decoders = new Map<string, TextDecoder>();
    void follow((after, signal) => api.watchOutput({ commandId, after }, { signal }), control.signal, event => {
      if (!event.data.length) return;
      let decoder = decoders.get(event.channel); if (!decoder) { decoder = new TextDecoder(); decoders.set(event.channel, decoder); }
      const chunk = decoder.decode(event.data, { stream: true });
      setText(value => (value + chunk).slice(-TAIL_BYTES));
    }, () => {}, () => {});
    return () => control.abort();
  }, [api, commandId]);
  return text;
}

/** This conversation's commands, while it works: the Runner owns their output. */
function useSessionCommands(api: RunnerClient, sessionId: string, active: boolean): CommandStatus[] {
  const [commands, setCommands] = useState<CommandStatus[]>([]);
  useEffect(() => {
    if (!active || !sessionId) { setCommands([]); return; }
    const control = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const update = async () => {
      try {
        const result = await api.listCommands({ sessionId }, { signal: control.signal, timeoutMs: 5000 });
        if (!control.signal.aborted) setCommands(result.commands);
      } catch { /* Keep the last known command until a poll succeeds. */ }
      if (!control.signal.aborted) timer = setTimeout(() => void update(), 1500);
    };
    void update();
    return () => { control.abort(); clearTimeout(timer); };
  }, [api, sessionId, active]);
  return commands;
}

/**
 * What the running turn is doing, in one calm bar above the composer: the
 * step in words with its actual command and where it runs, how long it has
 * taken, the live output of a Lazybox-managed command (the last lines, more
 * on request, hidden when it is in the way, or the whole of it in the
 * console), the right Stop, and the sub-agents and background commands at work.
 */
export function LiveBar({ api, session, sessionID, live, disconnected, activity, viewState, onInterrupt, onOpenCommand, onOpenFile }: {
  api: RunnerClient;
  session?: Session;
  sessionID: string;
  live: LiveStatus;
  disconnected: boolean;
  /** The conversation's retained activity events (tool calls). */
  activity: readonly JournalEvent[];
  /** Remembers whether the output is shown. */
  viewState?: ViewStateStore;
  onInterrupt(): void;
  /** Shows a managed command in the console. */
  onOpenCommand?(commandId: string): void;
  /** Opens a file the output names (`main.go:42`). */
  onOpenFile?(link: FileLink, how: OpenModifiers): void;
}) {
  const rootBusy = !!session?.busy && !session.closed && live.tone === 'busy';
  const background = useBackgroundRunning();
  const step = useMemo(() => rootBusy ? rootStep(activity) : undefined, [rootBusy, activity]);
  const commands = useSessionCommands(api, sessionID, rootBusy);
  const managed = rootBusy ? runningCommand(commands, sessionID) : undefined;
  // Behind another tab, the command's output is not followed (AgentWindow, the transcript's stream).
  const shown = useContext(PanelShownContext);
  const tail = useCommandTail(api, shown ? managed?.id || '' : '');
  const [expanded, setExpanded] = useState(() => {
    try { return readPreference(viewState, EXPANDED_PREFERENCE) === 'expanded'; } catch { return false; }
  });
  const expand = (value: boolean) => {
    setExpanded(value);
    try { writePreference(viewState, EXPANDED_PREFERENCE, value ? 'expanded' : 'compact'); } catch { /* The choice holds in this bar. */ }
  };
  const [hidden, setHidden] = useState(() => outputHidden(viewState));
  // The command whose whole output is open in the console: its tail here would only repeat it, and take room
  // from the messages in a chat the console has made short. Show output brings it back.
  const [inConsole, setInConsole] = useState('');
  const tailHidden = hidden || (!!managed && inConsole === managed.id);
  const openInConsole = (id: string) => { setInConsole(id); onOpenCommand?.(id); };
  const showOutput = (show: boolean) => {
    setHidden(!show); setInConsole('');
    try { writePreference(viewState, OUTPUT_PREFERENCE, show ? 'shown' : 'hidden'); } catch { /* the choice still holds for this window */ }
  };
  const [stopping, setStopping] = useState('');
  const [moving, setMoving] = useState('');
  // What moving to the background did, said for a few seconds after the command left the bar.
  const [moved, setMoved] = useState('');
  const [error, setError] = useState('');
  const outputId = useId();
  // Pages and ports in the output open in the Web panel (⌘-click: the browser), files in the editor.
  const openFileRef = useRef(onOpenFile); openFileRef.current = onOpenFile;
  const tailActions = useMemo<LogActions>(() => ({
    openLink: (url, browser) => { if (browser || !openInViewer(url)) void openInBrowser(url).catch(e => setError(`Could not open ${url}: ${humanError(e)}`)); },
    openFile: onOpenFile ? (link, how) => openFileRef.current?.(link, how) : undefined,
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }), [!!onOpenFile]);
  // When a command was first seen running, for one the journal gives no start of.
  const seen = useRef(new Map<string, number>());
  if (managed && !seen.current.has(managed.id)) seen.current.set(managed.id, Date.now());
  // When its output last changed: a command quiet for a while may never end.
  const changed = useRef({ id: '', at: 0, tail: '' });
  if (managed && (changed.current.id !== managed.id || changed.current.tail !== tail)) changed.current = { id: managed.id, at: Date.now(), tail };
  const now = useNow(rootBusy);
  useEffect(() => { setStopping(''); setMoving(''); setError(''); }, [managed?.id]);
  useEffect(() => { if (!moved) return; const timer = setTimeout(() => setMoved(''), 8000); return () => clearTimeout(timer); }, [moved]);
  // What moving to the background did is said even when the turn ended right after (the bar itself is gone then).
  const movedNotice = moved && <p className="live-bar-notice" role="status"><Icon name="check" size={12}/><span>{moved}</span><button type="button" className="icon-button" aria-label="Dismiss" onClick={() => setMoved('')}><Icon name="close" size={11}/></button></p>;
  const withNotice = (rest: ReactNode) => movedNotice ? <>{rest}<section className="live-bar" data-only="notice" aria-label="Moved to the background">{movedNotice}</section></> : rest;
  if (disconnected || !session || session.closed) return withNotice(<LiveActivity disconnected={disconnected} live={live} session={session}/>);
  // Working without a turn the Runner holds (a status between turns): the plain line, as before.
  if (!rootBusy && !background) return withNotice(<LiveActivity disconnected={false} live={live} session={session}/>);
  if (!rootBusy) return withNotice(<div className="live-bar" data-only="background"><SubagentTray live={live} rootBusy={false}/></div>);

  const detail = managed && (!step || isManagedCommandTool(step.name) || !stepDetail(step).code)
    ? { ...commandParts(managed.argv.length ? managed.argv.join(' ') : managed.title), full: managed.argv.join(' ') || managed.title }
    : step ? stepDetail(step) : {};
  const startedAt = step?.startedAt ?? (managed ? seen.current.get(managed.id) : undefined);
  const lines = managed ? outputLines(tail, expanded ? 40 : 3) : [];
  const stop = async () => {
    if (!managed) { onInterrupt(); return; }
    setStopping(managed.id); setError('');
    try { await api.cancelCommand({ commandId: managed.id }); }
    catch (e) { setError(`Could not stop the command: ${humanError(e)}`); setStopping(''); }
  };
  // A `lazybox run` that will not end (a server, a watcher): its call returns now and it keeps running.
  const canMove = !!managed && offersBackground(managed, now - (changed.current.id === managed.id ? changed.current.at : now));
  const moveToBackground = async () => {
    if (!managed) return;
    setMoving(managed.id); setError('');
    try {
      const status = await api.backgroundCommand({ commandId: managed.id });
      const port = status.ports[0] || managed.ports[0];
      setMoved(`Moved to the background${port ? ` · port ${port}` : ''}. ${sentence(agentLabel(session))} continues; stop it from the console or Processes.`);
    } catch (e) { setError(`Could not move it to the background: ${humanError(e)}`); setMoving(''); }
  };
  // The agent's own shell step (not `lazybox run`) that may never end: only interrupting the turn ends the wait.
  const neverEnds = !managed && !!step && !isManagedCommandTool(step.name) && mayNeverEnd(detail.full, startedAt !== undefined ? now - startedAt : 0);
  // The bar's buttons, and copying what runs, on a right-click.
  const menu = contextMenu('live-bar', () => [
    canMove && { label: 'Move to background', icon: 'collapse', disabled: moving === managed!.id && 'Moving…', run: () => moveToBackground() },
    { label: managed ? 'Stop command' : 'Interrupt', icon: 'stop', danger: true, disabled: !!managed && stopping === managed.id && 'Stopping…', run: () => stop() },
    managed && managed.ports.length > 0 && { label: `Preview :${managed.ports[0]}`, icon: 'globe', run: () => openPreview(managed.ports[0]) },
    managed && onOpenCommand && { label: 'Open in console', icon: 'output', run: () => openInConsole(managed.id) },
    managed && !tailHidden && { label: expanded ? 'Less output' : 'More output', icon: expanded ? 'collapse' : 'expand', run: () => expand(!expanded) },
    managed && { label: tailHidden ? 'Show output' : 'Hide output', icon: tailHidden ? 'eye' : 'close', run: () => showOutput(tailHidden) },
    'separator',
    // The command Stop and the console are about, else the step's own.
    !!(managed ? managed.argv.join(' ') || managed.title : detail.full) && { label: 'Copy command', icon: 'copy', run: () => copyText(managed ? managed.argv.join(' ') || managed.title : detail.full!, 'Copied command') },
    managed && !!tail && { label: 'Copy output', icon: 'copy', run: () => copyText(stripAnsi(tail), 'Copied output') },
  ], { sessionId: sessionID, commandId: managed?.id || '' }, 'Running turn actions');
  return <section className="live-bar" aria-label="What the agent is doing" onContextMenu={menu}>
    <div className="live-bar-row" onDoubleClick={event => {
      if (!managed || (event.target instanceof Element && event.target.closest('button, a'))) return;
      if (tailHidden) { showOutput(true); expand(true); }
      else expand(!expanded);
    }}>
      <LiveIndicator tone="busy"/>
      {/* The step and its time, as the status line says them. */}
      <span className="live-activity" data-tone="busy" aria-hidden="true">{detail.code ? live.label : detail.words || live.label}{startedAt !== undefined && now >= startedAt ? ` · ${elapsedLabel(now - startedAt)}` : live.text.slice(live.label.length) || '…'}</span>
      {detail.code && <code className="live-bar-code" title={detail.full}>{detail.code}</code>}
      {detail.where && <span className="live-bar-where" title={detail.where}>in <code>{detail.where}</code></span>}
      <span className="live-bar-actions">
        {managed && tailHidden && <button type="button" className="live-bar-button" aria-expanded={false} onClick={() => showOutput(true)} title={inConsole === managed.id && !hidden ? 'The output is open in the console. Show its latest lines here too' : 'Show the command’s latest output here'}><Icon name="eye" size={12}/><span>Show output</span></button>}
        {managed && !tailHidden && <button type="button" className="live-bar-button" aria-label={expanded ? 'Less output' : 'More output'} aria-expanded={expanded} aria-controls={outputId} onClick={() => expand(!expanded)}><Icon name={expanded ? 'collapse' : 'expand'} size={12}/><span>{expanded ? 'Less output' : 'More output'}</span></button>}
        {managed && managed.ports.length > 0 && <button type="button" className="live-bar-button live-bar-preview" title={`Open http://localhost:${managed.ports[0]} in the Web panel (or your browser)`}
          onClick={() => void openPreview(managed.ports[0]).catch(e => setError(`Could not open port ${managed.ports[0]}: ${humanError(e)}`))}><Icon name="globe" size={12}/><span>Preview :{managed.ports[0]}</span></button>}
        {managed && onOpenCommand && <button type="button" className="live-bar-button" onClick={() => openInConsole(managed.id)} title="The whole output, with search, in the command console"><Icon name="output" size={12}/><span>Open in console</span></button>}
        {canMove && <button type="button" className="live-bar-button live-bar-move" disabled={moving === managed!.id} aria-label="Move to background"
          title={`This command keeps running${managed!.ports.length ? ` (listening on port ${managed!.ports[0]})` : ''}. Move it to the background: ${agentLabel(session)} gets its report now and continues, and the command keeps running, its output in the console.`}
          onClick={() => void moveToBackground()}><Icon name="collapse" size={12}/><span>{moving === managed!.id ? 'Moving…' : 'Move to background'}</span></button>}
        <button type="button" className="live-bar-button live-bar-stop" disabled={!!managed && stopping === managed.id}
          title={managed ? 'Stop this command: its process group gets SIGTERM, then SIGKILL. The agent sees it ended.' : 'Interrupt the agent’s turn. The conversation continues with your next message.'}
          onClick={() => void stop()}>
          <Icon name="stop" size={11}/><span>{managed ? stopping === managed.id ? 'Stopping…' : 'Stop command' : 'Interrupt'}</span>
        </button>
      </span>
    </div>
    {managed && !tailHidden && <div className="live-bar-output-frame">
      <pre id={outputId} className="live-bar-output" data-expanded={expanded || undefined} aria-label={`Latest output of ${managed.title || 'the command'}`} tabIndex={expanded ? 0 : -1} data-link-routing="own"
        onDoubleClick={event => { if (event.target instanceof Element && !event.target.closest('a, button')) expand(!expanded); }}
        onKeyDown={event => { if (event.key === 'Escape' && expanded) { event.stopPropagation(); expand(false); } }}>{lines.length ? <LogTail text={tail} count={expanded ? 40 : 3} actions={tailActions}/> : <span className="live-bar-quiet">No output yet.</span>}</pre>
      <button type="button" className="icon-button live-bar-hide" aria-label="Hide output" title="Hide the output here. The command keeps running; Show output brings it back." onClick={() => showOutput(false)}><Icon name="close" size={12}/></button>
    </div>}
    {neverEnds && <p className="live-bar-hint" role="note"><Icon name="alert" size={12}/><span>This command may never end. Interrupt the turn and ask {agentLabel(session)} to start it with <code>lazybox run</code> — it moves servers to the background.</span></p>}
    {movedNotice}
    {error && <p className="live-bar-error" role="alert">{error}</p>}
    {background > 0 && <SubagentTray live={live} rootBusy={false}/>}
  </section>;
}
