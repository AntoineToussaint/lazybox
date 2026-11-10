import { useEffect, useRef, useState } from "react";
import type { Collection } from "@lazybox/protocol";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { RepositoryBrowser } from "./RepositoryBrowser";
import { useRepositoryCatalog } from "./repository-catalog";
import { useRunner } from "./RunnerContext";
import { useRunnerSettings } from "./RunnerSettings";
/** One selection workflow for creating a workspace and adding to an existing one. */
export function WorkspaceRepositoryDialog({
  workspace,
  onClose,
  onSaved,
  chooseRepositoryFolder,
}: {
  workspace?: Collection;
  onClose(): void;
  onSaved(workspace: Collection): void;
  chooseRepositoryFolder?(): Promise<string | null>;
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
  const [selected, setSelected] = useState(new Set<string>()),
    [customName, setCustomName] = useState<string>(),
    [empty, setEmpty] = useState(false),
    [busy, setBusy] = useState(false),
    [progress, setProgress] = useState(""),
    [error, setError] = useState("");
  useEffect(() => {
    dialog.current?.showModal();
    dialog.current?.querySelector<HTMLInputElement>('[data-autofocus]')?.focus();
  }, []);
  const suggested = (
      settings?.naming === "generated"
        ? generated.current || "New workspace"
        : [...selected]
            .map((id) =>
              catalog?.entries
                .find((r) => r.identity === id)
                ?.name.split("/")
                .at(-1),
            )
            .filter(Boolean)
            .join(" + ") || "New workspace"
    ).slice(0, 128),
    name = customName ?? suggested;
  const included = new Set(
    workspace?.members.map((m) => m.spec?.repositoryId || ""),
  );
  const canSave = !!(selected.size || empty) && !!(workspace || name.trim());
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
            {workspace
              ? "Choose the repositories to include in this workspace."
              : "Choose repositories for this workspace."}
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
        onSelected={(value) => {
          setSelected(value);
          setEmpty(false);
        }}
        chooseRepositoryFolder={chooseRepositoryFolder}
      />
      </div>
      {!workspace && (
        <div className="workspace-create-details">
          <label>
            Workspace name
            <input
              aria-label="Workspace name"
              value={name}
              maxLength={128}
              disabled={busy}
              onChange={(e) => setCustomName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (selected.size || empty)) void save();
              }}
            />
          </label>
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
