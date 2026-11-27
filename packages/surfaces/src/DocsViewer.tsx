import { isNavigationBack } from './navigation-back';
import { useEffect, useMemo, useRef, useState, useSyncExternalStore, type KeyboardEvent } from 'react';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { RichContent, type RichDocument } from './RichContent';
import { resolveDocImage, resolveDocLink, searchDocs, themedDocImage, type DocPage, type DocTarget } from './docs-model';
import { loadDocImages, loadDocs } from './docs-content';
import { getAppearance, subscribeAppearance } from './appearance/apply';
import { Resizer } from './Resizer';
import { SPLITS } from './resizable-model';
import './docs.css';

/**
 * Help → Documentation: the user docs (docs) bundled with the app, read
 * offline. Links between pages and anchors stay in the viewer, Back returns,
 * and the search box finds a page by its words. A link that leaves the docs
 * (a web page) opens in the browser as any link does.
 */
export function DocsViewer({ initial = 'README.md', onClose }: { /** The page to open, with an optional #anchor ("troubleshooting.md#…"). */ initial?: string; onClose(): void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const [pages, setPages] = useState<DocPage[]>();
  const [images, setImages] = useState<Map<string, string>>(new Map());
  const [error, setError] = useState('');
  const [target, setTarget] = useState<DocTarget>(() => { const [path, anchor = ''] = initial.split('#'); return { path: path || 'README.md', anchor }; });
  const [history, setHistory] = useState<DocTarget[]>([]);
  const [query, setQuery] = useState('');
  const [notice, setNotice] = useState('');
  // Screenshots in the window's theme: the light twin of each picture in the light theme.
  const scheme = useSyncExternalStore(subscribeAppearance, () => getAppearance().theme.scheme);
  useEffect(() => { const node = dialog.current; if (node && !node.open) node.showModal(); }, []);
  useEffect(() => {
    void loadDocs().then(setPages, e => setError(`The documentation could not be loaded: ${String(e)}`));
    void loadDocImages().then(map => setImages(new Map(map)), () => {});
  }, []);
  const page = pages?.find(p => p.path === target.path) || pages?.[0];
  // Land on the anchor, or the top of a new page.
  useEffect(() => {
    const pane = content.current;
    if (!pane || !page) return;
    const element = target.anchor ? pane.querySelector<HTMLElement>(`[id="${CSS.escape(target.anchor)}"]`) : null;
    if (element) element.scrollIntoView({ block: 'start' }); else pane.scrollTop = 0;
  }, [page, target]);
  const go = (next: DocTarget) => {
    setQuery(''); setNotice('');
    if (next.path === target.path && next.anchor === target.anchor) return;
    setHistory(h => [...h, target]);
    setTarget(next);
  };
  const back = () => {
    const previous = history.at(-1);
    if (!previous) { onClose(); return; }
    setQuery(''); setNotice('');
    setTarget(previous);
    setHistory(history.slice(0, -1));
  };
  const doc = useMemo<RichDocument | undefined>(() => page && {
    link: href => {
      const resolved = resolveDocLink(page.path, href, path => !!pages?.some(p => p.path === path));
      if (resolved) go(resolved);
      // A page for people working on Lazybox, not part of what the app ships.
      else setNotice(`That link leads to ${href.split('#')[0]}, which is not part of the documentation in the app.`);
    },
    image: src => { const path = resolveDocImage(page.path, src); return path ? images.get(themedDocImage(path, scheme, other => images.has(other))) : undefined; },
  }, [page, pages, images, target, scheme]);
  const hits = useMemo(() => pages && query.trim() ? searchDocs(pages, query) : undefined, [pages, query]);
  const sections = useMemo(() => {
    const groups = new Map<string, DocPage[]>();
    for (const p of pages || []) groups.set(p.section, [...(groups.get(p.section) || []), p]);
    return [...groups];
  }, [pages]);
  const onKey = (event: KeyboardEvent) => {
    if (event.key === '/' && !(event.target instanceof HTMLInputElement)) { event.preventDefault(); search.current?.focus(); }
  };
  // Following a link replaces the focused node. The next key can target the
  // document body even while this dialog is still the active modal.
  useEffect(() => {
    const node = dialog.current;
    if (!node) return;
    const doc = node.ownerDocument;
    const keydown = (event: globalThis.KeyboardEvent) => {
      if (!node.open || (doc.activeElement !== doc.body && !node.contains(doc.activeElement))) return;
      const legacyBack = event.metaKey && !event.altKey && !event.ctrlKey && !event.shiftKey && event.key === 'ArrowLeft';
      if (!isNavigationBack(event) && !legacyBack) return;
      if (event.key === 'ArrowLeft' && event.target instanceof HTMLInputElement) return;
      event.preventDefault();
      event.stopPropagation();
      back();
    };
    doc.addEventListener('keydown', keydown, true);
    return () => doc.removeEventListener('keydown', keydown, true);
  });
  return <DraggableDialog ref={dialog} className="docs-viewer" aria-labelledby="docs-title" onKeyDown={onKey}
    onCancel={event => { event.preventDefault(); if (query) setQuery(''); else onClose(); }} onClose={onClose} onDismiss={onClose}>
    <DialogHeader>
      <h2 id="docs-title">Documentation</h2>
      <button className="docs-back" aria-label="Back" title="Back (Alt+← or ⌘[)" onClick={back}><Icon name="back" size={14}/></button>
      <label className="docs-search"><Icon name="search" size={13}/><input ref={search} type="search" placeholder="Search the docs  /" aria-label="Search the documentation" value={query} onChange={e => setQuery(e.target.value)}
        onKeyDown={e => { if (e.key === 'Enter' && hits?.[0]) go({ path: hits[0].page.path, anchor: '' }); }}/></label>
      <button aria-label="Close documentation" onClick={onClose}>×</button>
    </DialogHeader>
    <div className="docs-body">
      <nav className="docs-nav" aria-label={hits ? 'Search results' : 'Pages'}>
        {hits ? (hits.length ? <ul>{hits.map(hit => <li key={hit.page.path}><button className={hit.page.path === page?.path ? 'selected' : ''} onClick={() => go({ path: hit.page.path, anchor: '' })}>
          <strong>{hit.page.title}</strong>{hit.snippet && <small>{hit.snippet}</small>}</button></li>)}</ul>
          : <p className="docs-empty" role="status">Nothing matches “{query}”.</p>)
          : sections.map(([section, list]) => <section key={section}><h3>{section}</h3><ul>{list.map(p => <li key={p.path}>
            <button aria-current={p.path === page?.path ? 'page' : undefined} className={p.path === page?.path ? 'selected' : ''} onClick={() => go({ path: p.path, anchor: '' })}>{p.title}</button></li>)}</ul></section>)}
      </nav>
      <Resizer split={SPLITS.docsNav} label="page list"/>
      <div className="docs-page" ref={content} tabIndex={-1}>
        {notice && <p className="docs-notice" role="status">{notice}<button aria-label="Dismiss" onClick={() => setNotice('')}>×</button></p>}
        {error ? <p role="alert">{error}</p> : !page ? <p role="status">Loading the documentation…</p>
          : <RichContent key={page.path} text={(page.text || '').replace(/<!--[\s\S]*?-->/g, '')} document={doc}/>}
      </div>
    </div>
  </DraggableDialog>;
}
