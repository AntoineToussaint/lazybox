import { conversationName } from './conversation-model';
import { useFileOpener } from './file-links';
import { useEffect, useId, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { ChangedFile, RepositoryChanges, ReviewComment, Session, TurnCheckpoint, WorkspaceDiff } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { RepositoryShip, useWorkConversation } from './PullRequestFlow';
import { composeInConversation } from './compose';
import type { ViewTarget } from './workspace-model';
import { ReviewDiff, type ReviewFocus } from './ReviewDiff';
import { ReviewTray } from './ReviewTray';
import { useWorkspaceReview } from './useWorkspaceReview';
import { isPending, isProposed, loopNeedsYou, type ReviewView } from './review-model';
import { Icon } from './Icons';
import { contextMenu } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import { isMac } from './workbench';
import { TurnChangesView } from './TurnChangesView';
import { parseTurnCompare, turnCompare, useRequestedTurn, useTurnCheckpoints } from './turn-checkpoints';
import { contentKey, type DiffMode } from './diff-layout';
import { useReviewLauncher, useReviewTrayRequests, type ReviewLauncher, type ReviewScope } from './review-client';
import { reviewMenuItems, ReviewWithButton } from './ReviewLauncher';
import { diffKeyTarget, fileKey, moveToHunk, useDiffMode, useNarrow, useViewedFiles } from './useDiffPreferences';
import { Resizer } from './Resizer';
import { SPLITS } from './resizable-model';

type Selection = { member: string; path: string; committed: boolean };

// Compare choices: uncommitted work only, the commit the workspace started
// from, or any branch (default branch first), compared from their merge base.
const UNCOMMITTED = ':uncommitted', START = ':start';
// A turn (`:turn:<session>:<n>`) is one agent turn's checkpoints, not a ref.
const compareRef = (compare: string) => compare === UNCOMMITTED || compare === START || compare.startsWith(':turn:') ? '' : compare;

const notices = new Map<string, string>();
// Whether the person opened or closed the review tray, per workspace.
const trays = new Map<string, boolean>();
// In a narrow panel the files and the review tray share the left column as
// two tabs; which one is in front, per workspace. Matches review.css.
type Pane = 'files' | 'review';
const panes = new Map<string, Pane>();
const COMPACT_WIDTH = 1101;
const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;
const age = (unix: bigint) => {
  const minutes = Math.max(0, Math.round((Date.now() / 1000 - Number(unix)) / 60));
  return minutes < 60 ? `${minutes}m ago` : minutes < 1440 ? `${Math.round(minutes / 60)}h ago` : `${Math.round(minutes / 1440)}d ago`;
};

const REVIEW_EVENT = 'lazybox-open-review';
/** Opens the review tray of a workspace's Changes panel: now if one is mounted, else when it next opens. */
export function requestReviewTray(workspaceId: string) {
  trays.set(workspaceId, true);
  window.dispatchEvent(new CustomEvent(REVIEW_EVENT, { detail: workspaceId }));
}

/** A workspace-scoped projection; all Git operations belong to the Runner.
 * It shows what the branch committed since the workspace started, then what is
 * still uncommitted: an agent's work is usually in both. Each repository has
 * its PR loop (commit, push, pull request, checks, merge) above its files. */
export function ChangesPanel({ workspaceId, viewState, sessionId = '', onShowSession }: { workspaceId: string; viewState?: ViewStateStore; sessionId?: string; onShowSession?(target: ViewTarget): void }) {
  const { api } = useRunner();
  const conversation = useWorkConversation(workspaceId, sessionId);
  const [notice, setNotice] = useState(() => { const value = notices.get(workspaceId) || ''; notices.delete(workspaceId); return value; });
  // Failing checks and review comments go to the message box of a
  // conversation (the one that did the work, unless the person picks another),
  // opened beside this page, for the person to review and send.
  const sendToAgent = (text: string, to: Session | undefined = conversation) => {
    if (!to) { void navigator.clipboard?.writeText(text).catch(() => {}); setNotice('No conversation in this workspace yet. The failing checks were copied: start one and paste them.'); return; }
    const said = `Added to the message box of “${conversationName(to, 'the conversation')}”. Review it and send.`;
    composeInConversation({ sessionId: to.id, text });
    // Opening the chat beside this panel remounts it; the notice survives that.
    notices.set(workspaceId, said); setNotice(said);
    onShowSession?.({ collectionId: workspaceId, sessionId: to.id, commandId: '' });
  };
  const review = useWorkspaceReview(workspaceId);
  const pending = review.comments.filter(isPending).length + review.comments.filter(isProposed).length;
  const runs = review.review?.runs || [];
  const [trayChoice, setTrayChoice] = useState<boolean | undefined>(() => trays.get(workspaceId));
  // The tray shows itself once there are comments or reviews, unless the person closed it.
  const trayOpen = trayChoice ?? (review.comments.length > 0 || runs.length > 0);
  const [pane, setPaneState] = useState<Pane>(() => panes.get(workspaceId) || 'review');
  const setPane = (value: Pane) => { panes.set(workspaceId, value); setPaneState(value); };
  // Opening the tray brings it to the front of a narrow panel's left column.
  const showTray = (open: boolean) => { trays.set(workspaceId, open); setTrayChoice(open); if (open) setPane('review'); };
  const [panel, setPanel] = useState<HTMLElement | null>(null);
  const columnId = useId();
  const compact = useNarrow(panel, COMPACT_WIDTH);
  // The header's Review button: a tray behind the files comes to the front before it closes.
  const toggleTray = () => trayOpen && compact && pane === 'files' ? setPane('review') : showTray(!trayOpen);
  useEffect(() => { const listener = (event: Event) => { if ((event as CustomEvent<string>).detail === workspaceId) showTray(true); }; window.addEventListener(REVIEW_EVENT, listener); return () => window.removeEventListener(REVIEW_EVENT, listener); }, [workspaceId]);
  // A review's card asked for its findings: the tray opens on them.
  const [focusReview, setFocusReview] = useState('');
  useReviewTrayRequests(workspaceId, request => { showTray(true); setFocusReview(request.reviewId || ''); });
  const [focus, setFocus] = useState<ReviewFocus>();
  const fileLinks = useFileOpener(workspaceId, viewState);
  const [repos, setRepos] = useState<RepositoryChanges[]>([]), [error, setError] = useState(''), [loading, setLoading] = useState(true), [revision, setRevision] = useState(0);
  const [compare, setCompare] = useState<string>(START), [selection, setSelection] = useState<Selection>(), [staged, setStaged] = useState(false), [savedDiff, setDiff] = useState<{ key: string; value: WorkspaceDiff }>(), [diffError, setDiffError] = useState('');
  // One agent turn's changes, asked for from its marker in the conversation.
  const turnView = parseTurnCompare(compare);
  // Review with another agent: the work of the page's conversation (the
  // author, when the page shows its reviewer), on what this page compares.
  const asReviewer = conversation && runs.find(run => run.reviewerSessionId === conversation.id);
  const author = turnView && turnView.sessionId !== (asReviewer?.sourceSessionId || conversation?.id) ? { id: turnView.sessionId, profileId: '', provider: '' }
    : asReviewer ? { id: asReviewer.sourceSessionId, profileId: '', provider: asReviewer.sourceProvider } : conversation;
  const launcher = useReviewLauncher(workspaceId, author);
  const reviewScope: ReviewScope = turnView ? { scope: 'turn', turn: turnView.turn } : { scope: 'changes', compare: compare === UNCOMMITTED ? ':uncommitted' : compare === START ? '' : compare };
  const turns = useTurnCheckpoints(api, sessionId, false, !!sessionId);
  // A new comparison starts empty: the old list is not shown under the new choice, and
  // nothing is opened from it before the new list arrives.
  const changeCompare = (value: string) => { if (value !== compare) { setRepos([]); setLoading(true); } setCompare(value); setSelection(undefined); };
  useRequestedTurn(workspaceId, request => changeCompare(turnCompare(request.sessionId, request.turn)));
  useEffect(() => {
    if (!workspaceId) { setLoading(false); return; }
    const control = new AbortController(); let busy = false;
    const load = async () => {
      if (busy) return; busy = true;
      try { const result = await api.listWorkspaceChanges({ collectionId: workspaceId, compare: compareRef(compare) }, { signal: control.signal, timeoutMs: 20000 }); if (!control.signal.aborted) { setRepos(result.repositories); setError(''); } }
      catch (e) { if (!control.signal.aborted) setError(String(e)); }
      finally { busy = false; if (!control.signal.aborted) setLoading(false); }
    };
    void load(); const timer = setInterval(() => void load(), 10000); return () => { control.abort(); clearInterval(timer); };
  }, [api, workspaceId, revision, compare]);
  const choose = (member: string, file: ChangedFile, committed: boolean) => { setSelection({ member, path: file.path, committed }); setStaged(!committed && file.worktreeStatus === ' ' && file.indexStatus !== '?'); };
  // A file committed (or reverted) since it was chosen leaves its list; choose again.
  useEffect(() => {
    if (!selection) return;
    const repo = repos.find(value => value.member === selection.member);
    if (repo && !(selection.committed ? repo.committed : repo.files).some(file => file.path === selection.path)) setSelection(undefined);
  }, [repos]);
  // Open the first change instead of an empty "select a file" pane.
  useEffect(() => {
    if (selection) return;
    for (const repo of repos) {
      if (compare !== UNCOMMITTED && repo.committed[0]) { choose(repo.member, repo.committed[0], true); return; }
      if (repo.files[0]) { choose(repo.member, repo.files[0], false); return; }
    }
  }, [repos, selection, compare]);
  const diffKey = JSON.stringify([workspaceId, selection, staged, revision, compare]);
  const diff = savedDiff?.key === diffKey ? savedDiff.value : undefined;
  useEffect(() => {
    setDiff(undefined); setDiffError(''); if (!selection) return;
    const control = new AbortController();
    void api.readWorkspaceDiff({ collectionId: workspaceId, member: selection.member, path: selection.path, staged: !selection.committed && staged, committed: selection.committed, compare: compareRef(compare) }, { signal: control.signal, timeoutMs: 20000 })
      .then(value => { if (!control.signal.aborted) setDiff({ key: diffKey, value }); }, error => { if (!control.signal.aborted) setDiffError(String(error)); });
    return () => control.abort();
  }, [api, workspaceId, selection, staged, revision, compare, diffKey]);
  // Show a comment in its diff: the file it was made on, in the list it was
  // made from when the file is still there, else wherever it is changed now.
  const jump = (comment: ReviewComment) => {
    const repo = repos.find(value => value.member === comment.member);
    const inBranch = compare !== UNCOMMITTED && !!repo?.committed.some(file => file.path === comment.path), inWork = !!repo?.files.some(file => file.path === comment.path);
    if (!inBranch && !inWork) { void fileLinks.open({ path: `${comment.member}/${comment.path}`, line: comment.currentStartLine || comment.startLine }, {}, { exact: true }); return; }
    const committed = comment.view === 'branch' ? inBranch : !inWork;
    setSelection({ member: comment.member, path: comment.path, committed }); setStaged(!committed && comment.view === 'staged');
    setFocus(previous => ({ id: comment.id, nonce: (previous?.nonce || 0) + 1 }));
  };
  // Unified or side by side (unified whatever the choice when the pane is too narrow), and files marked viewed.
  const [diffMode, setDiffMode] = useDiffMode(viewState);
  const [diffPane, setDiffPane] = useState<HTMLElement | null>(null);
  const narrow = useNarrow(diffPane);
  const shownMode: DiffMode = narrow ? 'unified' : diffMode;
  const viewed = useViewedFiles(workspaceId, viewState);
  const diffContent = diff && !diff.binary ? contentKey(diff.text) : '';
  const selectedKey = selection ? fileKey(selection.member, selection.path, selection.committed) : '';
  useEffect(() => { if (selectedKey && diffContent) viewed.reconcile(selectedKey, diffContent); }, [selectedKey, diffContent]);
  if (!workspaceId) return <p className="module-empty">Choose a workspace to review its changes.</p>;
  const selectedRepo = selection && repos.find(repo => repo.member === selection.member);
  const showCommitted = compare !== UNCOMMITTED;
  // Every file in the list, in its order: n/p move between hunks, Shift+N/P between these.
  const listed = repos.flatMap(repo => [...(showCommitted && !repo.committedError ? repo.committed.map(file => ({ member: repo.member, file, committed: true })) : []), ...repo.files.map(file => ({ member: repo.member, file, committed: false }))]);
  const listedKeys = listed.map(entry => fileKey(entry.member, entry.file.path, entry.committed));
  const position = listedKeys.indexOf(selectedKey);
  const moveFile = (step: number, unviewed = false) => {
    for (let i = position + step; i >= 0 && i < listed.length; i += step) {
      if (unviewed && viewed.isViewed(listedKeys[i])) continue;
      choose(listed[i].member, listed[i].file, listed[i].committed);
      requestAnimationFrame(() => document.querySelector<HTMLElement>(`.changes-panel nav [data-file-key="${CSS.escape(listedKeys[i])}"]`)?.focus({ preventScroll: false }));
      return;
    }
  };
  const readNewSide = async (member: string, path: string) => {
    const file = await api.readWorkspaceFile({ collectionId: workspaceId, path: `${member}/${path}` }, { timeoutMs: 10000 });
    if (file.directory || file.binary || file.truncated) throw new Error('This file is too large to show its unchanged lines here. Open it to read them.');
    return file.text;
  };
  const markViewed = (key: string, on: boolean) => viewed.set(key, on, key === selectedKey ? diffContent : '');
  const keys = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (!diffKeyTarget(event) || turnView) return;
    const root = event.currentTarget;
    if (event.key === 'n' || event.key === 'p') { event.preventDefault(); moveToHunk(root, event.key === 'n' ? 1 : -1); }
    else if (event.key === 'N' || event.key === 'P') { event.preventDefault(); moveFile(event.key === 'N' ? 1 : -1); }
    else if (event.key === 'v' && selectedKey) {
      event.preventDefault();
      const on = !viewed.isViewed(selectedKey);
      markViewed(selectedKey, on);
      // Marking a file viewed moves on to the next one still to review.
      if (on) moveFile(1, true);
    }
    else if (event.key === 's' && selection && !narrow) { event.preventDefault(); setDiffMode(diffMode === 'split' ? 'unified' : 'split'); }
  };
  const committed = showCommitted ? repos.reduce((sum, repo) => sum + repo.committed.length, 0) : 0, uncommitted = repos.reduce((sum, repo) => sum + repo.files.length, 0);
  const turnPrompt = turnView && turnView.sessionId === sessionId ? turns.byNumber.get(turnView.turn)?.prompt || '' : '';
  const summary = turnView ? `Agent turn ${turnView.turn}${turnPrompt ? ` · “${turnPrompt}”` : ''}` : loading ? 'Reading changes…' : !committed && !uncommitted ? 'No changes yet' : [committed && `${plural(committed, 'file')} changed ${compare === START ? 'on the branch' : 'vs ' + compare}`, uncommitted && `${uncommitted} uncommitted`].filter(Boolean).join(' · ');
  const refresh = () => setRevision(value => value + 1);
  // A changed file, from its row or its diff: open it every way, or copy where it is.
  const fileItems = (member: string, path: string): MenuItem[] => {
    const reference = { path: `${member}/${path}` }, open = (how: Parameters<typeof fileLinks.open>[1]) => void fileLinks.open(reference, how, { exact: true });
    return [
      { label: 'Open file', icon: 'file', run: () => open({}) },
      { label: 'Open file to the side', icon: 'split-right', run: () => open({ metaKey: isMac, ctrlKey: !isMac }) },
      { label: 'Quick look', icon: 'eye', run: () => open({ altKey: true }) },
      { label: 'Reveal in Files', icon: 'target', run: () => open({ reveal: true }) },
      'separator',
      { label: 'Copy path', icon: 'copy', run: () => open({ copy: 'absolute' }) },
      { label: 'Copy relative path', icon: 'copy', run: () => open({ copy: 'relative' }) },
    ];
  };
  const rowItems = (member: string, file: ChangedFile, isCommitted: boolean): MenuItem[] => [
    { label: 'Show diff', icon: 'branch', run: () => choose(member, file, isCommitted) },
    'separator',
    ...fileItems(member, file.path),
    'separator',
    ...reviewMenuItems(launcher, reviewScope, 'the changes'),
    'separator',
    { label: 'Refresh changes', icon: 'refresh', run: refresh },
  ];
  return <section className="changes-panel" aria-label="Workspace changes" onKeyDown={keys} ref={setPanel}>
    <ChangesHeader summary={summary} compare={compare} repos={repos} onCompare={changeCompare} sessionId={sessionId} turns={[...turns.byNumber.values()]} onRefresh={refresh} pending={pending} trayOpen={trayOpen && (!compact || pane === 'review')} onToggleTray={toggleTray} launcher={launcher} reviewScope={reviewScope}/>
    {launcher.error && <p className="error review-start-error" role="alert"><span>{launcher.error}</span><button aria-label="Dismiss" onClick={() => launcher.setError('')}>×</button></p>}
    {turnView ? <TurnChangesView key={compare} sessionId={turnView.sessionId} turn={turnView.turn}/> : <>
    {error && <p className="error" role="alert">{error}</p>}
    {notice && <p className="changes-notice" role="status"><span>{notice}</span><button aria-label="Dismiss notice" onClick={() => setNotice('')}>×</button></p>}
    <div className={`changes-columns${trayOpen ? ' with-review' : ''}`} data-pane={trayOpen ? pane : undefined}>
      {trayOpen && <ColumnTabs id={columnId} pane={pane} onPane={setPane} files={listed.length} pending={pending} needsYou={runs.some(loopNeedsYou)}/>}
      <nav aria-label="Changed files" id={`${columnId}-files`}>
        {repos.map(repo => <RepositoryFiles key={repo.member} workspaceId={workspaceId} repo={repo} compare={compare} selection={selection} conversation={conversation} launcher={launcher} onChoose={choose} onChanged={refresh} onSendToAgent={sendToAgent} isViewed={viewed.isViewed} onViewed={markViewed} menu={rowItems}/>)}
      </nav>
      <Resizer split={SPLITS.changesFiles} label="file list"/>
      <div className="diff-view" ref={setDiffPane} onContextMenu={selection ? contextMenu('diff', () => fileItems(selection.member, selection.path), { member: selection.member, path: selection.path }) : undefined}>
        {selection
          ? <>
            <header>
              <strong>{selection.member}/{selection.path}</strong>
              <button title="Open in the editor · Alt-click for a quick look" onClick={event => void fileLinks.open({ path: `${selection.member}/${selection.path}` }, event, { exact: true })}>Open file</button>
              {!selection.committed && <label><input type="checkbox" checked={staged} onChange={event => setStaged(event.target.checked)}/> Staged</label>}
              <label className="diff-viewed" title="Mark this file reviewed (v) · it is unmarked if its diff changes"><input type="checkbox" checked={viewed.isViewed(selectedKey)} onChange={event => markViewed(selectedKey, event.target.checked)}/> Viewed</label>
              {listed.length > 0 && <span className="changes-viewed-count" role="status">{viewed.count(listedKeys)} of {listed.length} viewed</span>}
              <DiffModeToggle mode={diffMode} narrow={narrow} onMode={setDiffMode}/>
            </header>
            <div className="diff-comparison">{selection.committed ? (compare === START ? 'Workspace start → latest commit' : `${compare} (merge base) → latest commit`) : staged ? 'Last commit → staged' : 'Staged → working tree (uncommitted)'}</div>
            <div className="diff-keys" aria-hidden="true"><kbd>n</kbd> <kbd>p</kbd> hunks · <kbd>⇧N</kbd> <kbd>⇧P</kbd> files · <kbd>v</kbd> viewed · <kbd>s</kbd> split</div>
            <DiffBody diff={diff} error={diffError} empty={selection.committed ? 'branch' : staged ? 'staged' : 'unstaged'}
              review={{ workspaceId, member: selection.member, path: selection.path, view: selection.committed ? 'branch' : staged ? 'staged' : 'uncommitted', base: selection.committed ? selectedRepo?.base || '' : '', focus, mode: shownMode,
                // More unchanged lines come from the working file: only when it is this diff's new side.
                loadFile: (selection.committed ? !selectedRepo?.files.some(file => file.path === selection.path) : !staged) ? () => readNewSide(selection.member, selection.path) : undefined }}/>
          </>
          : <div className="diff-empty"><h3>{loading ? 'Reading changes…' : 'No changes yet'}</h3><p>Nothing has been committed or edited in this workspace since it started.</p></div>}
      </div>
      {trayOpen && <Resizer split={SPLITS.changesReview} label="review tray"/>}
      {trayOpen && <ReviewTray id={`${columnId}-review`} workspaceId={workspaceId} review={review} conversation={conversation} focusReview={focusReview} onJump={jump} onSend={(text, to) => sendToAgent(text, to)} onClose={() => showTray(false)}/>}
    </div></>}
    {fileLinks.element}
  </section>;
}

/** What changed in total, what it is compared with, the review tray, and a refresh. */
function ChangesHeader({ summary, compare, repos, sessionId, turns, onCompare, onRefresh, pending, trayOpen, onToggleTray, launcher, reviewScope }: { summary: string; compare: string; repos: RepositoryChanges[]; sessionId: string; turns: TurnCheckpoint[]; onCompare(compare: string): void; onRefresh(): void; pending: number; trayOpen: boolean; onToggleTray(): void; launcher: ReviewLauncher; reviewScope: ReviewScope }) {
  const turnView = parseTurnCompare(compare);
  // The followed conversation's finished turns, newest first; a turn asked for from another conversation too.
  const turnChoices = turns.filter(turn => turn.endedAtUnixMillis).reverse();
  const requestedTurn = !!turnView && !turnChoices.some(turn => turnCompare(sessionId, turn.turn) === compare);
  const defaults = [...new Set(repos.map(repo => repo.defaultBranch).filter(Boolean))];
  const current = new Set(repos.map(repo => repo.branch));
  const others = [...new Set(repos.flatMap(repo => repo.branches))].filter(name => !defaults.includes(name) && !current.has(name)).sort();
  if (compare !== UNCOMMITTED && compare !== START && !turnView && !defaults.includes(compare) && !others.includes(compare)) others.unshift(compare);
  return <header>
    <div>
      <strong>{summary}</strong>
      <small>{turnView ? 'What the agent changed in that turn. Choose another comparison for the whole workspace.' : compare === UNCOMMITTED ? 'Staged, unstaged and untracked files' : compare === START ? 'Since the workspace started, plus uncommitted work' : `Branch commits since it left ${compare}, plus uncommitted work`}</small>
    </div>
    <label className="changes-compare">Compare with<select value={compare} onChange={event => onCompare(event.target.value)}>
      <option value={UNCOMMITTED}>Uncommitted only</option>
      <option value={START}>Workspace start</option>
      {defaults.map(name => <option key={'d:' + name} value={name}>{name}</option>)}
      {others.length > 0 && <optgroup label="Other branches">{others.map(name => <option key={name} value={name}>{name}</option>)}</optgroup>}
      {(turnChoices.length > 0 || requestedTurn) && <optgroup label="Agent turns in this conversation">
        {requestedTurn && <option value={compare}>Turn {turnView!.turn}</option>}
        {turnChoices.map(turn => <option key={turn.turn} value={turnCompare(sessionId, turn.turn)}>Turn {turn.turn}{turn.prompt ? ` · ${turn.prompt.slice(0, 48)}` : ''}{turn.files ? '' : ' (no changes)'}</option>)}
      </optgroup>}
    </select></label>
    <ReviewWithButton launcher={launcher} scope={reviewScope}/>
    <button className="changes-review-toggle" aria-pressed={trayOpen} aria-label={`Review comments${pending ? `, ${pending} pending` : ''}`} onClick={onToggleTray}><Icon name="chat" size={14}/>Review{pending > 0 && <span aria-hidden="true">{pending}</span>}</button>
    {!turnView && <button onClick={onRefresh}>Refresh changes</button>}
  </header>;
}

/** A narrow panel's left column: the changed files or the review tray, one
 * in front at a time with the whole height. Hidden (both shown) when wide. */
function ColumnTabs({ id, pane, onPane, files, pending, needsYou }: { id: string; pane: Pane; onPane(pane: Pane): void; files: number; pending: number; needsYou: boolean }) {
  const keys = (event: ReactKeyboardEvent<HTMLElement>) => {
    const next: Pane | undefined = event.key === 'ArrowRight' || event.key === 'ArrowLeft' ? (pane === 'files' ? 'review' : 'files') : event.key === 'Home' ? 'files' : event.key === 'End' ? 'review' : undefined;
    if (!next) return;
    event.preventDefault(); event.stopPropagation();
    onPane(next);
    requestAnimationFrame(() => document.getElementById(`${id}-tab-${next}`)?.focus());
  };
  const tab = (value: Pane, label: string, icon: 'file' | 'chat', count: number, attention = false) =>
    <button type="button" role="tab" id={`${id}-tab-${value}`} aria-controls={`${id}-${value}`} aria-selected={pane === value} tabIndex={pane === value ? 0 : -1} onClick={() => onPane(value)}
      title={value === 'files' ? 'Changed files' : attention ? 'A review loop needs you' : 'Comments and agent reviews'}>
      <Icon name={icon} size={13}/>{label}{count > 0 && <span className="changes-pane-count">{count}</span>}{attention && <span className="changes-pane-alert" role="img" aria-label="needs you"><Icon name="bell" size={11}/></span>}
    </button>;
  return <div className="changes-panes" role="tablist" aria-label="Left column" onKeyDown={keys}>
    {tab('files', 'Files', 'file', files)}
    {tab('review', 'Review', 'chat', pending, needsYou)}
  </div>;
}

/** One repository: its PR loop, then its committed and uncommitted files. */
function RepositoryFiles({ workspaceId, repo, compare, selection, conversation, launcher, onChoose, onChanged, onSendToAgent, isViewed, onViewed, menu }: {
  menu?(member: string, file: ChangedFile, committed: boolean): MenuItem[];
  launcher?: ReviewLauncher;
  workspaceId: string;
  repo: RepositoryChanges;
  compare: string;
  selection?: Selection;
  conversation?: Session;
  onChoose(member: string, file: ChangedFile, committed: boolean): void;
  onChanged(): void;
  onSendToAgent(text: string): void;
  isViewed(key: string): boolean;
  onViewed(key: string, viewed: boolean): void;
}) {
  const showCommitted = compare !== UNCOMMITTED;
  const fileButton = (file: ChangedFile, isCommitted: boolean) => {
    const key = fileKey(repo.member, file.path, isCommitted), seen = isViewed(key);
    return <div key={key} className={`changes-file${seen ? ' is-viewed' : ''}`}>
      <button data-file-key={key} className={selection?.member === repo.member && selection.path === file.path && selection.committed === isCommitted ? 'selected' : ''} onClick={() => onChoose(repo.member, file, isCommitted)}
        onContextMenu={menu && contextMenu('changed-file', () => menu(repo.member, file, isCommitted), { member: repo.member, path: file.path, committed: isCommitted })}>
        <code title={isCommitted ? 'Change since the workspace started' : 'Index / working tree status'}>{isCommitted ? file.indexStatus : file.indexStatus + file.worktreeStatus}</code>
        <span>{file.path}{file.originalPath && <small>from {file.originalPath}</small>}</span>
      </button>
      <input type="checkbox" aria-label={`Viewed ${file.path}`} title="Viewed" checked={seen} onChange={event => onViewed(key, event.target.checked)}/>
    </div>;
  };
  return <section>
    <h3>{repo.member}<small>{repo.branch === 'HEAD' ? 'Detached HEAD' : repo.branch} · {repo.head.slice(0, 8)}</small></h3>
    <RepositoryShip workspaceId={workspaceId} repo={repo} conversation={conversation} launcher={launcher} onChanged={onChanged} onSendToAgent={onSendToAgent}/>
    {repo.error ? <p role="alert" className="error">{repo.error}</p> : <>
      {!showCommitted ? null : repo.committedError ? <p className="module-empty" role="status">Branch changes unavailable: {repo.committedError}</p> : repo.committed.length > 0 && <div className="changes-group">
        <h4>{compare === START ? 'Committed on this branch' : `Committed vs ${compare}`}</h4>
        <details className="changes-commits">
          <summary>{plural(repo.commits.length, 'commit')}{compare === START ? (repo.base ? ` since ${repo.base.slice(0, 8)}` : '') : ` not on ${compare}`}</summary>
          <ol>{repo.commits.map(c => <li key={c.id}><code>{c.id.slice(0, 8)}</code><span>{c.subject}</span><small>{age(c.committedAtUnix)}</small></li>)}</ol>
        </details>
        {repo.committed.map(file => fileButton(file, true))}
        {repo.committedTruncated && <p role="status">Limited. Some branch changes are not shown.</p>}
      </div>}
      {repo.files.length > 0 && <div className="changes-group">
        <h4>Uncommitted</h4>
        {repo.files.map(file => fileButton(file, false))}
        {repo.truncated && <p role="status">File list limited. Some changes are not shown.</p>}
      </div>}
      {!repo.files.length && (!showCommitted || !repo.committed.length) && !repo.committedError && <p className="module-empty">{compare === UNCOMMITTED ? 'No uncommitted changes' : compare === START ? 'No changes since the workspace started' : `No changes vs ${compare}`}</p>}
    </>}
  </section>;
}

/** Unified or side by side. A pane too narrow for two columns shows unified and says so. */
function DiffModeToggle({ mode, narrow, onMode }: { mode: DiffMode; narrow: boolean; onMode(mode: DiffMode): void }) {
  return <>
    <div className="diff-mode" role="group" aria-label="Diff layout">
      {(['unified', 'split'] as const).map(value => <button key={value} type="button" aria-pressed={mode === value} title={value === 'split' ? `Side by side (s)${narrow ? ' · shown when the pane is wider' : ''}` : 'One column (s)'} onClick={() => onMode(value)}>{value === 'split' ? 'Split' : 'Unified'}</button>)}
    </div>
    {narrow && mode === 'split' && <small className="diff-mode-note" role="status">Unified while the pane is narrow</small>}
  </>;
}

type DiffReview = { workspaceId: string; member: string; path: string; view: ReviewView; base: string; focus?: ReviewFocus; mode: DiffMode; loadFile?(): Promise<string> };
/** The selected file's diff, open to review comments, or why there is none. */
function DiffBody({ diff, error, empty, review }: { diff?: WorkspaceDiff; error: string; empty: 'branch' | 'staged' | 'unstaged'; review: DiffReview }) {
  if (error) return <p role="alert" className="error">{error}</p>;
  if (!diff) return <p role="status">Loading diff…</p>;
  return <>
    {diff.truncated && <p role="status">Preview limited to 1 MiB.</p>}
    {diff.binary
      ? <p>Binary file · text preview unavailable</p>
      : diff.text
        ? <ReviewDiff {...review} text={diff.text}/>
        : <p className="module-empty">No {empty} diff for this file. Refresh if the agent has changed it.</p>}
  </>;
}
