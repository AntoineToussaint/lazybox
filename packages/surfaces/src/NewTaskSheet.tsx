import { BehaviorOptions, behaviorForProvider, CHECK_IN_FIRST, readDefaultBehavior, saveDefaultBehavior } from './BehaviorProfile';
import { useEffect, useId, useMemo, useRef, useState, type ClipboardEvent, type FormEvent, type KeyboardEvent, type RefObject } from 'react';
import { SubscriptionOptions } from './SubscriptionGroups';
import type { RunnerClient } from '@lazybox/client-core';
import type { InfoResponse, RepositoryReplacement } from '@lazybox/protocol';
import { ReplaceRepositoryCopy } from './RepositoryReplacement';
import type { ViewStateStore } from '@lazybox/platform';
import { actionEnabled, bindings, displayBinding, runAction, useAction, useActionRevision } from './actions';
import { useKeymap } from './keyboard';
import { AgentPicker } from './AgentPicker';
import { composeInConversation } from './compose';
import { DialogHeader, DraggableDialog } from './DraggableDialog';
import { Icon } from './Icons';
import { agentTitle } from './providers';
import { useRepositoryCatalog } from './repository-catalog';
import { repositoryLabel } from './repository-model';
import { useRunner } from './RunnerContext';
import { RunnerHealthDot, RunnerScope, useRunnerDirectory, useRunnerIdentity } from './RunnerDirectory';
import { healthLabel, type RunnerHealth } from './runner-directory';
import { useRunnerSettings } from './RunnerSettings';
import { showToast } from './toasts';
import { conversationDefaults, readPreference, writeLaunchChoices, writePreference } from './view-state';
import { modeOf, parseTaskChoices, rankRepositories, rememberTask, TASK_CHOICES_PREFERENCE, TASK_MODES, taskBranch, taskName, taskProblem, type TaskChoices, type TaskMode, type TaskProblem } from './new-task-model';
import { itemBranch, itemMessage, itemReference, itemSafeMode, itemTaskName, type ItemLike } from './item-context';
import { ItemPicker, ItemSource, StartedNotice, useItemDetail, useStartedFrom } from './WorkItems';
import { forgetStarts, StartFromChip, StartFromPicker, useRepositoryStarts } from './StartFrom';
import { effectiveStart, startsFor } from './start-from-model';
import type { ViewTarget } from './workspace-model';
import './new-task.css';

/**
 * New task (⌘N): one sheet for everything a piece of work starts with — its
 * repositories, the branch, the agent, how it works, and the first message.
 * Enter creates the workspace and the conversation together and sends the
 * message; ⌘Enter does the same in the background, so several can be started
 * in a row. The sheet remembers the last repositories, agent and mode, and an
 * unsent message survives closing it. A new conversation in the workspace
 * already open stays one key away (⌘⇧N, New conversation).
 */

/** The work item a task starts from (an issue), and whether its comments go to the agent. */
type ItemChoice = { provider: string; key: string };
type Draft = { prompt: string; identities: string[]; empty: boolean; name?: string; branch?: string; /** The branch each repository starts from, chosen in this sheet. */ starts?: Record<string, string>; provider: string; profile: string; model?: string; gateway?: string; mode: TaskMode; behaviorProfile?: string; item?: ItemChoice; withoutComments?: boolean };
/** `message` is what is sent: the prompt, then the item's quoted context when there is one. */
type Plan = { prompt: string; message: string; name: string; branch: string; identities: string[]; starts: Record<string, string>; empty: boolean; provider: string; profileId: string; model: string; gateway: string; mode: TaskMode; behaviorProfile: string; agent: string; item?: ItemChoice };
type Progress = (message: string) => void;
/** Where a start got to, so trying again picks up there instead of making a second workspace. */
type Attempt = { spec: string; requestKey: string; operationId: string; collectionId?: string; sessionId?: string; sent?: boolean; message?: string;
  /** The repository the Runner refused because its history changed on GitHub (the error card's way forward). */
  replaced?: RepositoryReplacement };

/** The unsent sheet of this window: an Escape never loses a long message. */
let unsent: Draft | undefined;
/** When the sheet was remade for another Runner from its Runner row: focus goes back there (a time, so a repeated mount effect agrees). */
let runnerPickedAt = 0;

async function startTask(api: RunnerClient, plan: Plan, attempt: Attempt, progress: Progress): Promise<{ collectionId: string; sessionId: string }> {
  if (!attempt.collectionId) {
    progress(plan.identities.length ? 'Preparing the workspace…' : 'Creating the workspace…');
    let collection: { id: string } | undefined;
    attempt.replaced = undefined;
    for await (const update of api.prepareWorkspace({ requestKey: attempt.requestKey, collectionId: '', name: plan.name, repositoryIdentities: [...plan.identities].sort(), branch: plan.empty ? '' : plan.branch, startFrom: plan.starts, item: plan.item }, { timeoutMs: 600_000 })) {
      if (update.message) progress(update.message);
      if (update.collection) collection = update.collection;
      if (update.replaced) attempt.replaced = update.replaced;
    }
    if (!collection) throw new Error('The Runner stopped before the workspace was ready. Start again: it picks up the same workspace.');
    attempt.collectionId = collection.id;
    // The Runner now remembers each choice (and may have just cloned a repository): the next sheet asks it again.
    forgetStarts(api, plan.identities);
  }
  if (!attempt.sessionId) {
    progress(`Starting ${plan.agent}…`);
    const mode = modeOf(plan.mode);
    const opened = await api.openSession({ profileId: plan.profileId, collectionId: attempt.collectionId, options: { model: plan.model, permission: mode.permission, workflow: mode.workflow, behaviorProfile: plan.behaviorProfile, gatewayId: plan.gateway, rotationProfileIds: [], noRotation: false, instructions: '' } });
    if (!opened.session) throw new Error('The Runner did not start the conversation.');
    attempt.sessionId = opened.session.id;
  }
  if (plan.message.trim() && !attempt.sent) {
    progress(plan.item ? `Sending your message with ${plan.item.key}…` : 'Sending your message…');
    await api.apply({ sessionId: attempt.sessionId, operationId: attempt.operationId, operation: { case: 'message', value: { text: plan.message, attachments: [] } } }, { timeoutMs: 30_000 });
    attempt.sent = true;
  }
  return { collectionId: attempt.collectionId, sessionId: attempt.sessionId };
}

/** The Runner a `task.new` asks for (`{ runner: id }`), if any. */
function newTaskRunner(argument: unknown): string {
  const runner = argument && typeof argument === 'object' ? (argument as { runner?: unknown }).runner : undefined;
  return typeof runner === 'string' ? runner : '';
}

/** A Runner the sheet can start on, when the window shows several. */
type RunnerChoice = { id: string; label: string; channel: string; health: RunnerHealth };

/**
 * The sheet and its key, mounted once per workspace window. With several
 * Runners in the window the sheet starts on the one the page shows, and a
 * Runner row picks another: the sheet then talks to that Runner only (its
 * repositories, agents and modes), and a started task opens on its page.
 */
export function NewTaskHost({ viewState, onStarted }: { viewState?: ViewStateStore; onStarted(target: ViewTarget): void }) {
  const [open, setOpen] = useState(false);
  const [failure, setFailure] = useState('');
  const sheet = useRef<HTMLDialogElement | null>(null);
  const directory = useRunnerDirectory();
  const { id: here } = useRunnerIdentity();
  const [runner, setRunner] = useState(here);
  // "Start from an issue…" (the palette): the sheet, with its issue picker open. A count, so asking again reopens it.
  const [pick, setPick] = useState(0);
  // `{ runner }` (a Runner section's +, its "Create one"): the sheet starts on that Runner, if it answers; else on the page's.
  const usable = (id: string) => id === here || directory?.shown.some(row => row.id === id && row.health === 'online' && !!row.state);
  const asked = (argument: unknown) => { const id = newTaskRunner(argument); return id && usable(id) ? id : here; };
  useAction('task.new', argument => {
    // Opened (or moved) by the person, not from the sheet's own Runner menu: the message has the focus.
    runnerPickedAt = 0;
    if (sheet.current?.open) { if (newTaskRunner(argument)) setRunner(asked(argument)); sheet.current.querySelector<HTMLTextAreaElement>('.new-task-prompt')?.focus(); }
    else { setFailure(''); setRunner(asked(argument)); setPick(0); setOpen(true); }
  });
  useAction('task.fromIssue', () => { if (!sheet.current?.open) { runnerPickedAt = 0; setFailure(''); setRunner(here); } setPick(n => n + 1); setOpen(true); });
  if (!open) return null;
  const choices: RunnerChoice[] | undefined = directory?.multi ? directory.shown.map(row => ({ id: row.id, label: row.label, channel: row.channel, health: row.health })) : undefined;
  const chosen = choices?.some(choice => choice.id === runner) ? runner : here;
  const started = (target: ViewTarget) => onStarted(chosen === here ? target : { ...target, runner: chosen } as ViewTarget);
  const body = <NewTaskSheet key={chosen} ref={sheet} viewState={viewState} failure={failure} pickIssue={pick} onClose={() => setOpen(false)} runners={choices} runner={chosen} onRunner={setRunner}
    onStarted={started} onFailed={message => { setFailure(message); setOpen(true); }}/>;
  return chosen === here ? body : <RunnerScope runner={chosen}>{body}</RunnerScope>;
}

/**
 * "on Dev ▾" in the sheet's title, when the window shows several Runners:
 * where the task starts, and a menu to start it on another. A Runner that
 * does not answer is listed, and cannot be picked.
 */
function RunnerPick({ runners, runner, disabled, onPick }: { runners: RunnerChoice[]; runner: string; disabled: boolean; onPick(id: string): void }) {
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLSpanElement>(null), button = useRef<HTMLButtonElement>(null);
  const current = runners.find(choice => choice.id === runner);
  const items = () => [...box.current?.querySelectorAll<HTMLButtonElement>('[role=menuitemradio]:not(:disabled)') || []];
  useEffect(() => {
    if (!open) return;
    (items().find(item => item.getAttribute('aria-checked') === 'true') || items()[0])?.focus();
    const outside = (event: PointerEvent) => { if (!box.current?.contains(event.target as Node)) setOpen(false); };
    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [open]);
  const close = () => { setOpen(false); button.current?.focus(); };
  const pick = (id: string) => { setOpen(false); if (id !== runner) onPick(id); else button.current?.focus(); };
  const menuKeys = (event: KeyboardEvent<HTMLDivElement>) => {
    const list = items(), at = list.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.key === 'ArrowDown' ? list[(at + 1) % list.length] : event.key === 'ArrowUp' ? list[(at - 1 + list.length) % list.length]
      : event.key === 'Home' ? list[0] : event.key === 'End' ? list.at(-1) : undefined;
    if (next) { event.preventDefault(); next.focus(); return; }
    // Escape closes the menu, not the sheet.
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); return; }
    if (event.key === 'Tab') setOpen(false);
  };
  return <span className="new-task-runner" ref={box}>
    <button ref={button} type="button" className="new-task-runner-pick" aria-haspopup="menu" aria-expanded={open} disabled={disabled}
      aria-description="The Runner this task starts on. Choose another." title={`Starts on ${current?.label || 'this Runner'}${current?.channel && current.channel !== current.label ? ` (${current.channel})` : ''} · choose another Runner`}
      onClick={() => setOpen(value => !value)}
      onKeyDown={event => { if (event.key === 'ArrowDown' || event.key === 'ArrowUp') { event.preventDefault(); setOpen(true); } }}>
      on {current?.label || 'this Runner'}<Icon name="chevron" size={12}/>
    </button>
    {open && <div className="new-task-runner-menu" role="menu" aria-label="Start on" onKeyDown={menuKeys}>
      {runners.map(choice => <button key={choice.id} type="button" role="menuitemradio" aria-checked={choice.id === runner} disabled={choice.health !== 'online' && choice.id !== runner}
        title={choice.health === 'online' ? `Start on ${choice.label}${choice.channel && choice.channel !== choice.label ? ` (${choice.channel})` : ''}` : `${choice.label}: ${healthLabel[choice.health]}`}
        onClick={() => pick(choice.id)}>
        <RunnerHealthDot health={choice.health}/><span className="new-task-runner-name">{choice.label}</span>
        {choice.health !== 'online' && <small aria-hidden="true">{healthLabel[choice.health]}</small>}
        {choice.id === runner && <Icon name="check" size={12}/>}
      </button>)}
    </div>}
  </span>;
}

function NewTaskSheet({ ref, viewState, failure, pickIssue = 0, onClose, onStarted, onFailed, runners, runner, onRunner }: { ref: RefObject<HTMLDialogElement | null>; viewState?: ViewStateStore; failure: string; /** Changes when the palette asks for the issue picker. */ pickIssue?: number; onClose(): void; onStarted(target: ViewTarget): void; onFailed(message: string): void; runners?: RunnerChoice[]; runner?: string; onRunner?(id: string): void }) {
  const { api, info } = useRunner();
  const { settings } = useRunnerSettings();
  const { catalog, loading: catalogLoading, refreshing: catalogRefreshing, reload: reloadCatalog } = useRepositoryCatalog(api);
  const [remembered] = useState<TaskChoices>(() => { try { return parseTaskChoices(readPreference(viewState, TASK_CHOICES_PREFERENCE)); } catch { return parseTaskChoices(null); } });
  const [draft, setDraft] = useState<Draft>(() => {
    if (unsent) return unsent;
    const last = conversationDefaults(viewState, info);
    return {
      prompt: '', identities: remembered.identities, empty: !!remembered.empty,
      provider: last.provider || remembered.provider || 'claude', profile: last.provider ? last.profile || '' : remembered.profile,
      model: last.model, gateway: last.gateway, mode: remembered.mode,
    };
  });
  const change = (next: Partial<Draft>) => setDraft(current => { const value = { ...current, ...next }; unsent = value; return value; });
  const behaviorProfile = behaviorForProvider(draft.behaviorProfile ?? readDefaultBehavior(viewState), draft.provider);
  const [query, setQuery] = useState(''), [listOpen, setListOpen] = useState(false), [active, setActive] = useState(0);
  const [editingName, setEditingName] = useState(draft.name !== undefined || draft.branch !== undefined);
  const [problem, setProblem] = useState<TaskProblem>(), [error, setError] = useState(failure);
  const [replaced, setReplaced] = useState<RepositoryReplacement>();
  const [busy, setBusy] = useState(false), [progress, setProgress] = useState('');
  const attempt = useRef<Attempt>({ spec: '', requestKey: '', operationId: '' });
  const background = useRef(false);
  const prompt = useRef<HTMLTextAreaElement>(null), search = useRef<HTMLInputElement>(null);
  const id = useId();

  useEffect(() => {
    ref.current?.showModal();
    if (Date.now() - runnerPickedAt < 1000) { ref.current?.querySelector<HTMLElement>('.new-task-runner-pick')?.focus(); return; }
    const field = prompt.current;
    if (field) { field.focus(); field.setSelectionRange(field.value.length, field.value.length); }
  }, []);

  const entries = catalog?.entries || [];
  // Remembered repositories the catalog no longer lists are dropped once it has loaded.
  useEffect(() => {
    if (!catalog) return;
    const known = draft.identities.filter(identity => entries.some(entry => entry.identity === identity));
    if (known.length !== draft.identities.length) change({ identities: known });
    else if (!known.length && !draft.empty && !unsent && entries.length === 1) change({ identities: [entries[0].identity] });
  }, [catalog]);
  // Where each repository's new branch starts ("from main ▾"; ⌥B).
  const [starts, setStartInfo] = useRepositoryStarts(api, draft.empty ? [] : draft.identities);
  const [startPicker, setStartPicker] = useState<string>();
  const startOf = (identity: string) => effectiveStart(draft.starts?.[identity], starts[identity]);
  const openStart = (identity: string) => { setListOpen(false); setStartPicker(identity); };
  const closeStart = (identity: string) => { setStartPicker(undefined); requestAnimationFrame(() => ref.current?.querySelector<HTMLElement>(`[data-start-from="${CSS.escape(identity)}"]`)?.focus()); };
  const label = (identity: string) => { const entry = entries.find(e => e.identity === identity); return entry ? repositoryLabel({ gitIdentity: entry.identity, name: entry.name }) : identity.replace(/^github:/, ''); };
  const chosen = useMemo(() => new Set(draft.identities), [draft.identities]);
  const options = useMemo(() => rankRepositories(entries, { query, recent: remembered.recent, chosen }), [entries, query, chosen]);

  const profiles = (info?.profiles || []).filter(p => p.provider === draft.provider);
  const profileId = profiles.find(p => p.id === draft.profile)?.id || profiles.find(p => p.managed)?.id || profiles[0]?.id || '';
  const agent = agentTitle(draft.provider, info?.profiles);

  // The issue the task starts from: its text for the agent, and the workspaces already started from it.
  const [pickerOpen, setPickerOpen] = useState(false);
  const [another, setAnother] = useState(false);
  const itemRef = draft.item;
  const itemState = useItemDetail(api, itemRef);
  const startedFrom = useStartedFrom(api, itemRef);
  const item: ItemLike | undefined = itemState.detail?.item || (itemRef ? { ref: itemRef, kind: '', title: '' } : undefined);
  const itemRepositoryIdentity = itemState.detail?.item?.repository || '';
  const repositoryMissing = !!itemRepositoryIdentity && !!catalog && !entries.some(entry => entry.identity === itemRepositoryIdentity);
  // A new item picks its own repository, once, when the catalog lists it.
  const appliedItem = useRef('');
  useEffect(() => {
    if (!itemRef || !itemRepositoryIdentity || !catalog || appliedItem.current === itemRef.key) return;
    if (!entries.some(entry => entry.identity === itemRepositoryIdentity)) return;
    appliedItem.current = itemRef.key;
    change({ identities: [itemRepositoryIdentity], empty: false });
    setProblem(undefined);
  }, [itemRef?.key, itemRepositoryIdentity, catalog]);
  useEffect(() => { setAnother(false); }, [itemRef?.key]);
  // The palette's "Start from an issue…".
  useEffect(() => { if (pickIssue > 0 && !busy) setPickerOpen(true); }, [pickIssue]);

  const generated = settings?.naming === 'generated' ? settings.suggestedName : '';
  const taken = useMemo(() => new Set(startedFrom.flatMap(w => w.members.map(m => m.spec?.branch || '')).filter(Boolean)), [startedFrom]);
  const name = item && draft.name === undefined ? (item.title ? itemTaskName(item) : taskName({ prompt: draft.prompt, labels: [], generated }) || itemTaskName(item)) : taskName({ custom: draft.name, prompt: draft.prompt, labels: draft.identities.map(label), generated });
  const branch = item && draft.branch === undefined ? itemBranch(item, taken) : taskBranch(name, draft.branch);
  // Item text is untrusted: the Runner default never gives it full access.
  const runnerPermission = settings?.permission || info?.defaultPermission || '';
  const safe = item ? itemSafeMode(draft.mode, runnerPermission) : { mode: draft.mode, guarded: false };
  const context = itemState.detail ? (draft.withoutComments ? itemState.detail.contextWithoutComments : itemState.detail.context) : '';
  const message = item ? itemMessage(draft.prompt, item, context) : draft.prompt;

  const attach = (choice: ItemChoice) => { change({ item: choice, withoutComments: false, name: undefined, branch: undefined }); setEditingName(false); setProblem(undefined); setPickerOpen(false); prompt.current?.focus(); };
  const detach = () => { appliedItem.current = ''; change({ item: undefined, withoutComments: false }); prompt.current?.focus(); };
  // A pasted issue link (alone on the clipboard) becomes the task's item instead of text.
  const pasted = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const text = event.clipboardData.getData('text/plain');
    if (!itemReference(text) || draft.item) return;
    event.preventDefault();
    void api.resolveItemUrl({ url: text.trim() }, { timeoutMs: 10_000 }).then(ref => attach({ provider: ref.provider, key: ref.key }), () => {
      // Not an item after all (the Runner decides): the text goes in as typed.
      const field = prompt.current; if (!field) return;
      const at = field.selectionStart ?? draft.prompt.length, end = field.selectionEnd ?? at;
      change({ prompt: draft.prompt.slice(0, at) + text + draft.prompt.slice(end) });
    });
  };

  const plan = (): Plan | undefined => {
    const found = taskProblem({ identities: draft.identities, empty: draft.empty, profileId, name, branch, ready: !!info });
    setProblem(found);
    if (found) {
      if (found.field === 'repositories') { search.current?.focus(); setListOpen(true); }
      if (found.field === 'name' || found.field === 'branch') setEditingName(true);
      return undefined;
    }
    return { prompt: draft.prompt, message, name, branch, identities: draft.empty ? [] : draft.identities, starts: draft.empty ? {} : startsFor(draft.identities, draft.starts || {}), empty: draft.empty, provider: draft.provider, profileId, model: draft.model || '', gateway: draft.gateway || '', mode: behaviorProfile === CHECK_IN_FIRST ? 'read-only' : safe.mode as TaskMode, behaviorProfile, agent, item: itemRef ? { provider: itemRef.provider, key: itemRef.key } : undefined };
  };

  const start = async (inBackground: boolean) => {
    if (busy) { if (inBackground) moveToBackground(); return; }
    const value = plan();
    if (!value) return;
    const spec = JSON.stringify({ ...value, prompt: '', message: '' });
    if (spec !== attempt.current.spec) attempt.current = { spec, requestKey: crypto.randomUUID(), operationId: crypto.randomUUID() };
    else if (value.message !== attempt.current.message) attempt.current.operationId = crypto.randomUUID();
    try { if (draft.behaviorProfile !== undefined) saveDefaultBehavior(viewState, value.behaviorProfile); } catch { /* the task can still start */ }
    attempt.current.message = value.message;
    setBusy(true); setError(''); setReplaced(undefined); setProgress('');
    background.current = inBackground;
    // In the background the sheet gets out of the way at once; the start goes on and reports in a toast.
    if (inBackground) { announceBackground(value); onClose(); }
    const run = attempt.current;
    try {
      const target = await startTask(api, value, run, message => { if (!background.current) setProgress(message); });
      try {
        writePreference(viewState, TASK_CHOICES_PREFERENCE, JSON.stringify(rememberTask(parseTaskChoices(readPreference(viewState, TASK_CHOICES_PREFERENCE)), { identities: value.identities, empty: value.empty, provider: value.provider, profile: value.profileId, mode: draft.mode })));
        writeLaunchChoices(viewState, { provider: value.provider, profile: value.profileId, model: value.model, gateway: value.gateway, behaviorProfile: value.behaviorProfile });
      } catch { /* The task has already started. */ }
      unsent = undefined;
      if (background.current) showToast({ id: `task:${run.requestKey}`, tone: 'review', title: `Started “${value.name}”`, body: value.message.trim() ? `${value.agent} has your message.` : `${value.agent} is ready.`, action: { label: 'Open', run: () => onStarted({ ...target, commandId: '' }) }, button: { label: 'Open', run: () => onStarted({ ...target, commandId: '' }) } });
      else { onClose(); onStarted({ ...target, commandId: '' }); }
    } catch (failure) {
      const message = String(failure).replace(/^(Connect)?Error: /, '');
      // The conversation exists but did not take the message: it waits in its message box.
      if (run.sessionId && !run.sent && value.prompt.trim()) composeInConversation({ sessionId: run.sessionId, text: value.prompt });
      if (background.current) showToast({ id: `task:${run.requestKey}`, tone: 'failed', title: `Could not start “${value.name}”`, body: message, action: { label: 'Show', run: () => onFailed(message) }, button: { label: 'Show', run: () => onFailed(message) } });
      else { setError(message); setReplaced(run.replaced); setBusy(false); setProgress(''); }
    }
  };
  const announceBackground = (value: Plan) => showToast({ id: `task:${attempt.current.requestKey}`, tone: 'info', title: `Starting “${value.name}”…`, body: value.identities.length ? `${value.identities.length === 1 ? label(value.identities[0]) : `${value.identities.length} repositories`} · ${value.agent}` : value.agent, timeout: 4000 });
  /** Escape or ⌘Enter while starting: the start goes on, the sheet gets out of the way. */
  const moveToBackground = () => {
    if (!background.current) {
      background.current = true;
      const value = plan(); if (value) announceBackground(value);
    }
    onClose();
  };

  const submit = (event?: FormEvent) => { event?.preventDefault(); void start(false); };
  // A pick closes the list (typing or ↓ opens it again for another), so the sheet stays readable.
  const pick = (identity: string) => { change({ identities: [...draft.identities, identity], empty: false }); setQuery(''); setActive(0); setListOpen(false); setProblem(undefined); };
  const remove = (identity: string) => change({ identities: draft.identities.filter(value => value !== identity) });
  const noRepository = () => { change({ identities: [], empty: true }); setQuery(''); setListOpen(false); setProblem(undefined); prompt.current?.focus(); };
  const rows = options.length + 1; // the repositories, then "Start without a repository"

  const searchKeys = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing) return;
    switch (event.key) {
      case 'ArrowDown': event.preventDefault(); setListOpen(true); setActive(a => listOpen ? (a + 1) % rows : 0); return;
      case 'ArrowUp': event.preventDefault(); setListOpen(true); setActive(a => (a - 1 + rows) % rows); return;
      case 'Enter':
        if (event.metaKey || event.ctrlKey) return;
        if (listOpen && (query.trim() || active > 0 || !draft.identities.length)) { event.preventDefault(); if (active < options.length) pick(options[active].entry.identity); else noRepository(); return; }
        event.preventDefault(); submit(); return;
      case 'Escape': if (listOpen) { event.preventDefault(); event.stopPropagation(); setListOpen(false); } return;
      case 'Backspace':
        if (!query && draft.identities.length) { event.preventDefault(); remove(draft.identities.at(-1)!); if (draft.identities.length === 1) setListOpen(true); }
        else if (!query && draft.empty) { change({ empty: false }); setListOpen(true); }
        return;
      case 'Tab': setListOpen(false); return;
    }
  };
  const promptKeys = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === 'Enter' && !event.shiftKey && !event.altKey && !event.metaKey && !event.ctrlKey && !event.nativeEvent.isComposing) { event.preventDefault(); submit(); }
  };
  // ⌘Enter anywhere in the sheet starts in the background, before a field (the agent picker) takes it.
  const sheetKeys = (event: KeyboardEvent<HTMLDialogElement>) => {
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && !event.nativeEvent.isComposing) { event.preventDefault(); event.stopPropagation(); void start(true); }
    // ⌥B: where the branch starts — the focused repository's, else the first's.
    if (event.altKey && !event.metaKey && !event.ctrlKey && event.code === 'KeyB' && !busy && !draft.empty && draft.identities.length) {
      event.preventDefault(); event.stopPropagation();
      const focused = (document.activeElement as HTMLElement | null)?.closest<HTMLElement>('[data-start-from]')?.dataset.startFrom;
      openStart(focused && draft.identities.includes(focused) ? focused : draft.identities[0]);
    }
  };
  useEffect(() => { if (active >= rows) setActive(0); }, [rows]);
  useEffect(() => { if (listOpen) ref.current?.querySelector(`#${CSS.escape(`${id}-repo-${active}`)}`)?.scrollIntoView({ block: 'nearest' }); }, [active, listOpen]);

  const mod = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent) ? '⌘' : 'Ctrl+';
  return <DraggableDialog ref={ref} className="new-task-sheet" aria-labelledby={`${id}-title`} draft="kept" onKeyDownCapture={sheetKeys}
    onCancel={event => { event.preventDefault(); if (busy) moveToBackground(); else onClose(); }} onClose={() => { if (!busy) onClose(); }}>
    <DialogHeader>
      <div><h2 id={`${id}-title`}>New task{runners && <> <RunnerPick runners={runners} runner={runner || ''} disabled={busy} onPick={next => { runnerPickedAt = Date.now(); onRunner?.(next); }}/></>}</h2>
        <p>A workspace on its own branch, an agent, and your first message.</p></div>
      <button type="button" aria-label="Close new task" title={busy ? 'Keeps starting in the background' : 'Close · your message is kept'} onClick={() => busy ? moveToBackground() : onClose()}>×</button>
    </DialogHeader>
    <form onSubmit={submit} aria-busy={busy || undefined}>
      <textarea ref={prompt} className="new-task-prompt" aria-label="First message" rows={itemRef ? 2 : 4} data-item={itemRef ? '' : undefined} maxLength={100_000} disabled={busy} placeholder={itemRef ? `Anything to add about ${itemRef.key}? (optional)` : 'What should the agent do? Paste an issue link to start from it.'}
        value={draft.prompt} onChange={event => { change({ prompt: event.target.value }); setProblem(undefined); }} onKeyDown={promptKeys} onPaste={pasted}/>

      <div className="new-task-item">
        {itemRef
          ? <ItemSource refValue={itemRef} detail={itemState.detail} error={itemState.error} loading={itemState.loading} withoutComments={!!draft.withoutComments} disabled={busy}
            onComments={include => change({ withoutComments: !include })} onRetry={itemState.retry} onRemove={detach}/>
          : <p className="new-task-from">
            <button type="button" className="link-button" disabled={busy} aria-haspopup="dialog" aria-expanded={pickerOpen} onClick={() => setPickerOpen(open => !open)}><Icon name="issue" size={13}/>From an issue…</button>
            <small>or paste its link in the message</small>
          </p>}
        {pickerOpen && !itemRef && <ItemPicker onPick={picked => picked.ref && attach({ provider: picked.ref.provider, key: picked.ref.key })} onClose={() => { setPickerOpen(false); prompt.current?.focus(); }}/>}
      </div>
      {itemRef && !another && <StartedNotice refValue={itemRef} workspaces={startedFrom}
        onOpen={workspace => { if (unsent) unsent = { ...unsent, item: undefined, withoutComments: false }; onClose(); onStarted({ collectionId: workspace.id, sessionId: '', commandId: '' }); }}
        onAnother={() => { setAnother(true); prompt.current?.focus(); }}/>}


      <div className="new-task-field" data-invalid={problem?.field === 'repositories' || undefined}>
        <label className="new-task-label" htmlFor={`${id}-search`}><Icon name="repository" size={13}/>Repositories</label>
        <div className="new-task-repos" onClick={event => { if (event.target === event.currentTarget) search.current?.focus(); }}>
          {draft.identities.map(identity => <span key={identity} className="new-task-chip" title={identity}>
            <span className="new-task-chip-name">{label(identity)}</span>
            <StartFromChip identity={identity} repository={label(identity)} branch={startOf(identity)} info={starts[identity]} disabled={busy} expanded={startPicker === identity}
              onOpen={() => startPicker === identity ? closeStart(identity) : openStart(identity)}/>
            <button type="button" aria-label={`Remove ${label(identity)}`} disabled={busy} onClick={() => remove(identity)}><Icon name="close" size={12}/></button>
          </span>)}
          {draft.empty && <span className="new-task-chip is-empty"><span>No repository</span><button type="button" aria-label="Choose repositories instead" disabled={busy} onClick={() => { change({ empty: false }); search.current?.focus(); }}><Icon name="close" size={11}/></button></span>}
          <input ref={search} id={`${id}-search`} role="combobox" aria-expanded={listOpen} aria-controls={`${id}-repos`} aria-autocomplete="list" autoComplete="off" spellCheck={false} disabled={busy}
            aria-activedescendant={listOpen ? `${id}-repo-${active}` : undefined} aria-label="Add a repository"
            placeholder={draft.identities.length || draft.empty ? 'Add another…' : 'Search your repositories…'}
            value={query} onChange={event => { setQuery(event.target.value); setActive(0); setListOpen(true); }} onFocus={() => { if (!draft.identities.length && !draft.empty) setListOpen(true); }} onClick={() => setListOpen(true)} onBlur={() => setListOpen(false)} onKeyDown={searchKeys}/>
        </div>
        {listOpen && <ul className="new-task-repo-list" id={`${id}-repos`} role="listbox" aria-label="Repositories" onMouseDown={event => event.preventDefault()}>
          {options.map((option, index) => <li key={option.entry.identity} id={`${id}-repo-${index}`} role="option" aria-selected={index === active} className={index === active ? 'active' : undefined}
            onPointerMove={() => setActive(index)} onClick={() => pick(option.entry.identity)}>
            <Icon name="repository" size={13}/><span className="new-task-repo-name">{label(option.entry.identity)}</span>{option.recent && <small>Recent</small>}
          </li>)}
          {!options.length && <li className="new-task-repo-empty" aria-disabled="true">{catalogLoading ? 'Loading your repositories…' : query ? `No repository matches “${query}”` : entries.length ? 'Every repository is chosen' : 'No repositories yet'}</li>}
          <li id={`${id}-repo-${options.length}`} role="option" aria-selected={active === options.length} className={`new-task-repo-none${active === options.length ? ' active' : ''}`} onPointerMove={() => setActive(options.length)} onClick={noRepository}>
            <Icon name="folder" size={13}/><span>Start without a repository</span>
          </li>
          <li className="new-task-repo-more" aria-hidden="true"><button type="button" tabIndex={-1} onClick={() => { onClose(); runAction('settings.open', 'repositories'); }}><Icon name="plus" size={12}/>Add repositories…</button></li>
        </ul>}
        {startPicker && draft.identities.includes(startPicker) && !busy && <StartFromPicker api={api} identity={startPicker} repository={label(startPicker)} info={starts[startPicker]} current={startOf(startPicker)}
          onInfo={info => setStartInfo(startPicker, info)} onClose={() => closeStart(startPicker)}
          onPick={picked => { change({ starts: { ...draft.starts, [startPicker]: picked } }); closeStart(startPicker); }}/>}
        {repositoryMissing && <p className="new-task-missing" role="status">
          <span><strong>{itemRepositoryIdentity.replace(/^github:/, '')}</strong>, the issue’s repository, is not on this Runner.</span>
          <button type="button" className="link-button" disabled={busy || catalogRefreshing} onClick={() => void reloadCatalog(true)}>{catalogRefreshing ? 'Looking on GitHub…' : 'Find it on GitHub'}</button>
          <button type="button" className="link-button" disabled={busy} onClick={() => { onClose(); runAction('settings.open', 'repositories'); }}>Add or clone it…</button>
        </p>}
      </div>

      {!draft.empty && <div className="new-task-field" data-invalid={problem?.field === 'branch' || problem?.field === 'name' || undefined}>
        <span className="new-task-label"><Icon name="branch" size={13}/>Branch</span>
        {editingName
          ? <div className="new-task-naming">
            <label>Workspace<input aria-label="Workspace name" value={draft.name ?? name} maxLength={128} disabled={busy} onChange={event => change({ name: event.target.value })}/></label>
            <label>Branch<input aria-label="Branch" className="mono" spellCheck={false} value={draft.branch ?? branch} maxLength={200} disabled={busy} placeholder="The Runner’s default" onChange={event => change({ branch: event.target.value })}/></label>
            <button type="button" className="link-button" disabled={busy} onClick={() => { change({ name: undefined, branch: undefined }); setEditingName(false); }}>Use the suggestion</button>
          </div>
          : <p className="new-task-branch"><code data-testid="new-task-branch">{branch || 'the Runner’s default'}</code><span className="new-task-name" title="The workspace's name">{name}</span>
            <button type="button" className="link-button" disabled={busy} onClick={() => setEditingName(true)}>Edit</button></p>}
      </div>}

      <div className="new-task-field new-task-agent" data-invalid={problem?.field === 'agent' || undefined}>
        <span className="new-task-label" aria-hidden="true"><Icon name="agent" size={13}/>Agent</span>
        <div className="new-task-controls">
          <AgentPicker value={draft.provider} disabled={!info || busy} onChange={provider => change({ provider, profile: '', model: '', gateway: '' })}/>
          {profiles.length > 1 && <select aria-label="Account" value={profileId} disabled={busy} onChange={event => change({ profile: event.target.value })}>
            <SubscriptionOptions profiles={profiles}/>
          </select>}
          <input aria-label="Model" placeholder="Provider default model" value={draft.model || ''} disabled={busy} onChange={event => change({ model: event.target.value })}/>
        </div>
      </div>
      <div className="new-task-field">
        <label className="new-task-label" htmlFor={`${id}-behavior`}><Icon name="agent" size={13}/>Personality</label>
        <div className="new-task-controls"><select id={`${id}-behavior`} value={behaviorProfile} disabled={busy} onChange={event => change({ behaviorProfile: event.target.value })}><BehaviorOptions provider={draft.provider}/></select></div>
        {behaviorProfile === CHECK_IN_FIRST && <p className="new-task-hint">Starts read only so you can review the plan before edits.</p>}
      </div>
      <div className="new-task-field">
        <label className="new-task-label" htmlFor={`${id}-mode`}><Icon name="shield" size={13}/>Mode</label>
        <div className="new-task-controls">
          <select id={`${id}-mode`} aria-label="Mode" value={behaviorProfile === CHECK_IN_FIRST ? 'read-only' : draft.mode} disabled={busy || behaviorProfile === CHECK_IN_FIRST} onChange={event => change({ mode: event.target.value as TaskMode })}>
            {TASK_MODES.map(mode => <option key={mode.id} value={mode.id}>{mode.id === 'default' ? defaultModeLabel(info, settings?.permission) : mode.label}</option>)}
          </select>
        </div>
        {safe.guarded && behaviorProfile !== CHECK_IN_FIRST && <p className="new-task-hint" data-testid="new-task-item-mode">From an issue the agent asks first, not YOLO: its text is not yours. Choose YOLO to change that.</p>}
      </div>

      {replaced && !problem && !busy
        // The error card's way forward: replace Lazybox's copy, then start again.
        ? <ReplaceRepositoryCopy key={replaced.identity} replacement={replaced} autoFocus onReplaced={() => { setReplaced(undefined); setError(''); void reloadCatalog(); void start(false); }}/>
        : (problem || error) && <p className="new-task-problem" role="alert">{problem?.message || error}</p>}
      {busy && <div className="new-task-progress" role="status"><span>{progress || 'Starting…'}</span><progress aria-label="Starting the task"/><small>Esc keeps it going in the background.</small></div>}
      <footer>
        <span className="new-task-keys" aria-hidden="true"><kbd>↵</kbd> start · <kbd>{mod}↵</kbd> in the background · {draft.identities.length && !draft.empty ? <><kbd>⌥B</kbd> start from</> : <><kbd>⇧↵</kbd> new line</>}</span>
        <button type="button" className="new-task-background" disabled={busy} title={`Start it and keep working here (${mod}↵)`} onClick={() => void start(true)}>Start in background</button>
        <button type="submit" className="primary" disabled={busy}>{busy ? 'Starting…' : itemRef && startedFrom.length && !another ? 'Start another' : message.trim() ? 'Start' : 'Create'}<kbd>↵</kbd></button>
      </footer>
    </form>
  </DraggableDialog>;
}

const defaultModeLabel = (info: InfoResponse | undefined, permission?: string) => {
  const value = permission || info?.defaultPermission || '';
  return `Runner default${value ? ` · ${value === 'yolo' ? 'YOLO' : value === 'read-only' ? 'read only' : value === 'ask' ? 'ask' : value}` : ''}`;
};

/**
 * Where there is nothing to show yet (no workspace chosen, none created): the
 * one way in, New task, with its key; choosing a workspace in the sidebar is
 * the other.
 */
export function NewTaskStart({ viewState, compact = false }: { viewState?: ViewStateStore; compact?: boolean }) {
  const keymap = useKeymap(viewState);
  useActionRevision();
  const keys = bindings(keymap['task.new'])[0];
  const shortcut = keys ? displayBinding(keys).join(' ') : '';
  const button = <button type="button" className="primary new-task-start-button" disabled={!actionEnabled('task.new')} onClick={() => runAction('task.new')}><Icon name="plus" size={14}/>New task{shortcut && <kbd>{shortcut}</kbd>}</button>;
  if (compact) return <div className="new-task-start is-compact">{button}</div>;
  return <section className="new-task-start" aria-label="Start">
    <span className="new-task-start-icon" aria-hidden="true"><Icon name="agent" size={20}/></span>
    <h2>Start something</h2>
    <p>Pick the repositories, an agent and say what to do: Lazybox makes the workspace on its own branch and the agent starts on your message.</p>
    {button}
    <small>Or choose a workspace or a conversation on the left.</small>
  </section>;
}
