import { useEffect, useState } from 'react';
import type { CommandStatus } from '@lazybox/protocol';
import { useRunner } from '../RunnerContext';
import { Console } from '../session-parts';
import { Icon } from '../Icons';
import { phoneCommands } from './commands';

/** A conversation's commands (its own and its workspace's runs), kept current while shown. */
export function useConversationCommands(sessionId: string): CommandStatus[] {
  const { api } = useRunner();
  const [commands, setCommands] = useState<CommandStatus[]>([]);
  useEffect(() => {
    const control = new AbortController();
    const load = () => void api.listCommands({ sessionId }, { signal: control.signal, timeoutMs: 10000 }).then(result => { if (!control.signal.aborted) setCommands(result.commands); }, () => {});
    load(); const timer = setInterval(load, 4000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, sessionId]);
  return commands;
}

/** Under a conversation's header: the commands its agent runs, live ones first; a tap follows one's output. */
export function CommandStrip({ commands, onOpen }: { commands: CommandStatus[]; onOpen(command: string): void }) {
  const shown = phoneCommands(commands);
  if (!shown.length) return null;
  return <nav className="remote-commands" aria-label="Commands">
    {shown.map(command => <button key={command.id} type="button" data-running={!command.done || undefined} data-failed={command.done && ((command.exitCode ?? 0) !== 0 || !!command.signal) || undefined} onClick={() => onOpen(command.id)}>
      {!command.done ? <span className="remote-command-live" aria-label="running"/> : <Icon name={(command.exitCode ?? 0) === 0 && !command.signal ? 'check' : 'cross'} size={12}/>}
      <span>{command.title || command.argv.join(' ') || 'Command'}</span>
    </button>)}
  </nav>;
}

/**
 * One command's output on the phone: the computer's console (the same log
 * view, search, folds and text size), following new output as it arrives.
 * A phone can read output but not stop or rerun the command.
 */
export function PhoneCommand({ commandId, commands, onBack }: { commandId: string; commands: CommandStatus[]; onBack(): void }) {
  const { api } = useRunner();
  const [error, setError] = useState('');
  const status = commands.find(command => command.id === commandId);
  return <main className="remote-command" aria-label={status?.title || 'Command output'}>
    <header className="remote-bar">
      <button className="remote-icon-button" aria-label="Back to conversation" onClick={onBack}><Icon name="back" size={20}/></button>
      <div className="remote-bar-title"><h1>{status?.title || 'Command output'}</h1><span>{status ? (status.done ? 'Ended' : 'Running · output follows live') : 'Output'}</span></div>
    </header>
    {error && <p className="remote-error remote-floating" role="alert">{error}<button aria-label="Dismiss" onClick={() => setError('')}>×</button></p>}
    <div className="remote-console"><Console api={api} command={commandId} readOnly onError={setError}/></div>
  </main>;
}
