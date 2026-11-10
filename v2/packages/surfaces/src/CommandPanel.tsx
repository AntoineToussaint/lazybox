import { useEffect, useState } from 'react';
import type { CommandStatus } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { Console } from './session-parts';
import { useRunner } from './RunnerContext';
import type { ViewTarget } from './workspace-model';

export function CommandPanel({ target, onTarget, viewState, onOpenViewer }: { target: ViewTarget; onTarget(target: ViewTarget): void; viewState?: ViewStateStore; onOpenViewer?(sessionId: string, commandId: string): Promise<void> }) {
  const { api } = useRunner();
  const [commands, setCommands] = useState<CommandStatus[]>([]), [error, setError] = useState('');
  useEffect(() => {
    setCommands([]); setError(''); if (!target.sessionId) return;
    const control = new AbortController(); let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      try { const result = await api.listCommands({ sessionId: target.sessionId }, { signal: control.signal }); if (!control.signal.aborted) { setCommands(result.commands); setError(''); } }
      catch (error) { if (!control.signal.aborted) setError(String(error)); }
      finally { updating = false; }
    };
    void update(); const timer = setInterval(() => void update(), 1500);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, target.sessionId]);
  const selected = target.commandId || commands.at(-1)?.id || '';
  return <section className="command-panel">{error && <div className="error" role="alert">{error}</div>}
    {!target.sessionId ? <p className="module-empty">Select an existing session to inspect its command output.</p> : <>
      <label className="command-picker">Command<select value={selected} onChange={event => onTarget({ ...target, commandId: event.target.value })}><option value="">Select command</option>{commands.map(command => <option key={command.id} value={command.id}>{command.title} · {command.done ? 'completed' : 'running'}</option>)}</select></label>
      {selected && commands.some(command => command.id === selected) ? <Console key={selected} api={api} command={selected} viewState={viewState} onError={setError} onOpenViewer={onOpenViewer ? async () => { try { await onOpenViewer(target.sessionId, selected); } catch (error) { setError(String(error)); } } : undefined}/> : <p className="module-empty">{selected ? 'The saved command is unavailable in this session.' : 'No captured commands. This viewer does not start a process.'}</p>}
    </>}
  </section>;
}
