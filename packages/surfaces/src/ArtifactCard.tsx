import { useContext, useEffect, useMemo, useRef, useState } from 'react';
import { ArtifactHostContext } from './ArtifactPage';
import { artifactChip, asKind, kindIcon, kindLabel } from './artifact-model';
import { openArtifact } from './artifact-events';
import { errorText, restoreArtifacts } from './artifact-client';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import type { PublishedArtifact, RetiredArtifact } from './tool-lines';
import { OTHER_ARTIFACTS_POLL_MS, useArtifacts } from './useArtifacts';
import './artifacts.css';

/**
 * An artifact in the conversation: what an agent published (`lazybox artifact
 * put`) or a message tracked, where it happened. Open shows its page in the
 * workspace's Artifacts panel; an update also offers its changes.
 */
export function ArtifactCard({ collectionId, artifactId, title, kind, version, how }: {
  collectionId: string;
  artifactId: string;
  title: string;
  kind: string;
  /** The version this card is about: the page opens on it. */
  version: number;
  /** What happened, in a few words: "Published by the agent", "Tracked from this message". */
  how: string;
}) {
  const host = useContext(ArtifactHostContext);
  const [note, setNote] = useState('');
  const timer = useRef(0);
  useEffect(() => () => clearTimeout(timer.current), []);
  const shown = asKind(kind);
  const open = (compare: boolean) => {
    const request = { collectionId, artifactId, version, ...(compare ? { compareWith: version - 1 } : {}) };
    if (openArtifact(request)) return;
    // No workspace in this window (a chat in its own window): the page in a window of its own.
    if (host.openArtifact) { void host.openArtifact({ collectionId, artifactId }).catch(() => setNote('Open it from the Artifacts panel of its workspace.')); return; }
    setNote('Open it from the Artifacts panel of its workspace.');
    clearTimeout(timer.current); timer.current = window.setTimeout(() => setNote(''), 6000);
  };
  return <div className="artifact-card" role="group" aria-label={`Artifact “${title}”, v${version}`} data-artifact-id={artifactId}>
    <span className={`artifact-kind-icon kind-${shown}`} aria-hidden="true"><Icon name={kindIcon[shown]} size={15}/></span>
    <span className="artifact-card-text">
      <strong title={title}>{title}</strong>
      <small>{kindLabel[shown]} · v{version} · {how}</small>
    </span>
    <span className="artifact-card-actions">
      {version > 1 && <button type="button" onClick={() => open(true)}>Compare with v{version - 1}</button>}
      <button type="button" className="artifact-card-open" onClick={() => open(false)}>Open</button>
    </span>
    {note && <small className="artifact-card-note" role="status">{note}</small>}
  </div>;
}

const quoted = (list: readonly { title: string }[]) => list.map(r => `“${r.title}”`).join(', ');

/**
 * What an agent retired, where it happened: a merge ("Merged 3 artifacts into
 * “Status”", from `put --supersedes`) or a retirement (`lazybox artifact
 * retire`). Undo restores every one still retired; once restored, the card
 * says so. Open shows the artifact merged into, or the one retired.
 */
export function RetiredArtifactsCard({ collectionId, retired, text, into }: {
  collectionId: string;
  retired: readonly RetiredArtifact[];
  /** "Merged 3 artifacts into “Status”", "Retired “Limitations”". */
  text: string;
  /** The artifact the others were merged into. */
  into?: PublishedArtifact;
}) {
  const { api } = useRunner();
  const host = useContext(ArtifactHostContext);
  const { artifacts, retired: retiredNow, loaded, reload } = useArtifacts(collectionId, OTHER_ARTIFACTS_POLL_MS);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const [undone, setUndone] = useState<string>();
  const card = useRef<HTMLDivElement>(null);
  const ids = retired.map(r => r.id);
  const still = retiredNow.filter(a => ids.includes(a.id));
  // Retired again after an Undo (a new retirement time) offers Undo again.
  const stamp = still.map(a => `${a.id}:${a.retired?.atUnixMillis}`).join(',');
  const canUndo = still.length > 0 && stamp !== undone;
  // The list may predate the agent's call (its output arrives first): a merge's list is current once it has the
  // artifact merged into; a retirement's, once it showed one retired. Until then the card says what the agent did.
  const [seen, setSeen] = useState(false);
  useEffect(() => { if (still.length) setSeen(true); }, [still.length]);
  useEffect(() => { reload(); }, [reload]);
  const current = into ? [...artifacts, ...retiredNow].some(a => a.id === into.id) : seen;
  const restored = !canUndo && (undone !== undefined || (loaded && current && ids.every(id => artifacts.some(a => a.id === id))));
  const undo = async () => {
    setBusy(true); setError('');
    try { await restoreArtifacts(api, collectionId, still.map(a => a.id)); setUndone(stamp); card.current?.focus(); }
    catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  };
  const target = into ? { id: into.id, version: into.version } : retired.length === 1 ? { id: retired[0].id } : undefined;
  const open = () => {
    if (!target) return;
    if (openArtifact({ collectionId, artifactId: target.id, ...(target.version ? { version: target.version } : {}) })) return;
    if (host.openArtifact) void host.openArtifact({ collectionId, artifactId: target.id }).catch(() => setError('Open it from the Artifacts panel of its workspace.'));
    else setError('Open it from the Artifacts panel of its workspace.');
  };
  const single = retired.length === 1 ? retired[0] : undefined;
  const detail = restored ? `Undone: ${quoted(retired)} ${retired.length === 1 ? 'is' : 'are'} back in the list`
    : into ? `Retired ${quoted(retired)} · kept with ${retired.length === 1 ? 'its' : 'their'} versions`
    : single?.supersededBy ? `Superseded by “${single.supersededBy.title}” · kept with its versions`
    : `Kept with ${retired.length === 1 ? 'its' : 'their'} versions`;
  return <div ref={card} tabIndex={-1} className={`artifact-card is-retired${restored ? ' is-restored' : ''}`} role="group" aria-label={text} data-retired-ids={ids.join(',')}>
    <span className="artifact-kind-icon kind-other" aria-hidden="true"><Icon name={into ? 'merge' : 'archive'} size={15}/></span>
    <span className="artifact-card-text">
      <strong title={text}>{text}</strong>
      <small title={detail} role="status">{detail}</small>
    </span>
    <span className="artifact-card-actions">
      {canUndo && <button type="button" disabled={busy} aria-label={`Undo: restore ${still.length === 1 ? `“${still[0].title}”` : `${still.length} artifacts`}`} onClick={() => void undo()}><Icon name="history" size={12}/>{busy ? 'Restoring…' : 'Undo'}</button>}
      {target && <button type="button" className="artifact-card-open" onClick={open}>Open</button>}
    </span>
    {error && <small className="artifact-card-note" role="alert">{error}</small>}
  </div>;
}

/**
 * The chat header's "3 artifacts": this conversation's artifacts, else the
 * workspace's. It opens the Artifacts panel on them; with none it is not there.
 */
export function ArtifactsChip({ collectionId, sessionId, conversationId, title }: { collectionId: string; sessionId: string; conversationId: string; title?: string }) {
  const { artifacts } = useArtifacts(collectionId, OTHER_ARTIFACTS_POLL_MS);
  const host = useContext(ArtifactHostContext);
  const chip = useMemo(() => artifactChip(artifacts, { sessionId, conversationId }), [artifacts, sessionId, conversationId]);
  if (!chip) return null;
  const show = () => {
    if (openArtifact({ collectionId, artifactId: '', ...(chip.mine ? { conversation: { sessionId, conversationId, title } } : {}) })) return;
    // No workspace in this window: the newest one in a window of its own.
    const newest = [...artifacts].sort((a, b) => Number(b.updatedAtUnixMillis) - Number(a.updatedAtUnixMillis))[0];
    if (newest && host.openArtifact) void host.openArtifact({ collectionId, artifactId: newest.id });
  };
  return <button type="button" className="chat-artifacts-button" title={`${chip.title}\nShow them in the Artifacts panel`} aria-label={`${chip.title}: show them`} onClick={show}>
    <Icon name="artifact" size={14}/><span>{chip.label}</span>
  </button>;
}
