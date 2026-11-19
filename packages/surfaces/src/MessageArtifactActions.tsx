import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ArtifactCard } from './ArtifactCard';
import { OTHER_ARTIFACTS_POLL_MS, useArtifacts } from './useArtifacts';
import type { Artifact } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';
import { useContextMenuContribution } from './ContextMenu';
import type { MessageMenuDetail } from './chat-context-menu';
import { TrackArtifactDialog, UpdateArtifactDialog } from './ArtifactDialogs';
import { openArtifact } from './artifact-events';
import { linkedMarkdownFiles, mostlyNamesFile, trackableFile } from './artifact-model';
import { linkedWorkspaceFiles } from './FileArtifactActions';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import type { TranscriptMessage } from './transcript-model';
import './artifacts.css';

/** The conversation a message list shows, for its message actions. */
export type MessageScope = { collectionId: string; sessionId: string; conversationId: string };
const MessageScopeContext = createContext<MessageScope | undefined>(undefined);
/** The workspace's artifacts by the message each version was tracked from, for the cards under those messages. */
type TrackedFrom = Map<string, { artifact: Artifact; version: number }[]>;
const TrackedContext = createContext<TrackedFrom>(new Map());
/**
 * The conversation a message list shows. It keeps the workspace's artifact
 * list (shared with the panel and the header chip, so one poll for all) to
 * put a card under each message that was tracked.
 */
export function MessageScopeProvider({ scope, children }: { scope?: MessageScope; children: ReactNode }) {
  const { artifacts } = useArtifacts(scope?.collectionId || '', OTHER_ARTIFACTS_POLL_MS);
  const tracked = useMemo(() => scope ? trackedMessages(artifacts, scope) : new Map() as TrackedFrom, [artifacts, scope]);
  return <MessageScopeContext.Provider value={scope}><TrackedContext.Provider value={tracked}>{children}</TrackedContext.Provider></MessageScopeContext.Provider>;
}
export const useMessageScope = () => useContext(MessageScopeContext);

/** Each message of this conversation that a version was tracked from, with the newest such version per artifact. */
export function trackedMessages(artifacts: readonly Artifact[], scope: MessageScope): TrackedFrom {
  const out: TrackedFrom = new Map();
  for (const artifact of artifacts) for (const version of artifact.versions) {
    const source = version.source;
    if (source?.kind !== 'message' || !source.messageId || !(source.sessionId === scope.sessionId || (!!scope.conversationId && source.conversationId === scope.conversationId))) continue;
    const list = out.get(source.messageId) || [];
    const same = list.find(entry => entry.artifact.id === artifact.id);
    if (same) same.version = Math.max(same.version, version.number); else list.push({ artifact, version: version.number });
    out.set(source.messageId, list);
  }
  return out;
}

/** Only an agent's prose can become an artifact; prompts, notices and failures cannot. */
export const trackable = (message: TranscriptMessage) => message.type === 'message_delta' && !!message.text.trim();

/** The artifacts first taken from this conversation, and whether `message` is newer than what each holds. */
export function conversationArtifacts(artifacts: Artifact[], scope: MessageScope) {
  return artifacts.filter(a => (scope.conversationId && a.originConversationId === scope.conversationId) || a.originSessionId === scope.sessionId);
}
const newerThanLatest = (artifact: Artifact, scope: MessageScope, message: TranscriptMessage) => {
  if (artifact.format && artifact.format !== 'markdown') return false;
  const source = artifact.versions.at(-1)?.source;
  if (source?.kind !== 'message' || source.sessionId !== scope.sessionId) return true;
  try { return BigInt(message.id) > BigInt(source.messageId); } catch { return message.id !== source.messageId; }
};

type Open = { kind: 'track'; existing: Artifact[]; files: string[]; preferFile: boolean } | { kind: 'update'; artifact: Artifact };

/** The workspace files a message's Markdown links name, for tracking the file instead of the words; none when they cannot be resolved. */
async function messageFiles(api: RunnerClient, collectionId: string, text: string): Promise<string[]> {
  const links = linkedMarkdownFiles(text).slice(0, 4);
  const resolved = await Promise.all(links.map(link => linkedWorkspaceFiles(api, collectionId, link).then(found => found.paths, () => [])));
  return [...new Set(resolved.flat().filter(trackableFile))].slice(0, 4);
}

/**
 * The artifact actions of one agent message: a hover button, and "Track as
 * artifact…" in the message's context menu (the `message` scope of the app's
 * context menus). The track dialog offers updating an existing artifact, and
 * tracking a Markdown file the message links instead of its text: first when
 * the message mostly names the file. A right-click on a file link has the
 * link's own "Track as artifact" (`useFileLinkTracking`), so the message's
 * item says it is about the message.
 */
export function MessageArtifactActions({ message }: { message: TranscriptMessage }) {
  const scope = useContext(MessageScopeContext), tracked = useContext(TrackedContext);
  const { api } = useRunner();
  const anchor = useRef<HTMLSpanElement>(null);
  const [open, setOpen] = useState<Open>(), [note, setNote] = useState('');
  const enabled = !!scope?.collectionId && trackable(message);
  const fresh = useRef(true);
  useEffect(() => { fresh.current = true; return () => { fresh.current = false; }; }, []);
  const source = scope ? { kind: 'message', sessionId: scope.sessionId, conversationId: scope.conversationId, messageId: message.id, turnId: message.turnId } : {};
  const existing = async () => {
    if (!scope) return [];
    try { return conversationArtifacts((await api.listArtifacts({ collectionId: scope.collectionId }, { timeoutMs: 5000 })).artifacts, scope); }
    catch { return []; }
  };
  const done = (artifact: Artifact, verb: string) => {
    setOpen(undefined);
    if (!openArtifact({ collectionId: artifact.collectionId, artifactId: artifact.id })) flash(`${verb} “${artifact.title}”. Open it from the Artifacts panel of this workspace.`);
  };
  const flash = (text: string) => { setNote(text); setTimeout(() => { if (fresh.current) setNote(''); }, 6000); };
  const startTrack = async (fromLink = false) => {
    const [found, files] = await Promise.all([existing(), scope ? messageFiles(api, scope.collectionId, message.text) : []]);
    if (fresh.current) setOpen({ kind: 'track', existing: found, files, preferFile: !fromLink && mostlyNamesFile(message.text) });
  };
  useContextMenuContribution<MessageMenuDetail>('message', async ({ detail, element, target }) => {
    if (!enabled || !scope || detail.message.id !== message.id) return [];
    const onLink = !!target.closest('.file-reference') && element.contains(target);
    const updates = (await existing()).filter(a => newerThanLatest(a, scope, message)).slice(0, 4);
    return ['separator', { label: onLink ? 'Track this message as artifact…' : 'Track as artifact…', icon: 'artifact', run: () => void startTrack(onLink) },
      ...updates.map(artifact => ({ label: `Update “${artifact.title}” from this message…`, icon: 'history', run: () => setOpen({ kind: 'update', artifact }) }))];
  });
  if (!enabled || !scope) return null;
  const cards = tracked.get(message.id);
  return <>{cards?.length ? <div className="artifact-cards">
    {cards.map(({ artifact, version }) => <ArtifactCard key={artifact.id} collectionId={scope.collectionId} artifactId={artifact.id} title={artifact.title} kind={artifact.kind} version={version} how={version > 1 ? 'updated from this message' : 'tracked from this message'}/>)}
  </div> : null}<span ref={anchor} className="message-artifact-actions">
    <button type="button" className="message-artifact-button" aria-label="Track as artifact…" title="Track as artifact… (or right-click the message)" onClick={() => void startTrack()}
>
      <Icon name="artifact" size={14}/>
    </button>
    {note && <span className="message-artifact-note" role="status">{note}</span>}
    {open?.kind === 'track' && <TrackArtifactDialog collectionId={scope.collectionId} content={message.text} source={source} existing={open.existing.filter(a => newerThanLatest(a, scope, message))} files={open.files} preferFile={open.preferFile}
      onClose={() => setOpen(undefined)} onTracked={artifact => done(artifact, 'Tracked')} onUpdateInstead={artifact => setOpen({ kind: 'update', artifact })}/>}
    {open?.kind === 'update' && <UpdateArtifactDialog collectionId={scope.collectionId} artifactId={open.artifact.id} content={message.text} source={source}
      onClose={() => setOpen(undefined)} onSaved={artifact => done(artifact, 'Updated')}/>}
  </span></>;
}
