import { ActionMenu } from './ActionMenu';
import { useRef, useState } from "react";
import type { Collection, Session } from "@lazybox/protocol";
import { conversationDetails } from "./conversation-model";
import { sessionStatus } from "./agent-status";
import { Icon } from "./Icons";
import { useRunner } from "./RunnerContext";

/** Row actions retain their original workspace identity, independently of navigation. */
export function WorkspaceRow({
  workspace,
  repositoryNames,
  selected,
  onSelect,
  onDelete,
  onManage,
  onAdd,
  onSaved,
  conversation,
  conversationCount,
  onNewConversation,
  onDeleteConversation,
}: {
  conversation?: Session;
  conversationCount: number;
  onNewConversation(): void;
  onDeleteConversation(): void;
  workspace: Collection;
  repositoryNames: string[];
  selected: boolean;
  onSelect(): void;
  onDelete(): void;
  onManage(): void;
  onAdd(): void;
  onSaved(): void;
}) {
  const { api, info } = useRunner();
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState("");
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const pending = useRef(false);
  const action = useRef<HTMLDetailsElement>(null);
  return (
    <div className="workspace-row" onContextMenu={event => {
      if (editing || !action.current) return;
      event.preventDefault();
      for (const menu of document.querySelectorAll<HTMLDetailsElement>('.workspace-actions[open]')) menu.open = false;
      action.current.open = true;
      action.current.querySelector<HTMLButtonElement>('button')?.focus();
    }} onKeyDown={event => {
      if (event.key === 'F2' && !editing) { event.preventDefault(); setName(workspace.name); setError(''); setEditing(true); }
    }}>
      {editing ? (
        <form
          className="workspace-inline-rename"
          aria-label={`Rename ${workspace.name}`}
          onSubmit={async (event) => {
            event.preventDefault();
            if (pending.current || !name.trim()) return;
            pending.current = true;
            setSaving(true);
            setError("");
            try {
              await api.renameCollection({
                collectionId: workspace.id,
                name: name.trim(),
              });
              setEditing(false);
              onSaved();
            } catch (error) {
              setError(String(error));
            } finally {
              pending.current = false;
              setSaving(false);
            }
          }}
        >
          <label>
            <span className="sr-only">Workspace name</span>
            <input
              autoFocus
              aria-label="Workspace name"
              value={name}
              maxLength={128}
              disabled={saving}
              onFocus={(event) => event.target.select()}
              onChange={(event) => setName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape" && !saving) {
                  event.preventDefault();
                  setEditing(false);
                }
              }}
            />
          </label>
          <div className="inline-editor-actions">
            <button
              className="rename-save"
              aria-label="Save name"
              disabled={saving || !name.trim()}
            >
              {saving ? "Saving…" : "Save"}
            </button>
            <button
              type="button"
              disabled={saving}
              onClick={() => setEditing(false)}
            >
              Cancel
            </button>
          </div>
          {error && <p role="alert">{error}</p>}
        </form>
      ) : (
        <button
          className="workspace-link"
          data-navigation-item
          aria-current={selected ? "page" : undefined}
          onClick={onSelect}
        >
          <Icon name="folder" size={16} />
          <span className="workspace-label">
            <strong>{workspace.name}</strong>
            <small className="workspace-repo-names" aria-label="Repositories in workspace">
              {repositoryNames.length ? repositoryNames.map((name, i) => <span key={i} title={name}>{name}</span>) : "No repositories"}
            </small>
            {conversation && (
              <small
                className="workspace-agent"
                title={
                  conversationDetails(conversation, info?.profiles || []) +
                  " · " +
                  sessionStatus(conversation)
                }
              >
                <span
                  className={`session-dot status-${conversation.closed ? "saved" : conversation.status}`}
                />
                <span className="workspace-agent-label">{conversation.provider === "codex" ? "Codex" : "Claude"}
                {conversation.model ? ` · ${conversation.model}` : ""}</span>
                <span className="workspace-agent-status">
                  {sessionStatus(conversation)}
                </span>
              </small>
            )}
            {conversationCount > 1 && (
              <small className="workspace-conversation-count">
                {conversationCount} conversations
              </small>
            )}
          </span>
          {workspace.ownershipUncertain && (
            <span title="Owner uncertain">!</span>
          )}
        </button>
      )}
      {!editing && (
        <button
          className="workspace-new-conversation icon-button"
          aria-label={`New conversation in ${workspace.name}`}
          title="New conversation"
          disabled={workspace.state !== "ready" || workspace.ownershipUncertain}
          onClick={onNewConversation}
        >
          <Icon name="plus" size={15} />
        </button>
      )}
      {!editing && (
        <ActionMenu ref={action} label={`Actions for ${workspace.name}`}>
            <button
              onClick={() => {
                action.current!.open = false;
                onAdd();
              }}
            >
              Add repositories…
            </button>
            <button
              onClick={() => {
                action.current!.open = false;
                onManage();
              }}
            >
              Manage repositories…
            </button>
            <button
              onClick={() => {
                action.current!.open = false;
                setName(workspace.name);
                setError("");
                setEditing(true);
              }}
            >
              Rename workspace
            </button>
            {conversation && (
              <button
                className="danger"
                onClick={() => {
                  action.current!.open = false;
                  onDeleteConversation();
                }}
              >
                Delete conversation…
              </button>
            )}
            <button
              className="danger"
              onClick={() => {
                action.current!.open = false;
                onDelete();
              }}
            >
              Delete workspace…
            </button>
        </ActionMenu>
      )}
    </div>
  );
}
