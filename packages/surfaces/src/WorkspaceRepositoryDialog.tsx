import { useEffect, useRef, useState, type ReactNode } from "react";
import type { Collection } from "@lazybox/protocol";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { RepositoryBrowser } from "./RepositoryBrowser";
import { useRepositoryCatalog } from "./repository-catalog";
import { useRunner } from "./RunnerContext";
import { useRunnerSettings } from "./RunnerSettings";
import { branchForName, suggestWorkspaceName } from "./repository-model";
import { Icon } from "./Icons";
/** One selection workflow for creating a workspace and adding to an existing one. */
export function WorkspaceRepositoryDialog({
  workspace,
  onClose,
  onSaved,
  chooseRepositoryFolder,
  preselected = [],
  initialQuery,
  repositoryRequestId,
  intro,
}: {
  workspace?: Collection;
  onClose(): void;
  onSaved(workspace: Collection): void;
  chooseRepositoryFolder?(): Promise<string | null>;
  /** Repositories chosen when the dialog opens. */
  preselected?: string[];
  /** The repository search the dialog opens with. */
  initialQuery?: string;
  /** Adding answers this agent's request for a repository: the agent is told. */
  repositoryRequestId?: string;
  /** Replaces the subtitle: why the dialog opened. */
  intro?: ReactNode;
}) {
  const { api } = useRunner(),
    dialog = useRef<HTMLDialogElement>(null),
    pending = useRef(false),
    attempt = useRef({ spec: "", key: "" });
  const { catalog, reload } = useRepositoryCatalog(api);
  const { settings } = useRunnerSettings(),
    generated = useRef("");
  if (!generated.current && settings?.suggestedName)
    generated.current = settings.suggestedName;
  const [selected, setSelected] = useState(() => new Set<string>(preselected)),
    [customName, setCustomName] = useState<string>(),
    [empty, setEmpty] = useState(false),
    [busy, setBusy] = useState(false),
    [progress, setProgress] = useState(""),
    [error, setError] = useState("");
  useEffect(() => {
    dialog.current?.showModal();
    // Creating starts on the name, its suggestion selected: typing replaces it.
    (dialog.current?.querySelector<HTMLInputElement>('[data-name-autofocus]') || dialog.current?.querySelector<HTMLInputElement>('[data-autofocus]'))?.focus();
  }, []);
  const suggested = (
      settings?.naming === "generated"
        ? generated.current || "New workspace"
        : suggestWorkspaceName(
            [...selected]
              .map((id) => catalog?.entries.find((r) => r.identity === id)?.name || "")
              .filter(Boolean),
          )
    ).slice(0, 128),
    name = customName ?? suggested;
  // The branch follows the name ("Login fix" → feature/login-fix) until the
  // person edits it, so a good name makes a good branch.
  const [customBranch, setCustomBranch] = useState<string>(),
    [editingBranch, setEditingBranch] = useState(false);
  const branch = workspace ? "" : (customBranch ?? branchForName(name)).trim();
  const included = new Set(
    workspace?.members.map((m) => m.spec?.repositoryId || ""),
  );
  const canSave = !!(selected.size || empty) && !!(workspace || name.trim());
  // A double-clicked repository is added, then the dialog finishes on the next
  // render, once the name suggestion reflects the new selection.
  const [finishSoon, setFinishSoon] = useState(false);
  useEffect(() => {
    if (!finishSoon) return;
    setFinishSoon(false);
    void save();
  }, [finishSoon, selected]);
  const save = async () => {
    if (pending.current || !canSave) return;
    pending.current = true;
    setBusy(true);
    setError("");
    setProgress("Preparing workspace…");
    const request = {
      collectionId: workspace?.id || "",
      name: name.trim(),
      repositoryIdentities: [...selected].sort(),
      branch: empty ? "" : branch,
      ...(repositoryRequestId ? { repositoryRequestId } : {}),
    };
    const spec = JSON.stringify(request);
    if (spec !== attempt.current.spec)
      attempt.current = { spec, key: crypto.randomUUID() };
    try {
      let result: Collection | undefined;
      for await (const update of api.prepareWorkspace(
        { ...request, requestKey: attempt.current.key },
        { timeoutMs: 600000 },
      )) {
        setProgress(update.message);
        if (update.collection) result = update.collection;
      }
      if (!result)
        throw Error(
          "Preparation ended before the Runner confirmed the workspace. Retry to check the same request.",
        );
      await reload();
      onSaved(result);
    } catch (error) {
      setError(String(error));
    } finally {
      pending.current = false;
      setBusy(false);
    }
  };
  return (
    <DraggableDialog
      ref={dialog}
      className="workspace-repository-dialog unified-workspace-dialog"
      aria-label={workspace ? "Add repository to workspace" : "New workspace"}
      onCancel={(e) => {
        if (busy) e.preventDefault();
        else onClose();
      }}
    >
      <DialogHeader>
        <div>
          <h2>
            {workspace
              ? `Add repositories to ${workspace.name}`
              : "New workspace"}
          </h2>
          <p>
            {intro ??
              (workspace
                ? "Choose the repositories to include in this workspace."
                : "Choose repositories for this workspace.")}
          </p>
        </div>
        <button
          disabled={busy}
          aria-label="Close workspace dialog"
          onClick={onClose}
        >
          ×
        </button>
      </DialogHeader>
      <div className="workspace-picker-body">
      <RepositoryBrowser
        selected={selected}
        included={included}
        disabled={busy}
        initialQuery={initialQuery}
        onSelected={(value) => {
          setSelected(value);
          setEmpty(false);
        }}
        onActivate={(identity) => {
          setSelected((current) => new Set([...current, identity]));
          setEmpty(false);
          setFinishSoon(true);
        }}
        chooseRepositoryFolder={chooseRepositoryFolder}
      />
      </div>
      {!workspace && (
        <div className="workspace-create-details">
          <label className="workspace-name-field">
            Workspace name
            <input
              aria-label="Workspace name"
              data-name-autofocus
              value={name}
              maxLength={128}
              disabled={busy}
              placeholder="What is this work about?"
              onFocus={(e) => e.target.select()}
              onChange={(e) => setCustomName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (selected.size || empty)) void save();
              }}
            />
          </label>
          {!empty && (editingBranch ? (
            <label className="workspace-branch-field">
              <span className="sr-only">Branch</span>
              <Icon name="branch" size={13} />
              <input
                aria-label="Branch"
                value={branch}
                maxLength={200}
                disabled={busy}
                spellCheck={false}
                autoFocus
                onChange={(e) => setCustomBranch(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); setCustomBranch(undefined); setEditingBranch(false); }
                  if (e.key === "Enter") { e.preventDefault(); setEditingBranch(false); }
                }}
              />
            </label>
          ) : (
            <p className="workspace-branch-preview" aria-live="polite">
              <Icon name="branch" size={13} />
              <span>Branch <code data-testid="branch-preview">{branch || "chosen by the Runner"}</code></span>
              <button type="button" className="link-button" disabled={busy} onClick={() => setEditingBranch(true)}>Edit branch</button>
            </p>
          ))}
          <label className="empty-workspace-choice">
            <input
              type="checkbox"
              checked={empty}
              disabled={busy}
              onChange={(e) => {
                setEmpty(e.target.checked);
                if (e.target.checked) setSelected(new Set());
              }}
            />
            Start without repositories
          </label>
        </div>
      )}
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {busy && (
        <div role="status" className="workspace-preparation"><span>{progress}</span><progress aria-label="Preparing workspace"/><small>Downloading repositories can take a moment. Progress updates appear here.</small></div>
      )}
      <footer>
        <span>
          {selected.size
            ? `${selected.size} ${selected.size === 1 ? "repository" : "repositories"} selected`
            : empty
              ? "Empty workspace"
              : ""}
        </span>
        <button type="button" disabled={busy} onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="primary"
          disabled={
            busy || !canSave
          }
          onClick={() => void save()}
        >
          {busy
            ? "Preparing…"
            : workspace
              ? "Add to workspace"
              : "Create workspace"}
        </button>
      </footer>
    </DraggableDialog>
  );
}
