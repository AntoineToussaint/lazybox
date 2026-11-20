import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { useBackdropDismiss } from './dismiss';
import type { ViewStateStore } from '@lazybox/platform';
import { actionDefinition, actionDefinitions, actionEnabled, bindings, displayBinding, runAction, useActionRevision, type ActionDefinition } from './actions';
import { showToast } from './toasts';
import { spokenAnswer, spokenDecision, spokenScore } from './voice/voice-model';
import { cancelDictation, registerDictationTarget, takeOverDictation, toggleDictation, useVoiceState, recognizers, voiceState, type DictationSink } from './voice/voice-session';
import { VoicePill } from './voice/VoiceLayer';
import { attentionTitle, type Attention } from './attention';
import { openArtifact } from './artifact-events';
import { conversationName } from './conversation-model';
import { FileIcon } from './DirectoryListing';
import { bestMatch, fuzzyMatch } from './fuzzy';
import { Icon } from './Icons';
import { AttentionIcon } from './Inbox';
import { highlight } from './QuickOpen';
import { settingsTabs } from './SettingsPage';
import { userPaletteItems, useUserCommands } from './user-commands';
import { looksLikeLayoutYaml } from './config-files';
import type { Keymap } from './keyboard';
import { useArtifactsOf } from './useArtifacts';
import { artifactPaletteEntries } from './artifact-model';
import { RunnerBadge, useMergedInbox, useRunnerDirectory } from './RunnerDirectory';
import { healthLabel, rowTarget } from './runner-directory';
import { useMissions } from './useMissions';
import { useTrackedItems } from './useTrackedPulls';
import { groupPulls, needsYou as pullNeedsYou } from './pull-requests-model';
import { PullStatusIcon } from './PullRequestsPanel';
import { selectMission } from './navigate-requests';
import { readPreference, writePreference } from './view-state';
import { modKey, useWorkbenchState, type Workbench } from './workbench';
import type { CommandNavigator } from './CommandCenter';
import type { ViewTarget } from './workspace-model';
import './command-center.css';

type Kind = 'action' | 'conversation' | 'workspace' | 'file' | 'setting' | 'panel' | 'artifact' | 'mission' | 'pull';
const kindLabel: Record<Kind, string> = { action: 'Action', conversation: 'Conversation', workspace: 'Workspace', file: 'File', setting: 'Settings', panel: 'Panel', artifact: 'Artifact', mission: 'Mission', pull: 'Pull request' };
/** `runner`: the item is on that Runner of the window (a badge says which). */
type Item = { key: string; kind: Kind; title: string; detail?: string; extra?: string; icon: ReactNode; keys?: string[]; current?: boolean; runner?: string; run(how: { side: boolean }): void };
type Shown = Item & { positions: number[]; score: number };
type Section = { title: string; items: Shown[] };

/** The prefixes that narrow the palette to one kind of thing. */
export const prefixes: { prefix: string; kind: Kind; label: string }[] = [
  { prefix: '>', kind: 'action', label: 'Actions' },
  { prefix: '@', kind: 'conversation', label: 'Conversations' },
  { prefix: '#', kind: 'workspace', label: 'Workspaces' },
  { prefix: '/', kind: 'file', label: 'Files' },
  { prefix: '!', kind: 'pull', label: 'Pull requests' },
  { prefix: '*', kind: 'artifact', label: 'Artifacts' },
];
const recentKey = 'palette:recent:v1';
function readRecent(store?: ViewStateStore): string[] {
  try { const value = JSON.parse(readPreference(store, recentKey) || '[]'); return Array.isArray(value) ? value.filter(v => typeof v === 'string').slice(0, 20) : []; } catch { return []; }
}
function remember(store: ViewStateStore | undefined, key: string) {
  try { writePreference(store, recentKey, JSON.stringify([key, ...readRecent(store).filter(value => value !== key)].slice(0, 20))); } catch { /* a convenience */ }
}
/** Rows that need the person: input or approval, an unseen failure, an unseen finished turn. */
export function needsYou<T extends Attention>(rows: readonly T[]): T[] {
  return rows.filter(row => row.state === 'needs-input' || (row.unread && (row.state === 'failed' || row.state === 'review')));
}
const firstKeys = (value: string | undefined) => bindings(value).slice(0, 1).map(binding => displayBinding(binding).join(' '));
/** Voice runs these only after a yes: the person's own actions (they run scripts, send messages) and anything that cannot be undone. */
export function voiceConfirms(item: { key: string; kind: string }): boolean {
  if (item.key.startsWith('user-action:')) return true;
  return item.kind === 'action' && !!actionDefinition(item.key.replace(/^action:/, ''))?.destructive;
}
type VoiceNote = { tone: 'info' | 'confirm'; text: string; item?: Item };

/**
 * The command palette: every action of the registry plus "go to" anything —
 * conversations in every workspace, workspaces, files, settings sections,
 * panels, artifacts of every workspace and missions — in one fuzzy list.
 * Keyboard only: type, ↑/↓, Enter. A prefix narrows it: `>` actions,
 * `@` conversations, `#` workspaces, `/` files, `*` artifacts.
 */
export function CommandPalette({ keymap, navigator, viewState, initialQuery = '', onClose }: { keymap: Keymap; navigator: CommandNavigator; viewState?: ViewStateStore; initialQuery?: string; onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null), input = useRef<HTMLInputElement>(null), list = useRef<HTMLDivElement>(null);
  useBackdropDismiss(dialog);
  const returnFocus = useRef<HTMLElement | null>(document.activeElement as HTMLElement | null);
  const [query, setQuery] = useState(initialQuery), [selected, setSelected] = useState(0);
  // A query said rather than typed ranks by words (voice-model.ts) and may run on its own.
  const [spoken, setSpoken] = useState<{ text: string; live: boolean } | null>(null);
  const [voiceNote, setVoiceNote] = useState<VoiceNote | null>(null);
  const [decide, setDecide] = useState<string | null>(null);
  const voice = useVoiceState();
  const listening = voice.kind === 'palette' && (voice.phase === 'starting' || voice.phase === 'listening' || voice.phase === 'stopping');
  const latestVoice = useRef({ query, voiceNote }); latestVoice.current = { query, voiceNote };
  const recent = useMemo(() => readRecent(viewState), [viewState]);
  const recentRank = (key: string) => { const i = recent.indexOf(key); return i < 0 ? 99 : i; };
  useActionRevision();
  const userActions = useUserCommands('palette');
  // However it closes (Esc, ⌘K again, a choice), focus returns where it was — unless the choice moves it.
  const restore = useRef(true);
  useEffect(() => {
    dialog.current?.showModal(); input.current?.focus();
    const previous = returnFocus.current;
    return () => { if (restore.current && previous?.isConnected) requestAnimationFrame(() => previous.focus?.({ preventScroll: true })); };
  }, []);
  const spokenMode = !!spoken && spoken.text === query;
  const scope = spokenMode ? undefined : prefixes.find(value => query.startsWith(value.prefix));
  const term = (scope ? query.slice(1) : query).trim();
  // Conversations and workspaces of every Runner the window shows; choosing another Runner's shows its page.
  const inbox = useMergedInbox();
  const directory = useRunnerDirectory();
  const workspaceId = navigator.current.collectionId;
  // Artifacts of every workspace: one chosen elsewhere switches to its workspace and opens there.
  const { items: found } = useArtifactsOf(navigator.showPanel && (!scope || scope.kind === 'artifact') ? navigator.collections : []);
  const missions = useMissions();
  const tracked = useTrackedItems();
  const files = useFileResults(navigator.workbench, scope?.kind === 'file' || (!scope && term.length >= 2) ? term : null);

  const close = () => onClose();
  const choose = (item: Item | undefined, side = false) => {
    if (!item) return;
    remember(viewState, item.key);
    restore.current = item.kind === 'action';
    close();
    // After the palette is gone, so an action sees the focus it was invoked from.
    requestAnimationFrame(() => item.run({ side }));
  };

  const items = useMemo(() => {
    const workspaceName = (id: string) => navigator.collections.find(value => value.id === id)?.name || (id ? 'Unavailable workspace' : 'No workspace');
    const actions: Item[] = (actionDefinitions as readonly ActionDefinition[]).filter(definition => definition.palette !== false && actionEnabled(definition.id)).map(definition => ({
      key: `action:${definition.id}`, kind: 'action', title: definition.title, detail: definition.group, extra: definition.keywords, icon: <Icon name={definition.icon} size={15}/>,
      keys: firstKeys(keymap[definition.id as keyof Keymap]), run: () => { runAction(definition.id, undefined, 'palette'); },
    }));
    const runnerOf = (id: string) => directory?.runners.find(value => value.id === id);
    const nameIn = (runner: string, id: string) => runner === inbox.here ? workspaceName(id) : runnerOf(runner)?.collections.find(value => value.id === id)?.name || (id ? 'Unavailable workspace' : 'No workspace');
    const conversations: Item[] = inbox.rows.map(row => ({
      key: `conversation:${row.key}`, kind: 'conversation', title: conversationName(row.session),
      detail: `${nameIn(row.runner, row.session.collectionId)} · ${row.state === 'idle' ? row.reason : attentionTitle[row.state]}`, extra: [nameIn(row.runner, row.session.collectionId), inbox.multi ? runnerOf(row.runner)?.label : ''].filter(Boolean).join(' '),
      icon: <AttentionIcon state={row.state} size={15}/>, current: row.runner === inbox.here && row.session.id === navigator.current.sessionId, runner: inbox.multi ? row.runner : undefined,
      run: () => navigator.goTo(row.runner === inbox.here ? { collectionId: row.session.collectionId, sessionId: row.session.id, commandId: '' } : rowTarget(row)),
    }));
    const workspaces: Item[] = navigator.collections.map((collection, index) => ({
      key: `workspace:${collection.id}`, kind: 'workspace', title: collection.name || collection.id,
      detail: collection.members.map(member => member.spec?.name).filter(Boolean).join(', ') || 'No repositories', icon: <Icon name="collections" size={15}/>,
      keys: index < 9 ? firstKeys(keymap[`workspace.select.${index + 1}` as keyof Keymap]) : undefined, current: collection.id === workspaceId,
      runner: inbox.multi ? inbox.here : undefined,
      run: () => navigator.goTo({ collectionId: collection.id, sessionId: '', commandId: '' }),
    }));
    // Another Runner's workspaces, and each Runner itself: "Show Working".
    if (inbox.multi && directory) for (const runner of directory.shown) {
      if (runner.id !== inbox.here) {
        workspaces.push({ key: `runner:${runner.id}`, kind: 'workspace', title: `Show ${runner.label}`, detail: `Runner · ${[runner.channel !== runner.label ? runner.channel : '', healthLabel[runner.health]].filter(Boolean).join(' · ')}`, extra: `runner machine switch ${runner.channel}`,
          icon: <Icon name="processes" size={15}/>, runner: runner.id, run: () => directory.activate(runner.id) });
        if (runner.health === 'online') for (const collection of runner.collections) workspaces.push({
          key: `workspace:${runner.id}:${collection.id}`, kind: 'workspace', title: collection.name || collection.id,
          detail: collection.members.map(member => member.spec?.name).filter(Boolean).join(', ') || 'No repositories', extra: runner.label, icon: <Icon name="collections" size={15}/>, runner: runner.id,
          run: () => navigator.goTo({ collectionId: collection.id, sessionId: '', commandId: '', runner: runner.id } as ViewTarget),
        });
      }
    }
    const settings: Item[] = navigator.openSettings ? settingsTabs.map(tab => ({
      key: `setting:${tab.id}`, kind: 'setting', title: `${tab.label} settings`, detail: `Settings · ${tab.group}`, extra: tab.id === 'extensions' ? 'plugins add-ons install manage settings' : 'settings preferences', icon: <Icon name={tab.icon} size={15}/>,
      run: () => navigator.openSettings!(tab.id),
    })) : [];
    const panels: Item[] = (navigator.panels || []).map(panel => ({
      key: `panel:${panel.kind}`, kind: 'panel', title: `Show ${panel.title}`, detail: 'Panel', extra: panel.title, icon: <Icon name={panel.icon} size={15}/>,
      run: () => navigator.showPanel!(panel.kind),
    }));
    const windowItems: Item[] = (navigator.windows || []).map(item => ({
      key: `window:${item.id}`, kind: 'panel', title: `Put ${item.title} back in main window`, detail: 'In another window', extra: `${item.title} dock attach return window`, icon: <Icon name="put-back" size={15}/>,
      run: () => item.putBack(),
    }));
    const artifactItems: Item[] = navigator.showPanel ? artifactPaletteEntries(found, workspaceId).map(entry => ({
      key: entry.key, kind: 'artifact', title: entry.title, detail: entry.detail, extra: entry.extra, icon: <Icon name={entry.icon} size={15}/>,
      run: () => {
        const request = { collectionId: entry.collectionId, artifactId: entry.artifactId };
        if (entry.here) { if (!openArtifact(request)) navigator.showPanel!('artifacts'); return; }
        // Asked for first, so the workspace shows its Artifacts panel on the page it switches to.
        openArtifact(request, { whenShown: true });
        navigator.goTo({ collectionId: entry.collectionId, sessionId: '', commandId: '' });
      },
    })) : [];
    const missionItems: Item[] = navigator.showPanel ? missions.missions.filter(mission => !mission.canceled).map(mission => ({
      key: `mission:${mission.id}`, kind: 'mission', title: mission.title || 'Untitled mission', detail: `Mission · ${mission.tasks.length} ${mission.tasks.length === 1 ? 'task' : 'tasks'}`, extra: mission.brief.slice(0, 200), icon: <Icon name="target" size={15}/>,
      run: () => { navigator.showPanel!('missions'); selectMission(mission.id); },
    })) : [];
    // The tracked pull requests (pull-requests-model.ts): Enter opens the
    // workspace at the conversation that delivered it.
    const pulls = groupPulls(tracked.links, Date.now());
    const pullItems: Item[] = [...pulls.needsYou, ...pulls.waiting, ...pulls.merged].map(pull => ({
      key: `pull:${pull.key}`, kind: 'pull', title: `${pull.repository}#${pull.number}${pull.title ? ` ${pull.title}` : ''}`,
      detail: `${workspaceName(pull.link.collectionId) === 'Unavailable workspace' ? pull.link.collectionName || 'Deleted workspace' : workspaceName(pull.link.collectionId)} · ${pull.detail}`, extra: `pull request pr ${pull.link.item?.pullRequest?.head || ''}`,
      icon: <PullStatusIcon pull={pull} size={15}/>, current: pullNeedsYou.has(pull.status) ? false : undefined,
      run: () => navigator.goTo({ collectionId: pull.link.collectionId, sessionId: pull.link.sessionId, commandId: '' }),
    }));
    // The person's own actions (POWER.md), after the built-in ones.
    actions.push(...userPaletteItems().map(item => ({ ...item, icon: <Icon name={item.icon || 'play'} size={15}/> })));
    return { actions, conversations, workspaces, settings, panels: [...windowItems, ...panels], artifactItems, missionItems, pullItems, pullsNeedingYou: pulls.needsYou.map(pull => `pull:${pull.key}`) };
  }, [keymap, navigator, inbox.rows, inbox.multi, inbox.here, directory, found, missions.missions, workspaceId, userActions, tracked.links]);

  const fileItem = (path: string): Item => {
    const slash = path.lastIndexOf('/');
    return { key: `file:${workspaceId}:${path}`, kind: 'file', title: path.slice(slash + 1), detail: slash < 0 ? '' : path.slice(0, slash), extra: path, icon: <FileIcon name={path.slice(slash + 1)} size={15}/>, run: ({ side }: { side: boolean }) => navigator.workbench?.open(path, { side: side ? 'row' : undefined, focus: true }) };
  };
  const fileItems: Item[] = useMemo(() => (files.paths || []).map(fileItem), [files.paths, navigator.workbench, workspaceId]);

  const sections: Section[] = useMemo(() => {
    const all: Item[] = [...items.actions, ...items.conversations, ...items.workspaces, ...items.settings, ...items.panels, ...items.artifactItems, ...items.missionItems, ...items.pullItems];
    const plain = (rows: Item[]): Shown[] => rows.map(row => ({ ...row, positions: [], score: 0 }));
    if (!term) {
      if (scope?.kind === 'action') return [{ title: 'Actions', items: plain(items.actions) }];
      if (scope?.kind === 'conversation') return [{ title: 'Conversations', items: plain(items.conversations) }];
      if (scope?.kind === 'workspace') return [{ title: 'Workspaces', items: plain(items.workspaces) }];
      if (scope?.kind === 'pull') return [{ title: 'Pull requests', items: plain(items.pullItems) }];
      if (scope?.kind === 'artifact') return [{ title: 'Artifacts · newest first', items: plain(items.artifactItems) }];
      if (scope?.kind === 'file') return [{ title: navigator.workbench ? 'Recently opened' : '', items: plain(fileItems) }];
      const byKey = new Map(all.map(item => [item.key, item]));
      // Files chosen recently in this workspace come back as files.
      if (navigator.workbench) for (const key of recent) if (key.startsWith(`file:${workspaceId}:`)) byKey.set(key, fileItem(key.slice(`file:${workspaceId}:`.length)));
      const recentItems = recent.map(key => byKey.get(key)).filter((item): item is Item => !!item && !item.current).slice(0, 5);
      const used = new Set(recentItems.map(item => item.key));
      const waiting = [...needsYou(inbox.rows).map(row => `conversation:${row.key}`), ...items.pullsNeedingYou].map(key => byKey.get(key)).filter((item): item is Item => !!item && !used.has(item.key)).slice(0, 6);
      waiting.forEach(item => used.add(item.key));
      return [
        { title: 'Recent', items: plain(recentItems) },
        { title: 'Needs you', items: plain(waiting) },
        { title: 'Actions', items: plain(items.actions.filter(item => !used.has(item.key))) },
      ].filter(section => section.items.length);
    }
    const pool = scope ? (scope.kind === 'file' ? [] : all.filter(item => item.kind === scope.kind)) : all;
    const ranked: Shown[] = [];
    if (spokenMode) {
      // Said, not typed: words matter, not letters in order (the decision reads these scores).
      if (/^open that[.!?]?$/i.test(term.trim())) {
        const announcement = pool.find(item => item.key === 'action:voice.open-announcement');
        return announcement ? [{ title: '', items: [{ ...announcement, positions: [], score: 100 }] }] : [];
      }
      for (const item of [...pool, ...fileItems]) {
        const score = spokenScore(term, item.title, `${item.extra || ''} ${item.detail || ''}`);
        if (score > 0) ranked.push({ ...item, positions: [], score });
      }
      ranked.sort((a, b) => b.score - a.score || recentRank(a.key) - recentRank(b.key));
      // What was clearly said about; a few best guesses when nothing was.
      const strong = ranked.filter(item => item.score >= 25);
      return [{ title: '', items: (strong.length ? strong : ranked).slice(0, strong.length ? 30 : 8) }];
    }
    for (const item of pool) {
      const match = bestMatch(term, item.title, item.extra || item.detail);
      if (!match) continue;
      const seen = recent.indexOf(item.key);
      ranked.push({ ...item, positions: match.positions, score: match.score + (seen >= 0 ? 60 - seen * 2 : 0) + (item.kind === 'action' ? 5 : 0) });
    }
    for (const item of fileItems) {
      const match = fuzzyMatch(term, item.title) || fuzzyMatch(term, item.extra || '');
      ranked.push({ ...item, positions: match && fuzzyMatch(term, item.title) ? match.positions : [], score: (match?.score ?? 0) - (scope ? 0 : 30) });
    }
    ranked.sort((a, b) => b.score - a.score);
    return [{ title: '', items: ranked.slice(0, 60) }];
  }, [items, fileItems, term, scope, recent, inbox.rows, navigator.workbench, workspaceId, spokenMode]);

  const flat = sections.flatMap(section => section.items);

  // The palette takes dictation while it is open: what is said is the query.
  const voiceRun = (item: Item, said: string) => { showToast({ id: 'voice:ran', tone: 'info', title: item.title, body: `You said “${said}”` }); choose(item); };
  // Closing the palette while it listens ends that dictation.
  useEffect(() => () => { const now = voiceState(); if (now.kind === 'palette' && now.phase !== 'idle' && now.phase !== 'error') cancelDictation(); }, []);
  useEffect(() => registerDictationTarget({
    kind: 'palette',
    element: () => dialog.current,
    available: () => !!dialog.current?.open,
    begin(): DictationSink {
      const before = latestVoice.current.query;
      // Answering "Run …?": the list stays as it is while the person says yes or no.
      const answering = latestVoice.current.voiceNote?.tone === 'confirm';
      return {
        update(view) { if (answering) return; setQuery(view.text); setSpoken({ text: view.text, live: view.liveStart < view.text.length }); },
        end(reason, view) {
          const said = view.text.trim();
          if (reason === 'cancel' || reason === 'error' || !said) { setQuery(before); setSpoken(null); input.current?.focus(); return; }
          // The answer to "Run …? Say yes".
          const asked = latestVoice.current.voiceNote;
          const answer = asked?.tone === 'confirm' ? spokenAnswer(said) : undefined;
          if (asked?.item && answer) {
            setVoiceNote(null);
            if (answer === 'yes') voiceRun(asked.item, said); else { setQuery(before); setSpoken(null); input.current?.focus(); }
            return;
          }
          setVoiceNote(null);
          setQuery(said); setSpoken({ text: said, live: false }); setDecide(said);
          input.current?.focus();
        },
      };
    },
  }), []);
  // Once the spoken query is ranked: run a clear winner, ask about one that needs a yes, or let the person pick.
  useEffect(() => {
    if (decide === null || !spokenMode) return;
    setDecide(null);
    const decision = spokenDecision(flat.map(item => ({ key: item.key, title: item.title, score: item.score, confirm: voiceConfirms(item) })));
    const item = 'key' in decision ? flat.find(row => row.key === decision.key) : undefined;
    if (decision.kind === 'run' && item) { voiceRun(item, decide); return; }
    if (decision.kind === 'confirm' && item) {
      setSelected(flat.indexOf(item));
      setVoiceNote({ tone: 'confirm', item, text: `Run “${item.title}”? Say “yes” (hold the voice key) or press ↵.` });
      return;
    }
    setVoiceNote({ tone: 'info', text: decision.kind === 'none' ? `Nothing matches “${decide}”. Say it another way, or type.` : 'Several things match: pick one with ↑ ↓ and ↵, or say it again.' });
  }, [decide, sections]);
  useEffect(() => { setSelected(0); }, [query]);
  useEffect(() => { list.current?.querySelector<HTMLElement>('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' }); }, [selected, flat.length]);
  const keydown = (event: KeyboardEvent<HTMLInputElement>) => {
    const move = (step: number) => { event.preventDefault(); setSelected(value => flat.length ? Math.max(0, Math.min(flat.length - 1, value + step)) : 0); };
    const wrap = (step: number) => { event.preventDefault(); setSelected(value => flat.length ? (value + step + flat.length) % flat.length : 0); };
    if (event.key === 'ArrowDown' || (event.ctrlKey && event.key === 'n')) wrap(1);
    else if (event.key === 'ArrowUp' || (event.ctrlKey && event.key === 'p')) wrap(-1);
    else if (event.key === 'PageDown') move(8);
    else if (event.key === 'PageUp') move(-8);
    else if (event.key === 'Enter' && !event.nativeEvent.isComposing) { event.preventDefault(); choose(flat[selected], modKey(event)); }
    else if (event.key === 'Backspace' && scope && query.length === 1) { event.preventDefault(); setQuery(''); }
  };
  const placeholder = scope ? { action: 'Run an action…', conversation: 'Go to a conversation in any workspace…', workspace: 'Go to a workspace…', artifact: 'Go to an artifact in any workspace…', file: navigator.workbench ? 'Go to a file in this workspace…' : 'Files open in the main window' }[scope.kind as 'action'] : 'Search actions, conversations, workspaces, files, settings…';
  const empty = scope?.kind === 'file' && !navigator.workbench ? 'Files are searched in a workspace window. Open the main window to go to a file.'
    : scope?.kind === 'artifact' && !navigator.showPanel ? 'Artifacts open in the main window.'
    : files.searching && !flat.length ? '' : files.error && scope?.kind === 'file' ? files.error : term ? `Nothing matches “${term}”.` : scope?.kind === 'file' ? 'Type a file name.' : scope?.kind === 'artifact' ? 'No artifacts in any workspace yet.' : 'Nothing here yet.';
  let index = -1;
  return <dialog ref={dialog} className="command-palette" aria-label="Command palette" onCancel={event => { event.preventDefault(); close(); }}>
    <div className="command-palette-field">
      <Icon name="search" size={16}/>
      {scope && <span className="command-palette-scope">{scope.label}</span>}
      <input ref={input} value={query} className={listening && spoken?.live ? 'is-dictating' : undefined} onChange={event => { if (listening) takeOverDictation(); setSpoken(null); setVoiceNote(null); setQuery(event.target.value); }} onKeyDown={keydown}
        onPaste={event => {
          // A layout's YAML pasted here imports it (POWER.md, "Config files"): the import dialog opens with it.
          const text = event.clipboardData.getData('text/plain');
          if (!looksLikeLayoutYaml(text) || !actionEnabled('layout.import')) return;
          event.preventDefault(); close();
          requestAnimationFrame(() => runAction('layout.import', text, 'palette'));
        }} placeholder={placeholder} aria-label="Search commands" role="combobox" aria-expanded="true" aria-controls="command-palette-results" aria-autocomplete="list" aria-activedescendant={flat[selected] ? `command-palette-${selected}` : undefined} spellCheck={false} autoComplete="off"/>
      {files.searching && <span className="quick-open-busy" aria-hidden="true"/>}
      {recognizers().length > 0 && <button type="button" className="command-palette-mic" aria-label={listening ? 'Stop listening' : 'Say a command'} aria-pressed={listening} title={listening ? 'Stop listening' : `Say a command${firstKeys(keymap['voice.command' as keyof Keymap])[0] ? ` · hold ${firstKeys(keymap['voice.command' as keyof Keymap])[0]}` : ''}`} onClick={() => toggleDictation({ kind: 'palette' })}><Icon name="mic" size={15}/></button>}
    </div>
    {voice.kind === 'palette' && (listening || voice.phase === 'error') && <VoicePill state={voice} keymap={keymap} inline/>}
    {voiceNote && <div className="command-palette-voice" role="status" data-tone={voiceNote.tone}><Icon name="mic" size={13}/><span>{voiceNote.text}</span>
      {voiceNote.tone === 'confirm' && <button type="button" onClick={() => { const item = voiceNote.item; setVoiceNote(null); if (item) choose(item); }}>Run</button>}
    </div>}
    {!query && <div className="command-palette-prefixes" aria-label="Narrow the search">
      {prefixes.map(value => <button key={value.prefix} type="button" onClick={() => { setQuery(value.prefix); input.current?.focus(); }}><kbd>{value.prefix}</kbd>{value.label}</button>)}
    </div>}
    <div ref={list} id="command-palette-results" className="command-palette-results" role="listbox" aria-label="Results">
      {sections.map(section => <div key={section.title || 'results'} role="group" aria-label={section.title || 'Results'}>
        {section.title && <small className="command-palette-heading" aria-hidden="true">{section.title}</small>}
        {section.items.map(item => {
          const i = ++index;
          return <div key={item.key} id={`command-palette-${i}`} role="option" aria-selected={i === selected} className="command-palette-row" data-kind={item.kind} onPointerMove={() => setSelected(i)} onClick={event => choose(item, modKey(event))}>
            <span className="command-palette-icon">{item.icon}</span>
            <span className="command-palette-text">
              <span className="command-palette-title">{highlight(item.title, item.positions)}{item.current && <small className="command-palette-current">Current</small>}</span>
              {(item.detail || item.runner) && <span className="command-palette-detail">{item.runner && <RunnerBadge runner={item.runner}/>}{item.detail}</span>}
            </span>
            {!!item.keys?.length && <span className="command-palette-keys">{item.keys[0].split(' ').map((key, k) => <kbd key={k}>{key}</kbd>)}</span>}
            {term && !scope && <span className="command-palette-kind">{kindLabel[item.kind]}</span>}
          </div>;
        })}
      </div>)}
      {!flat.length && empty && <p className="command-palette-empty" role="status">{empty}</p>}
    </div>
    <footer className="command-palette-footer">
      <span><kbd>↑</kbd><kbd>↓</kbd> choose</span><span><kbd>↵</kbd> run</span><span><kbd>Esc</kbd> close</span>
      <span className="command-palette-legend">{prefixes.map(value => <span key={value.prefix}><kbd>{value.prefix}</kbd> {value.label.toLowerCase()}</span>)}</span>
    </footer>
  </dialog>;
}

/** Files for the palette: the workspace's recent files with no query, its fuzzy search with one. */
function useFileResults(workbench: Workbench | undefined, term: string | null): { paths: string[]; searching: boolean; error: string } {
  const state = useOptionalWorkbenchState(workbench);
  const [result, setResult] = useState<{ paths: string[]; searching: boolean; error: string }>({ paths: [], searching: false, error: '' });
  const recent = state?.layout.recent;
  useEffect(() => {
    if (!workbench || term === null) { setResult({ paths: [], searching: false, error: '' }); return; }
    if (!term) { setResult({ paths: [...recent || []], searching: false, error: '' }); return; }
    const control = new AbortController();
    setResult(value => ({ ...value, searching: true }));
    const timer = setTimeout(() => {
      workbench.api.searchWorkspaceFiles({ collectionId: workbench.workspaceId, query: term, limit: 30 }, { signal: control.signal, timeoutMs: 15000 })
        .then(found => { if (!control.signal.aborted) setResult({ paths: found.matches.map(match => match.path), searching: false, error: '' }); },
          error => { if (!control.signal.aborted) setResult({ paths: [], searching: false, error: String(error) }); });
    }, 60);
    return () => { clearTimeout(timer); control.abort(); };
  }, [workbench, term, recent]);
  return result;
}
const noWorkbench = { subscribe: () => () => {}, getState: () => undefined };
function useOptionalWorkbenchState(workbench: Workbench | undefined) {
  return useWorkbenchState((workbench || noWorkbench) as Workbench) as ReturnType<typeof useWorkbenchState> | undefined;
}
