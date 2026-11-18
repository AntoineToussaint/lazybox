import { ActivityTime, exactTime, relativeTime, useActivityClock, validActivityTime } from './ActivityTime';
import { useEffect, useRef, useState } from 'react';
import type { CommandStatus } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { Console } from './session-parts';
import { useRunner } from './RunnerContext';
import type { ViewTarget } from './workspace-model';
import { offerToComposer, runOrigin } from './command-runs';
import { newerRuns, noteConsoleShows, noteConsoleUse, readFollowRuns, writeFollowRuns } from './console-follow';
import { useFileOpener } from './file-links';
import { Icon } from './Icons';

export function CommandPanel({ target, onTarget, viewState, onOpenViewer }: { target: ViewTarget; onTarget(target: ViewTarget): void; viewState?: ViewStateStore; onOpenViewer?(sessionId: string, commandId: string): Promise<void> }) {
  const { api } = useRunner();
  const now = useActivityClock();
  // Files the output names open in this workspace's editor.
  const fileLinks = useFileOpener(target.collectionId || undefined, viewState);
  const [commands, setCommands] = useState<CommandStatus[]>([]), [error, setError] = useState('');
  const refresh = useRef<() => Promise<void>>(undefined);
  useEffect(() => {
    setCommands([]); setError(''); if (!target.sessionId) return;
    const control = new AbortController(); let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      try { const result = await api.listCommands({ sessionId: target.sessionId }, { signal: control.signal }); if (!control.signal.aborted) { setCommands(result.commands); setError(''); } }
      catch (error) { if (!control.signal.aborted) setError(String(error)); }
      finally { updating = false; }
    };
    refresh.current = update;
    void update(); const timer = setInterval(() => void update(), 1500);
    return () => { control.abort(); clearInterval(timer); refresh.current = undefined; };
  }, [api, target.sessionId]);
  // Shown a command that just started (Run myself): list it now, not at the next poll.
  const known = commands.some(command => command.id === target.commandId);
  useEffect(() => { if (target.commandId && !known) void refresh.current?.(); }, [target.commandId, known]);
  // With no run chosen, a command still running is what you want to see, even when a later one already finished.
  // That choice holds once made: runs starting later are offered (below), they do not replace it.
  const fallback = [...commands].reverse().find(command => !command.done)?.id || commands.at(-1)?.id || '';
  const [settled, setSettled] = useState({ sessionId: target.sessionId, commandId: '' });
  const held = settled.sessionId === target.sessionId && (!commands.length || commands.some(command => command.id === settled.commandId)) ? settled.commandId : '';
  useEffect(() => { if (!target.commandId && !held && fallback) setSettled({ sessionId: target.sessionId, commandId: fallback }); }, [target.commandId, target.sessionId, held, fallback]);
  const selected = target.commandId || held || fallback;
  useEffect(() => noteConsoleShows(target.sessionId, selected), [target.sessionId, selected]);
  const choose = (commandId: string) => { noteConsoleUse(target.sessionId); onTarget({ ...target, commandId }); };
  // "Follow new runs": on, a new run takes the console over when nobody is reading it; off, it is only offered.
  const [follow, setFollowState] = useState(() => readFollowRuns(viewState));
  const setFollow = (on: boolean) => { setFollowState(on); try { writeFollowRuns(viewState, on); } catch (error) { setError(`Could not save “Follow new runs”: ${String(error)}`); } };
  const followItem = <button type="button" className="console-follow" aria-pressed={follow} title={follow ? 'A new run replaces the output you see only when it finished and you have not used it for a while. Click to never switch by itself.' : 'The output never switches by itself; newer runs are offered above it. Click to follow new runs.'} onClick={() => setFollow(!follow)}><span className="console-follow-mark" aria-hidden="true">{follow && <Icon name="check"/>}</span><span>Follow new runs</span></button>;
  const mark = (command: CommandStatus) => command.done ? (command.exitCode === 0 || (command.exitCode === undefined && command.report?.exitCode === 0) ? '✓ ' : command.signal ? '■ ' : '✗ ') : '● ';
  // The run to read: the newest first, as people look for what just happened.
  const selectedAt = Number(commands.find(command => command.id === selected)?.startedAtUnixMillis);
  const picker = <label className="command-picker"><select aria-label="Command" title="Choose which command’s output to read" value={commands.some(command => command.id === selected) ? selected : ''} onChange={event => choose(event.target.value)}>
    <option value="" disabled>Choose a command…</option>
    {[...commands].reverse().map(command => {
      const at = Number(command.startedAtUnixMillis);
      return <option key={command.id} value={command.id} title={validActivityTime(at) ? exactTime(at) : undefined}>
        {mark(command)}{command.title} · {runOrigin(command)}{command.done ? 'completed' : 'running'}{validActivityTime(at) ? ` · ${relativeTime(at, now)}` : ''}
      </option>;
    })}
  </select></label>;
  // Runs newer than the one shown: offered, never imposed. Show jumps to the newest.
  const newer = newerRuns(commands, selected), newest = newer.at(-1);
  const offer = newest && <button type="button" className="console-newer" data-running={!newest.done || undefined}
    aria-label={`${newer.length} newer ${newer.length === 1 ? 'run' : 'runs'}, the newest ${newest.title}${newest.done ? '' : ' (running)'}. Show it`}
    title={`Show the newest run: ${newest.title}`} onClick={() => choose(newest.id)}>
    <span className="console-newer-count">{newer.length} newer</span><span className="console-newer-title">{newest.title}</span><span className="console-newer-show" aria-hidden="true">↵ Show</span>
  </button>;
  const toolbar = <>{picker}{validActivityTime(selectedAt) && <ActivityTime key={selected} at={selectedAt} className="command-history-time"/>}{offer}</>;
  return <section className="command-panel">{error && <div className="error" role="alert">{error}</div>}{fileLinks.element}
    {!target.sessionId ? <p className="module-empty">Open a conversation to see the output of the commands it runs.</p> : <>
      {selected && commands.some(command => command.id === selected) ? <Console key={selected} api={api} command={selected} viewState={viewState} onError={setError} toolbar={toolbar} menu={followItem} onUse={() => noteConsoleUse(target.sessionId)} onOpenFile={target.collectionId ? (link, how) => void fileLinks.open(link, how) : undefined} onSendToAgent={text => offerToComposer(target.sessionId, text) ? '' : 'Show this conversation’s chat on the page to add the output to your message.'} onOpenViewer={onOpenViewer ? async () => { try { await onOpenViewer(target.sessionId, selected); } catch (error) { setError(String(error)); } } : undefined}/> : <>{commands.length > 0 && <div className="console-tools">{toolbar}</div>}<p className="module-empty">{selected ? 'That command’s output is no longer kept. Pick another one above.' : 'No command has run in this conversation yet. Output appears here when the agent, an action or `lazybox run` starts one.'}</p></>}
    </>}
  </section>;
}
