import { useState } from 'react';
import { Icon } from './Icons';
import { PAGE_LIMIT, allPanels, clonePage, defaultLayout, type WorkspaceLayout, type WorkspacePage } from './workspace-model';

/**
 * Customizing a workspace's pages: a page's name, duplicating or removing it,
 * resetting them all. Panels are arranged in place (drag a tab) and added from
 * the Panels menu; a page takes a whole arrangement from the Layout menu.
 */
export function PageConfig({ page, layout, hasDetached, pending, commit, changePage, onDone }: {
  page: WorkspacePage;
  layout: WorkspaceLayout;
  hasDetached: boolean;
  pending: boolean;
  commit(next: WorkspaceLayout, reset?: boolean): boolean;
  changePage(next: WorkspacePage): boolean;
  onDone(): void;
}) {
  const duplicate = () => { const next = clonePage(page, () => crypto.randomUUID()); commit({ ...layout, selectedPage: next.id, pages: [...layout.pages, next] }); };
  const remove = () => commit({ ...layout, selectedPage: layout.pages.find(value => value.id !== page.id)!.id, pages: layout.pages.filter(value => value.id !== page.id) });
  return <div className="page-config">
    <PageName key={page.id} name={page.name} onSave={name => changePage({ ...page, name })}/>
    <button disabled={layout.pages.length >= PAGE_LIMIT} title={layout.pages.length >= PAGE_LIMIT ? `A workspace has at most ${PAGE_LIMIT} pages` : undefined} onClick={duplicate}>Duplicate page</button>
    <button disabled={layout.pages.length <= 1 || allPanels(page).some(panel => panel.detached)} onClick={remove}>Remove page</button>
    <button disabled={hasDetached || pending} onClick={() => commit(defaultLayout(page.selection), true)}>Reset pages</button>
    <button className="primary" onClick={onDone}>Done</button>
  </div>;
}

/** A page's name, trimmed; one of spaces only is not a name. */
function PageName({ name, onSave }: { name: string; onSave(name: string): void }) {
  const [draft, setDraft] = useState(name), [problem, setProblem] = useState('');
  return <form noValidate onSubmit={event => {
    event.preventDefault();
    const trimmed = draft.trim();
    if (!trimmed) { setProblem('Give the page a name.'); return; }
    setProblem(''); setDraft(trimmed); onSave(trimmed);
  }}>
    <label>Page name<input maxLength={64} value={draft} aria-invalid={!!problem || undefined} aria-describedby={problem ? 'page-name-problem' : undefined} onChange={event => { setDraft(event.target.value); setProblem(''); }}/></label>
    <button>Rename page</button>
    {problem && <span id="page-name-problem" className="page-name-problem" role="alert">{problem}</span>}
  </form>;
}

/** The pages as JSON: panel references and arrangement, to inspect or import. */
export function LayoutConfiguration({ json, disabled, onChange, onImport, onClose }: { json: string; disabled: boolean; onChange(json: string): void; onImport(): void; onClose(): void }) {
  return <section className="layout-json">
    <div className="inspector-heading">
      <strong>Page configuration</strong>
      <button className="icon-button" aria-label="Close layout configuration" title="Close" onClick={onClose}><Icon name="close" size={14}/></button>
    </div>
    <label className="sr-only" htmlFor="page-config-json">Page configuration</label>
    <textarea id="page-config-json" value={json} onChange={event => onChange(event.target.value)} spellCheck={false}/>
    <button disabled={disabled} onClick={onImport}>Import layout</button>
    <small>Panel references and arrangement only. Your account and permissions stay unchanged.</small>
  </section>;
}
