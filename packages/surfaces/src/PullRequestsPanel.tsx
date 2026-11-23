import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import type { ItemLink } from '@lazybox/protocol';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { ageLabel } from './attention';
import type { ModuleProps } from './PanelRegistry';
import { agentName, confirmation, groupPulls, needsYou, pullWords, pullReactionKey, statusIcon, statusLabel, statusTone, workspacePulls, type PullTone, type TrackedPull } from './pull-requests-model';
import { usePullActions, useTrackedItems, type PullAction } from './useTrackedPulls';
import './pull-requests.css';

/** The dot or icon of a pull request's state, the same everywhere. */
export function PullStatusIcon({ pull, size = 14 }: { pull: Pick<TrackedPull, 'status'> & { tone?: PullTone }; size?: number }) {
  return <span className={`pr-status is-${pull.tone || statusTone[pull.status]}`} aria-hidden="true"><Icon name={statusIcon[pull.status]} size={size}/></span>;
}

/** Which reaction a pull request's state offers, if any. */
export function reactionFor(pull: TrackedPull): PullAction | undefined {
  return pull.status === 'checks-failed' ? 'send' : pull.status === 'changes-requested' ? 'review' : pull.status === 'ready' ? 'merge' : undefined;
}
export const reactionLabel = (action: PullAction, agent: string) => action === 'send' ? `Send the failures to ${agent}` : action === 'review' ? 'Address the review' : 'Merge';

type Confirming = { key: string; eventKey: string; action: PullAction };

/**
 * Every pull request the workspaces deliver, in three groups — needs you,
 * waiting on others, merged this week — and what conversations mentioned,
 * apart. Keyboard first: j/k move, Enter opens the workspace (at the
 * conversation that delivered it), s sends the failing checks, r addresses
 * the review, m merges, o opens it on GitHub; a write asks first.
 */
export function PullRequestsPanel({ collections, onShowSession }: Pick<ModuleProps, 'collections' | 'onShowSession'>) {
  const tracked = useTrackedItems();
  const { info } = useRunner();
  const actions = usePullActions();
  const [now, setNow] = useState(Date.now);
  const [confirming, setConfirming] = useState<Confirming>();

  const root = useRef<HTMLDivElement>(null);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 30_000); return () => clearInterval(timer); }, []);
  useEffect(() => setNow(Date.now()), [tracked.links]);
  const groups = useMemo(() => groupPulls(tracked.links, now), [tracked.links, now]);
  const all = [...groups.needsYou, ...groups.waiting, ...groups.merged];
  useEffect(() => {
    if (!confirming) return;
    const current = groups.needsYou.find(pull => pullReactionKey(pull) === confirming.eventKey);
    const phase = actions.states.get(confirming.eventKey)?.phase;
    if (!current || phase === 'sending' || phase === 'sent') setConfirming(undefined);
  }, [confirming, groups, actions.states]);
  const workspaceName = (link: ItemLink) => collections.find(c => c.id === link.collectionId)?.name || link.collectionName || 'Deleted workspace';
  const agentOf = (pull: TrackedPull) => agentName(info?.sessions.find(s => s.id === pull.link.sessionId)?.provider);
  const rows = () => [...root.current?.querySelectorAll<HTMLButtonElement>('.pr-row') || []];
  const focused = () => all.find(pull => pull.key === (document.activeElement as HTMLElement | null)?.closest<HTMLElement>('[data-pull]')?.dataset.pull);
  const move = (step: number) => {
    const list = rows(), index = list.indexOf(document.activeElement as HTMLButtonElement);
    list[Math.max(0, Math.min(list.length - 1, index < 0 ? 0 : index + step))]?.focus();
  };
  const open = (pull: TrackedPull) => onShowSession?.({ collectionId: pull.link.collectionId, sessionId: pull.link.sessionId, commandId: '' });
  const ask = (pull: TrackedPull, action: PullAction) => {
    const phase = actions.states.get(pullReactionKey(pull))?.phase;
    if (pull.link.collectionDeleted || phase === 'sent' || phase === 'sending') return;
    setConfirming({ key: pull.key, eventKey: pullReactionKey(pull), action });
  };
  const confirm = async (pull: TrackedPull, action: PullAction) => {
    setConfirming(undefined);
    await actions.run(pull, action);
    // Back to its row only if focus went nowhere when the confirmation closed: a person who moved on
    // while the action ran (another row, another panel) keeps their place. Taking it back made m or
    // Enter on the next row act on this one.
    requestAnimationFrame(() => {
      const active = document.activeElement;
      if (active && active !== document.body && !active.closest(`[data-pull="${CSS.escape(pull.key)}"]`)) return;
      root.current?.querySelector<HTMLElement>(`[data-pull="${CSS.escape(pull.key)}"] .pr-row`)?.focus();
    });
  };
  const keyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey || (event.target as HTMLElement).matches('input,textarea,select')) return;
    if (confirming && event.key === 'Escape') { event.preventDefault(); const key = confirming.key; setConfirming(undefined); requestAnimationFrame(() => root.current?.querySelector<HTMLElement>(`[data-pull="${CSS.escape(key)}"] .pr-row`)?.focus()); return; }
    if (confirming) return;
    const pull = focused();
    if (event.key === 'j' || event.key === 'ArrowDown') { event.preventDefault(); move(1); }
    else if (event.key === 'k' || event.key === 'ArrowUp') { event.preventDefault(); move(-1); }
    else if (pull && event.key === 's' && pull.status === 'checks-failed') { event.preventDefault(); ask(pull, 'send'); }
    else if (pull && event.key === 'r' && pull.status === 'changes-requested') { event.preventDefault(); ask(pull, 'review'); }
    else if (pull && event.key === 'm' && pull.status === 'ready') { event.preventDefault(); ask(pull, 'merge'); }
    else if (pull && event.key === 'o' && pull.url) { event.preventDefault(); window.open(pull.url, '_blank', 'noopener,noreferrer'); }
  };
  const section = (id: string, title: string, list: TrackedPull[], empty: ReactNode) => <section className={`pr-group group-${id}`} aria-labelledby={`pr-group-${id}`}>
    <h3 id={`pr-group-${id}`}>{title}<span>{list.length}</span></h3>
    {list.length ? <ul>{list.map(pull => <PullRow key={pull.key} pull={pull} now={now} workspace={workspaceName(pull.link)} agent={agentOf(pull)} state={actions.states.get(pullReactionKey(pull))}
      confirming={confirming?.eventKey === pullReactionKey(pull) ? confirming.action : undefined}
      onOpen={() => open(pull)} onAsk={action => ask(pull, action)} onConfirm={action => void confirm(pull, action)} onCancel={() => setConfirming(undefined)}/>)}</ul>
      : <p className="pr-empty">{empty}</p>}
  </section>;
  return <div ref={root} className="pull-requests-panel" onKeyDown={keyDown}>
    {tracked.trackingError && <p className="pr-tracking-note" role="status"><Icon name="alert" size={13}/>{tracked.trackingError}</p>}
    {!tracked.loaded ? <p className="pr-empty" role="status">Reading tracked pull requests…</p>
      : tracked.unsupported ? <p className="pr-empty">This Runner does not track pull requests yet.</p>
      : !all.length && !groups.mentioned.length ? <div className="pr-blank"><Icon name="pullRequest" size={26}/><strong>No pull requests yet</strong><p>Pull requests your agents open show here: from <code>gh pr create</code>, from any branch a workspace worked on, or <code>lazybox link URL</code>.</p></div>
      : <>
        {section('needs', 'Needs you', groups.needsYou, 'Nothing needs you.')}
        {section('waiting', 'Waiting on others', groups.waiting, 'Nothing is waiting.')}
        {groups.merged.length > 0 && section('merged', 'Merged this week', groups.merged, '')}
        {groups.mentioned.length > 0 && <details className="pr-mentioned">
          <summary>Mentioned<span>{groups.mentioned.length}</span></summary>
          <p>Named in a conversation; listed, not tracked.</p>
          <ul>{groups.mentioned.map(link => <li key={`${link.collectionId}:${link.item?.ref?.key}`}>
            <a href={link.item?.url} target="_blank" rel="noreferrer">{link.item?.ref?.key}<Icon name="external" size={11}/></a>
            <small>{workspaceName(link)}</small>
          </li>)}</ul>
        </details>}
      </>}
    <footer className="pr-keys" aria-hidden="true"><kbd>j</kbd><kbd>k</kbd> move <kbd>↵</kbd> open <kbd>s</kbd> send failures <kbd>r</kbd> review <kbd>m</kbd> merge <kbd>o</kbd> GitHub</footer>
  </div>;
}

function PullRow({ pull, now, workspace, agent, state, confirming, onOpen, onAsk, onConfirm, onCancel }: {
  pull: TrackedPull; now: number; workspace: string; agent: string; state?: { phase: string; text: string }; confirming?: PullAction;
  onOpen(): void; onAsk(action: PullAction): void; onConfirm(action: PullAction): void; onCancel(): void;
}) {
  const confirmButton = useRef<HTMLButtonElement>(null);
  useEffect(() => { if (confirming) confirmButton.current?.focus(); }, [confirming]);
  const busy = state?.phase === 'sending';
  const note = state && ['manual','sent','error'].includes(state.phase) ? {ok: state.phase !== 'error', text: state.text} : undefined;
  const reaction = pull.link.collectionDeleted || state?.phase === 'sent' ? undefined : reactionFor(pull);
  return <li className="pr-item" data-pull={pull.key} data-status={pull.status}>
    <button className="pr-row" onClick={onOpen} title={`Open ${workspace}${pull.link.sessionId ? ' at the conversation that delivered it' : ''}`}>
      <PullStatusIcon pull={pull}/>
      <span className="pr-main">
        <span className="pr-title"><strong>{pull.repository}#{pull.number}</strong>{pull.title && <span>{pull.title}</span>}</span>
        <span className="pr-meta"><em>{needsYou.has(pull.status) || pull.status === 'checks-running' || pull.status === 'waiting' ? pull.detail : statusLabel[pull.status]}</em><span>{workspace}</span>{pull.link.source && <span>{sourceWords[pull.link.source] || pull.link.source}</span>}</span>
      </span>
      <time className="pr-age" dateTime={pull.at ? new Date(pull.at).toISOString() : undefined}>{pull.at ? ageLabel(pull.at, now) : ''}</time>
    </button>
    <span className="pr-actions">
      {reaction && <button type="button" className={reaction === 'merge' ? 'primary' : ''} disabled={busy} onClick={() => onAsk(reaction)} title={`${reactionLabel(reaction, agent)} (${reaction === 'send' ? 's' : reaction === 'review' ? 'r' : 'm'})`}>
        <Icon name={reaction === 'merge' ? 'merge' : reaction === 'review' ? 'review' : 'send'} size={13}/><span>{reaction === 'send' ? 'Send failures' : reaction === 'review' ? 'Address review' : 'Merge'}</span>
      </button>}
      {pull.url && <a className="icon-button" href={pull.url} target="_blank" rel="noreferrer" aria-label={`Open ${pull.repository}#${pull.number} on GitHub`} title="Open on GitHub (o)"><Icon name="external" size={13}/></a>}
    </span>
    {confirming && <div className="pr-confirm" role="group" aria-label={`Confirm ${confirming === 'merge' ? 'merge' : confirming === 'send' ? 'sending the failures' : 'addressing the review'}`}>
      <span>{confirmation(pull, confirming, agent)}</span>
      <button ref={confirmButton} type="button" className="primary" onClick={() => onConfirm(confirming)}>{confirming === 'merge' ? 'Merge' : 'Send'}</button>
      <button type="button" onClick={onCancel}>Cancel</button>
    </div>}
    {busy && <p className="pr-note" role="status">{reactionWorking}</p>}
    {!busy && note && <p className={`pr-note${note.ok ? '' : ' is-error'}`} role={note.ok ? 'status' : 'alert'}>{note.text}</p>}
  </li>;
}

const reactionWorking = 'Working…';
const sourceWords: Record<string, string> = { person: 'you opened it', branch: 'from a branch', 'agent-output': 'the agent opened it', cli: 'linked by the agent', 'started-from': 'started from it' };

/**
 * The workspace header's pull requests: a pill each (open ones first, by
 * urgency), with its checks dot. A pill opens the PR loop when the pull
 * request is its repository's current branch, else the pull request itself.
 */
export function PullRequestPills({ workspaceId, branches, onShowChanges, onShowPanel }: { workspaceId: string; branches: ReadonlyMap<string, string>; onShowChanges(): void; onShowPanel(): void }) {
  const tracked = useTrackedItems();
  const pulls = useMemo(() => workspacePullsOf(tracked.links, workspaceId), [tracked.links, workspaceId]);
  if (!workspaceId || !pulls.length) return null;
  const shown = pulls.slice(0, 3);
  return <div className="pr-pills" role="group" aria-label="Pull requests of this workspace">
    {shown.map(pull => {
      const loop = !!pull.link.member && branches.get(pull.link.member) === pull.link.item?.pullRequest?.head;
      const state = pull.status === 'waiting' ? pull.detail : `${statusLabel[pull.status]}${pull.status === 'checks-failed' ? `: ${pull.detail}` : ''}`;
      const title = `${pull.repository}#${pull.number}${pull.title ? ` · ${pull.title}` : ''}\n${state}\n${loop ? 'Open the PR loop in Changes; ↗ opens it on GitHub' : 'Open on GitHub'}`;
      // One waiting on a required review says so: it is nobody's failure, and not ready either.
      const note = pull.status === 'waiting' && pull.link.item?.pullRequest?.review === 'review_required' ? <span className="pr-pill-note">review required</span> : null;
      const body = <><i className={`pr-dot is-${pull.tone}`}/>#{pull.number}{note}</>;
      // The branch's own pull request opens the PR loop, and still offers GitHub.
      return loop
        ? <span key={pull.key} className={`pr-pill is-split is-${pull.tone}`}>
          <button type="button" title={title} aria-label={`Pull request #${pull.number}, ${pullWords(pull)}: open the PR loop`} onClick={onShowChanges}>{body}</button>
          {pull.url && <a className="pr-pill-out" href={pull.url} target="_blank" rel="noreferrer" title="Open on GitHub" aria-label={`Open #${pull.number} on GitHub`}><Icon name="external" size={11}/></a>}
        </span>
        : <a key={pull.key} className={`pr-pill is-${pull.tone}`} href={pull.url} target="_blank" rel="noreferrer" title={title} aria-label={`Pull request #${pull.number}, ${pullWords(pull)}: open on GitHub`}>{body}</a>;
    })}
    {pulls.length > shown.length && <button type="button" className="pr-pill is-more" title="Show every pull request" aria-label={`${pulls.length - shown.length} more pull requests: show them all`} onClick={onShowPanel}>+{pulls.length - shown.length}</button>}
  </div>;
}

function workspacePullsOf(links: readonly ItemLink[], workspaceId: string) {
  return workspacePulls(links, workspaceId, Date.now());
}
