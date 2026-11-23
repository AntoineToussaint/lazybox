import { RoadmapDocument, roadmapOutline } from './RoadmapDocument';
import { specOrPlanDocument } from './artifact-sections';
import { roadmapMarkdown, typedMissionPlan, type Roadmap } from './roadmap';
import { ArtifactSource, useArtifactSourceDraft } from './ArtifactSource';
import { artifactFormat } from './artifact-formats';
import { ArtifactExportContext, documentSnapshot, exportDocumentHTML, type DocumentHeading } from './artifact-export';
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { Artifact, ArtifactVersion, Collection } from '@lazybox/protocol';
import { ActionMenu } from './ActionMenu';
import { RoadmapDialog, CreateIssueDialog, DeleteArtifactDialog, DiffStats, DiffView, SaveToRepositoryDialog } from './ArtifactDialogs';
import { artifactsChanged, errorText, restoreArtifacts, retireArtifact } from './artifact-client';
import { TodoList, todoMarkdown, todoProgress } from './TodoList';
import { ARTIFACT_KINDS, agentReference, ago, asKind, kindIcon, kindLabel, lineDiff, roadmapMissionPlan, slug, structuredMarkdown, versionOrigin } from './artifact-model';
import { extractToc, readingMinutes, tocOutline, withoutRepeatedTitle } from './artifact-reader';
import { DocumentBody, RenderedDiff, TableOfContents, useScrollSpy } from './ArtifactReader';
import { openArtifact, revealMessage } from './artifact-events';
import { OTHER_ARTIFACTS_POLL_MS, retiredStamp, useArtifacts } from './useArtifacts';
import { composeInConversation } from './compose';
import { useLibrary } from './home-checkouts';
import { placeFor, type HomeTarget } from './home-run';
import { openHomeShell, type ShellPlace } from './home-shell';
import { TerminalPanel } from './TerminalPanel';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { useWorkbench } from './workbench';
import type { ViewTarget } from './workspace-model';
import { Resizer } from './Resizer';
import { SPLITS } from './resizable-model';
import './artifacts.css';

/** What the window offers an artifact page: its own native or browser window. */
export type ArtifactHost = { openArtifact?(target: { collectionId: string; artifactId: string }): Promise<void> };
export const ArtifactHostContext = createContext<ArtifactHost>({});

type Props = {
  collectionId: string;
  artifactId: string;
  /** From the list, when there is one: a newer `updatedAtUnixMillis` reloads the page. */
  summary?: Artifact;
  collection?: Collection;
  /** In a panel: back to the list (narrow layouts). */
  onBack?(): void;
  /** In a workspace: show a conversation (source message, Send to agent). */
  onShowSession?(target: ViewTarget): void;
  onDiscuss?(artifact: Artifact, reference: string): void;
  chatOpen?: boolean;
  /** The conversation the page is beside, for Send to agent. */
  conversationId?: string;
  conversationTitle?: string;
  onDeleted?(): void;
  /** In its own window: the window is the page, so there is no "open in window". */
  standalone?: boolean;
  /** A version to show, compared with another (a chat card's "Compare with v2"); a new `at` applies it again. */
  onNavigateRelated?(next: RelatedPage, fromVersion: number): void;
  focus?: { version?: number; pinned?: boolean; compareWith?: number; at: number };
};

/** Per-window reading preferences: kept for this window only, and harmless when storage is unavailable. */
const HISTORY_KEY = 'lazybox:artifact:history', DIFF_KEY = 'lazybox:artifact:diff';
const remembered = (key: string) => { try { return sessionStorage.getItem(key) || ''; } catch { return ''; } };
const remember = (key: string, value: string) => { try { sessionStorage.setItem(key, value); } catch { /* a convenience */ } };
const editable = (target: EventTarget | null) => target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));

const authorIcon = (version?: ArtifactVersion) => version?.author?.kind === 'agent' ? 'sparkle' : version?.author?.kind === 'file' || version?.source?.kind === 'file' ? 'file' : version?.source?.kind === 'message' ? 'chat' : 'pencil';
let dismissPrintSheet: (() => void) | undefined;

const capital = (text: string) => text ? text[0].toUpperCase() + text.slice(1) : text;
const fullDate = (unixMillis: bigint | number) => new Date(Number(unixMillis)).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });

/**
 * One artifact as a page: a reader for the document (title block, outline,
 * callouts, tables, diagrams), its versions with the changes between them,
 * where it came from, and every way out of Lazybox.
 */
type RelatedPage = { id: string; version?: number };

export function ArtifactPage(props: Props) {
  const { connection } = useRunner();
  const [trail, setTrail] = useState<RelatedPage[]>([]);
  const current = trail.at(-1);
  const back = () => setTrail(pages => pages.slice(0, -1));
  const navigate = (next: RelatedPage, fromVersion: number) => setTrail(pages => [
    ...pages.slice(0, -1), { id: current?.id ?? props.artifactId, version: fromVersion }, next,
  ]);
  return <ArtifactDocument {...props} key={JSON.stringify([connection.baseUrl, props.collectionId, current?.id ?? props.artifactId])}
    artifactId={current?.id ?? props.artifactId} summary={current ? undefined : props.summary}
    focus={current ? { version: current.version, pinned: true, at: trail.length } : props.focus}
    onBack={trail.length > 1 ? back : props.onBack} onDeleted={trail.length > 1 ? back : props.onDeleted}
    onRelated={props.onNavigateRelated ?? navigate}/>;
}

function ArtifactDocument({ collectionId, artifactId, summary, collection, onBack, onShowSession, conversationId, conversationTitle, onDeleted, onDiscuss, chatOpen, standalone, focus, onRelated }: Props & { onRelated(target: RelatedPage, fromVersion: number): void }) {
  const { api, info, connection } = useRunner();
  const host = useContext(ArtifactHostContext);
  const saveArtifactExport = useContext(ArtifactExportContext);
  const savePending = useRef(false);
  const workbench = useWorkbench();
  const [artifact, setArtifact] = useState<Artifact>(), [error, setError] = useState(''), [missing, setMissing] = useState(false);
  const library = useLibrary(api);
  const [terminal, setTerminal] = useState<{ id: string; command: string }>();
  const openingShell = useRef(false);
  const reviewShell = useCallback(async (command: string, target?: HomeTarget) => {
    if (openingShell.current) return;
    let place: ShellPlace = { directory: '', label: 'workspace root' };
    if (target) {
      const repositories = (collection?.members || []).flatMap(({ spec }) => spec?.repositoryId
        ? [{ repositoryId: spec.repositoryId, name: spec.name, identity: library.find(repo => repo.id === spec.repositoryId)?.gitIdentity || '', home: '' }] : []);
      const home = placeFor(target, repositories);
      if (home.kind === 'unknown') { setError(`No workspace repository matches “${home.name}”.`); return; }
      place = home.kind === 'user' ? { userHome: true, label: 'your home folder' }
        : { repositoryId: home.repository.repositoryId, label: `${home.repository.name} home` };
    }
    // PTY input is executable input. Allow source lines and tabs, but reject
    // raw control sequences that could submit or manipulate the prompt.
    if (Array.from(command).some(char => {
      const value = char.charCodeAt(0);
      return (value < 32 && char !== '\n' && char !== '\t') || (value >= 127 && value < 160);
    })) { setError('This command contains terminal control characters.'); return; }
    if (!command.trim()) return;
    openingShell.current = true;
    setError('');
    try {
      const result = await openHomeShell(api, collectionId, place, '', {
        showTerminal: id => setTerminal({ id, command }),
      });
      if (result.error) setError(result.error);
    } finally { openingShell.current = false; }
  }, [api, collection, collectionId, library]);
  const [selected, setSelected] = useState(focus?.pinned ? focus.version ?? 0 : 0); // 0 follows the latest version
  const [discussionVersion, setDiscussionVersion] = useState<number>();
  const [reviewingVersion, setReviewingVersion] = useState<number>();
  const [view, setView] = useState<'document' | 'source' | 'changes'>('document');
  const [diffMode, setDiffModeState] = useState<'rendered' | 'text'>(() => remembered(DIFF_KEY) === 'text' ? 'text' : 'rendered');
  const setDiffMode = (mode: 'rendered' | 'text') => { setDiffModeState(mode); remember(DIFF_KEY, mode); };
  const [sourceDraft, setSourceDraft] = useArtifactSourceDraft(info ? info.runnerId || connection.baseUrl : '', collectionId, artifactId);
  const [base, setBase] = useState(0); // 0 compares with the previous version
  const [contents, setContents] = useState(() => new Map<number, string>());
  // History is closed until the person opens it; this window remembers the choice.
  const [historyOpen, setHistoryState] = useState(() => remembered(HISTORY_KEY) === 'open');
  const setHistoryOpen = useCallback((open: boolean) => { setHistoryState(open); remember(HISTORY_KEY, open ? 'open' : 'closed'); }, []);
  const [wide, setWide] = useState(true);
  const sizes = useRef<ResizeObserver | null>(null);
  const measureRef = useCallback((node: HTMLElement | null) => { sizes.current?.disconnect(); if (!node) return; sizes.current = new ResizeObserver(([entry]) => setWide(entry.contentRect.width > 860)); sizes.current.observe(node); }, []);
  const [republishing, setRepublishing] = useState(false);
  const [roadmapInput, setRoadmapInput] = useState<{ version: number; markdown: string; existing?: Roadmap }>();
  const [renaming, setRenaming] = useState(false), [dialog, setDialog] = useState<'delete' | 'save' | 'issue'>();
  const [status, setStatus] = useState('');
  const statusTimer = useRef(0);
  const say = (text: string) => { setStatus(text); clearTimeout(statusTimer.current); statusTimer.current = window.setTimeout(() => setStatus(''), 4000); };
  useEffect(() => () => clearTimeout(statusTimer.current), []);
  const scroller = useRef<HTMLDivElement>(null);
  const changesHeading = useRef<HTMLHeadingElement>(null);
  const [scrolled, setScrolled] = useState(false);
  const [pendingExport, setPendingExport] = useState<'print' | 'html'>();
  const exportNow = useRef<(format: 'print' | 'html') => void>(() => {});

  // The latest version, again whenever the list says the artifact changed.
  const stamp = summary ? `${summary.updatedAtUnixMillis}:${summary.title}:${summary.kind}:${summary.trackedPath}:${summary.trackingProblem}:${retiredStamp(summary)}` : '';
  const [poll, setPoll] = useState(0);
  // How many times the latest version came back: a version asked for from outside waits for a fresh one.
  const [loads, setLoads] = useState(0);
  useEffect(() => {
    const control = new AbortController();
    void api.getArtifact({ collectionId, artifactId }, { signal: control.signal, timeoutMs: 10000 }).then(latest => {
      if (control.signal.aborted) return;
      setArtifact(latest); setError(''); setMissing(false); setLoads(value => value + 1);
      setContents(current => current.get(latest.contentVersion) === latest.content ? current : new Map(current).set(latest.contentVersion, latest.content));
    }, e => { if (!control.signal.aborted) { const text = errorText(e); setError(text); setMissing(/not.?found|no such|does not exist/i.test(text)); } });
    return () => control.abort();
  }, [api, collectionId, artifactId, stamp, poll]);
  // Without a list (its own window), the page watches for new versions itself.
  useEffect(() => { if (summary) return; const timer = setInterval(() => { if (document.visibilityState === 'visible') setPoll(value => value + 1); }, 4000); return () => clearInterval(timer); }, [!!summary]);

  // A retired page names what superseded it: the workspace's list (shared, retired included) has its title.
  const supersededBy = artifact?.retired?.supersededBy || '';
  const siblings = useArtifacts(collectionId, OTHER_ARTIFACTS_POLL_MS);
  const successor = supersededBy ? [...siblings.artifacts, ...siblings.retired].find(a => a.id === supersededBy) : undefined;
  const moreMenu = useRef<HTMLDetailsElement>(null);

  const versions = artifact?.versions || [];
  const latestNumber = versions.at(-1)?.number || 0;
  const shown = selected || latestNumber;
  const baseNumber = base || Math.max(0, shown - 1);
  const updateReady = !!discussionVersion && latestNumber > (reviewingVersion || discussionVersion);
  const inDiscussionReview = view === 'changes' && reviewingVersion === shown && baseNumber === discussionVersion;
  const readLatest = () => {
    setDiscussionVersion(undefined);
    setReviewingVersion(undefined);
    setSelected(0);
    setBase(0);
    setView('document');
  };
  const reviewUpdate = () => {
    if (!discussionVersion || !updateReady) return;
    setReviewingVersion(latestNumber);
    setSelected(latestNumber);
    setBase(discussionVersion);
    setView('changes');
    requestAnimationFrame(() => {
      scroller.current?.scrollTo({ top: 0 });
      changesHeading.current?.focus({ preventScroll: true });
    });
  };
  // A version asked for from outside (a chat card), each request once: after reading the
  // versions again, since the card may name one newer than the page has seen.
  const focused = useRef(0), focusAfter = useRef({ at: 0, loads: 0 });
  useEffect(() => {
    if (!focus || focused.current === focus.at) return;
    if (focusAfter.current.at !== focus.at) { focusAfter.current = { at: focus.at, loads }; setPoll(value => value + 1); return; }
    if (loads <= focusAfter.current.loads || !latestNumber) return;
    focused.current = focus.at;
    const version = focus.version && (focus.pinned || focus.version < latestNumber) ? focus.version : 0;
    setSelected(version); setBase(focus.compareWith && focus.compareWith < (version || latestNumber) ? focus.compareWith : 0);
    setView(focus.compareWith ? 'changes' : 'document');
  }, [focus, loads, latestNumber]);
  // Load the content of a version that is shown or compared.
  useEffect(() => {
    const control = new AbortController();
    for (const number of [shown, view === 'changes' ? baseNumber : 0]) {
      if (!number || contents.has(number)) continue;
      void api.getArtifact({ collectionId, artifactId, version: number }, { signal: control.signal, timeoutMs: 10000 })
        .then(result => { if (!control.signal.aborted) setContents(current => new Map(current).set(result.contentVersion, result.content)); }, e => { if (!control.signal.aborted) setError(errorText(e)); });
    }
    return () => control.abort();
  }, [api, collectionId, artifactId, shown, baseNumber, view, contents]);

  const content = contents.get(shown);
  const baseContent = baseNumber ? contents.get(baseNumber) : '';
  const structured = artifact?.format === 'json' || artifact?.kind === 'todo-list';
  const roadmap = useMemo<Roadmap | undefined>(() => {
    if (artifact?.kind !== 'plan' || !structured || !content) return undefined;
    const parsed = JSON.parse(content);
    return parsed.schema === 'roadmap.v1' ? parsed : undefined;
  }, [artifact?.kind, structured, content]);
  const fieldDocument = useMemo(() => {
    if (!structured || !content || (artifact?.kind !== 'spec' && artifact?.kind !== 'plan')) return;
    return specOrPlanDocument(JSON.parse(content));
  }, [artifact?.kind, structured, content]);
  const project = (body: string) => {
    if (!structured || !body) return body;
    if (artifact?.kind === 'todo-list') return todoMarkdown(body);
    const parsed = JSON.parse(body);
    if (artifact?.kind === 'plan' && parsed.schema === 'roadmap.v1') return roadmapMarkdown(parsed);
    const typed = body === content ? fieldDocument : specOrPlanDocument(parsed);
    return typed?.markdown ?? structuredMarkdown(parsed);
  };
  const currentText = useMemo(() => content === undefined ? undefined : project(content), [content, fieldDocument, structured, artifact?.kind]);
  const previousText = useMemo(() => baseContent === undefined ? undefined : project(baseContent), [baseContent, content, fieldDocument, structured, artifact?.kind]);
  const diff = useMemo(() => view === 'changes' && currentText !== undefined && previousText !== undefined ? lineDiff(previousText, currentText) : [], [view, currentText, previousText]);
  const version = versions.find(v => v.number === shown);
  const latest = versions.at(-1);
  const documentRef = useRef<HTMLDivElement>(null);
  const [excerpt, setExcerpt] = useState('');
  useEffect(() => {
    const readSelection = () => {
      const selection = window.getSelection();
      const body = documentRef.current;
      const inside = selection && body && body.contains(selection.anchorNode) && body.contains(selection.focusNode);
      setExcerpt(inside ? selection.toString().trim().slice(0, 2000) : '');
    };
    setExcerpt('');
    document.addEventListener('selectionchange', readSelection);
    return () => document.removeEventListener('selectionchange', readSelection);
  }, [artifactId, shown, view]);
  // One title on the page: a leading h1 that repeats the artifact's title, or nearly
  // ("(final)", a subtitle), is hidden and the artifact's title shows (repeatsTitle).
  const { Preview, extension, mime, label: formatLabel } = artifactFormat(artifact?.format ?? '');
  const reading = useMemo(() => content === undefined || !artifact ? undefined : structured ? currentText : Preview ? content : withoutRepeatedTitle(content, artifact.title), [content, currentText, structured, artifact?.title, Preview]);
  const headings = useMemo(() => roadmap ? roadmapOutline(roadmap) : reading === undefined || Preview ? [] : extractToc(reading), [reading, Preview, roadmap]);
  const outline = useMemo(() => tocOutline(headings), [headings]);
  const active = useScrollSpy(scroller, view === 'document' ? outline.map(entry => entry.id) : []);
  useEffect(() => {
    const node = scroller.current;
    if (!node) return;
    const onScroll = () => setScrolled(node.scrollTop > 150);
    onScroll();
    node.addEventListener('scroll', onScroll, { passive: true });
    return () => node.removeEventListener('scroll', onScroll);
  }, [view, !!artifact]);
  // H opens and closes the history; in its own window, from anywhere on the page.
  const toggleKey = (event: KeyboardEvent | ReactKeyboardEvent) => {
    if (event.key.toLowerCase() !== 'h' || event.metaKey || event.ctrlKey || event.altKey || editable(event.target) || event.defaultPrevented) return;
    event.preventDefault(); setHistoryOpen(!historyOpen);
  };
  useEffect(() => {
    if (!standalone) return;
    const listener = (event: KeyboardEvent) => { if (event.target === document.body) toggleKey(event); };
    window.addEventListener('keydown', listener);
    return () => window.removeEventListener('keydown', listener);
  });

  useEffect(() => {
    if (!pendingExport || view !== 'document' || reading === undefined) return;
    setPendingExport(undefined);
    requestAnimationFrame(() => exportNow.current(pendingExport));
  }, [pendingExport, view, reading]);

  if (missing) return <div className="artifact-page is-empty"><p className="module-empty">This artifact was deleted.</p>{onBack && <button onClick={onBack}>Back to artifacts</button>}</div>;
  if (!artifact) return <div className="artifact-page is-empty">{error ? <p className="artifact-error" role="alert">{error}<button onClick={() => setPoll(value => value + 1)}>Retry</button></p> : <p className="module-empty" role="status">Opening artifact…</p>}</div>;

  const kind = asKind(artifact.kind);
  const update = async (change: { title?: string; kind?: string; stopTracking?: boolean }, said: string) => {
    try { setArtifact(await api.updateArtifact({ collectionId, artifactId, ...change }, { timeoutMs: 10000 })); artifactsChanged(collectionId); say(said); }
    catch (e) { setError(errorText(e)); }
  };
  // Retiring is reversible and keeps every version: no confirmation, and Restore is on the banner it leaves.
  const retire = async () => {
    try { setArtifact(await retireArtifact(api, collectionId, artifactId)); say('Retired. It keeps every version; Restore brings it back.'); }
    catch (e) { setError(errorText(e)); }
  };
  const restore = async () => {
    try {
      await restoreArtifacts(api, collectionId, [artifactId]);
      setArtifact(current => current && { ...current, retired: undefined } as Artifact); setPoll(value => value + 1);
      say('Restored. It is back in the list.');
      // The banner that held the focus is gone: the page's actions keep it.
      requestAnimationFrame(() => moreMenu.current?.querySelector('summary')?.focus());
    } catch (e) { setError(errorText(e)); }
  };
  const saveRevision = async (body: string, message: string, expectedVersion = latestNumber) => {
    if (republishing) return false;
    setRepublishing(true);
    try {
      const saved = await api.putArtifact({ collectionId, artifactId, content: body, expectedVersion }, { timeoutMs: 10000 });
      setContents(current => new Map(current).set(saved.contentVersion, saved.content));
      setArtifact(saved);
      setDiscussionVersion(undefined);
      setReviewingVersion(undefined);
      setSelected(0);
      setBase(0);
      artifactsChanged(collectionId);
      say(message);
      return true;
    } catch (e) { setError(errorText(e)); return false; }
    finally { setRepublishing(false); }
  };
  const showSuccessor = () => {
    if (!supersededBy) return;
    if (openArtifact({ collectionId, artifactId: supersededBy })) return;
    if (host.openArtifact) void host.openArtifact({ collectionId, artifactId: supersededBy }).catch(e => setError(errorText(e)));
  };
  const retiredBy = artifact.retired?.by?.kind === 'agent' ? 'an agent' : 'you';
  const copy = async (text: string, said: string) => { try { await navigator.clipboard.writeText(text); say(said); } catch (e) { setError(`Copy failed: ${errorText(e)}`); } };
  const exportContent = structured ? currentText : content;
  const downloadFile = async (payload: string, extension: string, mime: string) => {
    if (savePending.current) return;
    setError('');
    const filename = `${slug(artifact.title)}.${extension}`;
    savePending.current = true;
    try {
      if (saveArtifactExport) {
        if (await saveArtifactExport(filename, payload)) say('Artifact saved.');
        return;
      }
      const url = URL.createObjectURL(new Blob([payload], { type: `${mime};charset=utf-8` }));
      const link = document.createElement('a');
      link.href = url;
      link.download = filename;
      document.body.append(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      say(`Download started for ${filename}.`);
    } catch (error) { setError(`Could not export the artifact: ${errorText(error)}`); }
    finally { savePending.current = false; }
  };
  const download = (source = false) => {
    const payload = source ? content : exportContent;
    if (payload !== undefined) void downloadFile(payload, source ? 'json' : extension, source ? 'application/json' : mime);
  };
  const shownAt = version?.createdAtUnixMillis || artifact.updatedAtUnixMillis;
  const exportRendered = (format: 'print' | 'html') => {
    setError('');
    if (view !== 'document') { setView('document'); setPendingExport(format); return; }
    exportNow.current(format);
  };
  exportNow.current = format => {
    if (view !== 'document') return;
    const node = documentRef.current;
    if (!node) return;
    const sheet = { title: artifact.title, eyebrow: kindLabel[kind], meta: [`Version ${shown}${shown !== latestNumber ? ` of ${latestNumber}` : ''}`, new Date(Number(shownAt)).toLocaleDateString(undefined, { dateStyle: 'long' }), capital(versionOrigin(version)), collection?.name || ''].filter(Boolean) };
    const started = Date.now();
    const ready = () => {
      if (node !== documentRef.current) return;
      if (node.querySelector('.diagram-pending') && Date.now() - started < 4000) {
        say('Preparing diagrams…');
        setTimeout(ready, 120);
        return;
      }
      if (format === 'html') {
        const dark = document.documentElement.dataset.scheme === 'dark';
        void downloadFile(exportDocumentHTML(sheet, node, dark), 'html', 'text/html');
      } else {
        void printDocument(sheet, node).catch(error => setError(`Could not open the print dialog: ${errorText(error)}`));
      }
    };
    ready();
  };
  const sourceOf = (v?: ArtifactVersion) => v?.source?.kind === 'message' && v.source.sessionId && v.source.messageId ? v.source : undefined;
  const message = sourceOf(version) || (artifact.originSessionId ? sourceOf(versions.find(v => sourceOf(v))) : undefined);
  const filePath = artifact.trackedPath || (version?.source?.kind === 'file' ? version.source.path : '');
  const openMessage = () => {
    if (!message || !onShowSession) return;
    onShowSession({ collectionId, sessionId: message.sessionId, commandId: '' });
    revealMessage({ sessionId: message.sessionId, messageId: message.messageId });
  };
  const openFile = () => { if (filePath && workbench) workbench.open(filePath, { focus: true }); };
  const discussionReference = () => {
    const reference = agentReference(artifact, currentText ?? '', 1200, shown);
    return excerpt ? `${reference}Selected excerpt:\n${excerpt.split('\n').map(line => `> ${line}`).join('\n')}\n\n` : reference;
  };
  const sendToAgent = (text = discussionReference()) => {
    if (onDiscuss) {
      if (!discussionVersion && shown) {
        setDiscussionVersion(shown);
        setSelected(shown);
      }
      onDiscuss(artifact, text);
      return;
    }
    if (conversationId && onShowSession) {
      composeInConversation({ sessionId: conversationId, text });
      onShowSession({ collectionId, sessionId: conversationId, commandId: '' });
      say(`Added to the message box of “${conversationTitle || 'the conversation'}”. Review it and send.`);
    } else void copy(text, `Copied. Paste into a conversation in ${collection?.name || 'this workspace'}.`);
  };
  const pick = (number: number) => { setSelected(number === latestNumber ? 0 : number); setBase(0); };
  const compare = (number: number) => { pick(number); setView('changes'); if (!wide) setHistoryOpen(false); };
  const minutes = reading === undefined || Preview ? 0 : readingMinutes(reading);
  const discussLabel = chatOpen ? (excerpt ? 'Add selection to companion' : 'Add to companion') : (excerpt ? 'Discuss selection with agent companion' : 'Agent companion');
  const progress = artifact.kind === 'todo-list' && content ? todoProgress(content) : undefined;
  const historyLabel = `History · ${versions.length} version${versions.length === 1 ? '' : 's'}`;

  return <article ref={measureRef} className={`artifact-page${historyOpen ? ' has-history' : ''}${wide ? ' is-wide' : ''}${scrolled ? ' is-scrolled' : ''}`} aria-label={artifact.title} onKeyDown={toggleKey}>
    <header className="artifact-toolbar">
      {onBack && <button type="button" className="icon-button artifact-back" aria-label="Back to artifacts" title="Back to artifacts" onClick={onBack}><Icon name="back"/></button>}
      {/* The toolbar names the kind, and is where it changes: the title block has no kind pill to repeat it. */}
      <div className="artifact-crumb">
        <label className={`artifact-kind-picker kind-${kind}`} title={`${kindLabel[kind]}: choose another kind`}>
          <span className={`artifact-kind-dot kind-${kind}`} aria-hidden="true"><Icon name={kindIcon[kind]} size={13}/></span>
          <span className="artifact-kind-name" aria-hidden="true">{kindLabel[kind]}</span>
          <select disabled={structured} aria-label="Kind" value={kind} onChange={event => void update({ kind: event.target.value }, `Marked as ${kindLabel[asKind(event.target.value)].toLowerCase()}.`)}>{ARTIFACT_KINDS.filter(value => value !== 'todo-list' || kind === value).map(value => <option key={value} value={value}>{kindLabel[value]}</option>)}</select>
        </label>
        {(scrolled || collection?.name) && <span className="artifact-crumb-text" aria-hidden="true">{scrolled ? artifact.title : collection?.name}</span>}
      </div>
      <div className="artifact-segmented" role="tablist" aria-label="View">
        <button type="button" role="tab" aria-selected={view === 'document'} onClick={() => setView('document')}>{Preview ? 'Preview' : 'Document'}</button>
        <button type="button" role="tab" aria-selected={view === 'source'} onClick={() => { if (!selected && sourceDraft) setSelected(sourceDraft.version); setView('source'); }}>Source</button>
        <button type="button" role="tab" aria-selected={view === 'changes'} disabled={versions.length < 2 && shown <= 1} onClick={() => setView('changes')}>Changes</button>
      </div>
      <div className="artifact-page-actions">
        <span className="artifact-status" role="status" aria-live="polite">{status}</span>
        <button type="button" className="artifact-tool artifact-history-toggle" aria-expanded={historyOpen} aria-label={historyLabel} aria-keyshortcuts="H" title={`${historyLabel} (H)`} onClick={() => setHistoryOpen(!historyOpen)}><Icon name="history" size={15}/><span className="artifact-tool-label">History</span><span className="artifact-count">{versions.length}</span></button>
        <button type="button" className={onDiscuss ? "artifact-tool artifact-discuss" : "artifact-tool"}
          aria-label={onDiscuss ? discussLabel : (conversationId ? (excerpt ? 'Send selection to agent' : 'Send to agent') : 'Copy artifact reference')}
          onMouseDown={event => { if (excerpt) event.preventDefault(); }} onClick={() => sendToAgent()}
          title={onDiscuss ? 'Open the agent companion with this version in its draft' : conversationId ? `Put a reference in the message box of “${conversationTitle || 'the conversation'}” (not sent)` : 'Copy a reference an agent can read'}>
          <Icon name={onDiscuss ? 'chat' : 'send'} size={14}/><span className={onDiscuss ? 'artifact-companion-label' : 'artifact-tool-label'}>{onDiscuss ? (chatOpen ? 'Add to companion' : 'Agent companion') : conversationId ? (excerpt ? 'Send selection' : 'Send to agent') : 'Copy reference'}</span>
        </button>
        <ActionMenu label="Export" className="artifact-export-menu" summary={<><Icon name="download" size={14}/><span className="artifact-tool-label">Export</span></>}>
          <button type="button" disabled={exportContent === undefined} onClick={() => void copy(exportContent ?? '', `Copied ${formatLabel}.`)}><Icon name="copy"/><span>{Preview ? 'Copy HTML source' : 'Copy as Markdown'}</span></button>
          <button type="button" disabled={exportContent === undefined} onClick={() => download()}><Icon name="download"/><span>Download .{extension}</span></button>
          {artifact.kind === 'plan' && artifact.format !== 'html' && <button type="button" disabled={currentText === undefined} onClick={() => {
            try {
              const plan = roadmap ? typedMissionPlan(roadmap, artifact.title, artifactId, shown) : roadmapMissionPlan(currentText ?? '', artifact.title, artifactId, shown);
              void downloadFile(JSON.stringify(plan, null, 2), 'mission.json', 'application/json');
            } catch (e) { setError(errorText(e)); }
          }}><Icon name="download"/><span>Download mission plan</span></button>}
          {structured && <button type="button" disabled={content === undefined} onClick={() => download(true)}><Icon name="download"/><span>Download .json</span></button>}
          {!Preview && <><button type="button" disabled={reading === undefined} onClick={() => exportRendered('html')}><Icon name="download"/><span>Download .html</span></button>
          <button type="button" disabled={reading === undefined} onClick={() => exportRendered('print')}><Icon name="print"/><span>Print or save as PDF…</span></button>
          <button type="button" disabled={!collection?.members.length} onClick={() => setDialog('save')}><Icon name="repository"/><span>Save to repository…</span></button>
          <button type="button" disabled={!collection?.members.length} onClick={() => setDialog('issue')}><Icon name="external"/><span>Create GitHub issue…</span></button></>}
        </ActionMenu>
        {!standalone && host.openArtifact && <button type="button" className="artifact-tool artifact-open-window" aria-label="Open in its own window" title="Open in its own window" onClick={() => void host.openArtifact!({ collectionId, artifactId }).catch(e => setError(errorText(e)))}><Icon name="external" size={14}/></button>}
        <ActionMenu ref={moreMenu} label="Artifact actions" className="artifact-more-menu">
          {roadmap && <button type="button" disabled={shown !== latestNumber || !!artifact.retired || !!artifact.trackedPath} onClick={() => setRoadmapInput({ version: shown, markdown: '', existing: roadmap })}><Icon name="pencil"/><span>Edit roadmap milestones…</span></button>}
          <button type="button" disabled={content === undefined || !shown} onClick={() => setRoadmapInput({ version: shown, markdown: artifact.format === 'html' ? '' : currentText ?? '' })}><Icon name="artifact"/><span>Create implementation roadmap…</span></button>
          {artifact.kind === 'plan' && <button type="button" disabled={content === undefined} onClick={() => sendToAgent(
            `Prepare a mission proposal from this roadmap. Read the full pinned roadmap revision. Preserve milestone IDs, phases, dependencies, definitions of done, and open decisions in the task briefs. Carry forward existing verification evidence. Distinguish reported completion from owner acceptance. Leave execution for separate approval.\n\n${discussionReference()}`
          )}><Icon name="send"/><span>{conversationId || onDiscuss ? 'Draft mission proposal in chat…' : 'Copy mission proposal brief'}</span></button>}
          {onDiscuss && <button type="button" onClick={() => void copy(discussionReference(), 'Copied artifact reference.')}><Icon name="copy"/><span>Copy artifact reference</span></button>}
          <button type="button" onClick={() => { setView('document'); setRenaming(true); }}><Icon name="pencil"/><span>Rename</span></button>
          {!standalone && host.openArtifact && <button type="button" className="artifact-menu-window" onClick={() => void host.openArtifact!({ collectionId, artifactId }).catch(e => setError(errorText(e)))}><Icon name="external"/><span>Open in its own window</span></button>}
          <button type="button" disabled={!message || !onShowSession} title={!onShowSession && message ? 'Open it from the workspace window' : undefined} onClick={openMessage}><Icon name="chat"/><span>Open source message</span></button>
          <button type="button" disabled={!filePath || !workbench} onClick={openFile}><Icon name="file"/><span>Open source file</span></button>
          {artifact.trackedPath && <button type="button" onClick={() => void update({ stopTracking: true }, `Stopped following ${artifact.trackedPath}. Its versions are kept.`)}><Icon name="close"/><span>Stop syncing with file</span></button>}
          {artifact.retired
            ? <button type="button" onClick={() => void restore()}><Icon name="history"/><span>Restore</span></button>
            : <button type="button" onClick={() => void retire()}><Icon name="archive"/><span>Retire</span></button>}
          <button type="button" className="danger" onClick={() => setDialog('delete')}><Icon name="cross"/><span>Delete…</span></button>
        </ActionMenu>
      </div>
    </header>
    {(artifact.links || []).filter(link => link.kind === 'artifact').map(link => {
      const input = [...siblings.artifacts, ...siblings.retired].find(item => item.id === link.id);
      return <button type="button" className="link-button" key={link.id} disabled={!input} onClick={() => onRelated({ id: link.id, version: link.version }, shown)}>
        {link.role === 'review' ? 'Source review' : 'Source design'} · {input?.title ?? link.id} · v{link.version}{input && input.versions.length > link.version ? ' · newer revision available' : ''}
      </button>;
    })}
    {siblings.artifacts.filter(item => item.links?.some(link => link.kind === 'artifact' && link.id === artifactId)).map(plan =>
      <button type="button" key={plan.id} className="link-button" onClick={() => onRelated({ id: plan.id }, shown)}>Roadmap · {plan.title}</button>)}
    {error && <p className="artifact-error" role="alert">{error}<button type="button" className="link-button" onClick={() => setError('')}>Dismiss</button></p>}
    {updateReady && <div className="artifact-update-banner">
      <Icon name="history" size={16}/>
      <span role="status"><strong>v{latestNumber} is ready to review</strong><small>Compare with v{discussionVersion}, the version you started discussing.</small></span>
      <button type="button" className="primary" onClick={reviewUpdate}>Review update</button>
      <button type="button" onClick={readLatest}>Read latest</button>
    </div>}
    {artifact.retired && <div className="artifact-retired-banner" role="note" aria-label="Retired">
      <Icon name="archive" size={14}/>
      <span className="artifact-retired-text">
        <strong>Retired</strong>
        {supersededBy && <>: superseded by {successor
          ? <button type="button" className="link-button artifact-successor" title={`Open “${successor.title}”`} onClick={showSuccessor}>{successor.title}</button>
          : <span>{siblings.loaded ? 'an artifact since deleted' : '…'}</span>}</>}
        <small title={fullDate(artifact.retired.atUnixMillis)}>by {retiredBy} {ago(artifact.retired.atUnixMillis)}{artifact.retired.reason ? ` · ${artifact.retired.reason}` : ''} · kept with every version</small>
      </span>
      <button type="button" className="artifact-restore" onClick={() => void restore()}><Icon name="history" size={12}/>Restore</button>
    </div>}
    <div className="artifact-page-body">
      <main className="artifact-main">
        <div ref={scroller} className="artifact-scroll">
          {view !== 'changes' ? <div className={`doc-layout${view === 'document' && outline.length ? ' has-toc' : ''}`}>
            <div className="doc-column">
              <header className="doc-title-block">
                {(artifact.trackedPath || artifact.trackingProblem) && <div className="doc-eyebrow">
                  {artifact.trackedPath && <button type="button" className="artifact-tracked" disabled={!workbench} onClick={openFile} title="Versions are recorded when this file changes on disk"><Icon name="refresh" size={11}/><span>Synced with {artifact.trackedPath}</span></button>}
                  {artifact.trackingProblem && <span className="artifact-problem" role="status"><Icon name="alert" size={12}/>{artifact.trackingProblem}</span>}
                </div>}
                {renaming ? <RenameField value={artifact.title} onDone={title => { setRenaming(false); if (title && title !== artifact.title) void update({ title }, 'Renamed.'); }}/>
                  : <h1 className="doc-title" title="Double-click to rename" onDoubleClick={() => setRenaming(true)}>{artifact.title}</h1>}
                <div className="artifact-meta doc-meta">
                  <span className="doc-author"><span className="doc-author-icon"><Icon name={authorIcon(version)} size={12}/></span>{capital(versionOrigin(version))}</span>
                  <span className="doc-dot" aria-hidden="true"/>
                  <time dateTime={new Date(Number(shownAt)).toISOString()} title={fullDate(shownAt)}>{shown === latestNumber ? 'Updated' : 'Written'} {ago(shownAt)}</time>
                  <span className="doc-dot" aria-hidden="true"/>
                  {/* The shown version as text, the menu over it: a select sizes itself to its longest option. */}
                  <label className="doc-version" title="Choose a version"><span aria-hidden="true">v{shown}{shown === latestNumber ? ' · latest' : ''}</span>
                    <select aria-label="Version" value={shown} onChange={event => pick(Number(event.target.value))}>{[...versions].reverse().map(v => <option key={v.number} value={v.number}>v{v.number}{v.number === latestNumber ? ' · latest' : ` · ${ago(v.createdAtUnixMillis)}`}</option>)}</select>
                    <Icon name="chevron" size={11}/></label>
                  {view === 'document' && minutes > 0 && <><span className="doc-dot" aria-hidden="true"/><span>{minutes} min read</span></>}
                  {/* Where the outline has no room beside the text, it is one click away instead. */}
                  {view === 'document' && outline.length > 0 && <ActionMenu label="On this page" className="doc-contents-menu" summary={<><Icon name="checklist" size={12}/><span>On this page</span></>}>
                    {outline.map(entry => <button key={entry.id} type="button" className={entry.level > Math.min(...outline.map(e => e.level)) ? 'is-sub' : undefined} onClick={() => requestAnimationFrame(() => scroller.current?.querySelector<HTMLElement>(`#${CSS.escape(entry.id)}`)?.scrollIntoView({ block: 'start' }))}><span>{entry.text}</span></button>)}
                  </ActionMenu>}
                </div>
              </header>
              {shown !== latestNumber && (!updateReady || (!artifact.trackedPath && !artifact.retired)) && <div className="artifact-old-version" role="note">
                <Icon name="history" size={14}/><span>You are viewing v{shown} of {latestNumber}.</span>
                {!updateReady && <>
                  <button type="button" className="link-button" onClick={() => { setSelected(0); setBase(shown); setView('changes'); }}>What changed since</button>
                  <button type="button" className="link-button" onClick={readLatest}>Back to latest</button>
                </>}
                {!artifact.trackedPath && !artifact.retired && <button type="button" className="link-button" disabled={content === undefined || republishing}
                  onClick={() => content !== undefined && void saveRevision(content, `Republished v${shown}.`)}>Republish v{shown}</button>}
              </div>}
              {progress && view === 'document' && <div className="artifact-task-progress">
                <span>{progress.total - progress.remaining} of {progress.total} complete</span>
                {progress.total > 0 && <progress aria-label="Task completion" value={progress.total - progress.remaining} max={progress.total}/>}
                {progress.remaining > 0 && shown === latestNumber && !artifact.retired && <button type="button" className="link-button" onClick={() => sendToAgent(
                  agentReference(artifact, '', 0, shown) + 'Work through the items marked todo in this revision. Check the latest version before starting each item and record completed work using the same item IDs.'
                )}>{onDiscuss || (conversationId && onShowSession) ? 'Draft remaining tasks in chat' : 'Copy remaining tasks brief'}</button>}
              </div>}
              <div key={`${artifact.id}:${shown}:${view}`} ref={documentRef} className="artifact-document">{reading === undefined ? <p role="status" className="module-empty">Loading v{shown}…</p> : view === 'source' ? <ArtifactSource content={content ?? ''} language={structured ? 'json' : Preview ? 'html' : 'markdown'}
                version={shown} latest={latestNumber} editable={!!collection && collection.state !== 'deleted' && collection.state !== 'deleting' && !artifact.retired && !artifact.trackedPath} saving={republishing}
                draft={sourceDraft} onDraft={setSourceDraft} onPick={setSelected}
                onSave={(body, expectedVersion) => saveRevision(body, 'Saved a new version.', expectedVersion)}/> : artifact.kind === 'todo-list' && content ? <TodoList content={content} disabled={republishing || shown !== latestNumber || !!artifact.retired || !!artifact.trackedPath} onSave={body => void saveRevision(body, 'Updated TODO list.')}/> : Preview ? <Preview source={reading}/> : roadmap ? <RoadmapDocument plan={roadmap} headings={headings} scroller={scroller} onReviewShell={reviewShell}/> : <DocumentBody markdown={reading} sections={fieldDocument?.sections} headings={headings} scroller={scroller} onReviewShell={reviewShell}/>}</div>
            </div>
            {view === 'document' && outline.length > 0 && <aside className="doc-toc-rail"><TableOfContents entries={outline} active={active} scroller={scroller}/></aside>}
          </div>
            : <div className="artifact-changes">
              <div className="artifact-changes-bar">
                <h2 ref={changesHeading} tabIndex={-1}>Changes</h2>
                {shown > 1 && <label className="artifact-compare">from <select aria-label="Compare with" value={baseNumber} onChange={event => setBase(Number(event.target.value))}>{versions.filter(v => v.number < shown).reverse().map(v => <option key={v.number} value={v.number}>v{v.number}{v.number === shown - 1 ? ' (previous)' : ''}</option>)}</select> to <strong>v{shown}</strong>{diff.length > 0 && <DiffStats lines={diff}/>}</label>}
                {inDiscussionReview && <button type="button" className="link-button" onClick={() => {
                  setSelected(discussionVersion || 0);
                  setReviewingVersion(undefined);
                  setView('document');
                }}>Back to v{discussionVersion}</button>}
                {inDiscussionReview && !updateReady && <button type="button" className="link-button" onClick={readLatest}>Read latest</button>}
                {artifact.kind !== 'todo-list' && !Preview && <div className="artifact-segmented" role="radiogroup" aria-label="Show changes as">
                  <button type="button" role="radio" aria-checked={diffMode === 'rendered'} onClick={() => setDiffMode('rendered')}>Rendered</button>
                  <button type="button" role="radio" aria-checked={diffMode === 'text'} onClick={() => setDiffMode('text')}>Text</button>
                </div>}
              </div>
              {content === undefined || baseContent === undefined ? <p role="status" className="module-empty">Loading versions…</p>
                : artifact.kind !== 'todo-list' && !Preview && diffMode === 'rendered' ? <RenderedDiff key={`${baseNumber}:${shown}`} before={withoutRepeatedTitle(previousText ?? '', artifact.title)} after={withoutRepeatedTitle(currentText ?? '', artifact.title)}
                  beforeLabel={<VersionLabel number={baseNumber} version={versions.find(v => v.number === baseNumber)} tone="before"/>} afterLabel={<VersionLabel number={shown} version={version} tone="after"/>}/>
                : <DiffView key={`${baseNumber}:${shown}`} lines={diff} label={`Changes from v${baseNumber} to v${shown}`}/>}
            </div>}
        </div>
      </main>
      {historyOpen && <Resizer split={SPLITS.artifactHistory} label="version history"/>}
      {historyOpen && <aside className="artifact-history" aria-label="Version history">
        <header><h3>History</h3><span>{versions.length} version{versions.length === 1 ? '' : 's'}</span><button type="button" className="icon-button" aria-label="Close history" title="Close history (H)" onClick={() => setHistoryOpen(false)}><Icon name="close" size={13}/></button></header>
        <ol role="listbox" aria-label="Versions" onKeyDown={event => {
          const step = event.key === 'ArrowDown' ? -1 : event.key === 'ArrowUp' ? 1 : 0;
          if (!step) return;
          event.preventDefault();
          const next = versions.find(v => v.number === shown + step);
          // React clears currentTarget once the handler returns: keep the list for the next frame.
          const list = event.currentTarget;
          if (next) { pick(next.number); requestAnimationFrame(() => list.querySelector<HTMLElement>('[aria-selected="true"]')?.focus()); }
        }}>
          {[...versions].reverse().map(v => <li key={v.number} role="option" aria-selected={v.number === shown} tabIndex={v.number === shown ? 0 : -1}
            onClick={() => { pick(v.number); setView('document'); if (!wide) setHistoryOpen(false); }}
            onKeyDown={event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); pick(v.number); setView('document'); if (!wide) setHistoryOpen(false); } }}>
            <span className="history-dot" aria-hidden="true"><Icon name={authorIcon(v)} size={11}/></span>
            <span className="history-text">
              <strong>v{v.number}{v.number === latestNumber && <em>Latest</em>}</strong>
              <span>{capital(versionOrigin(v))}</span>
              <time dateTime={new Date(Number(v.createdAtUnixMillis)).toISOString()} title={fullDate(v.createdAtUnixMillis)}>{ago(v.createdAtUnixMillis)}</time>
              {v.number > 1 && <button type="button" className="history-compare" tabIndex={-1} onClick={event => { event.stopPropagation(); compare(v.number); }}><Icon name="output" size={11}/>Compare with v{v.number - 1}</button>}
            </span>
          </li>)}
        </ol>
      </aside>}
    </div>
    {terminal && <section className="artifact-terminal" aria-label="Artifact terminal">
      <header><strong>Terminal</strong><span>Review the command at the prompt, then press Return.</span>
        <button type="button" className="icon-button" aria-label="Hide artifact terminal" onClick={() => setTerminal(undefined)}><Icon name="close" size={14}/></button>
      </header>
      <TerminalPanel key={terminal.id} workspaceId={collectionId} collections={collection ? [collection] : []} initialTerminalId={terminal.id} initialInput={terminal.command}
        sessionId={conversationId} onShowSession={onShowSession} onClosePanel={() => setTerminal(undefined)}/>
    </section>}
    {roadmapInput && <RoadmapDialog source={artifact} {...roadmapInput} reviews={siblings.artifacts.filter(item => item.kind === 'report' && item.id !== artifactId && item.versions.length > 0)}
      onClose={() => setRoadmapInput(undefined)} onSaved={plan => { setRoadmapInput(undefined); say('Roadmap saved.'); if (roadmapInput.existing) { setArtifact(plan); setSelected(0); } else onRelated({ id: plan.id }, shown); }}/>}
    {dialog === 'delete' && <DeleteArtifactDialog artifact={artifact} onClose={() => setDialog(undefined)} onDeleted={() => { setDialog(undefined); onDeleted?.(); }}/>}
    {dialog === 'save' && <SaveToRepositoryDialog artifact={artifact} collection={collection} onClose={() => setDialog(undefined)} onSaved={(path, next) => { setDialog(undefined); if (next) setArtifact(next); artifactsChanged(collectionId); say(`Saved to ${path}. It is not committed.`); }}/>}
    {dialog === 'issue' && <CreateIssueDialog artifact={artifact} collection={collection} onClose={() => setDialog(undefined)} onCreated={(url, number) => { setDialog(undefined); say(`Created issue #${number}.`); if (/^https:\/\//.test(url)) window.open(url, '_blank', 'noopener,noreferrer'); }}/>}
  </article>;
}

function VersionLabel({ number, version, tone }: { number: number; version?: ArtifactVersion; tone: 'before' | 'after' }) {
  return <span className={`doc-diff-label is-${tone}`}><strong>v{number}</strong><span>{capital(versionOrigin(version))}</span>{version && <time title={fullDate(version.createdAtUnixMillis)}>{ago(version.createdAtUnixMillis)}</time>}</span>;
}

function RenameField({ value, onDone }: { value: string; onDone(title: string): void }) {
  const [draft, setDraft] = useState(value);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => { input.current?.select(); }, []);
  return <input ref={input} className="artifact-rename" aria-label="Artifact title" value={draft} maxLength={200} autoFocus
    onChange={event => setDraft(event.target.value)}
    onBlur={() => onDone(draft.trim())}
    onKeyDown={event => { if (event.key === 'Enter') { event.preventDefault(); onDone(draft.trim()); } if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onDone(''); } }}/>;
}

/**
 * Print only the document, as a document: a cover title block (kind, title,
 * version, date, author), the rendered text with its diagrams, page margins
 * and page numbers, and none of the app. The sheet sits beside the app,
 * shown by the print stylesheet alone, and is removed after the print dialog
 * closes. "Save as PDF" is the system print dialog's own choice.
 */
export async function printDocument(metadata: DocumentHeading, rendered: HTMLElement) {
  dismissPrintSheet?.();
  const sheet = document.createElement('div');
  sheet.className = 'artifact-print-sheet';
  const { heading, body } = documentSnapshot(metadata, rendered);
  sheet.append(heading, body);
  document.body.append(sheet);
  document.documentElement.classList.add('printing-artifact');
  const previous = document.title;
  document.title = metadata.title;
  const cleanup = () => {
    if (dismissPrintSheet !== cleanup) return;
    dismissPrintSheet = undefined;
    clearTimeout(fallback);
    sheet.remove();
    document.documentElement.classList.remove('printing-artifact');
    document.title = previous;
    window.removeEventListener('afterprint', cleanup);
  };
  // Native print returns before its dialog closes. Allow another attempt if
  // the platform never reports afterprint.
  const fallback = setTimeout(cleanup, 60_000);
  dismissPrintSheet = cleanup;
  window.addEventListener('afterprint', cleanup);
  try {
    // Tauri's macOS implementation returns a Promise despite the DOM type.
    await window.print();
  } catch (error) {
    cleanup();
    throw error;
  }
}
