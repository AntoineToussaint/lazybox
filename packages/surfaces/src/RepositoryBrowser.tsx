import { type ReactNode, createContext, Fragment, useContext, useEffect, useRef, useState } from "react";
import type { WorkspaceHost } from "@lazybox/platform";
import { useRunner } from "./RunnerContext";
import { useRepositoryCatalog } from "./repository-catalog";
import {
  compareRepositories,
  repositoryOrganization,
} from "./repository-model";
import { shortName } from "./repository-request-model";
import { GitHubSettings } from "./GitHubSettings";
import { ReplaceRepositoryCopy } from "./RepositoryReplacement";
import "./repository-browser.css";
export const RepositoryHostContext = createContext<
  Pick<
    WorkspaceHost,
    "openSignIn" | "copySignInCode" | "chooseRepositoryFolder"
  >
>({});
type Props = {
  splitSelection?: boolean;
  renderBranch?(identity: string): ReactNode;
  selected?: ReadonlySet<string>;
  onSelected?(value: Set<string>): void;
  /** Double-click: choose this repository and finish (create or add) at once. */
  onActivate?(identity: string): void;
  included?: ReadonlySet<string>;
  disabled?: boolean;
  chooseRepositoryFolder?(): Promise<string | null>;
  /** The search it opens with. */
  initialQuery?: string;
};
/** Shared identity browser: selecting is presentation state, never a clone/import. */
export function RepositoryBrowser({
  splitSelection = false,
  renderBranch,
  selected = new Set(),
  onSelected,
  onActivate,
  included = new Set(),
  disabled = false,
  chooseRepositoryFolder,
  initialQuery = "",
}: Props) {
  const { api } = useRunner(),
    host = useContext(RepositoryHostContext);
  const { catalog, loading, refreshing, error, reload } =
    useRepositoryCatalog(api);
  const [query, setQuery] = useState(initialQuery),
    [sources, setSources] = useState(false),
    [localPath, setLocalPath] = useState(""),
    [busy, setBusy] = useState(false),
    [sourceError, setSourceError] = useState(""),
    [removing, setRemoving] = useState<string>(),
    [replacing, setReplacing] = useState<string>(),
    [replacedNote, setReplacedNote] = useState("");
  const search = useRef<HTMLInputElement>(null);
  const keyboardToggle = useRef(false);
  const results = useRef<HTMLDivElement>(null);
  useEffect(() => { if (results.current) results.current.scrollTop = 0; }, [query]);
  // Keep a clicked row under the pointer for the second click of a double-click.
  const [moving, setMoving] = useState<{ identity: string; wasSelected: boolean }>();
  useEffect(() => {
    if (!moving) return;
    const timer = window.setTimeout(() => setMoving(undefined), 450);
    return () => window.clearTimeout(timer);
  }, [moving]);
  const inSelectedSection = (id: string) => moving?.identity === id ? moving.wasSelected : selected.has(id);
  const entries = [...(catalog?.entries || [])].sort(compareRepositories),
    needle = query.trim().toLowerCase(),
    qualified = needle.includes("/"),
    visible = entries.filter((r) =>
      (!splitSelection || !inSelectedSection(r.identity)) &&
      (qualified ? r.name : shortName(r.name)).toLowerCase().includes(needle),
    );
  const github = catalog?.sources.find((s) => s.id === "github"),
    limited = github?.state !== "connected";
  const choose = chooseRepositoryFolder || host.chooseRepositoryFolder;
  const scan = async () => {
    setBusy(true);
    setSourceError("");
    try {
      const path = choose ? await choose() : localPath.trim();
      if (path) {
        await api.scanRepositoryRoot({ path }, { timeoutMs: 75000 });
        await reload();
        setLocalPath("");
      }
    } catch (error) {
      setSourceError(String(error));
    } finally {
      setBusy(false);
    }
  };
  const selectedRows = [...new Set([...selected, ...(moving ? [moving.identity] : [])])]
    .filter(inSelectedSection)
    .map(identity => entries.find(entry => entry.identity === identity) ??
      { identity, name: identity.replace(/^github:/, ""), repositoryId: "" })
    .sort(compareRepositories);
  const renderRows = (rows: { identity: string; name: string; repositoryId: string }[]) =>
    rows.map((repo, index) => (
      <Fragment key={repo.identity}>
        {(index === 0 ||
          repositoryOrganization(rows[index - 1].identity) !==
            repositoryOrganization(repo.identity)) && (
          <h3>{repositoryOrganization(repo.identity)}</h3>
        )}
        <div className="repository-browser-row">
          {onSelected ? (
            <label
              title={onActivate && !included.has(repo.repositoryId) ? "Double-click to choose this repository and continue" : undefined}
              onClick={(event) => {
                // The second click of a double-click: keep the repository
                // chosen (no toggle back) and, if offered, finish at once.
                if (event.detail !== 2 || disabled || included.has(repo.repositoryId)) return;
                event.preventDefault();
                if (onActivate) onActivate(repo.identity);
                else onSelected(new Set([...selected, repo.identity]));
              }}
            >
              <input
                type="checkbox"
                aria-label={repo.name}
                checked={
                  included.has(repo.repositoryId) ||
                  selected.has(repo.identity)
                }
                disabled={disabled || included.has(repo.repositoryId)}
                onKeyDown={(e) => { keyboardToggle.current = e.key === " "; }}
                onPointerDown={() => { keyboardToggle.current = false; }}
                onChange={(e) => {
                  const fromKeyboard = keyboardToggle.current;
                  keyboardToggle.current = false;
                  if (splitSelection && !fromKeyboard) {
                    setMoving({ identity: repo.identity, wasSelected: inSelectedSection(repo.identity) });
                  }
                  const next = new Set(selected);
                  if (e.target.checked) next.add(repo.identity);
                  else next.delete(repo.identity);
                  if (splitSelection && (!e.target.checked || fromKeyboard)) {
                    const neighbor = rows[index + 1] ?? rows[index - 1];
                    const input = neighbor && e.target.closest(".repository-browser-results")
                      ?.querySelector<HTMLInputElement>(`input[aria-label="${CSS.escape(neighbor.name)}"]`);
                    (input || search.current)?.focus({ preventScroll: true });
                  }
                  onSelected(next);
                }}
              />
              <span title={repo.name}>{shortName(repo.name)}</span>
              {included.has(repo.repositoryId) && (
                <small>In workspace</small>
              )}
            </label>
          ) : (
            <>
              <span title={repo.name}>{shortName(repo.name)}</span>
              {repo.repositoryId && repo.identity.startsWith("github:") && (
                // A history GitHub replaced (a force-push with a new root,
                // or a new repository under the name): a fresh copy.
                <button
                  type="button"
                  aria-label={`Replace Lazybox’s copy of ${repo.name}`}
                  aria-expanded={replacing === repo.identity}
                  title="Copy it again from GitHub, for example after its history was force-pushed"
                  onClick={() => setReplacing(replacing === repo.identity ? undefined : repo.identity)}
                >
                  Replace copy…
                </button>
              )}
              <button
                type="button"
                aria-label={`Remove ${repo.name} from suggestions`}
                onClick={() => setRemoving(repo.identity)}
              >
                Remove
              </button>
            </>
          )}
          {splitSelection && inSelectedSection(repo.identity) && renderBranch?.(repo.identity)}
        </div>
        {replacing === repo.identity && (
          <div className="repository-browser-replace">
            <p>Replace Lazybox’s copy of {repo.name} with the repository as GitHub has it now. Do this when its history was replaced on GitHub (a force-push with a new root, or a new repository under the same name).</p>
            <ReplaceRepositoryCopy
              replacement={{ identity: repo.identity, name: repo.name, hasCopy: true }}
              lead={false}
              autoFocus
              onCancel={() => setReplacing(undefined)}
              onReplaced={(done) => {
                setReplacing(undefined);
                setReplacedNote(`Replaced Lazybox’s copy of ${done.name || repo.name}. New workspaces start from it.`);
                void reload();
              }}
            />
          </div>
        )}
      </Fragment>
    ));
  return (
    <section className="repository-browser" aria-label="Repository browser">
      {splitSelection && <section aria-label="Selected repositories">
        <strong>Selected repositories ({selected.size})</strong>
        <div className="repository-browser-results repository-selected-results">
          {renderRows(selectedRows)}
        </div>
      </section>}
      {onSelected && !splitSelection && (
        <div
          className="repository-selection"
          role="region"
          aria-label="Selected repositories"
          tabIndex={0}
        >
          <strong>Selected repositories ({selected.size})</strong>
          {renderBranch && <small>Each repository starts from its default branch unless you choose another.</small>}
          {onActivate && <small>Double-click a result to continue with all selected repositories.</small>}
          {[...selected].map((id) => (
            <div className="workspace-start-from-repo" key={id}>
              <button
                type="button"
                disabled={disabled}
                aria-label={`Unselect ${entries.find((e) => e.identity === id)?.name || id}`}
                onClick={event => {
                  const row = event.currentTarget.parentElement;
                  const sibling = row?.nextElementSibling || row?.previousElementSibling;
                  (sibling?.querySelector("button") || search.current)?.focus();
                  const next = new Set(selected);
                  next.delete(id);
                  onSelected(next);
                }}
              >
                {entries.find((e) => e.identity === id)?.name || id} ×
              </button>
              {renderBranch?.(id)}
            </div>
          ))}
        </div>
      )}
      <div className="repository-browser-toolbar">
        <input
          autoFocus
          ref={search}
          data-autofocus
          type="search"
          spellCheck={false}
          autoComplete="off"
          aria-label="Find repositories"
          placeholder="Search by repository name or owner/repo…"
          value={query}
          onChange={(e) => { setMoving(undefined); setQuery(e.target.value); }}
        />
        <button
          type="button"
          disabled={disabled || refreshing || loading}
          onClick={() => void reload(true)}
        >
          Refresh
        </button>
        <button
          type="button"
          aria-expanded={sources}
          disabled={disabled}
          onClick={() => setSources(!sources)}
        >
          Connections & folders
        </button>
      </div>
      {sources && (
        <div className="repository-sources" inert={disabled}>
          <GitHubSettings
            openSignIn={host.openSignIn}
            copySignInCode={host.copySignInCode}
            onConnectionChange={() => void reload(true)}
          />
          <div className="local-discovery">
            <strong>Local repositories</strong>
            <p>
              Discover repositories in a folder. Existing checkouts are not
              moved.
            </p>
            {!choose && (
              <input
                aria-label="Folder to search"
                value={localPath}
                onChange={(e) => setLocalPath(e.target.value)}
                placeholder="Folder on the Runner"
              />
            )}
            <button
              type="button"
              disabled={disabled || busy || (!choose && !localPath.trim())}
              onClick={() => void scan()}
            >
              {busy
                ? "Finding repositories…"
                : choose
                  ? "Choose folder…"
                  : "Find repositories in folder"}
            </button>
          </div>
        </div>
      )}
      {(error || sourceError) && (
        <p className="error" role="alert">
          {error || sourceError}
        </p>
      )}
      {replacedNote && (
        <p className="repository-catalog-notice" role="status">{replacedNote}</p>
      )}
      {refreshing && (
        <p className="repository-catalog-notice" role="status">
          Refreshing GitHub repositories… Saved results remain available.
        </p>
      )}
      {github && limited && (
        <p className="repository-catalog-notice">
          {github.state === "not_connected" || github.state === "setup_required"
            ? "Connect GitHub to discover remote repositories."
            : github.detail}{" "}
          <button
            type="button"
            disabled={disabled}
            onClick={() => setSources(true)}
          >
            Manage connection
          </button>
        </p>
      )}
      {catalog?.sources
        .filter((s) => s.id !== "github" && s.state === "partial")
        .map((s) => (
          <p className="repository-catalog-notice" key={s.id}>
            {s.detail}
          </p>
        ))}
      {splitSelection && <strong>Unselected repositories</strong>}
      <div
        ref={results}
        className="repository-browser-results"
        role="region"
        aria-label={splitSelection ? "Unselected repositories" : "Available repositories"}
      >
        {loading ? (
          <p role="status">Loading repositories…</p>
        ) : (
          renderRows(visible)
        )}
        {!loading && !visible.length && splitSelection && !query.trim() && entries.length > 0 ? (
          <p role="status">All repositories are selected.</p>
        ) : !loading && !visible.length && (
          <div className="repository-browser-empty">
            <strong>
              {refreshing
                ? "Still looking for repositories…"
                : query
                  ? "No matches in the current catalog"
                  : "No repositories discovered yet"}
            </strong>
            <p>
              {query.trim()
                ? "Try a repository name or owner/repo. Your selection is kept when you search."
                : limited
                ? "The current GitHub connection may expose only part of your repositories. Check its access or discover an existing local checkout."
                : "Refresh discovery, check GitHub access, or choose a folder containing your repositories."}
            </p>
            {query.trim() && <button type="button" disabled={disabled} onClick={() => setQuery("")}>Clear search</button>}
            <button
              type="button"
              disabled={disabled}
              onClick={() => setSources(true)}
            >
              Connections & folders
            </button>
          </div>
        )}
      </div>
      {removing && (
        <div className="repository-catalog-notice">
          <p>
            Remove this repository from suggestions? Workspace files stay
            intact. Refreshing discovery can find it again.
          </p>
          <button type="button" onClick={() => setRemoving(undefined)}>
            Cancel
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                await api.removeRepository({
                  repositoryId: entries.find((e) => e.identity === removing)
                    ?.repositoryId,
                  gitIdentity: removing,
                });
                await reload();
                setRemoving(undefined);
              } catch (error) {
                setSourceError(String(error));
              } finally {
                setBusy(false);
              }
            }}
          >
            Remove from suggestions
          </button>
        </div>
      )}
    </section>
  );
}
