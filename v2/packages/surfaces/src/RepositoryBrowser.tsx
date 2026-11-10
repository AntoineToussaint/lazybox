import { createContext, Fragment, useContext, useState } from "react";
import type { WorkspaceHost } from "@lazybox/platform";
import { useRunner } from "./RunnerContext";
import { useRepositoryCatalog } from "./repository-catalog";
import {
  compareRepositories,
  repositoryOrganization,
} from "./repository-model";
import { GitHubSettings } from "./GitHubSettings";
import "./repository-browser.css";
export const RepositoryHostContext = createContext<
  Pick<
    WorkspaceHost,
    "openSignIn" | "copySignInCode" | "chooseRepositoryFolder"
  >
>({});
type Props = {
  selected?: ReadonlySet<string>;
  onSelected?(value: Set<string>): void;
  included?: ReadonlySet<string>;
  disabled?: boolean;
  chooseRepositoryFolder?(): Promise<string | null>;
};
/** Shared identity browser: selecting is presentation state, never a clone/import. */
export function RepositoryBrowser({
  selected = new Set(),
  onSelected,
  included = new Set(),
  disabled = false,
  chooseRepositoryFolder,
}: Props) {
  const { api } = useRunner(),
    host = useContext(RepositoryHostContext);
  const { catalog, loading, refreshing, error, reload } =
    useRepositoryCatalog(api);
  const [query, setQuery] = useState(""),
    [sources, setSources] = useState(false),
    [localPath, setLocalPath] = useState(""),
    [busy, setBusy] = useState(false),
    [sourceError, setSourceError] = useState(""),
    [removing, setRemoving] = useState<string>();
  const entries = [...(catalog?.entries || [])].sort(compareRepositories),
    visible = entries.filter((r) =>
      r.name.toLowerCase().includes(query.trim().toLowerCase()),
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
  return (
    <section className="repository-browser" aria-label="Repository browser">
      <div className="repository-browser-toolbar">
        <input
          autoFocus
          data-autofocus
          type="search"
          aria-label="Find repositories"
          placeholder="Search organization or repository…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
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
      <div
        className="repository-browser-results"
        role="region"
        aria-label="Available repositories"
      >
        {loading ? (
          <p role="status">Loading repositories…</p>
        ) : (
          visible.map((repo, index) => (
            <Fragment key={repo.identity}>
              {(index === 0 ||
                repositoryOrganization(visible[index - 1].identity) !==
                  repositoryOrganization(repo.identity)) && (
                <h3>{repositoryOrganization(repo.identity)}</h3>
              )}
              <div className="repository-browser-row">
                {onSelected ? (
                  <label
                    onDoubleClick={() => {
                      if (!disabled && !included.has(repo.repositoryId))
                        onSelected(new Set([...selected, repo.identity]));
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
                      onChange={(e) => {
                        const next = new Set(selected);
                        if (e.target.checked) next.add(repo.identity);
                        else next.delete(repo.identity);
                        onSelected(next);
                      }}
                    />
                    <span title={repo.name}>{repo.name.includes("/")?repo.name.slice(repo.name.indexOf("/")+1):repo.name}</span>
                    {included.has(repo.repositoryId) && (
                      <small>In workspace</small>
                    )}
                  </label>
                ) : (
                  <>
                    <span title={repo.name}>{repo.name.includes("/")?repo.name.slice(repo.name.indexOf("/")+1):repo.name}</span>
                    <button
                      type="button"
                      aria-label={`Remove ${repo.name} from suggestions`}
                      onClick={() => setRemoving(repo.identity)}
                    >
                      Remove
                    </button>
                  </>
                )}
              </div>
            </Fragment>
          ))
        )}
        {!loading && !visible.length && (
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
                ? "Try a different repository or organization name. Your selection is kept when you search."
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
      {onSelected && !!selected.size && (
        <div
          className="repository-selection"
          role="region"
          aria-label="Selected repositories"
          tabIndex={0}
        >
          {[...selected].map((id) => (
            <button
              type="button"
              disabled={disabled}
              key={id}
              aria-label={`Unselect ${entries.find((e) => e.identity === id)?.name || id}`}
              onClick={() => {
                const next = new Set(selected);
                next.delete(id);
                onSelected(next);
              }}
            >
              {entries.find((e) => e.identity === id)?.name || id} ×
            </button>
          ))}
        </div>
      )}
    </section>
  );
}
