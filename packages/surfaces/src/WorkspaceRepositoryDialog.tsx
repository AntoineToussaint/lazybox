import { useEffect, useRef, useState, type ReactNode } from "react";
import { Code, ConnectError } from "@lazybox/client-core";
import type { ViewStateStore } from "@lazybox/platform";
import type { Collection } from "@lazybox/protocol";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { RepositoryBrowser } from "./RepositoryBrowser";
import { useRepositoryCatalog } from "./repository-catalog";
import { useRunner } from "./RunnerContext";
import { useRunnerSettings } from "./RunnerSettings";
import { branchForName, suggestWorkspaceName } from "./repository-model";
import { Icon } from "./Icons";
import { forgetStarts, StartFromChip, StartFromPicker, useRepositoryStarts } from "./StartFrom";
import { parseTaskChoices, rememberTask, TASK_CHOICES_PREFERENCE } from "./new-task-model";
import { readPreference, writePreference } from "./view-state";
import { startsFor } from "./start-from-model";
/** One selection workflow for creating a workspace and adding to an existing one. */
export function WorkspaceRepositoryDialog({
  workspace,
  copy,
  viewState,
  onClose,
  onSaved,
  chooseRepositoryFolder,
  preselected = [],
  initialQuery,
  repositoryRequestId,
  intro,
}: {
  workspace?: Collection;
  copy?: Collection;
  viewState?: ViewStateStore;
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
  const [remembered] = useState(() => {
    try { return parseTaskChoices(readPreference(viewState, TASK_CHOICES_PREFERENCE)); }
    catch { return parseTaskChoices(null); }
  });
  const [selected, setSelected] = useState(() => new Set<string>(workspace || copy || preselected.length ? preselected : remembered.identities)),
    [customName, setCustomName] = useState<string | undefined>(copy ? `${copy.name.slice(0, 100)} copy` : undefined),
    [empty, setEmpty] = useState(copy ? !copy.members.length : !workspace && !preselected.length && !!remembered.empty),
    [busy, setBusy] = useState(false),
    [progress, setProgress] = useState(""),
    [error, setError] = useState(""),
    [branchConflict, setBranchConflict] = useState("");
  const [conflictingRepo, setConflictingRepo] = useState<{id: string; name: string}>();
  const [confirmDelete, setConfirmDelete] = useState(false);
  useEffect(() => {
    dialog.current?.showModal();
    // Creating starts on the name, its suggestion selected: typing replaces it.
    (dialog.current?.querySelector<HTMLInputElement>('[data-name-autofocus]') || dialog.current?.querySelector<HTMLInputElement>('[data-autofocus]'))?.focus();
  }, []);
  const chosen = empty ? [] : [...selected];
  const suggested = (
      settings?.naming === "generated"
        ? generated.current || "New workspace"
        : suggestWorkspaceName(
            chosen
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
  const conflict = !empty && !!branchConflict && branch === branchConflict;
  const branchInput = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (conflict && editingBranch && !busy) {
      branchInput.current?.focus();
      branchInput.current?.select();
    }
  }, [conflict, editingBranch, busy]);
  const included = new Set(
    workspace?.members.map((m) => m.spec?.repositoryId || ""),
  );
  // Where each chosen repository's new branch starts ("from main ▾"; ⌥B).
  const [startInfo, setStartInfo] = useRepositoryStarts(api, chosen);
  const [starts, setStarts] = useState<Record<string, string>>({}),
    [startPicker, setStartPicker] = useState<string>();
  const [copyLoaded, setCopyLoaded] = useState(!copy?.members.length);
  const [copyError, setCopyError] = useState("");
  useEffect(() => {
    if (!copy || !catalog || copyLoaded) return;
    const next: Record<string, string> = {};
    for (const member of copy.members) {
      const entry = catalog.entries.find(value => value.repositoryId === member.spec?.repositoryId);
      if (!entry) {
        setCopyError(`Could not find ${member.spec?.name || "a repository"} in this Runner’s catalog.`);
        return;
      }
      const base = member.spec?.baseRef || "";
      next[entry.identity] = base === "HEAD" ? "" : base.replace(/^refs\/(remotes\/origin|heads)\//, "");
    }
    setCopyError("");
    setSelected(new Set(Object.keys(next)));
    setStarts(next);
    setCopyLoaded(true);
  }, [copy, catalog, copyLoaded]);
  const repositoryName = (identity: string) =>
    catalog?.entries.find((r) => r.identity === identity)?.name || identity.replace(/^github:/, "");
  const closeStart = (identity: string) => {
    setStartPicker(undefined);
    requestAnimationFrame(() => dialog.current?.querySelector<HTMLElement>(`[data-start-from="${CSS.escape(identity)}"]`)?.focus());
  };
  const canSave = !conflict && copyLoaded && !!(selected.size || empty) && !!(workspace || name.trim());
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
    setBranchConflict("");
    setConflictingRepo(undefined);
    setConfirmDelete(false);
    setProgress("Preparing workspace…");
    const request = {
      collectionId: workspace?.id || "",
      name: name.trim(),
      repositoryIdentities: [...chosen].sort(),
      branch: empty ? "" : branch,
      startFrom: empty ? {} : startsFor(chosen, starts),
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
        if (update.conflictingRepositoryId) {
          setConflictingRepo({id: update.conflictingRepositoryId, name: update.conflictingRepositoryName || update.conflictingRepositoryId});
        }
        setProgress(update.message);
        if (update.collection) result = update.collection;
      }
      if (!result)
        throw Error(
          "Preparation ended before the Runner confirmed the workspace. Retry to check the same request.",
        );
      if (!workspace) {
        try {
          const previous = parseTaskChoices(readPreference(viewState, TASK_CHOICES_PREFERENCE));
          writePreference(viewState, TASK_CHOICES_PREFERENCE, JSON.stringify(rememberTask(previous, { ...previous, identities: request.repositoryIdentities, empty })));
        } catch { /* The workspace has already been created. */ }
      }
      forgetStarts(api, request.repositoryIdentities);
      await reload();
      onSaved(result);
    } catch (error) {
      const failure = ConnectError.from(error);
      if (!workspace && !empty && (failure.code === Code.AlreadyExists ||
          (failure.code === Code.FailedPrecondition &&
            failure.rawMessage === "branch already exists; choose a new collection branch"))) {
        setBranchConflict(branch);
        setEditingBranch(true);
      } else {
        setError(failure.rawMessage);
      }
    } finally {
      pending.current = false;
      setBusy(false);
    }
  };
  const deleteBranch = async () => {
    if (!conflict || !conflictingRepo || pending.current) return;
    pending.current = true;
    setBusy(true);
    setProgress("Deleting local branch…");
    setError("");
    try {
      await api.deleteLocalBranch({repositoryId: conflictingRepo.id, branch});
      setBranchConflict("");
      setConflictingRepo(undefined);
    } catch (error) {
      setError(ConnectError.from(error).rawMessage);
    } finally {
      pending.current = false;
      setConfirmDelete(false);
      setBusy(false);
    }
  };
  return (
    <DraggableDialog
      ref={dialog}
      className="workspace-repository-dialog unified-workspace-dialog"
      aria-label={workspace ? "Add repository to workspace" : copy ? "Clone workspace" : "New workspace"}
      onKeyDownCapture={(e) => {
        // ⌥B: where the branch starts — the focused repository's, else the first chosen.
        if (!e.altKey || e.metaKey || e.ctrlKey || e.code !== "KeyB" || busy || empty || !chosen.length) return;
        e.preventDefault();
        e.stopPropagation();
        const focused = (document.activeElement as HTMLElement | null)?.closest<HTMLElement>("[data-start-from]")?.dataset.startFrom;
        setStartPicker(focused && chosen.includes(focused) ? focused : chosen[0]);
      }}
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
              : copy ? "Clone workspace" : "New workspace"}
          </h2>
          {(intro || workspace || copy) && <p>
            {intro ??
              (workspace
                ? "Choose the repositories to include in this workspace."
                : copy ? "Choose a name for the copy. Each repository gets a fresh branch." : "")}
          </p>}
        </div>
        <button
          disabled={busy}
          aria-label="Close workspace dialog"
          onClick={onClose}
        >
          ×
        </button>
      </DialogHeader>
      {!workspace && (
        <label className="empty-workspace-choice">
          <input
            type="checkbox"
            checked={empty}
            disabled={busy || !copyLoaded}
            onChange={(e) => {
              setBranchConflict("");
              setEmpty(e.target.checked);
              setStartPicker(undefined);
            }}
          />
          Start without a repository
        </label>
      )}
      {copy && !copyLoaded && <p role="status">{copyError || "Loading the workspace’s repositories…"}</p>}
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
            <div className="workspace-branch-editor">
            <label className="workspace-branch-field">
              <span className="sr-only">Branch</span>
              <Icon name="branch" size={13} />
              <input
                aria-label="Branch"
                ref={branchInput}
                aria-invalid={conflict || undefined}
                aria-describedby={conflict ? "workspace-branch-conflict" : undefined}
                value={branch}
                maxLength={200}
                disabled={busy}
                spellCheck={false}
                autoFocus
                onChange={(e) => setCustomBranch(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") {
                    e.preventDefault();
                    e.stopPropagation();
                    setCustomBranch(undefined);
                    setEditingBranch(branchForName(name).trim() === branchConflict);
                  }
                  if (e.key === "Enter") { e.preventDefault(); if (!conflict) setEditingBranch(false); }
                }}
              />
            </label>
            {conflict && <p className="workspace-branch-error" id="workspace-branch-conflict" role="alert">This branch already exists. Enter a different branch name.</p>}
            {conflict && conflictingRepo && (confirmDelete ? <div>
              <p>Delete local branch <code>{branch}</code> in <strong>{conflictingRepo.name}</strong>?</p>
              <button type="button" disabled={busy} onClick={() => setConfirmDelete(false)}>Keep branch</button>
              <button type="button" disabled={busy} onClick={() => void deleteBranch()}>Confirm deletion</button>
            </div> : <button type="button" disabled={busy} onClick={() => setConfirmDelete(true)}>Delete local branch…</button>)}
            </div>
          ) : (
            <p className="workspace-branch-preview" aria-live="polite">
              <Icon name="branch" size={13} />
              <span>Branch <code data-testid="branch-preview">{branch || "chosen by the Runner"}</code></span>
              <button type="button" className="link-button" disabled={busy} onClick={() => setEditingBranch(true)}>Edit branch</button>
            </p>
          ))}
        </div>
      )}
      {!empty && <div className="workspace-picker-body">
        <RepositoryBrowser
          splitSelection={!workspace}
          renderBranch={identity => (<>
            <StartFromChip
              identity={identity}
              repository={repositoryName(identity)}
              branch={starts[identity] ?? ""}
              info={startInfo[identity]}
              disabled={busy}
              expanded={startPicker === identity}
              onOpen={() => (startPicker === identity ? closeStart(identity) : setStartPicker(identity))}
            />
            {startPicker === identity && !busy && (
              <StartFromPicker
                api={api}
                identity={identity}
                repository={repositoryName(identity)}
                info={startInfo[identity]}
                current={starts[identity] ?? ""}
                onInfo={(info) => setStartInfo(identity, info)}
                onClose={() => closeStart(identity)}
                onPick={(picked) => {
                  setStarts((current) => ({ ...current, [identity]: picked }));
                  closeStart(identity);
                }}
              />
            )}
          </>)}
          selected={selected}
          included={included}
          disabled={busy || !copyLoaded}
          initialQuery={initialQuery}
          onSelected={(value) => {
            setBranchConflict("");
            setSelected(value);
            setEmpty(false);
          }}
          onActivate={(identity) => {
            setBranchConflict("");
            setSelected((current) => new Set([...current, identity]));
            setEmpty(false);
            setFinishSoon(true);
          }}
          chooseRepositoryFolder={chooseRepositoryFolder}
        />
      </div>}
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
          {chosen.length
            ? `${chosen.length} ${chosen.length === 1 ? "repository" : "repositories"} selected`
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
