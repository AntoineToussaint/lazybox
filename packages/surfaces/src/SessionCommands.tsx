import { exactTime, relativeTime, useActivityClock, validActivityTime } from './ActivityTime';
import { useState, type FormEvent, type ReactNode } from 'react';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';
import type { RunnerClient } from '@lazybox/client-core';
import type { CommandStatus, Session } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { Console } from './session-parts';
import type { useTranscript } from './useTranscript';

/** The session's commands: start one, pick one, and read or act on its output. */
export function SessionCommands({ api, session, sessionID, commands, currentCommand, open, viewState, onToggle, onSelect, onError, onSendToAgent, onOpenViewer }: {
  api: RunnerClient;
  session?: Session;
  sessionID: string;
  commands: CommandStatus[];
  currentCommand?: string;
  open: boolean;
  viewState?: ViewStateStore;
  onToggle(open: boolean): void;
  onSelect(commandId: string): void;
  onError(message: string): void;
  onSendToAgent?(text: string): string;
  onOpenViewer?(sessionId: string, commandId: string): void | Promise<void>;
}) {
  const now = useActivityClock();
  const [shell, setShell] = useState('');
  const startCommand = async (event: FormEvent) => {
    event.preventDefault();
    if (!shell.trim()) return;
    try { const output = await api.startCommand({ sessionId: sessionID, title: shell.slice(0, 64), argv: ['/bin/sh', '-c', shell] }); onSelect(output.id); }
    catch (e) { onError(String(e)); }
  };
  return <details className="session-commands" open={open} onToggle={event => onToggle(event.currentTarget.open)}>
    <summary>Commands & output {commands.length > 0 && `(${commands.length})`}</summary>
    <form onSubmit={startCommand} className="command-form">
      <input aria-label="Shell command" value={shell} onChange={e => setShell(e.target.value)} disabled={!session || session.closed}/>
      <button disabled={!session || session.closed}>Run</button>
    </form>
    <div className="command-tabs">
      {commands.map(command => {
        const at = Number(command.startedAtUnixMillis);
        const exact = validActivityTime(at) ? exactTime(at) : '';
        return <button key={command.id} className={currentCommand === command.id ? 'selected' : ''} title={exact || undefined}
          aria-label={exact ? `${command.title}, started ${exact}` : command.title} onClick={() => onSelect(command.id)}>
          {command.done ? '◦' : '●'} {command.title.slice(0, 28)}{exact && <time dateTime={new Date(at).toISOString()}> · {relativeTime(at, now)}</time>}
        </button>;
      })}
    </div>
    {currentCommand && <>
      <Console key={currentCommand} api={api} command={currentCommand} viewState={viewState} onError={onError} onSendToAgent={onSendToAgent}
        onOpenViewer={onOpenViewer ? async () => { try { await onOpenViewer(sessionID, currentCommand); } catch (e) { onError(String(e)); } } : undefined}/>
      <button className="cancel" onClick={async () => { try { await api.cancelCommand({ commandId: currentCommand }); } catch (e) { onError(String(e)); } }}>Cancel owned command</button>
    </>}
  </details>;
}

/** The last journal entries and operation receipts, for diagnosis. */
export function EventLog({ events }: { events: ReturnType<typeof useTranscript>['recent'] }) {
  return <details className="event-detail">
    <summary>Operation receipts & events ({events.length})</summary>
    <pre>{events.slice(-40).map(e => `${e.cursor} ${e.event?.type} ${e.event?.status || e.receipt?.delivery || ''} ${e.receipt?.providerId || e.event?.turnId || ''}`).join('\n')}</pre>
  </details>;
}

/** A window that shows one command's output, independent of any chat. */
export function ConsoleWindow({ api, command, error, viewState, onError, onOpenFile, files }: { api: RunnerClient; command?: string; error: string; viewState?: ViewStateStore; onError(message: string): void;
  /** Opens a file the output names, and what that shows (a choice of matches, a quick look). */ onOpenFile?: (link: FileLink, how: OpenModifiers) => void; files?: ReactNode }) {
  return <main className="agent-window console-window">
    <header className="topbar"><div><h1>Command console</h1><p>Independent view · execution stays on the Runner</p></div></header>
    {error && <div role="alert" className="error">{error}</div>}
    {command ? <Console api={api} command={command} viewState={viewState} onError={onError} onOpenFile={onOpenFile}/> : <div className="empty">No command is selected.</div>}{files}
  </main>;
}
