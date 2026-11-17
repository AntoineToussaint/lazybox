import { readPreference, writePreference } from "./view-state";
import { readSnippetUsage, saveSnippetUse, sortSnippets, type SnippetUsage } from "./snippet-usage";
import { useEffect, useRef, useState } from "react";
import type { Snippet } from "@lazybox/protocol";
import type { ViewStateStore } from "@lazybox/platform";
import { useRunner } from "./RunnerContext";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { RichContent } from "./RichContent";
import { Resizer } from "./Resizer";
import { SPLITS } from "./resizable-model";
export function SnippetPicker({
  store,
  canSend,
  onClose,
  onInsert,
  onSend,
}: {
  store?: ViewStateStore;
  canSend: boolean;
  onClose(): void;
  onInsert(text: string): void;
  onSend(text: string): Promise<boolean>;
}) {
  const { api } = useRunner(),
    dialog = useRef<HTMLDialogElement>(null);
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
          if (!controller.signal.aborted) setRows(v.snippets);
        },
        (e) => {
          if (!controller.signal.aborted) setError(String(e));
        },
      )
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [api]);
  const matching = rows
    .filter(s=>view!=='favorites'||favorites.includes(s.key))
    .filter(s=>view!=='frequent'||(usage[s.key]?.count||0)>0)
    .filter((s) =>
      `${s.key} ${s.category} ${s.description} ${s.body}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    );
  // A search keeps the frequent order in that view; otherwise it reads by name.
  const filtered = !query || view==='frequent' ? sortSnippets(matching, usage, favorites, view==='frequent'?'frequent':'suggested') : sortSnippets(matching, {}, [], 'suggested');
  const lastUsed = !query && view==='all' ? filtered.find(s=>usage[s.key])?.key : undefined;
  const selected = filtered[Math.min(index, filtered.length - 1)];
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
      if (await onSend(selected.body)) {
        used(selected.key);
        onClose();
      } else
        setError(
          "The send was not confirmed. Close this picker to retry the retained draft.",
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
        if ((e.target as HTMLElement).tagName === "BUTTON") return;
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
        placeholder="Search key, category or description…"
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setIndex(0);
        }}
      />
      <div className="snippet-filters" role="group" aria-label="Show snippets">{([['all','All'],['favorites',`Favorites${favorites.length?` · ${favorites.length}`:''}`],['frequent','Frequent']] as const).map(([value,label])=><button key={value} aria-pressed={view===value} onClick={()=>{setView(value);setIndex(0);}}>{label}</button>)}<small>{filtered.length} {filtered.length===1?'snippet':'snippets'}</small></div>
      {error && <p role="alert">{error}</p>}
      {loading && <p role="status">Loading snippets…</p>}
      <div className="snippet-browser">
        <div role="listbox" aria-label="Available snippets">
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
        <article className="snippet-preview">
          {selected && (
            <>
              <small>
                {selected.origin} · {selected.category}
              </small>
              <button className="snippet-favorite" aria-pressed={favorites.includes(selected.key)} onClick={()=>{const next=favorites.includes(selected.key)?favorites.filter(k=>k!==selected.key):[...favorites,selected.key];try{writePreference(store,favoritesKey,JSON.stringify(next));setFavorites(next);}catch(e){setError(`Could not save favorites: ${String(e)}`);}}}>{favorites.includes(selected.key)?'★ Favorite':'☆ Add to favorites'}</button>
              <RichContent text={selected.body} />
            </>
          )}
        </article>
      </div>
      {!loading && !filtered.length && <p className="snippet-empty">{query ? 'No matching snippets.' : view==='frequent' ? 'Snippets you send or insert show up here, most used first.' : view==='favorites' ? 'Star a snippet to keep it here.' : 'No snippets yet.'}</p>}
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
          {busy ? "Sending…" : "Send snippet"}
        </button>
      </footer>
    </DraggableDialog>
  );
}
