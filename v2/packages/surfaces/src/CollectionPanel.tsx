import { useKeyboardAction } from "./keyboard";
import { useConversationIndex } from "./useConversationIndex";
import { DeleteConversationDialog } from "./DeleteConversationDialog";
import { repositoryLabel } from "./repository-model";
import { WorkspaceRepositories } from "./WorkspaceRepositories";
import { WorkspaceRow } from "./WorkspaceRow";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { DeleteWorkspaceDialog } from "./DeleteWorkspaceDialog";
import { WorkspaceRepositoryDialog } from "./WorkspaceRepositoryDialog";
import { ConversationList } from "./ConversationList";
import { useEffect, useRef, useState } from "react";
import type { Collection, Repository, Session } from "@lazybox/protocol";
import { useRunner } from "./RunnerContext";
import type { ViewTarget } from "./workspace-model";
import { Icon } from "./Icons";

export function CollectionPanel({
  target,
  onTarget,
  collections,
  error,
  refresh,
  chooseRepositoryFolder,
  onNewConversation,
}: {
  target: ViewTarget;
  onTarget(target: ViewTarget): void;
  collections: Collection[];
  error: string;
  refresh(): void;
  chooseRepositoryFolder?(): Promise<string | null>;
  onNewConversation?(workspaceId: string): void;
}) {
  const { info, api, error: connectionError } = useRunner();
  const index = useConversationIndex();
  const [deletingConversation, setDeletingConversation] = useState<Session>();
  const panel = useRef<HTMLElement>(null);
  useEffect(() => {
    const closeOutside = (event: PointerEvent) => {
      for (const menu of panel.current?.querySelectorAll<HTMLDetailsElement>(
        ".workspace-actions[open]",
      ) || [])
        if (!menu.contains(event.target as Node)) menu.open = false;
    };
    document.addEventListener("pointerdown", closeOutside);
    return () => document.removeEventListener("pointerdown", closeOutside);
  }, []);
  const [repositories, setRepositories] = useState<Repository[]>([]);
  const { revision } = useRunner();
  useEffect(() => {
    const control = new AbortController();
    void api
      .listRepositories({}, { signal: control.signal })
      .then((value) => {
        if (!control.signal.aborted) setRepositories(value.repositories);
      })
      .catch(() => {});
    return () => control.abort();
  }, [api, revision, collections]);
  const [historyOpen, setHistoryOpen] = useState(false);
  const historyDialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    if (historyOpen) {
      historyDialog.current?.showModal();
      historyDialog.current
        ?.querySelector<HTMLInputElement>('input[type="search"]')
        ?.focus();
    }
  }, [historyOpen]);
  const [deleting, setDeleting] = useState<Collection>();
  const [creating, setCreating] = useState(false);
  const [addingTo, setAddingTo] = useState<Collection>();
  const [managing, setManaging] = useState<Collection>();
  useKeyboardAction("workspace.new", () => setCreating(true));
  useKeyboardAction("conversation.new", () => {
    if (onNewConversation) { onNewConversation(target.collectionId); return; }
    if (target.collectionId)
      onTarget({ ...target, sessionId: "", commandId: "" });
  });
  const selected = collections.find(
    (value) => value.id === target.collectionId,
  );
  const choose = (collection: Collection) => {
    const rows = index.sessions.filter((s) => s.collectionId === collection.id);
    const sessionId =
      index.complete && rows.length === 1
        ? rows[0].id
        : target.collectionId === collection.id
          ? target.sessionId
          : rows[0]?.id || "";
    onTarget({ collectionId: collection.id, sessionId, commandId: "" });
  };
  return (
    <section ref={panel} className="collection-panel">
      <div className="explorer-heading">
        <span>Workspaces</span>
        <div>
          <button
            className="icon-button"
            aria-label="Refresh workspaces"
            title="Refresh"
            onClick={refresh}
          >
            <Icon name="refresh" size={14} />
          </button>
          <button
            className="icon-button"
            aria-label="New workspace"
            title="New workspace"
            onClick={() => {
              setCreating(true);
            }}
          >
            <Icon name="plus" size={16} />
          </button>
        </div>
      </div>
      {error && !connectionError && (
        <p className="error" role="alert">
          Workspaces unavailable: {error}
        </p>
      )}
      {index.unsupported&&<p className="module-empty">Live conversations · saved history requires a Runner update.</p>}
      {index.error&&!connectionError&&<p className="error" role="alert">Could not refresh conversation history. Previously loaded entries are retained.</p>}
      {creating && (
        <WorkspaceRepositoryDialog
          chooseRepositoryFolder={chooseRepositoryFolder}
          onClose={() => setCreating(false)}
          onSaved={(workspace) => {
            setCreating(false);
            refresh();
            onTarget({
              collectionId: workspace.id,
              sessionId: "",
              commandId: "",
            });
          }}
        />
      )}
      {deleting && (
        <DeleteWorkspaceDialog
          workspace={deleting}
          onClose={() => setDeleting(undefined)}
          onDeleted={() => {
            setDeleting(undefined);
            refresh();
            if (target.collectionId === deleting.id)
              onTarget({ collectionId: "", sessionId: "", commandId: "" });
          }}
        />
      )}
      <nav className="collection-list" aria-label="Workspaces">
        {collections.map((collection) => {
          const rows = index.sessions.filter(
            (s) => s.collectionId === collection.id,
          );
          const single =
            index.complete && rows.length === 1 ? rows[0] : undefined;
          return (
            <div key={collection.id} className="collection-group">
              <WorkspaceRow
                conversation={single}
                conversationCount={rows.length}
                onNewConversation={() => onNewConversation ? onNewConversation(collection.id) :
                  onTarget({
                    collectionId: collection.id,
                    sessionId: "",
                    commandId: "",
                  })
                }
                onDeleteConversation={() => setDeletingConversation(single)}
                workspace={collection}
                repositoryNames={collection.members.map((member) => {
                  const repo = repositories.find(
                    (value) => value.id === member.spec?.repositoryId,
                  );
                  return repo
                    ? repositoryLabel(repo)
                    : member.spec?.name || "Repository unavailable";
                })}
                selected={target.collectionId === collection.id}
                onSelect={() => choose(collection)}
                onDelete={() => setDeleting(collection)}
                onAdd={() => setAddingTo(collection)}
                onManage={() => setManaging(collection)}
                onSaved={refresh}
              />
              {target.collectionId === collection.id && rows.length > 1 && (
                <div className="workspace-conversations">
                  <ConversationList
                    sessions={rows}
                    compact
                    collectionId={collection.id}
                    target={target}
                    onTarget={onTarget}
                  />
                </div>
              )}
            </div>
          );
        })}
      </nav>
      {!error && !collections.length && (
        <p className="module-empty">
          Create a workspace to group your repositories and conversations.
        </p>
      )}
      <ConversationList
        sessions={index.sessions}
        compact
        hideWhenEmpty
        heading="Without a workspace"
        target={target}
        onTarget={onTarget}
      />
      <button
        aria-label="Conversation history"
        className="history-shortcut"
        onClick={() => setHistoryOpen(true)}
      >
        <Icon name="history" size={14} />
        History
      </button>
      {deletingConversation && (
        <DeleteConversationDialog
          session={deletingConversation}
          onClose={() => setDeletingConversation(undefined)}
          onDeleted={() => {
            if (target.sessionId === deletingConversation.id)
              onTarget({ ...target, sessionId: "", commandId: "" });
            setDeletingConversation(undefined);
          }}
        />
      )}
      {historyOpen && (
        <DraggableDialog
          ref={historyDialog}
          className="conversation-history-dialog"
          aria-label="Conversation history"
          onCancel={() => setHistoryOpen(false)}
        >
          <DialogHeader>
            <h2>Conversation history</h2>
            <button
              aria-label="Close conversation history"
              onClick={() => setHistoryOpen(false)}
            >
              ×
            </button>
          </DialogHeader>
          <p>
            All workspaces, including conversations kept after a workspace was
            deleted.
          </p>
          <ConversationList
            allWorkspaces
            target={target}
            onTarget={(value) => {
              setHistoryOpen(false);
              onTarget(value);
            }}
          />
        </DraggableDialog>
      )}
      {target.collectionId && !selected && !error && (
        <p className="module-empty">
          This workspace is unavailable. Its saved conversations can still be
          read.
        </p>
      )}
      {addingTo && (
        <WorkspaceRepositoryDialog
          workspace={addingTo}
          onClose={() => setAddingTo(undefined)}
          onSaved={() => {
            setAddingTo(undefined);
            refresh();
          }}
          chooseRepositoryFolder={chooseRepositoryFolder}
        />
      )}
      {managing && (
        <WorkspaceRepositories
          workspace={
            collections.find((value) => value.id === managing.id) || managing
          }
          onClose={() => setManaging(undefined)}
          onSaved={refresh}
          chooseRepositoryFolder={chooseRepositoryFolder}
        />
      )}
    </section>
  );
}
