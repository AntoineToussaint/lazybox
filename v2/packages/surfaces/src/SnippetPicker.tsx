import { useEffect, useRef, useState } from "react";
import type { Snippet } from "@lazybox/protocol";
import type { ViewStateStore } from "@lazybox/platform";
import { useRunner } from "./RunnerContext";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { RichContent } from "./RichContent";
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
  const favoritesKey = "lazybox:favorite-snippets:v2";
  const [favorites,setFavorites]=useState<string[]>(()=>{try{const value=JSON.parse(store?.getItem(favoritesKey)||'[]');return Array.isArray(value)?value.filter(v=>typeof v==='string'):[];}catch{return [];}});
  const [favoritesOnly,setFavoritesOnly]=useState(false);
  const recentKey = "lazybox:recent-snippets:v2";
  const [recent] = useState<string[]>(() => {
    try {
      return JSON.parse(store?.getItem(recentKey) || "[]");
    } catch {
      return [];
    }
  });
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
  const filtered = rows
    .filter(s=>!favoritesOnly||favorites.includes(s.key))
    .filter((s) =>
      `${s.key} ${s.category} ${s.description} ${s.body}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    )
    .sort((a, b) => {
      if (!query) {
        const starred=Number(favorites.includes(b.key))-Number(favorites.includes(a.key));if(starred)return starred;
        const ai = recent.indexOf(a.key),
          bi = recent.indexOf(b.key);
        if (ai >= 0 || bi >= 0)
          return (ai < 0 ? 999 : ai) - (bi < 0 ? 999 : bi);
      }
      return a.category.localeCompare(b.category) || a.key.localeCompare(b.key);
    });
  const selected = filtered[Math.min(index, filtered.length - 1)];
  const insert = () => {
    if (selected && !busy) {
      onInsert(selected.body);
      onClose();
    }
  };
  const send = async () => {
    if (!selected || !canSend || busy) return;
    setBusy(true);
    try {
      if (await onSend(selected.body)) {
        store?.setItem(
          recentKey,
          JSON.stringify(
            [selected.key, ...recent.filter((k) => k !== selected.key)].slice(
              0,
              5,
            ),
          ),
        );
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
      <div className="snippet-filters"><button aria-pressed={favoritesOnly} onClick={()=>{setFavoritesOnly(v=>!v);setIndex(0);}}>Favorites{favorites.length?` · ${favorites.length}`:''}</button><small>{filtered.length} snippets</small></div>
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
              <strong>{s.key}</strong>
              <span>{s.description || s.category}</span>
              <small>
                {favorites.includes(s.key)?"★ · ":!query && recent.includes(s.key) ? "Recent · " : ""}
                {s.category}
              </small>
            </button>
          ))}
        </div>
        <article className="snippet-preview">
          {selected && (
            <>
              <small>
                {selected.origin} · {selected.category}
              </small>
              <button className="snippet-favorite" aria-pressed={favorites.includes(selected.key)} onClick={()=>{const next=favorites.includes(selected.key)?favorites.filter(k=>k!==selected.key):[...favorites,selected.key];try{store?.setItem(favoritesKey,JSON.stringify(next));setFavorites(next);}catch(e){setError(`Could not save favorites: ${String(e)}`);}}}>{favorites.includes(selected.key)?'★ Favorite':'☆ Add to favorites'}</button>
              <RichContent text={selected.body} />
            </>
          )}
        </article>
      </div>
      {!loading && !filtered.length && <p>No matching snippets.</p>}
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
