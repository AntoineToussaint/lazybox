import { useEffect, useMemo, useRef, useState } from 'react';
import hljs from 'highlight.js/lib/core';
import xml from 'highlight.js/lib/languages/xml';
import markdown from 'highlight.js/lib/languages/markdown';
import { CodeBlock } from './RichContent';

hljs.registerLanguage('xml', xml);
hljs.registerLanguage('markdown', markdown);

type SourceDraft = { version: number; text: string };
type DraftState = { key: string; draft?: SourceDraft };

function loadDraft(key: string): DraftState {
  try {
    const draft = key && JSON.parse(sessionStorage.getItem(key) || 'null');
    if (Number.isInteger(draft?.version) && draft.version > 0 && typeof draft.text === 'string') return { key, draft };
  } catch { /* Unavailable storage or an unreadable draft. */ }
  return { key };
}

function persistDraft({ key, draft }: DraftState) {
  if (!key) return;
  try {
    if (draft) sessionStorage.setItem(key, JSON.stringify(draft));
    else sessionStorage.removeItem(key);
  } catch { /* Editing still works in memory. */ }
}

export function useArtifactSourceDraft(runner: string, workspace: string, artifact: string) {
  const key = runner && 'lazybox:artifact:source:' + JSON.stringify([runner, workspace, artifact]);
  const [state, setState] = useState(() => loadDraft(key));
  // Info can arrive after the artifact. Restore just the draft when the Runner
  // becomes known, keeping the reader's selected version and companion intact.
  if (state.key !== key) setState(!state.key && state.draft ? { key, draft: state.draft } : loadDraft(key));
  const latest = useRef(state);
  latest.current = state;
  useEffect(() => {
    const timer = window.setTimeout(() => persistDraft(state), 250);
    return () => clearTimeout(timer);
  }, [state]);
  useEffect(() => {
    const flush = () => persistDraft(latest.current);
    window.addEventListener('pagehide', flush);
    return () => { window.removeEventListener('pagehide', flush); flush(); };
  }, []);
  return [state.draft, (draft: SourceDraft | undefined) => setState({ key, draft })] as const;
}

/** Typed artifacts contain only schema-defined strings, arrays and objects.
 * Decode those values for editing (including escaped Unicode text). */
function readableSource(content: string, language: string) {
  if (language === 'json') {
    try { return JSON.stringify(JSON.parse(content), null, 2); }
    catch { /* Show the original source if an older artifact cannot be decoded. */ }
  }
  return content;
}

type Props = {
  content: string; language: string; version: number; latest: number;
  editable: boolean; saving: boolean; draft?: SourceDraft;
  onDraft(draft: SourceDraft | undefined): void;
  onPick(version: number): void;
  onSave(body: string, version: number): Promise<boolean>;
};

export function ArtifactSource({ content, language, version, latest, editable, saving, draft, onDraft, onPick, onSave }: Props) {
  const source = useMemo(() => readableSource(content, language), [content, language]);
  const [invalid, setInvalid] = useState('');
  if (!draft || draft.version !== version) {
    const edit = () => { onPick(version); onDraft({ version, text: source }); };
    return <div className="artifact-source-pane"><CodeBlock text={source} language={language} label={language === 'markdown' ? 'Markdown' : language.toUpperCase()}
      action={draft ? <button type="button" onClick={() => onPick(draft.version)}>Resume draft · v{draft.version}</button>
        : editable && version === latest && <button type="button" onClick={edit}>Edit source</button>}/></div>;
  }

  const canSave = editable && version === latest && !saving && draft.text !== source && !!draft.text.trim();
  const save = async () => {
    if (!canSave) return;
    if (language === 'json') {
      try { JSON.parse(draft.text); }
      catch (e) { setInvalid(e instanceof Error ? e.message : 'Invalid JSON'); return; }
    }
    if (await onSave(draft.text, draft.version)) onDraft(undefined);
  };
  return <section className="artifact-source-edit" aria-label="Edit artifact source">
    <div className="artifact-source-actions"><span>Editing v{version}</span>
      <button type="button" disabled={saving} onClick={() => { onDraft(undefined); onPick(latest); }}>Discard draft</button>
      <button type="button" className="primary" disabled={!canSave} onClick={() => void save()}>{saving ? 'Saving…' : 'Save version'}</button>
    </div>
    <textarea autoFocus spellCheck={false} aria-label="Artifact source" aria-invalid={!!invalid} readOnly={saving}
      value={draft.text} onChange={event => { setInvalid(''); onDraft({ ...draft, text: event.target.value }); }}
      onKeyDown={event => { if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') { event.preventDefault(); void save(); } }}/>
    {invalid && <p role="alert" className="artifact-error">{invalid}</p>}
    {version !== latest && <p role="status">A newer version is available. <button type="button" onClick={() => onPick(latest)}>View v{latest}</button></p>}
  </section>;
}
