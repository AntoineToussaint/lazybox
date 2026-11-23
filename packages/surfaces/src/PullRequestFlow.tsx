import { conversationName as nameOf } from './conversation-model';
import { nudgeGitSummaries } from './useGitSummaries';
import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import type { PullRequestCheck, PullRequestStatus, RepositoryChanges, Session } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { conversationReader } from './transcript-cache';
import { checkState, checksSummary, commitPaths, defaultCommitMessage, defaultPullRequest, draftRequest, extractDraft, failingChecks, failingChecksMessage, mergeBlocker, nextStep, splitPullRequestDraft, type DraftKind } from './pull-request-model';
import './pull-request.css';
import type { ReviewLauncher } from './review-client';
import { ReviewWithButton } from './ReviewLauncher';
import { humanError } from './human-error';

/** The PR loop for one workspace repository: commit, push, open a pull
 * request, follow its checks and reviews, send failures to the agent, merge.
 * Every write is an explicit click; the Runner owns Git and GitHub. */

/** A failure in words (human-error.ts): the Runner's own, never the Connect wrapping. */
export const errorText = humanError;
const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;
const submitOnModEnter = (submit: () => void) => (event: KeyboardEvent) => { if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) { event.preventDefault(); submit(); } };
/** Opens a modal with the caret at the end of its main field, ready to edit. */
const openWithFocus = (dialog: HTMLDialogElement | null) => {
  dialog?.showModal();
  const field = dialog?.querySelector<HTMLInputElement | HTMLTextAreaElement>('[data-autofocus]');
  if (field) { field.focus(); field.setSelectionRange(field.value.length, field.value.length); }
};

/** The conversation that did the work: the page's selection, else the
 * workspace's most recent open coding conversation. */
export function useWorkConversation(workspaceId: string, sessionId: string) {
  const { api } = useRunner();
  const [loaded, setLoaded] = useState<{ api: typeof api; workspaceId: string; sessionId: string; conversation?: Session }>();
  useEffect(() => {
    const control = new AbortController();
    const load = async () => {
      if (sessionId) {
        let session = (await api.getSession({ sessionId }, { signal: control.signal, timeoutMs: 10000 })).session;
        if (session?.successorExecutionId) session = (await api.getSession({ sessionId: session.successorExecutionId }, { signal: control.signal, timeoutMs: 10000 })).session || session;
        return session;
      }
      const history = await api.listHistory({ collectionId: workspaceId, limit: 20 }, { signal: control.signal, timeoutMs: 10000 });
      const coding = history.sessions.filter(session => !session.purpose || session.purpose === 'coding');
      return coding.find(session => !session.closed) || coding[0];
    };
    void load().then(value => { if (!control.signal.aborted) setLoaded({ api, workspaceId, sessionId, conversation: value }); }, () => { if (!control.signal.aborted) setLoaded(undefined); });
    return () => control.abort();
  }, [api, workspaceId, sessionId]);
  return loaded?.api === api && loaded.workspaceId === workspaceId && loaded.sessionId === sessionId ? loaded.conversation : undefined;
}

const conversationName = (session?: Session) => session ? `“${nameOf(session, 'this conversation')}”` : '';

/** Asks the conversation's agent to write a draft and waits for its fenced reply. */
function useAgentDraft(conversation?: Session) {
  const { api } = useRunner();
  const [waiting, setWaiting] = useState(false);
  const [error, setError] = useState('');
  const cancel = useRef<() => void>(undefined);
  useEffect(() => () => cancel.current?.(), []);
  const available = !!conversation && !conversation.closed && !conversation.fault;
  const unavailableReason = !conversation ? 'No conversation in this workspace yet' : conversation.closed ? 'The conversation has ended; continue it to ask its agent' : conversation.fault ? 'The conversation is in an error state' : '';
  const ask = (kind: DraftKind, member: string, subjects: string[]) => new Promise<string | undefined>((resolve, reject) => {
    if (!conversation || !available) { resolve(undefined); return; }
    const nonce = crypto.randomUUID().slice(0, 8), text = draftRequest(kind, member, subjects, nonce);
    const reader = conversationReader(api, conversation.id);
    let settled = false, unsubscribe = () => {};
    const timer = setTimeout(() => finish(undefined, new Error('The agent did not reply with a draft within 3 minutes; keep this text or write your own.')), 180000);
    const finish = (value?: string, failure?: Error) => {
      if (settled) return; settled = true; clearTimeout(timer); unsubscribe(); cancel.current = undefined; setWaiting(false);
      if (failure) { setError(failure.message); reject(failure); } else resolve(value);
    };
    const check = () => {
      const messages = reader.getSnapshot().messages;
      const start = messages.findIndex(message => message.type === 'operation_accepted' && message.text.includes(`[draft ${nonce}]`));
      if (start < 0) return;
      for (const message of messages.slice(start + 1)) {
        if (message.type !== 'message_delta') continue;
        const draft = extractDraft(message.text, kind);
        if (draft) { finish(draft); return; }
      }
    };
    unsubscribe = reader.subscribe(check);
    cancel.current = () => finish(undefined);
    setWaiting(true); setError('');
    api.apply({ sessionId: conversation.id, operationId: crypto.randomUUID(), operation: { case: 'message', value: { text, attachments: [] } } }, { timeoutMs: 30000 }).then(check, failure => finish(undefined, new Error(errorText(failure))));
  });
  return { ask, waiting, error, available, unavailableReason, stop: () => cancel.current?.() };
}

function AskAgentButton({ draft, onAsk }: { draft: ReturnType<typeof useAgentDraft>; onAsk(): void }) {
  if (draft.waiting) return <span className="ship-agent-waiting" role="status"><span className="ship-spinner" aria-hidden="true"/>Waiting for the agent…<button type="button" className="ship-link" onClick={draft.stop}>Stop waiting</button></span>;
  return <button type="button" className="ship-ask" disabled={!draft.available} title={draft.available ? 'Sends a short request to the conversation; its reply fills this box' : draft.unavailableReason} onClick={onAsk}><Icon name="sparkle" size={14}/>Ask the agent to write it</button>;
}

function CommitDialog({ workspaceId, repo, conversation, onClose, onDone }: { workspaceId: string; repo: RepositoryChanges; conversation?: Session; onClose(): void; onDone(notice: string): void }) {
  const { api } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const [message, setMessage] = useState(() => defaultCommitMessage(repo, conversation?.title));
  const [selected, setSelected] = useState(() => new Set(repo.files.map(file => file.path)));
  const [busy, setBusy] = useState(''), [error, setError] = useState(''), [drafted, setDrafted] = useState(false);
  const draft = useAgentDraft(conversation);
  useEffect(() => openWithFocus(dialog.current), []);
  const all = selected.size === repo.files.length;
  const submit = async (push: boolean) => {
    if (busy || !message.trim() || !selected.size) return;
    setError(''); setBusy('Committing…');
    try {
      const paths = all ? [] : commitPaths(repo.files.filter(file => selected.has(file.path)));
      const result = await api.commitWorkspaceChanges({ collectionId: workspaceId, member: repo.member, message, paths }, { timeoutMs: 150000 });
      let notice = `Committed ${plural(result.files, 'file')} · ${result.commit.slice(0, 8)} ${result.subject}`;
      if (push) { setBusy('Pushing…'); const pushed = await api.pushWorkspaceBranch({ collectionId: workspaceId, member: repo.member }, { timeoutMs: 360000 }); notice += ` · pushed to ${pushed.upstream}`; }
      onDone(notice);
    } catch (e) { setError(errorText(e)); setBusy(''); }
  };
  const ask = () => void draft.ask('commit', repo.member, repo.files.filter(file => selected.has(file.path)).map(file => file.path)).then(value => { if (value) { setMessage(value); setDrafted(true); } }, () => {});
  return <DraggableDialog ref={dialog} className="ship-dialog" aria-label={`Commit changes in ${repo.member}`} onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader><h2><Icon name="commit"/>Commit to <code>{repo.branch}</code></h2><button type="button" disabled={!!busy} aria-label="Close commit dialog" onClick={onClose}>×</button></DialogHeader>
    <fieldset className="ship-files" disabled={!!busy}>
      <legend><label><input type="checkbox" checked={all} ref={node => { if (node) node.indeterminate = !all && selected.size > 0; }} onChange={event => setSelected(event.target.checked ? new Set(repo.files.map(file => file.path)) : new Set())}/>{repo.member} · {selected.size} of {plural(repo.files.length, 'changed file')}</label></legend>
      <div>{repo.files.map(file => <label key={file.path}><input type="checkbox" checked={selected.has(file.path)} onChange={event => { const next = new Set(selected); if (event.target.checked) next.add(file.path); else next.delete(file.path); setSelected(next); }}/><code>{file.indexStatus + file.worktreeStatus}</code><span>{file.path}</span></label>)}</div>
    </fieldset>
    <label className="ship-field">Message<textarea data-autofocus value={message} rows={5} disabled={!!busy} onChange={event => { setMessage(event.target.value); setDrafted(false); }} onKeyDown={submitOnModEnter(() => void submit(false))} placeholder="Summarize the change in one line, then explain why"/></label>
    <div className="ship-assist"><AskAgentButton draft={draft} onAsk={ask}/>{drafted && <small>Written by the agent in {conversationName(conversation)} — edit as needed.</small>}{draft.error && <small className="ship-warning">{draft.error}</small>}</div>
    {error && <p role="alert" className="ship-error">{error}</p>}
    <footer><small>{busy || (navigator.platform.startsWith('Mac') ? '⌘↩ commits' : 'Ctrl+Enter commits')}</small><button type="button" disabled={!!busy} onClick={onClose}>Cancel</button><button type="button" disabled={!!busy || !message.trim() || !selected.size} onClick={() => void submit(true)}>Commit &amp; push</button><button type="button" className="primary" disabled={!!busy || !message.trim() || !selected.size} onClick={() => void submit(false)}>Commit</button></footer>
  </DraggableDialog>;
}

function CreatePullRequestDialog({ workspaceId, repo, conversation, defaultBranch, onClose, onCreated }: { workspaceId: string; repo: RepositoryChanges; conversation?: Session; defaultBranch: string; onClose(): void; onCreated(pr: PullRequestStatus, notice: string): void }) {
  const { api } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const initial = defaultPullRequest(repo, conversation?.title);
  const [title, setTitle] = useState(initial.title), [body, setBody] = useState(initial.body);
  const bases = [...new Set([defaultBranch, ...repo.branches.map(name => name.replace(/^origin\//, ''))].filter(name => name && name !== repo.branch && name !== 'HEAD'))];
  const [base, setBase] = useState(defaultBranch || bases[0] || '');
  const [draftPr, setDraftPr] = useState(false);
  const [busy, setBusy] = useState(''), [error, setError] = useState(''), [drafted, setDrafted] = useState(false);
  const draft = useAgentDraft(conversation);
  useEffect(() => openWithFocus(dialog.current), []);
  const needsPush = repo.ahead > 0 || !repo.upstream;
  const submit = async () => {
    if (busy || !title.trim()) return;
    setError('');
    try {
      if (needsPush) { setBusy(`Pushing ${plural(repo.ahead, 'commit')}…`); await api.pushWorkspaceBranch({ collectionId: workspaceId, member: repo.member }, { timeoutMs: 360000 }); }
      setBusy('Opening the pull request…');
      const pr = await api.createPullRequest({ collectionId: workspaceId, member: repo.member, title, body, base: base === defaultBranch ? '' : base, draft: draftPr }, { timeoutMs: 60000 });
      onCreated(pr, `Opened ${draftPr ? 'draft ' : ''}pull request #${pr.number}`);
    } catch (e) { setError(errorText(e)); setBusy(''); }
  };
  const ask = () => void draft.ask('pr', repo.member, repo.commits.map(commit => commit.subject)).then(value => { if (value) { const split = splitPullRequestDraft(value); setTitle(split.title || title); setBody(split.body); setDrafted(true); } }, () => {});
  return <DraggableDialog ref={dialog} className="ship-dialog" aria-label={`Create pull request for ${repo.member}`} onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader><h2><Icon name="pullRequest"/>Create pull request</h2><button type="button" disabled={!!busy} aria-label="Close pull request dialog" onClick={onClose}>×</button></DialogHeader>
    <p className="ship-route"><code>{repo.branch}</code><span aria-hidden="true">→</span><label className="ship-base">into<select aria-label="Base branch" value={base} disabled={!!busy} onChange={event => setBase(event.target.value)}>{bases.map(name => <option key={name} value={name}>{name}{name === defaultBranch ? ' (default)' : ''}</option>)}</select></label><small>{repo.githubRepository}</small></p>
    <label className="ship-field">Title<input data-autofocus value={title} disabled={!!busy} onChange={event => { setTitle(event.target.value); setDrafted(false); }} onKeyDown={submitOnModEnter(() => void submit())}/></label>
    <label className="ship-field">Description<textarea value={body} rows={9} disabled={!!busy} onChange={event => { setBody(event.target.value); setDrafted(false); }} onKeyDown={submitOnModEnter(() => void submit())} placeholder="What changed, why, and how it was tested"/></label>
    <div className="ship-assist"><AskAgentButton draft={draft} onAsk={ask}/>{drafted && <small>Written by the agent in {conversationName(conversation)} — edit as needed.</small>}{draft.error && <small className="ship-warning">{draft.error}</small>}</div>
    <label className="ship-toggle"><input type="checkbox" checked={draftPr} disabled={!!busy} onChange={event => setDraftPr(event.target.checked)}/>Open as a draft</label>
    <ul className="ship-notes">
      {needsPush && <li><Icon name="push" size={13}/>Pushes {repo.upstream ? plural(repo.ahead, 'new commit') : `the branch (${plural(repo.ahead, 'commit')})`} to origin first.</li>}
      {repo.files.length > 0 && <li className="ship-warning"><Icon name="commit" size={13}/>{plural(repo.files.length, 'uncommitted file')} will not be included; commit {repo.files.length === 1 ? 'it' : 'them'} first to include {repo.files.length === 1 ? 'it' : 'them'}.</li>}
    </ul>
    {error && <p role="alert" className="ship-error">{error}</p>}
    <footer><small>{busy || (navigator.platform.startsWith('Mac') ? '⌘↩ creates' : 'Ctrl+Enter creates')}</small><button type="button" disabled={!!busy} onClick={onClose}>Cancel</button><button type="button" className="primary" disabled={!!busy || !title.trim() || !base} onClick={() => void submit()}>{draftPr ? 'Create draft pull request' : 'Create pull request'}</button></footer>
  </DraggableDialog>;
}

const CHECK_ICONS = { failed: 'cross', running: 'pending', passed: 'check', skipped: 'skip' } as const;
function CheckRow({ check }: { check: PullRequestCheck }) {
  const state = checkState(check);
  return <li className={`ship-check is-${state}`}><Icon name={CHECK_ICONS[state]} size={13}/><span>{check.name}{check.summary && <small>{check.summary}</small>}</span>{check.url ? <a href={check.url} target="_blank" rel="noreferrer" aria-label={`Open ${check.name} on GitHub`}>Logs<Icon name="external" size={11}/></a> : <em>{state === 'running' ? 'running' : ''}</em>}</li>;
}

const REVIEW_LABEL: Record<string, string> = { approved: 'Approved', changes_requested: 'Changes requested', review_required: 'Review required' };
const MERGE_METHODS = [['squash', 'Squash and merge'], ['merge', 'Create a merge commit'], ['rebase', 'Rebase and merge']] as const;
const readMethod = () => { try { return localStorage.getItem('lazybox:merge-method') || 'squash'; } catch { return 'squash'; } };

function PullRequestCard({ workspaceId, member, pr, fix, launcher, onSendChecks, onMerged, onRefresh }: { workspaceId: string; member: string; pr: PullRequestStatus; fix: boolean; launcher?: ReviewLauncher; onSendChecks(): void; onMerged(pr: PullRequestStatus): void; onRefresh(): void }) {
  const { api } = useRunner();
  const [method, setMethod] = useState(readMethod);
  const [confirming, setConfirming] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState('');
  const failing = failingChecks(pr), blocker = mergeBlocker(pr);
  const state = pr.state === 'open' && pr.draft ? 'draft' : pr.state;
  const merge = async () => {
    setBusy(true); setError('');
    try { onMerged(await api.mergePullRequest({ collectionId: workspaceId, member, number: pr.number, method, expectedHead: pr.headSha }, { timeoutMs: 60000 })); setConfirming(false); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };
  // Behind a base its repository requires it to match: GitHub merges the base into the branch
  // (at the head shown here), then its checks run again before it can merge.
  const behind = pr.state === 'open' && pr.mergeableState === 'behind' && !failing.length;
  const update = async () => {
    setBusy(true); setError('');
    try { await api.updatePullRequestBranch({ collectionId: workspaceId, number: pr.number, expectedHead: pr.headSha, item: { provider: 'github', key: `${pr.githubRepository}#${pr.number}` } }, { timeoutMs: 60000 }); onRefresh(); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };
  return <section className={`ship-pr is-${state}`} aria-label={`Pull request #${pr.number}`}>
    <header><Icon name={pr.state === 'merged' ? 'merge' : 'pullRequest'} size={15}/><a href={pr.url} target="_blank" rel="noreferrer" title="Open on GitHub"><strong>#{String(pr.number)}</strong> {pr.title}<Icon name="external" size={11}/></a><button type="button" className="ship-icon" aria-label="Refresh pull request status" title="Refresh checks and reviews" onClick={onRefresh}><Icon name="refresh" size={13}/></button></header>
    <p className="ship-pr-meta"><span className={`ship-chip is-${state}`}>{{ open: 'Open', draft: 'Draft', merged: 'Merged', closed: 'Closed' }[state] || state}</span>
      {pr.state === 'open' && REVIEW_LABEL[pr.reviewDecision] && <span className={`ship-chip review-${pr.reviewDecision}`}>{REVIEW_LABEL[pr.reviewDecision]}</span>}
      <small>{pr.state === 'merged' ? `into ${pr.base}${pr.mergeCommit ? ` · ${pr.mergeCommit.slice(0, 8)}` : ''}` : `${pr.branch} → ${pr.base}`}</small></p>
    {pr.state === 'open' && <details className="ship-checks" open={failing.length > 0}>
      <summary><span className={`ship-checks-state is-${pr.checksState}`}><Icon name={pr.checksState === 'failure' ? 'cross' : pr.checksState === 'pending' ? 'pending' : pr.checksState === 'success' ? 'check' : 'skip'} size={13}/></span>Checks · {checksSummary(pr)}</summary>
      {pr.checks.length > 0 ? <ul>{pr.checks.map((check, index) => <CheckRow key={check.name + index} check={check}/>)}</ul> : <p className="ship-muted">No CI checks reported for this commit.</p>}
    </details>}
    {pr.state === 'open' && <div className="ship-pr-actions">
      {launcher && <ReviewWithButton launcher={launcher} scope={{ scope: 'pr', member }} label="Review the PR with"/>}
      {failing.length > 0 && <button type="button" className={fix ? 'primary' : ''} onClick={onSendChecks} title="Puts the failing checks and their log links in the conversation's message box for you to send"><Icon name="chat" size={14}/>Send {failing.length === 1 ? 'failing check' : `${failing.length} failing checks`} to the agent</button>}
      {behind && !confirming && <button type="button" className="primary" disabled={busy} onClick={() => void update()} title={`Merge ${pr.base} into this branch on GitHub; its checks run again`}><Icon name="refresh" size={14}/>{busy ? 'Updating…' : 'Update branch'}</button>}
      {confirming ? <span className="ship-confirm" role="group" aria-label="Confirm merge">
        <span>{MERGE_METHODS.find(([value]) => value === method)?.[1]} #{String(pr.number)} into {pr.base}?</span>
        <button type="button" className="primary" disabled={busy} autoFocus onClick={() => void merge()}>{busy ? 'Merging…' : 'Confirm merge'}</button><button type="button" disabled={busy} onClick={() => setConfirming(false)}>Cancel</button>
      </span> : <span className="ship-merge">
        <select aria-label="Merge method" value={method} onChange={event => { setMethod(event.target.value); try { localStorage.setItem('lazybox:merge-method', event.target.value); } catch { /* per-viewer convenience only */ } }}>{MERGE_METHODS.map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
        <button type="button" className={!blocker && !fix ? 'primary' : ''} disabled={!!blocker} title={blocker || 'Merge on GitHub; refused if the branch moved since this view'} onClick={() => setConfirming(true)}><Icon name="merge" size={14}/>Merge</button>
      </span>}
    </div>}
    {pr.state === 'open' && blocker && !confirming && <p className="ship-muted ship-blocker">{blocker}{pr.mergeableState === 'blocked' && <> · <a href={pr.url} target="_blank" rel="noreferrer">details on GitHub</a></>}</p>}
    {error && <p role="alert" className="ship-error">{error}</p>}
  </section>;
}

/** The action bar and pull request card above one repository's file list. */
export function RepositoryShip({ workspaceId, repo, conversation, launcher, onChanged: changed, onSendToAgent }: { workspaceId: string; repo: RepositoryChanges; conversation?: Session; launcher?: ReviewLauncher; onChanged(): void; onSendToAgent(text: string): void }) {
  const { api } = useRunner();
  // A commit, push or merge also changes the sidebar's git line.
  const onChanged = () => { changed(); nudgeGitSummaries(api); };
  const [pr, setPr] = useState<PullRequestStatus>(), [prError, setPrError] = useState(''), [unsupported, setUnsupported] = useState(false), [prRevision, setPrRevision] = useState(0);
  const [dialog, setDialog] = useState<'commit' | 'create' | ''>(''), [pushing, setPushing] = useState(false), [notice, setNotice] = useState(''), [error, setError] = useState('');
  useEffect(() => {
    if (!repo.githubRepository || unsupported) { setPr(undefined); return; }
    const control = new AbortController(); let timer: ReturnType<typeof setTimeout> | undefined;
    let first = true;
    const load = async () => {
      // Each read is several GitHub API calls: none while the window is hidden.
      if (!first && document.hidden) { timer = setTimeout(() => void load(), 30000); return; }
      first = false;
      try {
        const value = await api.getPullRequest({ collectionId: workspaceId, member: repo.member }, { signal: control.signal, timeoutMs: 30000 });
        if (control.signal.aborted) return;
        setPr(value); setPrError('');
        // Follow running checks closely; settle down once nothing moves.
        const live = value.found && value.state === 'open' && (value.checksState === 'pending' || !['clean', 'unstable', 'dirty', 'blocked', 'behind', 'draft', 'has_hooks'].includes(value.mergeableState));
        if (value.found && value.state === 'open') timer = setTimeout(() => void load(), live ? 30000 : 120000);
      } catch (e) {
        if (control.signal.aborted) return;
        if (ConnectError.from(e).code === Code.Unimplemented) { setUnsupported(true); return; }
        setPrError(errorText(e)); timer = setTimeout(() => void load(), 60000);
      }
    };
    void load();
    return () => { control.abort(); clearTimeout(timer); };
  }, [api, workspaceId, repo.member, repo.githubRepository, repo.head, repo.upstream, prRevision, unsupported]);
  useEffect(() => { if (!notice) return; const timer = setTimeout(() => setNotice(''), 8000); return () => clearTimeout(timer); }, [notice]);
  const step = nextStep(repo, pr);
  const prOpen = !!pr?.found && pr.state === 'open';
  const canCreate = !!repo.githubRepository && !!pr && !pr.unavailable && !prOpen && pr.state !== 'merged' && repo.commits.length > 0;
  const push = async () => {
    setPushing(true); setError('');
    try { const pushed = await api.pushWorkspaceBranch({ collectionId: workspaceId, member: repo.member }, { timeoutMs: 360000 }); setNotice(`Pushed ${plural(repo.ahead, 'commit')} to ${pushed.upstream}`); onChanged(); setPrRevision(value => value + 1); }
    catch (e) { setError(errorText(e)); }
    finally { setPushing(false); }
  };
  const sendChecks = () => { if (pr) onSendToAgent(failingChecksMessage(repo.member, pr)); };
  const button = (kind: typeof step, content: ReactNode, props: { onClick(): void; disabled?: boolean; title: string }) => <button type="button" className={step === kind ? 'primary' : ''} {...props}>{content}</button>;
  const quiet = !repo.files.length && !repo.ahead && !canCreate && !prOpen;
  if (repo.error || repo.branch === 'HEAD') return null;
  return <div className="ship">
    {!quiet && <div className="ship-actions" role="toolbar" aria-label={`Ship ${repo.member}`}>
      {repo.files.length > 0 && button('commit', <><Icon name="commit" size={14}/>Commit {repo.files.length}…</>, { onClick: () => setDialog('commit'), title: `Commit ${plural(repo.files.length, 'uncommitted file')}` })}
      {repo.ahead > 0 && button('push', <><Icon name="push" size={14}/>{pushing ? 'Pushing…' : repo.upstream ? `Push ${repo.ahead}` : 'Publish branch'}</>, { onClick: () => void push(), disabled: pushing, title: repo.upstream ? `Push ${plural(repo.ahead, 'commit')} to ${repo.upstream}` : `Push ${repo.branch} to origin for the first time (${plural(repo.ahead, 'commit')})` })}
      {canCreate && button('create', <><Icon name="pullRequest" size={14}/>Create PR…</>, { onClick: () => setDialog('create'), title: `Open a pull request for ${repo.branch} on ${repo.githubRepository}` })}
    </div>}
    {quiet && repo.upstream && !pr?.found && <p className="ship-muted"><Icon name="check" size={12}/>Up to date with {repo.upstream}</p>}
    {repo.githubRepository && pr?.unavailable && <p className="ship-muted">{pr.unavailable}</p>}
    {prError && <p className="ship-muted ship-warning" role="status">Pull request status unavailable: {prError}</p>}
    {pr?.found && <PullRequestCard workspaceId={workspaceId} member={repo.member} pr={pr} fix={step === "fix"} launcher={launcher} onSendChecks={sendChecks} onRefresh={() => setPrRevision(value => value + 1)} onMerged={value => { setPr(value); setNotice(`Merged #${value.number} into ${value.base}`); onChanged(); }}/>}
    {(notice || error) && <p role={error ? 'alert' : 'status'} className={error ? 'ship-error' : 'ship-notice'}>{error || notice}</p>}
    {dialog === 'commit' && <CommitDialog workspaceId={workspaceId} repo={repo} conversation={conversation} onClose={() => setDialog('')} onDone={text => { setDialog(''); setNotice(text); onChanged(); setPrRevision(value => value + 1); }}/>}
    {dialog === 'create' && <CreatePullRequestDialog workspaceId={workspaceId} repo={repo} conversation={conversation} defaultBranch={pr?.defaultBranch || repo.defaultBranch.replace(/^origin\//, '')} onClose={() => setDialog('')} onCreated={(value, text) => { setDialog(''); setPr(value); setNotice(text); onChanged(); }}/>}
  </div>;
}
