import { useState } from 'react';
import type { WorkspaceAction } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { ActionMenu } from './ActionMenu';
import { Icon } from './Icons';
import { ScriptApprovalDialog } from './ScriptApprovalDialog';
import { actionName, defaultAction, elapsed, isLive, needsApproval, portLabel, requestActionFocus, requestPreview, runLabel, setupsRunning, useRecentNotice, useTicker, useWorkspaceRuntime } from './workspace-runtime';
import './workspace-runtime.css';

type Props = {
  workspaceId: string;
  /** The workspace is not ready (being created or deleted): nothing can run. */
  disabled?: boolean;
  /** Shows the page's panel of this kind ("actions", "preview"). */
  onShowPanel(kind: string): unknown;
};

/**
 * The workspace header's Run control: ▶ starts the default action, the menu
 * the others; while something runs it shows what, where and for how long, with
 * Stop; while setup runs it says so; scripts waiting for approval show Review.
 */
export function RunControl({ workspaceId, disabled, onShowPanel }: Props) {
  const { api } = useRunner();
  const { runtime, unsupported, notice, busy, store } = useWorkspaceRuntime(api, workspaceId);
  const [reviewing, setReviewing] = useState('');
  const setups = setupsRunning(runtime);
  const actions = runtime?.actions || [];
  const live = actions.filter(a => isLive(a.runState));
  const now = useTicker(!!setups.length || !!live.length);
  const recent = useRecentNotice(notice);
  if (!workspaceId || unsupported || !runtime || !store) return null;

  const pending = needsApproval(runtime);
  const fallback = defaultAction(runtime);
  const current = live.find(a => a.key === fallback?.key) || live[0];
  const name = (action: WorkspaceAction) => actionName(action, actions);
  const showActions = (target = '') => { if (target) requestActionFocus(workspaceId, target); onShowPanel('actions'); };
  const preview = (action: WorkspaceAction) => { requestPreview(workspaceId, action.key); onShowPanel('preview'); };
  const review = pending[0];

  let primary;
  if (setups.length) {
    const setup = setups[0];
    const label = `Setting up ${setup.member}${setups.length > 1 ? ` +${setups.length - 1}` : ''}`;
    primary = <button type="button" className="run-segment run-status tone-busy" aria-label={`${label}, ${elapsed(setup.runState?.startedAtUnixMillis || 0n, now)}. Show its log`} title={setup.runState?.lastLine || 'Show the setup log'} onClick={() => showActions(`setup:${setup.member}`)}>
      <span className="run-spinner" aria-hidden="true"/><span className="run-text">{label}</span><span className="run-meta">{elapsed(setup.runState?.startedAtUnixMillis || 0n, now)}</span>
    </button>;
  } else if (current) {
    const state = current.runState!;
    const stopping = state.state === 'stopping';
    primary = <>
      <button type="button" className={`run-segment run-status tone-${stopping ? 'busy' : 'ok'}`} aria-label={`${name(current)} ${stopping ? 'stopping' : 'running'}${current.port ? ` on port ${current.port}` : ''}, ${elapsed(state.startedAtUnixMillis, now)}. ${current.url ? 'Open its preview' : 'Show its log'}`} title={state.lastLine || (current.url ? `Preview ${current.url}` : 'Show its log')} onClick={() => current.url ? preview(current) : showActions(current.key)}>
        <span className="run-dot" aria-hidden="true"/><span className="run-text">{name(current)}</span>
        {current.port > 0 && <span className="run-meta mono">{portLabel(current.port)}</span>}
        <span className="run-meta">{stopping ? 'stopping…' : elapsed(state.startedAtUnixMillis, now)}</span>
        {live.length > 1 && <span className="run-count" title={`${live.length} actions running`}>+{live.length - 1}</span>}
      </button>
      <button type="button" className="run-segment run-stop" disabled={stopping || busy.has(current.key)} aria-label={`Stop ${name(current)}`} title={`Stop ${name(current)}`} onClick={() => void store.stopAction(current)}><span className="stop-square" aria-hidden="true"/></button>
    </>;
  } else if (fallback && !fallback.blocked) {
    primary = <button type="button" className="run-segment run-start" disabled={disabled || busy.has(fallback.key)} aria-label={`Run ${name(fallback)}`} title={`Run ${name(fallback)}: ${fallback.run}`} onClick={() => void store.startAction(fallback, false, actions)}>
      <span className="run-play" aria-hidden="true">▶</span><span className="run-text">{busy.has(fallback.key) ? 'Starting…' : name(fallback)}</span>
    </button>;
  } else if (review) {
    primary = null;
  } else if (fallback) {
    primary = <button type="button" className="run-segment run-start" disabled aria-label={`Run ${name(fallback)}`} title={fallback.blocked}><span className="run-play" aria-hidden="true">▶</span><span className="run-text">{name(fallback)}</span></button>;
  } else {
    // Nothing configured: one quiet button that explains how to add actions.
    return <div className="run-control is-empty">
      <button type="button" className="toolbar-action run-empty" aria-label="Actions" title="No actions yet: add .lazybox/workspace.yaml to a repository, or your own in Settings → Workspaces" onClick={() => showActions()}><Icon name="play"/><span>Run</span></button>
    </div>;
  }

  return <div className="run-control" role="group" aria-label="Run">
    {primary}
    {review && <button type="button" className={`run-segment run-review${primary ? ' is-compact' : ''}`} aria-label={`Review ${review.member}’s scripts`} title={review.approval === 'changed' ? `${review.member}’s scripts changed since you approved them` : `${review.member}’s scripts run only after you approve them`} onClick={() => setReviewing(review.member)}>
      <Icon name="alert" size={14}/>{!primary && <span className="run-text">Review scripts</span>}
    </button>}
    {actions.length > 0 && <ActionMenu label="More actions" className="run-menu" summary={<Icon name="chevron" size={14}/>}>
      {actions.map(action => {
        const running = isLive(action.runState), state = runLabel(action.runState);
        return <div key={action.key} className="run-menu-row">
          <button type="button" className="run-menu-action" disabled={!running && (!!action.blocked || disabled || busy.has(action.key))} title={action.blocked || action.run} aria-label={running ? `Show ${name(action)}, ${state.text}` : `Run ${name(action)}`} onClick={() => running ? (action.url ? preview(action) : showActions(action.key)) : void store.startAction(action, false, actions)}>
            <span className={`run-state-dot tone-${state.tone}`} aria-hidden="true"/>
            <span className="run-menu-name">{name(action)}{action.key === fallback?.key && <small> default</small>}</span>
            <small className="mono">{portLabel(action.port)}</small>
            <small>{running ? state.text : action.blocked ? 'Needs approval' : '▶'}</small>
          </button>
          {running && <>
            <button type="button" className="run-menu-icon" aria-label={`Restart ${name(action)}`} title="Restart" disabled={busy.has(action.key)} onClick={() => void store.startAction(action, true, actions)}><Icon name="refresh" size={13}/></button>
            <button type="button" className="run-menu-icon" aria-label={`Stop ${name(action)}`} title="Stop" disabled={busy.has(action.key)} onClick={() => void store.stopAction(action)}><span className="stop-square" aria-hidden="true"/></button>
          </>}
        </div>;
      })}
      <button type="button" className="run-menu-footer" onClick={() => showActions()}><Icon name="output" size={14}/><span>Show actions and logs</span></button>
      {actions.some(a => a.url) && <button type="button" className="run-menu-footer" onClick={() => onShowPanel('preview')}><Icon name="eye" size={14}/><span>Show preview</span></button>}
    </ActionMenu>}
    {recent && <p className={`run-notice tone-${recent.tone}`} role="status">{recent.text}</p>}
    {reviewing && <ScriptApprovalDialog workspaceId={workspaceId} member={reviewing} onClose={() => setReviewing('')}/>}
  </div>;
}
