import { peekProps } from './peek-model';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import { contextMenu } from './ContextMenu';
import type { MenuItem } from './context-menu-model';
import { useRef, useState } from "react";
import type { Collection, ItemLink, Session, WorkspaceGitSummary } from "@lazybox/protocol";
import { branchSummary } from "./conversation-model";
import { agentLine, type RowGit } from "./workspace-row-model";
import { GitLine } from "./WorkspaceGitLine";
import { Icon } from "./Icons";
import { useRunner } from "./RunnerContext";
import { workspaceTerminalItem } from "./handoff";
import { useUnread } from "./useAttention";
import { ConversationProviderIcon, SessionMark, useLiveStatus } from "./LiveIndicator";
import { generatedName, repositoryGroups, suggestWorkspaceName } from "./repository-model";
import { useWorkspaceHover, WorkspaceHoverCard } from "./WorkspaceHover";
import { OwnershipDialog } from "./OwnershipPanel";
import "./inbox.css";

/**
 * A workspace's repositories on one line, whatever their number: the owner
 * once, then short names, "+N" for the rest ("codefly-dev · authority, cli,
 * core +2"). The full list is the tooltip, and the workspace's repository
 * dialog. Screen readers (and copy) get every "owner/name".
 */
export function RepositorySummary({ names, limit = 3 }: { names: readonly string[]; limit?: number }) {
  // Not "empty": the app-wide .empty style (a centred, padded placeholder) would make this one row tall.
  if (!names.length) return <small className="workspace-repo-names is-empty">No repositories</small>;
  const { owner, repositories } = repositoryGroups(names);
  const shown = repositories.length <= limit + 1 ? repositories : repositories.slice(0, limit);
  const hidden = repositories.length - shown.length;
  return <small className="workspace-repo-names" title={repositories.map(repo => repo.full).join("\n")}>
    <span className="sr-only">Repositories: {repositories.map(repo => repo.full).join(", ")}</span>
    <span aria-hidden="true">{owner && <span className="workspace-repo-owner">{owner} · </span>}{shown.map(repo => repo.short).join(", ")}{hidden > 0 && <span className="workspace-repo-more"> +{hidden}</span>}</span>
  </small>;
}

/** Row actions retain their original workspace identity, independently of navigation. */
export function WorkspaceRow({
  workspace,
  repositoryNames,
  selected,
  onSelect,
  onDelete,
  onClone,
  onManage,
  onAdd,
  onSaved,
  conversation,
  featured = conversation,
  git,
  conversationCount,
  onNewConversation,
  onDeleteConversation,
  fold,
  hover,
}: {
  /** What its hover card shows beyond the row (WorkspaceHover.tsx); without it, no card. */
  hover?: { sessions: readonly Session[]; summary?: WorkspaceGitSummary; links: readonly ItemLink[]; onOpenConversation(sessionId: string): void };
  /** Its conversations listed under it, and whether they are folded away. */
  fold?: { folded: boolean; count: number; toggle(): void };
  /** The workspace's only conversation, which the row's menu can delete. */
  conversation?: Session;
  /** The conversation the agent line speaks for (WorkspaceRow model: attention, then work, then newest). */
  featured?: Session;
  /** The git line; undefined until the Runner answered. */
  git?: RowGit;
  conversationCount: number;
  onNewConversation(): void;
  onDeleteConversation(): void;
  workspace: Collection;
  repositoryNames: string[];
  selected: boolean;
  onSelect(): void;
  onDelete(): void;
  onClone?(): void;
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
  const unread = useUnread();
  const card = useWorkspaceHover(workspace.id);
  const marker = featured ? unread(featured) : "";
  const line = featured ? agentLine(featured, info?.profiles || [], Math.max(0, conversationCount - 1)) : undefined;
  const rename = () => { setName(workspace.name); setError(""); setEditing(true); };
  // What a previous Lazybox left here unconfirmed: shown and settled first,
  // then the action that waited for it goes on ("then").
  const [settling, setSettling] = useState<{ then(): void }>();
  const newConversation = async () => {
    if (!workspace.ownershipUncertain) return onNewConversation();
    // The Runner clears by itself what it proves stopped: then nothing is asked.
    try {
      if ((await api.getOwnership({ collectionId: workspace.id, sessionId: "" }, { timeoutMs: 30_000 })).settled) { onSaved(); onNewConversation(); return; }
    } catch { /* the dialog says what failed */ }
    setSettling({ then: onNewConversation });
  };
  // The ⋯ menu and the row's context menu: one list.
  const items: MenuItem[] = [
    { label: "New conversation", icon: "plus", disabled: workspace.state !== "ready" && "The workspace is not ready", run: newConversation },
    workspace.ownershipUncertain && { label: "Settle unconfirmed processes…", icon: "alert", run: () => setSettling({ then: onSaved }) },
    "separator",
    { label: "Add repositories…", icon: "repository", run: onAdd },
    { label: "Manage repositories…", icon: "sliders", run: onManage },
    !!onClone && { label: "Clone workspace…", icon: "plus", disabled: workspace.state !== "ready" && "The workspace is not ready", run: onClone },
    { label: "Rename workspace", icon: "pencil", shortcut: "F2", run: rename },
    workspaceTerminalItem(api, workspace.id),
    "separator",
    conversation && { label: "Delete conversation…", danger: true, run: onDeleteConversation },
    { label: "Delete workspace…", danger: true, run: onDelete },
  ];
  return (
    <div ref={card.row} className="workspace-row" data-unread={marker || undefined} data-folded={fold?.folded || undefined} onContextMenu={editing ? undefined : contextMenu("workspace", () => items, { workspaceId: workspace.id })} onKeyDown={event => {
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
          {...(featured ? peekProps({ kind: "conversation", sessionId: featured.id, workspaceId: workspace.id, title: featured.title }) : {})}
          data-navigation-item
          aria-current={selected ? "page" : undefined}
          onClick={onSelect}
          {...(hover ? { ...card.props, "data-hover-card": "" } : {})}
        >
          <Icon name="folder" size={16} />
          <span className="workspace-label">
            <strong>{workspace.name}</strong>
            <RepositorySummary names={repositoryNames} />
            <GitLine git={git} fallback={branchSummary(workspace)} empty={!workspace.members.length} />
            {fold && !fold.folded ? null : featured && line ? (
              // Its conversations listed under the card say this line already; folded, it speaks for them.
              <small className="workspace-agent" title={line.tooltip}>
                <SessionMark session={featured} unread={marker} />
                {marker && <span className="sr-only">{marker === "failed" ? "Failed, unread" : "Ready for review, unread"}</span>}
                <ConversationProviderIcon session={featured} />
                <span className="workspace-agent-label">{line.title}</span>
                {line.more && <span className="workspace-agent-more" title={`${conversationCount} conversations`}>{line.more}</span>}
                <AgentStatus session={featured} />
              </small>
            ) : (
              // Every card has the same four lines, so the list reads as a grid.
              <small className="workspace-agent workspace-conversation-count">
                <span className="session-mark" aria-hidden="true"><Icon name="chat" size={11} /></span>
                <span className="workspace-agent-label">{conversationCount ? `${conversationCount} ${conversationCount === 1 ? "conversation" : "conversations"}` : "No conversation yet"}</span>
              </small>
            )}
          </span>
          {workspace.ownershipUncertain && (
            <span className="workspace-owner-uncertain" role="img" aria-label="Owner uncertain" title="Owner uncertain: a previous Lazybox left agents or commands here that it can’t confirm stopped. New conversations and deleting wait until that is settled. New conversation, or Settle unconfirmed processes in this workspace’s menu, shows which and lets you stop them or say they’ve stopped.">
              <Icon name="alert" size={12} />
            </span>
          )}
        </button>
      )}
      {!editing && fold && (
        // In the folder icon's place: the workspace's conversations, shown or folded.
        <button
          className="workspace-fold"
          aria-expanded={!fold.folded}
          aria-label={`${fold.folded ? "Show" : "Fold"} the ${fold.count} conversations of ${workspace.name}`}
          title={fold.folded ? `Show ${fold.count} conversations` : "Fold conversations"}
          onClick={fold.toggle}
        >
          <Icon name={fold.folded ? "chevronRight" : "chevron"} size={13} />
        </button>
      )}
      {!editing && !repositoryNames.length && (
        // In the repository line's slot, so an empty workspace is the same card as any other.
        <button className="workspace-add-repositories" aria-label={`Add repositories to ${workspace.name}`} onClick={onAdd}>Add…</button>
      )}
      {!editing && generatedName(workspace.name) && (
        // A generated name says nothing about the work: one click names it, never a prompt.
        <button
          className="workspace-name-it"
          aria-label={`Name ${workspace.name}`}
          title="Give this workspace a name that says what the work is"
          onClick={() => { setName(suggestWorkspaceName(repositoryNames)); setError(""); setEditing(true); }}
        >
          <Icon name="pencil" size={11} />
          <span>Name it</span>
        </button>
      )}
      {!editing && (
        <button
          className="workspace-new-conversation icon-button"
          aria-label={`New conversation in ${workspace.name}`}
          title="New conversation"
          disabled={workspace.state !== "ready"}
          onClick={newConversation}
        >
          <Icon name="plus" size={15} />
        </button>
      )}
      {settling && (
        <OwnershipDialog target={{ collectionId: workspace.id }} title="Settle what a previous Lazybox left here" subject={workspace.name}
          what="New conversations and deleting this workspace wait" onClose={() => setSettling(undefined)}
          onSettled={() => { const then = settling.then; setSettling(undefined); onSaved(); then(); }}/>
      )}
      {!editing && (
        <ActionMenu ref={action} label={`Actions for ${workspace.name}`}>
          <ActionMenuItems items={items}/>
        </ActionMenu>
      )}
      {hover && card.open && !editing && (
        <WorkspaceHoverCard open={card.open} workspace={workspace} repositoryNames={repositoryNames} sessions={hover.sessions} summary={hover.summary} links={hover.links}
          onOpen={onSelect} onNewConversation={newConversation} onOpenConversation={hover.onOpenConversation} />
      )}
    </div>
  );
}

/**
 * The agent line's status: its words, then the time the step has taken. One
 * indicator per row (the leading mark); the elapsed time is never cut, the
 * words give way before it, and the title before both.
 */
function AgentStatus({ session }: { session: Session }) {
  const status = useLiveStatus(session);
  const elapsed = status.since !== undefined && status.text.startsWith(status.label + " · ") ? status.text.slice(status.label.length + 3) : "";
  const words = elapsed ? status.label : status.text;
  return <span className="workspace-agent-status" data-tone={status.tone} title={status.text}>
    <span className="workspace-agent-words">{words}</span>
    {elapsed && <span className="workspace-agent-elapsed"> · {elapsed}</span>}
  </span>;
}
