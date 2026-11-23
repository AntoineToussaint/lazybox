import { isReviewSnippet } from './snippet-usage';
import { readPreference, writePreference } from "./view-state";
import { readSnippetUsage, saveSnippetUse, searchSnippets, sortSnippets, type SnippetUsage } from "./snippet-usage";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { Snippet } from "@lazybox/protocol";
import type { ViewStateStore } from "@lazybox/platform";
import { useRunner } from "./RunnerContext";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { RichContent } from "./RichContent";
import { Resizer } from "./Resizer";
import { SPLITS } from "./resizable-model";
import { WorkflowReview } from "./WorkflowReview";
export function SnippetPicker({
  workspace,
  store,
  canSend,
  onClose,
  onCatalog,
  onInsert,
  onSend,
}: {
  workspace?: string;
  store?: ViewStateStore;
  canSend: boolean;
  onClose(): void;
  onCatalog(rows: Snippet[]): void;
  onInsert(text: string): void;
  onSend(text: string, key: string): Promise<boolean>;
}) {
  const { api } = useRunner(),
    dialog = useRef<HTMLDialogElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const preview = useRef<HTMLElement>(null);
  const [rows, setRows] = useState<Snippet[]>([]),
    [query, setQuery] = useState(""),
    [index, setIndex] = useState(0),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false),
    [loading, setLoading] = useState(true);
  const favoritesKey = "favorite-snippets:v2";
  const [favorites,setFavorites]=useState<string[]>(()=>{try{const value=JSON.parse(readPreference(store, favoritesKey)||'[]');return Array.isArray(value)?value.filter(v=>typeof v==='string'):[];}catch{return [];}});
  // All, Favorites or Frequent: one view at a time, like tabs over the same list.
  const [view,setView]=useState<'all'|'favorites'|'frequent'>('all');
  const [usage,setUsage]=useState<SnippetUsage>(()=>readSnippetUsage(store));
  const used=(key:string)=>setUsage(saveSnippetUse(store,key));
  useEffect(() => {
    dialog.current?.showModal();
    dialog.current
      ?.querySelector<HTMLInputElement>("[data-autofocus]")
      ?.focus();
    const controller = new AbortController();
    void api
      .listSnippets({}, { signal: controller.signal })
      .then(
        (v) => {
          if (!controller.signal.aborted) { setRows(v.snippets); onCatalog(v.snippets); }
        },
        (e) => {
          if (!controller.signal.aborted) setError(String(e));
        },
      )
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [api, onCatalog]);
  const matching = rows.filter(row =>
    view === 'all' || (view === 'favorites' ? favorites.includes(row.key) : (usage[row.key]?.count || 0) > 0),
  );
  const ordered = sortSnippets(matching, usage, favorites, view === 'frequent' ? 'frequent' : 'suggested');
  const filtered = searchSnippets(ordered, query);
  const lastUsed = !query.trim() && view === 'all' ? filtered.find(row => usage[row.key])?.key : undefined;
  const selected = filtered[Math.min(index, filtered.length - 1)];
  useLayoutEffect(() => {
    const node = list.current;
    if (!node) return;
    const row = node.querySelector<HTMLElement>('[aria-selected="true"]');
    if (!row || index === 0) { node.scrollTop = 0; return; }
    const top = node.getBoundingClientRect().top;
    const option = row.getBoundingClientRect();
    if (option.top < top) node.scrollTop += option.top - top;
    else if (option.bottom > top + node.clientHeight) node.scrollTop += option.bottom - top - node.clientHeight;
  }, [index, query, view, selected?.key]);

  useLayoutEffect(() => {
    if (preview.current) preview.current.scrollTop = 0;
  }, [selected?.key]);

  const insert = () => {
    if (selected && !busy) {
      used(selected.key);
      onInsert(selected.body);
      onClose();
    }
  };
  const send = async () => {
    if (!selected || !canSend || busy) return;
    setBusy(true);
    try {
      if (await onSend(selected.body, selected.key)) {
        used(selected.key);
        onClose();
      } else
        setError(
          isReviewSnippet(selected.key) ? "Review start was not confirmed. Close the picker and retry the retained draft." : "The send was not confirmed. Close this picker to retry the retained draft.",
        );
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <DraggableDialog
      ref={dialog}
      className="snippet-picker"
      aria-label="Snippets"
      onCancel={(e) => {
        if (busy) e.preventDefault();
        else onClose();
      }}
      onKeyDown={(e) => {
        if ((e.target as HTMLElement).closest("button, .workflow-review")) return;
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          setIndex((i) =>
            Math.max(
              0,
              Math.min(
                filtered.length - 1,
                i + (e.key === "ArrowDown" ? 1 : -1),
              ),
            ),
          );
        }
        if (e.key === "Enter" && !e.nativeEvent.isComposing) {
          e.preventDefault();
          if (e.shiftKey) insert();
          else void send();
        }
      }}
    >
      <DialogHeader>
        <h2>Snippets</h2>
        <button disabled={busy} onClick={onClose} aria-label="Close snippets">
          ×
        </button>
      </DialogHeader>
      <input
        data-autofocus
        autoFocus
        type="search"
        aria-label="Find snippets"
        placeholder="Search name, description or category…"
        title="Use body: followed by text to search inside snippets."
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setIndex(0);
        }}
      />
      <div className="snippet-filters" role="group" aria-label="Show snippets">{([['all','All'],['favorites',`Favorites${favorites.length?` · ${favorites.length}`:''}`],['frequent','Frequent']] as const).map(([value,label])=><button key={value} aria-pressed={view===value} onClick={()=>{setView(value);setIndex(0);}}>{label}</button>)}<small>{filtered.length} {filtered.length===1?'snippet':'snippets'}</small></div>
      <div className="snippet-browser">
        <div ref={list} role="listbox" aria-label="Available snippets">
          {error && <p role="alert">{error}</p>}
          {loading && <p role="status">Loading snippets…</p>}
          {!loading && !filtered.length && <p className="snippet-empty">{query.trim() ? 'No matching snippets.' : view==='frequent' ? 'Snippets you send or insert show up here, most used first.' : view==='favorites' ? 'Star a snippet to keep it here.' : 'No snippets yet.'}</p>}
          {!loading && !filtered.length && !!query.trim() && !/^body:/i.test(query.trim()) && <p className="snippet-empty">Search snippet text with <code>body:</code> followed by a phrase.</p>}
          {filtered.map((s, i) => (
            <button
              type="button"
              role="option"
              aria-selected={selected?.key === s.key}
              key={s.key}
              onClick={() => setIndex(i)}
            >
              <strong>{favorites.includes(s.key)&&<span className="snippet-star" aria-label="Favorite">★</span>}{s.key}</strong>
              <span>{s.description || s.category}</span>
              <small>
                {s.key===lastUsed?"Last used · ":""}
                {s.category}
                {usage[s.key]?.count ? <span className="snippet-uses" title={`Used ${usage[s.key].count} ${usage[s.key].count===1?'time':'times'}`}> · {usage[s.key].count}×</span> : null}
              </small>
            </button>
          ))}
        </div>
        <Resizer split={SPLITS.snippetList} label="snippet list" store={store}/>
        <article ref={preview} className="snippet-preview">
          {selected && (
            <>
              <small>
                {selected.origin} · {selected.category}
              </small>
              <button className="snippet-favorite" aria-pressed={favorites.includes(selected.key)} onClick={()=>{const next=favorites.includes(selected.key)?favorites.filter(k=>k!==selected.key):[...favorites,selected.key];try{writePreference(store,favoritesKey,JSON.stringify(next));setFavorites(next);}catch(e){setError(`Could not save favorites: ${String(e)}`);}}}>{favorites.includes(selected.key)?'★ Favorite':'☆ Add to favorites'}</button>
              <RichContent text={selected.body} />
              {workspace && <WorkflowReview key={selected.key} workspace={workspace} snippet={selected.key}/>}
            </>
          )}
        </article>
      </div>
      <footer>
        <small>
          {canSend
            ? "Enter to send · Shift+Enter to insert"
            : "Insert into your existing draft, then send from the composer."}
        </small>
        <button disabled={!selected || busy} onClick={insert}>
          Insert
        </button>
        <button
          className="primary"
          disabled={!selected || !canSend || busy}
          onClick={() => void send()}
        >
          {busy ? "Starting…" : selected && isReviewSnippet(selected.key) ? "Start review" : "Send snippet"}
        </button>
      </footer>
    </DraggableDialog>
  );
}
