import { useEffect, useMemo, useRef, useState } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import { artifactsChanged, errorText } from './artifact-client';
import type { Artifact, ArtifactSource, Collection } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { RichContent } from './RichContent';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { ARTIFACT_KINDS, asKind, diffStats, guessKind, hunks, kindIcon, kindLabel, lineDiff, suggestedPath, titleFromMarkdown, type ArtifactKind, type DiffLine } from './artifact-model';
import { newMilestone, milestoneStatuses, type Milestone, type Roadmap } from './roadmap';
import { extractToc } from './artifact-reader';
import './artifacts.css';

export { errorText } from './artifact-client';
type SourceInput = Partial<Pick<ArtifactSource, 'kind' | 'sessionId' | 'conversationId' | 'messageId' | 'turnId' | 'path'>>;

function useModal(onClose: () => void, busy: boolean) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  return { ref: dialog, onCancel: (event: { preventDefault(): void }) => { event.preventDefault(); if (!busy) onClose(); } };
}

function KindPicker({ value, onChange, disabled }: { value: ArtifactKind; onChange(kind: ArtifactKind): void; disabled?: boolean }) {
  return <div className="artifact-kind-picker" role="radiogroup" aria-label="Kind">
    {ARTIFACT_KINDS.map(kind => <button key={kind} type="button" role="radio" aria-checked={value === kind} disabled={disabled} onClick={() => onChange(kind)}
      onKeyDown={event => {
        const step = event.key === 'ArrowRight' || event.key === 'ArrowDown' ? 1 : event.key === 'ArrowLeft' || event.key === 'ArrowUp' ? -1 : 0;
        if (!step) return;
        event.preventDefault();
        const next = ARTIFACT_KINDS[(ARTIFACT_KINDS.indexOf(value) + step + ARTIFACT_KINDS.length) % ARTIFACT_KINDS.length];
        onChange(next);
        // React clears currentTarget once the handler returns: keep the group for the next frame.
        const group = event.currentTarget.parentElement;
        requestAnimationFrame(() => group?.querySelector<HTMLElement>('[aria-checked="true"]')?.focus());
      }} tabIndex={value === kind ? 0 : -1}><Icon name={kindIcon[kind]} size={14}/><span>{kindLabel[kind]}</span></button>)}
  </div>;
}

const measure = (text: string) => { const lines = text.split('\n').length, words = text.trim() ? text.trim().split(/\s+/).length : 0; return `${lines.toLocaleString()} line${lines === 1 ? '' : 's'} · ${words.toLocaleString()} word${words === 1 ? '' : 's'}`; };

/**
 * "Track as artifact…" for a chat message: a title taken from its first
 * heading, a suggested kind, and a preview. If this conversation already has
 * an artifact, updating it is one click away. When the message links Markdown
 * files of the workspace (`files`, workspace-relative), tracking one of them
 * comes first: the artifact then follows the file on disk. `preferFile`
 * chooses it already, for a message that mostly names the file.
 */
export function TrackArtifactDialog({ collectionId, content, source, existing = [], files = [], preferFile = false, onClose, onTracked, onUpdateInstead }: { collectionId: string; content: string; source: SourceInput; existing?: Artifact[]; files?: string[]; preferFile?: boolean; onClose(): void; onTracked(artifact: Artifact): void; onUpdateInstead?(artifact: Artifact): void }) {
  const { api } = useRunner();
  const suggested = useMemo(() => titleFromMarkdown(content, 'Untitled artifact'), [content]);
  const [title, setTitle] = useState(suggested), [kind, setKind] = useState<ArtifactKind>(() => guessKind(suggested, content));
  const [kindTouched, setKindTouched] = useState(false);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  // The file to track instead of the message's text, or '' for the text.
  const [file, setFile] = useState(() => preferFile ? files[0] || '' : '');
  const modal = useModal(onClose, busy);
  const titleInput = useRef<HTMLInputElement>(null), fileChoice = useRef<HTMLInputElement>(null);
  // The title is ready to type over; a file chosen already has the focus instead, so Enter tracks it.
  useEffect(() => { titleInput.current?.select(); fileChoice.current?.focus(); }, []);
  const submit = async () => {
    if ((!file && !title.trim()) || busy) return;
    setBusy(true); setError('');
    try {
      const tracked = file ? await api.trackArtifactFile({ collectionId, path: file }, { timeoutMs: 15000 }) : await api.putArtifact({ collectionId, title: title.trim(), kind, content, source }, { timeoutMs: 15000 });
      artifactsChanged(collectionId); onTracked(tracked);
    } catch (e) { setError(errorText(e)); setBusy(false); }
  };
  return <DraggableDialog ref={modal.ref} className="artifact-dialog" aria-label="Track as artifact" onCancel={modal.onCancel}>
    <DialogHeader><h2><Icon name="artifact" size={18}/>Track as artifact</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}><Icon name="close"/></button></DialogHeader>
    <p className="artifact-dialog-lead">It becomes its own page in this workspace, with versions you can compare, export and hand to an agent.</p>
    <form onSubmit={event => { event.preventDefault(); void submit(); }}>
      {files.length > 0 && <fieldset className="artifact-source-choice" disabled={busy}><legend>What to track</legend>
        {files.map(path => <label key={path} className="artifact-check"><input type="radio" name="artifact-source" ref={file === path ? fileChoice : undefined} checked={file === path} onChange={() => setFile(path)}/><span>Track <code>{path}</code><small>Stays in sync with the file: its later edits, by you or an agent, become new versions.</small></span></label>)}
        <label className="artifact-check"><input type="radio" name="artifact-source" checked={!file} onChange={() => setFile('')}/><span>Track this message’s text<small>A page of its own made from what the agent wrote here.</small></span></label>
      </fieldset>}
      {!file && <>
      <label className="artifact-field"><span>Title</span><input ref={titleInput} data-autofocus value={title} maxLength={200} disabled={busy} onChange={event => { setTitle(event.target.value); if (!kindTouched) setKind(guessKind(event.target.value, content)); }}/></label>
      <div className="artifact-field"><span id="artifact-kind-label">Kind</span><KindPicker value={kind} disabled={busy} onChange={next => { setKind(next); setKindTouched(true); }}/></div>
      {existing.length > 0 && onUpdateInstead && <div className="artifact-existing" role="note"><Icon name="history" size={14}/><span>This conversation already has {existing.length === 1 ? <>“{existing[0].title}”</> : `${existing.length} artifacts`}.</span>{existing.slice(0, 3).map(artifact => <button key={artifact.id} type="button" className="link-button" onClick={() => onUpdateInstead(artifact)}>Update “{artifact.title}” instead</button>)}</div>}
      <section className="artifact-preview" aria-label="Preview"><header><span>Preview</span><small>{measure(content)}</small></header><div className="artifact-preview-body"><RichContent text={content}/></div></section>
      </>}
      {error && <p className="artifact-error" role="alert">{error}</p>}
      <footer><button type="button" disabled={busy} onClick={onClose}>Cancel</button><button type="submit" className="primary" disabled={busy || (!file && !title.trim())}>{busy ? 'Tracking…' : file ? 'Track file' : 'Track artifact'}</button></footer>
    </form>
  </DraggableDialog>;
}

export function RoadmapDialog({ source, version, markdown, existing, reviews, onClose, onSaved }: {
  source: Artifact; version: number; markdown: string; existing?: Roadmap; reviews: Artifact[]; onClose(): void; onSaved(plan: Artifact): void;
}) {
  const { api } = useRunner();
  const [creationKey] = useState(() => crypto.randomUUID());
  const [title, setTitle] = useState(() => existing ? source.title : `Implementation roadmap · ${source.title}`.slice(0, 200));
  const [milestones, setMilestones] = useState(() => {
    if (existing) return existing.milestones;
    const headings = extractToc(markdown).filter(entry => entry.level === 2).map(entry => entry.text);
    return (headings.length ? headings : [source.title]).map((name, index) => newMilestone('M' + (index + 1), name));
  });
  const edit = (index: number, patch: Partial<Milestone>) =>
    setMilestones(rows => rows.map((row, i) => i === index ? { ...row, ...patch } : row));
  const [review, setReview] = useState<{ id: string; version: number }>();
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const modal = useModal(onClose, busy);
  const save = async () => {
    if (busy || !title.trim() || !milestones.length) return;
    setBusy(true); setError('');
    try {
      const links = [{ kind: 'artifact', id: source.id, role: source.kind === 'report' ? 'review' : 'design', version }];
      if (review) links.push({ kind: 'artifact', role: 'review', ...review });
      const clean = (rows: string[]) => rows.map(value => value.trim()).filter(Boolean);
      const document: Roadmap = { schema: 'roadmap.v1', milestones: milestones.map(m => ({ ...m,
        dependsOn: clean(m.dependsOn), definitionOfDone: clean(m.definitionOfDone), evidence: clean(m.evidence), openDecisions: clean(m.openDecisions),
      })) };
      const request = existing
        ? { artifactId: source.id, expectedVersion: version }
        : { creationKey, title: title.trim(), kind: 'plan', format: 'json', links };
      const plan = await api.putArtifact({ ...request, collectionId: source.collectionId, content: JSON.stringify(document, null, 2) });
      artifactsChanged(source.collectionId); onSaved(plan);
    } catch (e) { setBusy(false); setError(errorText(e)); }
  };
  const label = existing ? 'Edit roadmap milestones' : 'Create implementation roadmap';
  return <DraggableDialog ref={modal.ref} className="artifact-dialog" aria-label={label} onCancel={modal.onCancel}>
    <DialogHeader><h2>{label}</h2></DialogHeader>
    <p>{source.title} · v{version}</p>
    <form onSubmit={event => { event.preventDefault(); void save(); }}>
      <fieldset disabled={busy}>
        <label className="artifact-field">Title<input readOnly={!!existing} value={title} maxLength={200} onChange={e => setTitle(e.target.value)}/></label>
        {!existing && source.kind !== 'report' && <label className="artifact-field">Include a review<select value={review?.id || ''} onChange={e => {
          const item = reviews.find(candidate => candidate.id === e.target.value);
          setReview(item ? { id: item.id, version: item.versions.at(-1)!.number } : undefined);
        }}><option value="">No additional review</option>{reviews.map(item => <option key={item.id} value={item.id}>{item.title} · v{review?.id === item.id ? review.version : item.versions.at(-1)?.number}</option>)}</select></label>}
        {milestones.map((m, index) => <fieldset key={index}><legend>Milestone {index + 1}</legend>
          {(['id', 'title', 'phase', 'owner', 'deliverable'] as const).map((field, i) => <label className="artifact-field" key={field}>{['ID', 'Milestone title', 'Phase', 'Owner', 'Deliverable'][i]}
            <input required={field === 'id' || field === 'title'} value={m[field] ?? ''} onChange={e => edit(index, { [field]: e.target.value })}/></label>)}
          <label className="artifact-field">Status<select value={m.status} onChange={e => edit(index, { status: e.target.value as Milestone['status'] })}>
            {milestoneStatuses.map(status => <option key={status}>{status}</option>)}</select></label>
          {(['dependsOn', 'definitionOfDone', 'evidence', 'openDecisions'] as const).map((field, i) => <label className="artifact-field" key={field}>{['Dependency IDs', 'Definition of done', 'Verification evidence', 'Open decisions'][i]} (one per line)
            <textarea rows={2} value={m[field].join('\n')} onChange={e => edit(index, { [field]: e.target.value.split('\n') })}/></label>)}
          <button type="button" disabled={milestones.length === 1} onClick={() => setMilestones(rows => rows.filter((_, i) => i !== index))}>Remove milestone</button>
        </fieldset>)}
        <button type="button" onClick={() => setMilestones(rows => {
          let number = rows.length + 1;
          while (rows.some(row => row.id === 'M' + number)) number++;
          return [...rows, newMilestone('M' + number, '')];
        })}>Add milestone</button>
      </fieldset>
      {error && <p role="alert">{error}</p>}
      <footer><button type="button" disabled={busy} onClick={onClose}>Cancel</button><button className="primary" disabled={busy || !title.trim() || !milestones.length}>Save roadmap</button></footer>
    </form>
  </DraggableDialog>;
}

/** A unified diff, folded around the changes. */
export function DiffView({ lines, label, context = 3 }: { lines: DiffLine[]; label: string; context?: number }) {
  const [expanded, setExpanded] = useState(false);
  const shown = useMemo(() => expanded ? [{ lines, hiddenBefore: 0 }] : hunks(lines, context), [lines, expanded, context]);
  const stats = diffStats(lines);
  if (!stats.added && !stats.removed) return <p className="artifact-diff-empty" role="status">No changes.</p>;
  return <div className="artifact-diff" role="region" aria-label={label}>
    {shown.map((hunk, h) => <div key={h} className="artifact-diff-hunk">
      {hunk.hiddenBefore > 0 && <button type="button" className="artifact-diff-fold" onClick={() => setExpanded(true)}>{hunk.hiddenBefore.toLocaleString()} unchanged line{hunk.hiddenBefore === 1 ? '' : 's'} · show all</button>}
      {hunk.lines.map((line, i) => <div key={i} className={`artifact-diff-line is-${line.kind}`}><span className="artifact-diff-number" aria-hidden="true">{line.before ?? ''}</span><span className="artifact-diff-number" aria-hidden="true">{line.after ?? ''}</span><span className="artifact-diff-sign" aria-label={line.kind === 'add' ? 'Added' : line.kind === 'remove' ? 'Removed' : undefined}>{line.kind === 'add' ? '+' : line.kind === 'remove' ? '−' : ' '}</span><code>{line.text || ' '}</code></div>)}
    </div>)}
  </div>;
}
export function DiffStats({ lines }: { lines: DiffLine[] }) {
  const { added, removed } = diffStats(lines);
  return <span className="artifact-diff-stats"><span className="is-add">+{added}</span><span className="is-remove">−{removed}</span><span className="visually-hidden">{added} lines added, {removed} removed</span></span>;
}

/**
 * "Update artifact from this message": the diff against the latest version,
 * reviewed before anything is saved. The save names the version that was
 * compared, so a version written meanwhile is shown, never overwritten.
 */
export function UpdateArtifactDialog({ collectionId, artifactId, content, source, onClose, onSaved }: { collectionId: string; artifactId: string; content: string; source: SourceInput; onClose(): void; onSaved(artifact: Artifact): void }) {
  const { api } = useRunner();
  const [latest, setLatest] = useState<Artifact>(), [busy, setBusy] = useState(false), [error, setError] = useState(''), [notice, setNotice] = useState(''), [view, setView] = useState<'changes' | 'document'>('changes');
  const modal = useModal(onClose, busy);
  const load = async (signal?: AbortSignal) => { try { setLatest(await api.getArtifact({ collectionId, artifactId }, { signal, timeoutMs: 10000 })); } catch (e) { if (!signal?.aborted) setError(errorText(e)); } };
  useEffect(() => { const control = new AbortController(); void load(control.signal); return () => control.abort(); }, [collectionId, artifactId]);
  const lines = useMemo(() => latest ? lineDiff(latest.content, content) : [], [latest, content]);
  const same = !!latest && latest.content === content;
  const save = async () => {
    if (!latest || busy) return;
    setBusy(true); setError(''); setNotice('');
    try { const saved = await api.putArtifact({ collectionId, artifactId, content, source, expectedVersion: latest.contentVersion }, { timeoutMs: 15000 }); artifactsChanged(collectionId); onSaved(saved); }
    catch (e) {
      if (ConnectError.from(e).code === Code.Aborted) { setNotice(`A newer version was saved while you were reviewing. The changes below are now against it.`); await load(); }
      else setError(errorText(e));
      setBusy(false);
    }
  };
  const next = (latest?.versions.at(-1)?.number ?? 0) + 1;
  const saveButton = useRef<HTMLButtonElement>(null);
  // Once the diff is there, Enter saves it.
  useEffect(() => { if (latest && !same) saveButton.current?.focus(); }, [latest?.contentVersion, same]);
  return <DraggableDialog ref={modal.ref} className="artifact-dialog is-wide" aria-label="Update artifact" onCancel={modal.onCancel}>
    <DialogHeader><h2><Icon name="history" size={18}/>Update “{latest?.title || '…'}”</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}><Icon name="close"/></button></DialogHeader>
    {!latest && !error && <p role="status">Loading the latest version…</p>}
    {latest && <>
      <div className="artifact-update-summary"><span>This message compared with <strong>v{latest.contentVersion}</strong></span>{!same && <DiffStats lines={lines}/>}
        <div className="artifact-segmented" role="tablist" aria-label="Show"><button type="button" role="tab" aria-selected={view === 'changes'} onClick={() => setView('changes')}>Changes</button><button type="button" role="tab" aria-selected={view === 'document'} onClick={() => setView('document')}>Result</button></div></div>
      {notice && <p className="artifact-notice" role="status">{notice}</p>}
      {same ? <p className="artifact-diff-empty" role="status">This message matches v{latest.contentVersion}. There is nothing new to save.</p>
        : view === 'changes' ? <DiffView lines={lines} label={`Changes from v${latest.contentVersion}`}/> : <div className="artifact-preview-body is-tall"><RichContent text={content}/></div>}
    </>}
    {error && <p className="artifact-error" role="alert">{error}</p>}
    <footer><button type="button" disabled={busy} onClick={onClose}>Cancel</button><button ref={saveButton} type="button" className="primary" data-autofocus disabled={busy || !latest || same} onClick={() => void save()}>{busy ? 'Saving…' : `Save as v${next}`}</button></footer>
  </DraggableDialog>;
}

export function DeleteArtifactDialog({ artifact, onClose, onDeleted }: { artifact: Artifact; onClose(): void; onDeleted(): void }) {
  const { api } = useRunner();
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const modal = useModal(onClose, busy);
  return <DraggableDialog ref={modal.ref} className="artifact-dialog is-narrow" aria-label="Delete artifact" onCancel={modal.onCancel}>
    <DialogHeader><h2>Delete artifact?</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}><Icon name="close"/></button></DialogHeader>
    <p className="artifact-dialog-subject">{artifact.title}</p>
    <p className="artifact-dialog-lead">All {artifact.versions.length} version{artifact.versions.length === 1 ? '' : 's'} are removed from Lazybox.{artifact.trackedPath ? ` The file ${artifact.trackedPath} stays in the repository.` : ''} Conversations are not changed.</p>
    {error && <p className="artifact-error" role="alert">{error}</p>}
    <footer><button type="button" data-autofocus disabled={busy} onClick={onClose}>Cancel</button><button type="button" className="danger" disabled={busy} onClick={async () => { setBusy(true); try { await api.deleteArtifact({ collectionId: artifact.collectionId, artifactId: artifact.id }, { timeoutMs: 10000 }); artifactsChanged(artifact.collectionId); onDeleted(); } catch (e) { setError(errorText(e)); setBusy(false); } }}>{busy ? 'Deleting…' : 'Delete artifact'}</button></footer>
  </DraggableDialog>;
}

/**
 * "Save to repository…": a Markdown file in one of the workspace's
 * repositories. It is written, never staged or committed; the Changes panel
 * shows it like any other edit. Replacing an existing file is a second,
 * explicit step.
 */
export function SaveToRepositoryDialog({ artifact, collection, onClose, onSaved }: { artifact: Artifact; collection?: Collection; onClose(): void; onSaved(path: string, artifact?: Artifact): void }) {
  const { api } = useRunner();
  const members = collection?.members.map(member => member.spec?.name || '').filter(Boolean) || [];
  const [member, setMember] = useState(() => artifact.trackedPath.split('/')[0] || members[0] || '');
  const [path, setPath] = useState(() => artifact.trackedPath ? artifact.trackedPath.split('/').slice(1).join('/') : suggestedPath('', artifact.title).replace(/\.md$/, artifact.format === 'json' ? '.json' : '.md'));
  const [track, setTrack] = useState(!artifact.trackedPath);
  const [busy, setBusy] = useState(false), [error, setError] = useState(''), [replace, setReplace] = useState<{ path: string; version: string }>();
  const modal = useModal(onClose, busy);
  const full = `${member}/${path.replace(/^\/+/, '')}`;
  const valid = !!member && !!path.trim() && !path.split('/').some(part => part === '..' || part === '.') && !path.endsWith('/');
  const save = async (expectedFileVersion = '') => {
    setBusy(true); setError('');
    try {
      const saved = await api.saveArtifactToRepository({ collectionId: artifact.collectionId, artifactId: artifact.id, path: full, expectedFileVersion, track }, { timeoutMs: 20000 });
      onSaved(saved.path || full, saved.artifact);
    } catch (e) {
      const failure = ConnectError.from(e);
      if (failure.code === Code.AlreadyExists && !expectedFileVersion) {
        try { const file = await api.readWorkspaceFile({ collectionId: artifact.collectionId, path: full }, { timeoutMs: 10000 }); setReplace({ path: file.path || full, version: file.version }); }
        catch (read) { setError(errorText(read)); }
      } else setError(failure.code === Code.Aborted ? 'The file changed on disk while you were deciding. Check it, then save again.' : errorText(e));
      setReplace(current => failure.code === Code.AlreadyExists ? current : undefined);
      setBusy(false);
    }
  };
  return <DraggableDialog ref={modal.ref} className="artifact-dialog" aria-label="Save to repository" onCancel={modal.onCancel}>
    <DialogHeader><h2><Icon name="repository" size={18}/>Save to repository</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}><Icon name="close"/></button></DialogHeader>
    <p className="artifact-dialog-lead">Writes v{artifact.versions.at(-1)?.number ?? 1} as a Markdown file in the workspace. Nothing is staged or committed: review it in Changes like any other edit.</p>
    <form onSubmit={event => { event.preventDefault(); if (valid && !busy) void save(replace?.version); }}>
      <div className="artifact-path-row">
        <label className="artifact-field"><span>Repository</span><select value={member} disabled={busy || !members.length} onChange={event => { setMember(event.target.value); setReplace(undefined); }}>{members.length ? members.map(name => <option key={name} value={name}>{name}</option>) : <option value="">No repository in this workspace</option>}</select></label>
        <label className="artifact-field is-grow"><span>Path</span><input data-autofocus value={path} disabled={busy} spellCheck={false} onChange={event => { setPath(event.target.value); setReplace(undefined); }}/></label>
      </div>
      <label className="artifact-check"><input type="checkbox" checked={track} disabled={busy} onChange={event => setTrack(event.target.checked)}/><span>Keep the artifact in sync with this file<small>Later edits to the file, by you or an agent, become new versions.</small></span></label>
      {replace && <div className="artifact-replace" role="alert"><Icon name="alert" size={15}/><span><strong>{replace.path}</strong> already exists. Replace its content with this artifact? The previous content stays recoverable through Git if it was committed.</span></div>}
      {error && <p className="artifact-error" role="alert">{error}</p>}
      <footer><button type="button" disabled={busy} onClick={onClose}>Cancel</button><button type="submit" className={replace ? 'danger' : 'primary'} disabled={busy || !valid}>{busy ? 'Saving…' : replace ? 'Replace file' : 'Save file'}</button></footer>
    </form>
  </DraggableDialog>;
}

/** "Create GitHub issue…": the artifact as the body of an issue in a workspace repository's GitHub project. */
export function CreateIssueDialog({ artifact, collection, onClose, onCreated }: { artifact: Artifact; collection?: Collection; onClose(): void; onCreated(url: string, number: bigint): void }) {
  const { api } = useRunner();
  const members = collection?.members.map(member => member.spec?.name || '').filter(Boolean) || [];
  const [member, setMember] = useState(members[0] || ''), [title, setTitle] = useState(artifact.title);
  const [busy, setBusy] = useState(false), [error, setError] = useState('');
  const modal = useModal(onClose, busy);
  return <DraggableDialog ref={modal.ref} className="artifact-dialog" aria-label="Create GitHub issue" onCancel={modal.onCancel}>
    <DialogHeader><h2><Icon name="external" size={18}/>Create GitHub issue</h2><button type="button" aria-label="Close" disabled={busy} onClick={onClose}><Icon name="close"/></button></DialogHeader>
    <p className="artifact-dialog-lead">Files a new issue with v{artifact.versions.at(-1)?.number ?? 1} of this artifact as its description, through your GitHub connection. It is public to everyone who can see the repository.</p>
    <form onSubmit={async event => {
      event.preventDefault(); if (busy || !member || !title.trim()) return;
      setBusy(true); setError('');
      try { const created = await api.createArtifactIssue({ collectionId: artifact.collectionId, artifactId: artifact.id, member, title: title.trim() }, { timeoutMs: 30000 }); onCreated(created.url, created.number); }
      catch (e) { setError(errorText(e)); setBusy(false); }
    }}>
      <label className="artifact-field"><span>Repository</span><select value={member} disabled={busy || !members.length} onChange={event => setMember(event.target.value)}>{members.length ? members.map(name => <option key={name} value={name}>{name}</option>) : <option value="">No repository in this workspace</option>}</select></label>
      <label className="artifact-field"><span>Issue title</span><input data-autofocus value={title} maxLength={256} disabled={busy} onChange={event => setTitle(event.target.value)}/></label>
      {error && <p className="artifact-error" role="alert">{error}</p>}
      <footer><button type="button" disabled={busy} onClick={onClose}>Cancel</button><button type="submit" className="primary" disabled={busy || !member || !title.trim()}>{busy ? 'Creating…' : 'Create issue'}</button></footer>
    </form>
  </DraggableDialog>;
}

export const kindOf = (artifact: Pick<Artifact, 'kind'>) => asKind(artifact.kind);
