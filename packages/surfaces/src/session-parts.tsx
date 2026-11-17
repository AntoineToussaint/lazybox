import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { AgentRequestCard, type RequestCardProps } from './RequestCards';
import { follow, type RunnerClient } from '@lazybox/client-core';
import type { CommandStatus } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { readConsoleState, readPreference, writeConsoleState, writePreference } from './view-state';
import { composeRunMessage, formatBytes, isRuntimeRun, isWorkspaceRun, reportSummary, runningLabel } from './command-runs';
import { stripAnsi } from './ansi';
import { hashText, logRows, LogModel } from './log-lines';
import { LogList, SourceFilter, type LogActions, type LogLayout } from './LogView';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';
import { openInBrowser, openInViewer, openPreview } from './web-links';
import { Icon } from './Icons';
import { contextMenu, copyText, showMenuNotice } from './ContextMenu';
import { ActionMenu } from './ActionMenu';

/** A question or approval the agent waits on (RequestCards.tsx). */
export function RequestCard(props: RequestCardProps) {
  return <AgentRequestCard {...props}/>;
}

/** onSendToAgent adds text to the viewed conversation's message and returns
 * "" or why it could not. Offered for runs from a workspace terminal. */
/**
 * One command's output. `toolbar` makes it the compact, docked form: what the
 * host passes (the command picker) leads a single row with the outcome, Stop,
 * find and copy, so a short panel is output, not chrome. That row never wraps:
 * at narrow widths its lesser controls move into its ⋯ menu, which also holds
 * what the host passes as `menu`.
 */
export function Console({ api, command, onOpenViewer, viewState, onError, onSendToAgent, onOpenFile, toolbar, menu: hostMenu, onUse, readOnly }: { api: RunnerClient; command: string;
  /** The docked form's ⋯ menu items from the host, first. */
  menu?: ReactNode;
  /** The person read the output their way: found, scrolled, selected, resized its text. */
  onUse?: () => void;
  /** A paired phone follows output but may not stop or rerun it (REMOTE.md). */
  readOnly?: boolean; onOpenViewer?: () => void; viewState?: ViewStateStore; onError?: (error: string) => void; onSendToAgent?: (text: string) => string;
  /** Opens a file the output names (`main.go:42`); without it, file names are plain text. */
  onOpenFile?: (link: FileLink, how: OpenModifiers) => void; toolbar?: ReactNode }) {
  // The output, read line by line as it arrives (log-lines.ts); `version` re-renders at most once a frame.
  const model = useRef(new LogModel());
  const [version, setVersion] = useState(0);
  const frame = useRef(0);
  const text = model.current.raw;
  // The output's text size: one choice for every console, remembered (A− / A+, ⌘− / ⌘+ in the output).
  const [fontSize, setFontSizeState] = useState(() => { try { const n = Number(localStorage.getItem('lazybox:console.font-size')); return n >= 9 && n <= 16 ? n : 11; } catch { return 11; } });
  const setFontSize = (n: number) => { onUse?.(); const next = Math.min(16, Math.max(9, n)); setFontSizeState(next); try { localStorage.setItem('lazybox:console.font-size', String(next)); } catch { /* a convenience */ } };
  const [status, setStatus] = useState('Connecting');
  const [metadata, setMetadata] = useState<CommandStatus>();
  const [followOutput, setFollowOutput] = useState(true);
  const [search, setSearch] = useState('');
  const [offset, setOffset] = useState(0n);
  // Sources whose lines are hidden ('' for lines with no source), folds shown line by line, lists shown in full.
  const [hidden, setHiddenState] = useState<ReadonlySet<string>>(() => new Set());
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(() => new Set());
  const [openLists, setOpenLists] = useState<ReadonlyMap<number, ReadonlySet<number>>>(() => new Map());
  const [hideNoise, setHideNoiseState] = useState(false);
  const pane = useRef<HTMLDivElement>(null);
  const restoreScroll = useRef<number | null>(null);
  // When the reader last moved the output themselves (wheel, keys, touch, the scrollbar).
  const readerScrolled = useRef(0);
  const readerScrolls = () => { readerScrolled.current = Date.now(); onUse?.(); };
  const hiddenRef = useRef(hidden); hiddenRef.current = hidden;
  const saveView = (search: string, follow: boolean, hiddenSources: ReadonlySet<string> = hiddenRef.current, scrollTop = pane.current?.scrollTop || 0) => {
    scrollSave.current = null; window.clearTimeout(scrollTimer.current);
    try { writeConsoleState(viewState, command, { search, follow, scrollTop, ...(hiddenSources.size ? { hidden: [...hiddenSources] } : {}) }); }
    catch (error) { onError?.(`Could not save console view: ${String(error)}`); }
  };
  // Scrolling saves where the reader is once it settles (each save is a write to the host),
  // at once when following changes, and before the console shows another command or goes.
  const scrollSave = useRef<(() => void) | null>(null), scrollTimer = useRef(0);
  const saveScrollSoon = (save: () => void) => {
    scrollSave.current = save; window.clearTimeout(scrollTimer.current);
    scrollTimer.current = window.setTimeout(() => scrollSave.current?.(), 400);
  };
  useEffect(() => () => scrollSave.current?.(), [command]);
  const setHidden = (next: ReadonlySet<string>) => { setHiddenState(next); restoreScroll.current = null; saveView(search, followOutput, next); };
  useEffect(() => {
    model.current = new LogModel(); setVersion(value => value + 1); setMetadata(undefined); setOffset(0n);
    setExpanded(new Set()); setOpenLists(new Map());
    try { const saved = readConsoleState(viewState, command); setSearch(saved.search); setFollowOutput(saved.follow); setHiddenState(new Set(saved.hidden)); restoreScroll.current = saved.scrollTop; }
    catch (error) { onError?.(`Could not restore console view: ${String(error)}`); }
    const control = new AbortController(); const decoders = new Map<string, TextDecoder>();
    void follow((after, signal) => api.watchOutput({ commandId: command, after }, { signal }), control.signal, event => {
      if (event.data.length) {
        let decoder = decoders.get(event.channel); if (!decoder) { decoder = new TextDecoder(); decoders.set(event.channel, decoder); }
        model.current.append(decoder.decode(event.data, { stream: true }));
        setOffset(event.offset + BigInt(event.data.length));
        if (!frame.current) frame.current = requestAnimationFrame(() => { frame.current = 0; setVersion(value => value + 1); });
      }
      if (event.status) setMetadata(event.status);
    }, setStatus);
    return () => { control.abort(); cancelAnimationFrame(frame.current); frame.current = 0; };
  }, [api, command, viewState]);
  useEffect(() => {
    const element = pane.current; if (!element) return;
    if (followOutput) element.scrollTo({ top: element.scrollHeight });
    else if (restoreScroll.current !== null && (element.scrollHeight - element.clientHeight >= restoreScroll.current || status === 'Complete')) {
      element.scrollTo({ top: restoreScroll.current }); restoreScroll.current = null;
    }
  }, [version, followOutput, status, hidden, hideNoise]);
  const lines = useMemo(() => model.current.all(), [version]);
  const { rows, noise, counts } = useMemo(() => logRows(lines, { search, hidden, hideNoise, expanded }), [lines, search, hidden, hideNoise, expanded]);
  const { hasTime, sourceWidth, sources, colours } = model.current;
  const layout = useMemo<LogLayout>(() => ({ hasTime, sourceWidth, colour: name => colours.get(name), search }), [hasTime, sourceWidth, colours, search]);
  const openFileRef = useRef(onOpenFile); openFileRef.current = onOpenFile;
  const actions = useMemo<LogActions>(() => ({
    openLink: (url, browser) => { if (browser || !openInViewer(url)) void openInBrowser(url).catch(error => onError?.(`Could not open ${url}: ${String(error)}`)); },
    openFile: onOpenFile ? (link, how) => openFileRef.current?.(link, how) : undefined,
    toggleList: (lineId, start) => setOpenLists(current => { const next = new Map(current), open = new Set(next.get(lineId)); if (open.has(start)) open.delete(start); else open.add(start); if (open.size) next.set(lineId, open); else next.delete(lineId); return next; }),
    toggleFold: firstId => setExpanded(current => { const next = new Set(current); if (next.has(firstId)) next.delete(firstId); else next.add(firstId); return next; }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }), [!!onOpenFile]);
  const report = metadata?.report;
  const running = !!metadata && !metadata.done;
  // What happened, in words: never a stale "exit 0" for a command still running.
  const outcome = !metadata ? { tone: 'idle', text: status === 'Connecting' ? 'Connecting…' : status }
    : running ? { tone: 'running', text: runningLabel(metadata) }
    : metadata.captureError ? { tone: 'failed', text: 'Output incomplete' }
    : metadata.signal ? { tone: 'stopped', text: `Stopped (${metadata.signal})` }
    : metadata.exitCode === undefined ? (report && !report.piped ? (report.exitCode === 0 ? { tone: 'ok', text: 'Finished' } : { tone: 'failed', text: `Failed · exit ${report.exitCode}` }) : { tone: 'idle', text: 'Ended' })
    : metadata.exitCode === 0 ? { tone: 'ok', text: 'Finished' } : { tone: 'failed', text: `Failed · exit ${metadata.exitCode}` };
  const fromTerminal = !!metadata && isWorkspaceRun(metadata) && !isRuntimeRun(metadata);
  const runtimeEyebrow = metadata && isRuntimeRun(metadata) ? ({ action: 'Workspace action', setup: 'Setup script', archive: 'Archive script' } as Record<string, string>)[metadata.origin] : metadata?.origin === 'runner' ? 'Lazybox' : '';
  const [handed, setHanded] = useState<{ ok: boolean; text: string }>();
  useEffect(() => setHanded(undefined), [command]);
  const view = report?.agentView || '';
  const sendToAgent = () => {
    if (!metadata || !view || !onSendToAgent) return;
    const problem = onSendToAgent(composeRunMessage(metadata, view));
    setHanded(problem ? { ok: false, text: problem } : { ok: true, text: `Added ${formatBytes(report!.agentBytes)} to your message. Review it and send when ready.` });
  };
  const shareTitle = !metadata?.done ? 'Available when the output ends.' : !view ? 'This run has no compact view: its output did not end cleanly.' : `Adds the compact view (${formatBytes(report!.agentBytes)}: outcome and failures first, plus the full-output path) to your message. You decide when to send it.`;
  // The command as typed: a shell line is its script, anything else its words.
  const commandLine = metadata ? (metadata.argv.length === 3 && /(^|\/)(ba|z)?sh$/.test(metadata.argv[0]) && /^-l?c$/.test(metadata.argv[1]) ? metadata.argv[2] : metadata.argv.join(' ')) : '';
  // "Hide noise" is remembered per command (as typed), so the next run of `codefly run` starts quiet too.
  const noisePreference = `console.hide-noise.${hashText(commandLine || metadata?.title || '').toString(36)}`;
  const known = !!(commandLine || metadata?.title);
  useEffect(() => { if (!known) return; try { setHideNoiseState(readPreference(viewState, noisePreference) === 'on'); } catch { /* off */ } }, [known, noisePreference, viewState]);
  const setHideNoise = (on: boolean) => { setHideNoiseState(on); restoreScroll.current = null; try { writePreference(viewState, noisePreference, on ? 'on' : 'off'); } catch { /* still applies here */ } };
  const empty = !text ? 'Waiting for output…' : !rows.length ? (hidden.size && [...counts.keys()].every(owner => hidden.has(owner)) ? 'Every source is hidden. Show one above.' : search ? `No line contains “${search.trim()}”.` : '') : '';
  const menu = contextMenu('command', () => [
    running && !readOnly && { label: 'Stop', icon: 'stop', danger: true, run: () => api.cancelCommand({ commandId: command }).then(() => showMenuNotice('Stopping…')) },
    running && metadata.ports.length > 0 && { label: `Preview :${metadata.ports[0]}`, icon: 'globe', run: () => openPreview(metadata.ports[0]) },
    !readOnly && metadata?.done && !fromTerminal && metadata.sessionId && metadata.argv.length > 0 && { label: 'Run again', icon: 'play', run: () => api.startCommand({ sessionId: metadata.sessionId, title: metadata.title, argv: metadata.argv, cwd: metadata.cwd }).then(() => showMenuNotice(`Started “${metadata.title}” again`)) },
    'separator',
    { label: 'Copy output', icon: 'copy', disabled: !text && 'No output yet', run: () => copyText(stripAnsi(text), 'Copied output') },
    commandLine && { label: 'Copy command', icon: 'copy', run: () => copyText(commandLine, 'Copied command') },
    report?.outputPath && { label: 'Copy log path', icon: 'copy', run: () => copyText(report.outputPath, 'Copied log path') },
    'separator',
    onOpenViewer && { label: 'Open output in a window', icon: 'external', run: onOpenViewer },
    fromTerminal && onSendToAgent && { label: 'Send to agent', icon: 'send', disabled: !metadata?.done ? 'Available when the output ends' : !view && 'This run has no compact view', run: sendToAgent },
  ], { commandId: command });
  const size = formatBytes(offset);
  // A server the command runs: its page, without defining a workspace action.
  const port = running ? metadata.ports[0] : undefined;
  const preview = port ? <button type="button" className="console-preview" title={`Open http://localhost:${port} in the Web panel (or your browser)`} onClick={() => void openPreview(port).catch(error => onError?.(`Could not open port ${port}: ${String(error)}`))}><Icon name="globe" size={11}/>Preview :{port}</button> : null;
  const state = <span className="console-state">{preview}{running && !readOnly && <button className="console-stop" title="Stop this command (its process group gets SIGTERM, then SIGKILL)" onClick={() => void api.cancelCommand({ commandId: command }).catch(error => onError?.(`Could not stop the command: ${String(error)}`))}><Icon name="stop" size={11}/>Stop</button>}<span className={`pill run-${outcome.tone}`} role="status" title={`${size} of output so far`}>{outcome.text}</span></span>;
  return <section className={`console${toolbar ? ' console-docked' : ''}`} onContextMenu={menu}>{!toolbar && <div className="panel-title"><div><span className="eyebrow">{runtimeEyebrow || (fromTerminal ? 'From your terminal' : 'Shared console')}</span><h2>{metadata?.title || 'Command output'}</h2></div>{state}</div>}
    {/* Shown in every console, including the workspace's (which hides the title): what an agent gets, and the hand-over. */}
    {(report || (fromTerminal && onSendToAgent)) && <div className="command-meta">{fromTerminal && <span className="command-origin">From your terminal</span>}
      {report && <p className="command-report" data-testid="command-report"><span>{reportSummary(report, fromTerminal)}</span>{report.outputPath && <button title={report.outputPath} onClick={() => navigator.clipboard.writeText(report.outputPath).catch(error => onError?.(String(error)))}>Copy log path</button>}</p>}
      {fromTerminal && onSendToAgent && <p className="command-share"><button className="primary" disabled={!metadata.done || !view} title={shareTitle} onClick={sendToAgent}>Send to agent</button>{handed ? <small role={handed.ok ? 'status' : 'alert'} className={handed.ok ? '' : 'error-text'}>{handed.text}</small> : <small>{metadata.done ? view ? `The agent gets ${formatBytes(report!.agentBytes)}, not the ${formatBytes(report!.outputBytes)} above.` : '' : 'You can send it to an agent when the output ends.'}</small>}</p>}</div>}
    <div className="console-tools">{toolbar}{toolbar && state}<input aria-label="Search output" placeholder="Find in output…" value={search}
      onKeyDown={e => { if (e.key === 'Escape' && search) { e.stopPropagation(); onUse?.(); setSearch(''); restoreScroll.current = null; saveView('', followOutput); } }}
      onChange={e => { onUse?.(); setSearch(e.target.value); restoreScroll.current = null; saveView(e.target.value, followOutput); }}/>{(noise > 0 || hideNoise) && <button type="button" className="console-noise" data-fold="600" aria-pressed={hideNoise}
        title={hideNoise ? `Showing each kind of warning once and folding progress lines (${noise} hidden). Click to show every line.` : `Show each kind of warning once and fold progress lines: ${noise} repeated warning${noise === 1 ? '' : 's'}`}
        onClick={() => setHideNoise(!hideNoise)}><Icon name={hideNoise ? 'eye' : 'collapse'} size={12}/>{hideNoise ? 'Noise hidden' : 'Hide noise'}{noise > 0 && <span className="console-noise-count">{noise}</span>}</button>}<span className="console-size" data-fold="820" role="group" aria-label="Output text size"><button type="button" className="icon-button" aria-label="Smaller output text" title={`Smaller text (${fontSize}px)`} disabled={fontSize <= 9} onClick={() => setFontSize(fontSize - 1)}>A−</button><button type="button" className="icon-button" aria-label="Larger output text" title={`Larger text (${fontSize}px)`} disabled={fontSize >= 16} onClick={() => setFontSize(fontSize + 1)}>A+</button></span>{toolbar
        ? <><button className="icon-button" data-fold="820" aria-label="Copy output" title="Copy the output" disabled={!text} onClick={() => copyText(stripAnsi(text), 'Copied output')}><Icon name="copy" size={14}/></button>{onOpenViewer && <button className="icon-button" aria-label="Open output in a window" title="Open this output in its own window" onClick={onOpenViewer}><Icon name="external" size={14}/></button>}
          <ActionMenu label="Output actions" className="console-menu">{hostMenu}
            {/* What the row has no room for at narrow widths (shown in the row otherwise). */}
            <button type="button" data-unfold="820" data-keep-open disabled={fontSize <= 9} onClick={() => setFontSize(fontSize - 1)}><Icon name="zoomOut"/><span>Smaller text</span><small>{fontSize}px</small></button>
            <button type="button" data-unfold="820" data-keep-open disabled={fontSize >= 16} onClick={() => setFontSize(fontSize + 1)}><Icon name="zoomIn"/><span>Larger text</span></button>
            {(noise > 0 || hideNoise) && <button type="button" data-unfold="600" aria-pressed={hideNoise} onClick={() => setHideNoise(!hideNoise)}><Icon name={hideNoise ? 'eye' : 'collapse'}/><span>{hideNoise ? 'Show every line' : 'Hide noise'}</span>{noise > 0 && <small>{noise}</small>}</button>}
            <button type="button" data-unfold="820" disabled={!text} onClick={() => copyText(stripAnsi(text), 'Copied output')}><Icon name="copy"/><span>Copy output</span></button>
          </ActionMenu></>
        : <><button onClick={() => navigator.clipboard.writeText(stripAnsi(text)).catch(error => onError?.(String(error)))}>Copy</button>{onOpenViewer && <button onClick={onOpenViewer}>Open viewer ↗</button>}</>}</div>
    {sources.length > 1 && <SourceFilter sources={sources} counts={counts} hidden={hidden} colour={layout.colour} onChange={setHidden}/>}
    <div ref={pane} tabIndex={0} className="output log-output" data-testid="console-output" data-link-routing="own" aria-label="Command output" style={{ fontSize: `calc(${fontSize}px * var(--code-scale, 1))` }} onPointerDown={readerScrolls} onTouchMove={readerScrolls} onMouseUp={() => { if (window.getSelection()?.toString()) onUse?.(); }} onKeyDown={event => { readerScrolls(); if (!(event.metaKey || event.ctrlKey)) return; const step = event.key === '=' || event.key === '+' ? 1 : event.key === '-' ? -1 : 0; if (step) { event.preventDefault(); event.stopPropagation(); setFontSize(fontSize + step); } }} onWheel={() => { readerScrolls(); restoreScroll.current = null; }} onScroll={event => {
      const node = event.currentTarget; const atEnd = node.scrollHeight - node.scrollTop - node.clientHeight < 24; if (restoreScroll.current !== null) return;
      // Only the reader leaves the end: a scroll that arriving output, a fold or the windowing caused keeps following.
      if (followOutput && !atEnd && Date.now() - readerScrolled.current > 800) { node.scrollTop = node.scrollHeight; return; }
      if (atEnd !== followOutput) { setFollowOutput(atEnd); saveView(search, atEnd); return; }
      const scrollTop = node.scrollTop, hiddenSources = hiddenRef.current;
      saveScrollSoon(() => saveView(search, atEnd, hiddenSources, scrollTop)); }}>
      {empty ? <p className="log-empty">{empty}</p> : <LogList rows={rows} layout={layout} actions={actions} openLists={openLists} scroller={pane} follow={followOutput} lineHeight={fontSize * 1.55}/>}
    </div>
    {!followOutput && text && <button className="console-jump" onClick={() => { setFollowOutput(true); restoreScroll.current = null; pane.current?.scrollTo({ top: pane.current.scrollHeight }); saveView(search, true); }}>↓ Latest output</button>}
    <footer><span className="mono" title={`Command ${command}`}>{size} of output</span><span>{metadata?.captureError || (metadata?.done ? metadata.exitCode === undefined ? report && !report.piped ? `Exit ${report.exitCode}` : 'Capture EOF · upstream exit unknown' : `Exit ${metadata.exitCode}${metadata.signal ? ' · ' + metadata.signal : ''}` : 'Running')}</span></footer>
    <small className="retention">Showing up to the last 512 KiB. Full retained output is available through command reads until the configured spool budget is exhausted.</small>
  </section>;
}

