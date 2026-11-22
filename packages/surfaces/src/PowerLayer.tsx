import { useEffect, useMemo, useRef, useState } from 'react';
import type { UiRequest, UserAction } from '@lazybox/protocol';
import type { ViewStateStore, WorkspaceHost } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { invokeAction, setLeaderExtensions } from './keyboard';
import { actionRuntime, type ActionNavigation } from './action-runtime';
import { actionSummary, runUserAction, type ActionProgress, type ActionResult, type ActionTarget } from './user-actions';
import { userCommands, userMenuItems, useUserCommands, type UserCommand } from './user-commands';
import { useContextMenuContribution, type MenuItem } from './ContextMenu';
import { useUserActionsFile } from './useUserActions';
import { panelRect, useUiRequests } from './useUiRequests';
import { useConfigFilesBridge } from './useConfigFiles';
import { actionDefinition, actionEnabled, runAction } from './actions';
import './power.css';

/** The ]] key that opens the "Run an action…" picker. */
export const PICKER_KEY = 'x';

type Run = { id: string; title: string; progress?: ActionProgress; result?: ActionResult; control: AbortController };

export type PowerLayerProps = {
  workspaceId: string;
  /** The workspace and conversation in view: where an action runs by default. */
  selection: ActionTarget;
  /** The main workspace window answers the CLI's requests; other windows do not. */
  primary: boolean;
  host?: WorkspaceHost;
  viewState?: ViewStateStore;
  /** Shows a workspace (and conversation) in this window. */
  onSelect(target: ActionTarget): void;
  onShowSession(target: ActionTarget): void;
  onShowPanel(kind: string, target: ActionTarget): void;
  onOpenSettings(tab: 'power' | 'subscriptions'): void;
  onQuickLook(path: string, line: number): void;
  /** A name for what is shown, for the CLI ("Alpha feature · fix the tests"). */
  describe(target: ActionTarget): string;
};

/**
 * Everything the person's actions file adds to the window (POWER.md): its
 * actions in the ]] menu and the menus that list them, the "Run an action…"
 * picker, one quiet progress card per running action, and the answers to the
 * `lazybox` CLI's requests (open, shot, actions run).
 */
export function PowerLayer(props: PowerLayerProps) {
  const { api } = useRunner();
  const { file } = useUserActionsFile();
  const [runs, setRuns] = useState<Run[]>([]);
  const [confirming, setConfirming] = useState<{ action: UserAction; target: ActionTarget }>();
  const [picker, setPicker] = useState(false);
  const live = useRef(props); live.current = props;
  const pending = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  // keys.yaml, settings.yaml and layouts/: read here, applied by the main window.
  useConfigFilesBridge(props.primary, props.viewState, props.host?.zoom);

  /**
   * Runs an action of the app's registry, as its key or the palette would:
   * in the workspace and conversation given (shown first), with its argument.
   */
  const runAppAction = async (id: string, argument: string, target?: ActionTarget): Promise<string> => {
    const definition = actionDefinition(id);
    if (!definition) throw new Error(`The app has no action ${id}; lazybox do --list lists them`);
    const p = live.current, here = p.selection;
    if (id !== 'missions.open' && id !== 'missions.page' && target && ((target.collectionId && target.collectionId !== here.collectionId) || (target.sessionId && target.sessionId !== here.sessionId))) {
      if (target.sessionId) p.onShowSession(target); else p.onSelect(target);
      // Let the new workspace mount, so its handlers are the ones that run.
      await new Promise(resolve => setTimeout(resolve, 150));
    }
    if (!actionEnabled(id) && !id.startsWith('navigation.')) throw new Error(`“${definition.title}” is not available in the window right now`);
    runAction(id, argument || undefined, 'other');
    await p.host?.focusWindow?.().catch(() => undefined);
    return `Ran “${definition.title}”${argument ? ` (${argument})` : ''}`;
  };
  const runAppActionRef = useRef(runAppAction); runAppActionRef.current = runAppAction;

  const navigation: ActionNavigation = useMemo(() => ({
    showSession: target => live.current.onShowSession(target),
    openPanel: (kind, target) => {
      const p = live.current;
      if (kind === 'settings') p.onOpenSettings('subscriptions');
      else if (kind === 'inbox') invokeAction('inbox.open');
      else if (kind === 'chat') { if (target.sessionId) p.onShowSession(target); }
      else p.onShowPanel(kind, target);
    },
    runAppAction: async (id, argument, target) => {
      try { return { ok: true, detail: await runAppActionRef.current(id, argument, target) }; }
      catch (error) { return { ok: false, detail: error instanceof Error ? error.message : String(error) }; }
    },
    openFile: (target, path, line) => {
      const p = live.current;
      if (target.collectionId && target.collectionId !== p.workspaceId) { p.onSelect({ collectionId: target.collectionId, sessionId: target.sessionId }); setTimeout(() => live.current.onQuickLook(path, line), 250); }
      else p.onQuickLook(path, line);
    },
  }), []);
  const runtime = useMemo(() => actionRuntime(api, navigation, props.viewState), [api, navigation, props.viewState]);

  /** Runs an action now; the promise settles with its result. */
  const execute = (action: UserAction, target: ActionTarget): Promise<ActionResult> => {
    const id = crypto.randomUUID(), control = new AbortController();
    setRuns(list => [...list.filter(r => !r.result?.ok), { id, title: action.title, control }]);
    const update = (change: Partial<Run>) => setRuns(list => list.map(r => r.id === id ? { ...r, ...change } : r));
    return runUserAction(action, runtime, target, { signal: control.signal, onProgress: progress => update({ progress }) }).then(result => {
      update({ result });
      if (result.ok || result.cancelled) pending.current.set(id, setTimeout(() => { pending.current.delete(id); setRuns(list => list.filter(r => r.id !== id)); }, result.ok ? 4000 : 1500));
      return result;
    });
  };
  const start = (action: UserAction, target?: ActionTarget) => {
    const where = target || live.current.selection;
    if (action.confirm) setConfirming({ action, target: where });
    else void execute(action, where);
  };
  useEffect(() => () => { for (const timer of pending.current.values()) clearTimeout(timer); }, []);

  // The registry every menu reads, and the ]] keys.
  const actions = file?.actions || [];
  const startRef = useRef(start); startRef.current = start;
  useEffect(() => {
    userCommands.set(actions.map((action): UserCommand => ({ id: action.id, label: action.title, description: action.description, icon: action.icon || 'play', key: action.key, where: action.where, summary: actionSummary(action), run: target => startRef.current(action, target) })));
  }, [file?.revision]);
  useEffect(() => () => userCommands.set([]), []);
  // Right-click on a workspace row, a conversation row or a conversation's
  // header lists the actions offered there, in one "Your actions" submenu.
  const yours = (where: 'workspace' | 'conversation', target: ActionTarget): MenuItem[] => {
    const items = userMenuItems(where, target);
    return items.length ? ['separator', { label: 'Your actions', icon: 'play', submenu: items }] : [];
  };
  useContextMenuContribution<{ workspaceId?: string }>('workspace', ({ detail }) => yours('workspace', { collectionId: detail?.workspaceId || '', sessionId: '' }));
  useContextMenuContribution<{ workspaceId?: string; sessionId?: string }>('conversation', ({ detail }) => yours('conversation', { collectionId: detail?.workspaceId || props.workspaceId, sessionId: detail?.sessionId || '' }));
  useContextMenuContribution<{ sessionId?: string }>('session', ({ detail }) => yours('conversation', { collectionId: props.workspaceId, sessionId: detail?.sessionId || '' }));
  useEffect(() => setLeaderExtensions(() => [
    ...userCommands.list().filter(c => c.key && c.where.includes('menu')).map(c => ({ id: c.id, key: c.key, label: c.label, run: () => c.run() })),
    ...(userCommands.list().length ? [{ id: 'run-action', key: PICKER_KEY, label: 'Run an action…', run: () => setPicker(true) }] : []),
  ]), []);
  useEffect(() => {
    const open = () => setPicker(true);
    window.addEventListener('lazybox-run-action', open);
    return () => window.removeEventListener('lazybox-run-action', open);
  }, []);

  // The CLI's requests: `lazybox open`, `lazybox shot`, `lazybox actions run`, `lazybox do`.
  const surface = props.host?.captureWindow ? 'desktop' : 'web';
  useUiRequests(props.primary, surface, {
    open: async (request: UiRequest) => {
      const p = live.current, target = { collectionId: request.collectionId, sessionId: request.sessionId };
      if (request.panel === 'missions' && !request.file) {
        navigation.openPanel('missions', { collectionId: '', sessionId: '' });
        await p.host?.focusWindow?.().catch(() => undefined);
        return 'Showed Missions';
      }
      if (request.file) navigation.openFile(target, request.file, request.line || 1);
      else if (request.sessionId) p.onShowSession(target);
      else if (request.collectionId) p.onSelect(target);
      if (request.panel) navigation.openPanel(request.panel, target.collectionId ? target : p.selection);
      await p.host?.focusWindow?.().catch(() => undefined);
      const shown = request.file ? `${request.file}${request.line ? `:${request.line}` : ''}` : p.describe(target.collectionId || target.sessionId ? target : p.selection);
      return `Showed ${shown}${request.panel ? ` · ${request.panel}` : ''}`;
    },
    shot: async (request: UiRequest) => {
      const capture = live.current.host?.captureWindow;
      if (!capture) throw new Error('Screenshots need the Lazybox desktop app; this is a browser tab');
      let rect;
      if (request.panel) {
        rect = panelRect(request.panel);
        if (!rect) throw new Error(`No ${request.panel} panel is shown on this page; open it first, or take the whole window`);
      }
      // Paint once more so what the CLI asked to open is in the picture.
      await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      const png = await capture(rect);
      return { png, detail: request.panel ? `The ${request.panel} panel` : 'The window' };
    },
    action: async (request: UiRequest) => {
      const action = (file?.actions || []).find(a => a.id === request.actionId);
      if (!action) throw new Error(`No action ${request.actionId} in the actions file`);
      const target = request.collectionId || request.sessionId ? { collectionId: request.collectionId, sessionId: request.sessionId } : live.current.selection;
      const result = await execute(action, target);
      if (!result.ok) throw new Error(result.cancelled ? 'Cancelled in the window' : `Failed at step ${(result.failedStep ?? 0) + 1}: ${result.detail}`);
      return `Ran “${action.title}”`;
    },
    do: async (request: UiRequest) => runAppAction(request.actionId, request.argument, request.collectionId || request.sessionId ? { collectionId: request.collectionId, sessionId: request.sessionId } : undefined),
  });

  return <>
    {runs.length > 0 && <div className="power-toasts" aria-label="Running actions">
      {runs.map(run => <ActionToast key={run.id} run={run} onCancel={() => run.control.abort()} onClose={() => setRuns(list => list.filter(r => r.id !== run.id))}/>)}
    </div>}
    {confirming && <ConfirmAction action={confirming.action} onCancel={() => setConfirming(undefined)} onRun={() => { const { action, target } = confirming; setConfirming(undefined); void execute(action, target); }}/>}
    {picker && <ActionPicker onClose={() => setPicker(false)} onSettings={() => { setPicker(false); props.onOpenSettings('power'); }} onRun={command => { setPicker(false); command.run(); }}/>}
  </>;
}

function ActionToast({ run, onCancel, onClose }: { run: Run; onCancel(): void; onClose(): void }) {
  const { progress, result } = run;
  const failed = result && !result.ok && !result.cancelled;
  const state = !result ? 'running' : result.ok ? 'done' : result.cancelled ? 'cancelled' : 'failed';
  const step = progress ? `Step ${progress.index + 1} of ${progress.total} · ${progress.label}` : 'Starting…';
  return <article className={`power-toast is-${state}`} role={failed ? 'alert' : 'status'} aria-label={`Action ${run.title}`}>
    <span className="power-toast-icon" aria-hidden="true">{state === 'running' ? <span className="power-spinner"/> : <Icon name={state === 'done' ? 'check' : state === 'failed' ? 'alert' : 'stop'} size={15}/>}</span>
    <div>
      <strong>{run.title}</strong>
      <small>{!result ? step : result.ok ? (result.failedStep !== undefined ? 'Done · a failure was handled' : 'Done') : result.cancelled ? 'Cancelled' : `Failed at step ${(result.failedStep ?? 0) + 1}${progress ? ` · ${progress.label}` : ''}`}</small>
      {failed && <p>{result.detail}</p>}
    </div>
    {!result ? <button type="button" onClick={onCancel}>Cancel</button> : <button type="button" className="icon-button" aria-label={`Close ${run.title}`} title="Close" onClick={onClose}><Icon name="close" size={13}/></button>}
  </article>;
}

function ConfirmAction({ action, onRun, onCancel }: { action: UserAction; onRun(): void; onCancel(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { const node = dialog.current; node?.showModal(); const focus = () => node?.querySelector<HTMLElement>("[data-autofocus]")?.focus(); focus(); const frame = requestAnimationFrame(focus); return () => cancelAnimationFrame(frame); }, []);
  return <DraggableDialog ref={dialog} className="power-confirm" aria-labelledby="power-confirm-title" onCancel={event => { event.preventDefault(); onCancel(); }}>
    <DialogHeader><h2 id="power-confirm-title">Run “{action.title}”?</h2><button aria-label="Close" onClick={onCancel}>×</button></DialogHeader>
    {action.description && <p>{action.description}</p>}
    <ol className="power-confirm-steps">{action.steps.map((_, i) => <li key={i}>{actionSummary({ ...action, steps: [action.steps[i]] })}</li>)}</ol>
    <footer><button type="button" onClick={onCancel}>Cancel</button><button type="button" className="primary" data-autofocus autoFocus onClick={onRun}>Run</button></footer>
  </DraggableDialog>;
}

/** Every action, filterable: type to narrow, arrows to move, Enter to run. */
export function ActionPicker({ onRun, onClose, onSettings }: { onRun(command: UserCommand): void; onClose(): void; onSettings(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const all = useUserCommands();
  const [query, setQuery] = useState(''), [index, setIndex] = useState(0);
  useEffect(() => { const node = dialog.current; node?.showModal(); const focus = () => node?.querySelector<HTMLElement>("[data-autofocus]")?.focus(); focus(); const frame = requestAnimationFrame(focus); return () => cancelAnimationFrame(frame); }, []);
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const shown = all.filter(c => words.every(w => `${c.label} ${c.id} ${c.description} ${c.summary}`.toLowerCase().includes(w)));
  const selected = shown[Math.min(index, shown.length - 1)];
  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  }, [selected]);
  return <DraggableDialog ref={dialog} className="power-picker" aria-label="Run an action" onCancel={event => { event.preventDefault(); onClose(); }}
    onKeyDown={event => {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') { event.preventDefault(); setIndex(i => Math.max(0, Math.min(shown.length - 1, i + (event.key === 'ArrowDown' ? 1 : -1)))); }
      if (event.key === 'Enter' && !event.nativeEvent.isComposing && selected && (event.target as HTMLElement).tagName !== 'BUTTON') { event.preventDefault(); onRun(selected); }
    }}>
    <DialogHeader><h2>Run an action</h2><button aria-label="Close actions" onClick={onClose}>×</button></DialogHeader>
    <input data-autofocus autoFocus type="search" aria-label="Find actions" placeholder="Search your actions…" value={query} onChange={event => { setQuery(event.target.value); setIndex(0); }}/>
    <div ref={list} role="listbox" aria-label="Actions" className="power-picker-list">
      {shown.map((c, i) => <button type="button" role="option" key={c.id} aria-selected={c === selected} onMouseEnter={() => setIndex(i)} onClick={() => onRun(c)}>
        <Icon name={c.icon} size={15}/>
        <span><strong>{c.label}</strong><small>{c.summary}</small></span>
        {c.key && c.where.includes('menu') && <kbd>]] {c.key}</kbd>}
      </button>)}
      {!shown.length && <p className="power-empty" role="status">{all.length ? 'No matching actions.' : 'No actions yet. Declare them in your actions file.'}</p>}
    </div>
    <footer><small>Enter to run · Esc to close</small><button type="button" onClick={onSettings}><Icon name="sliders" size={13}/> Actions & hooks</button></footer>
  </DraggableDialog>;
}
