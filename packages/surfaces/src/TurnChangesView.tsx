import { useEffect, useState } from 'react';
import type { TurnChanges, TurnFile, WorkspaceDiff } from '@lazybox/protocol';
import { CodeBlock } from './RichContent';
import { useRunner } from './RunnerContext';
import { plural } from './turn-checkpoints';
import { Resizer } from './Resizer';
import { SPLITS } from './resizable-model';
import './turn-checkpoints.css';

type Selection = { member: string; file: TurnFile };

function Lines({ additions, deletions }: { additions: number; deletions: number }) {
  if (!additions && !deletions) return null;
  return <span className="turn-lines">{additions > 0 && <ins>+{additions.toLocaleString()}</ins>}{additions > 0 && deletions > 0 && ' '}{deletions > 0 && <del>−{deletions.toLocaleString()}</del>}</span>;
}
const STATUS: Record<string, string> = { A: 'Added', M: 'Modified', D: 'Deleted', R: 'Renamed', T: 'Type changed', C: 'Copied' };

/**
 * What one agent turn changed: its checkpoint before → after, per repository,
 * with each file's diff. Uncommitted and untracked files are included, since
 * checkpoints are taken of the files on disk, not of commits.
 */
export function TurnChangesView({ sessionId, turn, prompt = '' }: { sessionId: string; turn: number; prompt?: string }) {
  const { api } = useRunner();
  const [changes, setChanges] = useState<TurnChanges>();
  const [error, setError] = useState('');
  const [selection, setSelection] = useState<Selection>();
  const [diff, setDiff] = useState<{ key: string; value: WorkspaceDiff }>();
  const [diffError, setDiffError] = useState('');
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const control = new AbortController();
    setError('');
    void api.listTurnChanges({ sessionId, turn }, { signal: control.signal, timeoutMs: 30000 }).then(value => {
      if (control.signal.aborted) return;
      setChanges(value);
      // Open the first change instead of an empty pane.
      setSelection(current => {
        if (current && value.repositories.some(repo => repo.member === current.member && repo.files.some(file => file.path === current.file.path))) return current;
        for (const repo of value.repositories) if (repo.files[0]) return { member: repo.member, file: repo.files[0] };
        return undefined;
      });
    }, e => { if (!control.signal.aborted) setError(String(e)); });
    return () => control.abort();
  }, [api, sessionId, turn, revision]);
  const key = selection ? JSON.stringify([sessionId, turn, selection.member, selection.file.path, revision]) : '';
  useEffect(() => {
    setDiffError('');
    if (!selection) return;
    const control = new AbortController();
    void api.readTurnDiff({ sessionId, turn, member: selection.member, path: selection.file.path, originalPath: selection.file.originalPath }, { signal: control.signal, timeoutMs: 30000 })
      .then(value => { if (!control.signal.aborted) setDiff({ key, value }); }, e => { if (!control.signal.aborted) setDiffError(String(e)); });
    return () => control.abort();
  }, [api, key]);
  const shown = diff?.key === key ? diff.value : undefined;
  const files = changes?.repositories.reduce((sum, repo) => sum + repo.files.length, 0) || 0;
  const added = changes?.repositories.reduce((sum, repo) => sum + repo.files.reduce((n, f) => n + f.additions, 0), 0) || 0;
  const removed = changes?.repositories.reduce((sum, repo) => sum + repo.files.reduce((n, f) => n + f.deletions, 0), 0) || 0;
  return <section className="turn-changes" aria-label={`Changes in turn ${turn}`}>
    <header>
      <div><strong>{!changes ? 'Reading this turn’s changes…' : files ? `${plural(files, 'file')} changed in turn ${turn}` : `No file changes in turn ${turn}`}{' '}{changes && files > 0 && <Lines additions={added} deletions={removed}/>}</strong>
        <small>{prompt && <>“{prompt}” · </>}{changes?.inProgress ? 'Still running: what it changed so far, against the files now' : 'Files before the turn → after it, uncommitted and untracked included'}</small></div>
      <button onClick={() => setRevision(value => value + 1)}>Refresh</button>
    </header>
    {error && <p className="error" role="alert">{error}</p>}
    <div className="changes-columns">
      <nav aria-label={`Files changed in turn ${turn}`}>{changes?.repositories.map(repo => <section key={repo.member}>
        <h3>{repo.member}</h3>
        {repo.error ? <p className="module-empty" role="status">{repo.error}</p> : !repo.files.length ? <p className="module-empty">No changes</p> : repo.files.map(file => <button key={file.path} className={selection?.member === repo.member && selection.file.path === file.path ? 'selected' : ''} onClick={() => setSelection({ member: repo.member, file })} title={STATUS[file.status] || file.status}>
          <code>{file.status}</code><span>{file.path}{file.originalPath && <small>from {file.originalPath}</small>}</span>
          {file.binary ? <span className="turn-lines">binary</span> : <Lines additions={file.additions} deletions={file.deletions}/>}
        </button>)}
        {repo.truncated && <p role="status">File list limited. Some changes are not shown.</p>}
      </section>)}</nav>
      <Resizer split={SPLITS.changesFiles} label="file list"/>
      <div className="diff-view">{selection ? <>
        <header><strong>{selection.member}/{selection.file.path}</strong></header>
        <div className="diff-comparison">Before turn {turn} → {changes?.inProgress ? 'now' : 'after it'}</div>
        {diffError ? <p role="alert" className="error">{diffError}</p> : !shown ? <p role="status">Loading diff…</p> : <>
          {shown.truncated && <p role="status">Preview limited to 1 MiB.</p>}
          {shown.binary ? <p>Binary file · text preview unavailable</p> : shown.text ? <CodeBlock text={shown.text} language="diff" label="Turn diff"/> : <p className="module-empty">No text difference.</p>}
        </>}
      </> : <div className="diff-empty"><h3>{changes ? 'Nothing changed' : 'Reading changes…'}</h3><p>{changes ? 'This turn left every file as it found it.' : ''}</p></div>}</div>
    </div>
  </section>;
}
