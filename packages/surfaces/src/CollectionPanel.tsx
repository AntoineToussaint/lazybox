import { useKeyboardAction, useKeymap } from "./keyboard";
import { useConversationIndex } from "./useConversationIndex";
import { isWorkConversation } from "./attention";
import { DeleteConversationDialog } from "./DeleteConversationDialog";
import { repositoryLabel } from "./repository-model";
import { WorkspaceRepositories } from "./WorkspaceRepositories";
import { WorkspaceRow } from "./WorkspaceRow";
import { DraggableDialog, DialogHeader } from "./DraggableDialog";
import { DeleteWorkspaceDialog } from "./DeleteWorkspaceDialog";
import { WorkspaceRepositoryDialog } from "./WorkspaceRepositoryDialog";
import { ConversationList } from "./ConversationList";
import { NewTaskStart } from "./NewTaskSheet";
import { useEffect, useRef, useState } from "react";
import type { Collection, Repository, Session } from "@lazybox/protocol";
import { useRunner } from "./RunnerContext";
import { RunnerHealthDot, useMergedInbox, useRunnerDirectory, type RunnerView } from "./RunnerDirectory";
import { groupByMachine, linkStateText, routeLabel, rttText, runnerCounts, runnerStateText, SLOW_RTT_MS, thisMachineLabel, type Machine, type RunnerRow } from "./runner-directory";
import { AttentionIcon } from "./Inbox";
import type { AttentionState } from "./attention";
import { bindings, displayBinding, runAction, useActionRevision } from "./actions";
import type { ViewTarget } from "./workspace-model";
import { Icon } from "./Icons";
import { useListReorder } from "./useListReorder";
import { showToast } from "./toasts";
import { useGitSummaries } from "./useGitSummaries";
import { featuredConversation, rowGit } from "./workspace-row-model";
import { useTrackedItems } from "./useTrackedPulls";
import { rowPull } from "./pull-requests-model";
import { isArchivedAway } from "./agents-model";
import { readPreference, writePreference } from "./view-state";
import type { ViewStateStore } from "@lazybox/platform";

type ListProps = {
  target: ViewTarget;
  onTarget(target: ViewTarget): void;
  collections: Collection[];
  error: string;
  /** Whether the Runner's list has been received at all in this window. */
  loaded?: boolean;
  refresh(): void;
  chooseRepositoryFolder?(): Promise<string | null>;
  onNewConversation?(workspaceId: string): void;
  /** Where folded workspaces are remembered. */
  viewState?: ViewStateStore;
};

/**
 * Workspaces: every workspace of the Runner the page shows, with its
 * conversations. When the window shows several Runners, one collapsible
 * section per Runner (its name, channel, version and health on the header,
 * what waits for the person beside it). The page's Runner lists its
 * workspaces in full; another Runner's are one line each, with what waits
 * there, so every Runner fits on screen — choosing one shows that Runner's
 * page. A Runner that is not reachable says so on its header.
 */
export function CollectionPanel(props: ListProps) {
  const directory = useRunnerDirectory();
  if (!directory?.multi) return <WorkspaceList {...props}/>;
  return <RunnerGroups {...props}/>;
}

function RunnerGroups(props: ListProps) {
  const directory = useRunnerDirectory()!;
  const inbox = useMergedInbox();
  const [folded, setFolded] = useState<string[]>(() => { try { const value: unknown = JSON.parse(readPreference(props.viewState, "runners.folded") || "[]"); return Array.isArray(value) ? value.filter((id): id is string => typeof id === "string").slice(0, 64) : []; } catch { return []; } });
  const toggle = (id: string) => {
    const next = folded.includes(id) ? folded.filter((value) => value !== id) : [...folded, id];
    setFolded(next);
    try { writePreference(props.viewState, "runners.folded", JSON.stringify(next)); } catch { /* a convenience */ }
  };
  // Runners on other computers: grouped under the computer they run on, this one first.
  const machines = groupByMachine(directory.shown, thisMachineLabel());
  const byMachine = machines.some((machine) => machine.remote);
  const showing = directory.runners.find((row) => row.id === directory.active)?.label || "this Runner";
  const keymap = useKeymap(props.viewState);
  useActionRevision();
  const first = bindings(keymap["task.new"])[0], newKeys = first ? displayBinding(first).join("") : "";
  // A Runner's +: New task on that Runner (the page moves to it once the task starts).
  const newOn = (id: string) => runAction("task.new", { runner: id });
  return (
    <section className="collection-panel runner-grouped">
      <div className="explorer-heading">
        <span>Workspaces</span>
        <div>
          <button className="icon-button" aria-label="Refresh workspaces" title="Refresh" onClick={() => { props.refresh(); void directory.reread(); }}>
            <Icon name="refresh" size={14} />
          </button>
          <button className="icon-button" aria-label={`New workspace on ${showing}`} title={`New workspace on ${showing}, the Runner the page shows${newKeys ? ` (${newKeys})` : ""}\nOpens New task: repositories, an agent and your first message. Each Runner's + starts on that one.`} onClick={() => newOn(directory.active)}>
            <Icon name="plus" size={16} />
          </button>
        </div>
      </div>
      <nav className="runner-groups" aria-label="Runners">
        {machines.map((machine) => (
          <section key={machine.id} className="machine-group" data-machine={machine.id} data-remote={machine.remote || undefined} data-link={machine.remote ? (machine.state ? "down" : "up") : undefined} aria-label={byMachine ? machine.label : undefined}>
            {byMachine && <MachineHeader machine={machine} />}
            {machine.rows.map((row) => {
              const active = row.id === directory.active, shut = folded.includes(row.id);
              return (
                <section key={row.id} className="runner-group" data-runner={row.id} data-health={row.health} data-active={active || undefined} aria-label={`${row.label} workspaces`}>
                  <RunnerGroupHeader row={row} active={active} folded={shut} counts={inbox.byRunner[row.id]} onToggle={() => toggle(row.id)} onShow={() => directory.activate(row.id)} onNew={() => newOn(row.id)} />
                  {!shut && (active
                    ? <WorkspaceList {...props} embedded />
                    : row.state && (row.health === "online" || row.health === "offline")
                      ? <RunnerWorkspaces row={row} rows={inbox.rows.filter((value) => value.runner === row.id)} onNew={() => newOn(row.id)} onOpen={(collectionId) => props.onTarget({ collectionId, sessionId: "", commandId: "", runner: row.id } as ViewTarget)} />
                      : <RunnerAbsent row={row} />)}
                </section>
              );
            })}
          </section>
        ))}
      </nav>
    </section>
  );
}

/** Another Runner's workspaces, one line each: its name and what waits there. Choosing one shows that Runner's page on it. */
function RunnerWorkspaces({ row, rows, onOpen, onNew }: { row: RunnerView; rows: RunnerRow[]; onOpen(collectionId: string): void; onNew(): void }) {
  // Each Runner keeps its own order: a row moves within its Runner's section only.
  const api = row.health === "online" ? row.state?.api : undefined;
  const reorder = useListReorder({
    items: row.collections, grip: ".runner-workspace-row", name: (collection) => collection.name || collection.id,
    arrange: api && (async (collectionIds) => { await api.arrangeWorkspaces({ collectionIds }, { timeoutMs: 10000 }); }),
    onError: (body) => showToast({ tone: "failed", title: `Workspaces on ${row.label} not reordered`, body }),
  });
  if (!row.collectionsLoaded) return <p className="runner-absent" role="status">Loading workspaces…</p>;
  if (!row.collections.length) return (
    <p className="module-empty runner-empty">
      No workspaces on {row.label} yet.
      {row.health === "online" && <button type="button" className="link-button" title={`New workspace on ${row.label}: opens New task`} onClick={onNew}>Create one</button>}
    </p>
  );
  return (
    <ul className="runner-workspaces" aria-label={`${row.label} workspaces`} {...reorder.listProps}>
      <li className="sr-only" role="status" aria-live="polite">{reorder.announcement}</li>
      {reorder.items.map((collection) => {
        const here = rows.filter((value) => value.session.collectionId === collection.id && !isArchivedAway(value.session));
        const top = here[0], waiting = here.filter((value) => value.state === "needs-input").length;
        const summary = waiting ? `${waiting} waiting for you` : top?.state === "failed" ? "Failed" : top?.state === "review" ? "Ready for review" : top?.state === "working" ? `${here.filter((value) => value.state === "working").length} working` : here.length ? `${here.length} ${here.length === 1 ? "conversation" : "conversations"}` : "No conversations";
        return (
          <li key={collection.id} {...reorder.itemProps(collection.id)}>
            <button type="button" className="runner-workspace-row" data-state={top?.state} disabled={row.health !== "online"} title={`${collection.name || collection.id} on ${row.label}\nShow it here`} onClick={() => onOpen(collection.id)}>
              <Icon name="folder" size={13} />
              <span className="runner-workspace-name">{collection.name || collection.id}</span>
              <span className="runner-workspace-summary">{top && top.state !== "idle" && <AttentionIcon state={top.state} size={11} />}{summary}</span>
            </button>
          </li>
        );
      })}
    </ul>
  );
}

/** What a Runner that does not answer shows under its header: why, and what to do. Never an error. */
function RunnerAbsent({ row }: { row: RunnerView }) {
  // The header says the state; this says what to do about it.
  const text = row.listing?.remote?.state === "refused" ? row.listing.remote.problem || "That computer refused this one."
    : row.health === "stopped" ? row.listing?.problem || "Start it, and its workspaces show here."
    : row.health === "invalid" ? row.listing?.problem || "Its locator cannot be read."
    : linkStateText(row.listing) || "Connecting…";
  return <p className="runner-absent" role="status">{text}</p>;
}

/**
 * A computer's header, when the window shows Runners of more than one: what
 * kind of computer, its name, the route (SSH) and the round trip when it is
 * slow (always in the tooltip), and how its link is, in words.
 */
function MachineHeader({ machine }: { machine: Machine<RunnerView> }) {
  const rtt = rttText(machine.rttMs);
  const slow = !!machine.rttMs && machine.rttMs > SLOW_RTT_MS;
  const route = machine.remote ? routeLabel(machine.route) : "";
  const title = machine.remote
    ? [`${machine.label}${machine.host && machine.host !== machine.label ? ` (${machine.host})` : ""}`, route && `over ${route}`, rtt && `round trip ${rtt}`, machine.state, machine.problem, "Agents there keep running when this computer sleeps."].filter(Boolean).join("\n")
    : machine.label;
  return (
    <div className="machine-header" title={title}>
      <Icon name={machine.remote ? "cloud" : "home"} size={12} />
      <span className="machine-name">{machine.label}</span>
      {route && <span className="machine-route">{route}</span>}
      {rtt && <span className="machine-rtt" data-slow={slow || undefined}>{rtt}</span>}
      {machine.state && <span className="machine-state" role="status">{machine.state}</span>}
    </div>
  );
}

/** A Runner's section header: fold, name, channel, version, health, and what waits there. */
export function RunnerGroupHeader({ row, active, folded, counts, onToggle, onShow, onNew }: { row: RunnerView; active: boolean; folded: boolean; counts?: Record<AttentionState, number>; onToggle(): void; onShow(): void; onNew?(): void }) {
  const { waiting, review } = runnerCounts(row.health === "online" || active ? counts : undefined);
  const detail = [row.channel && row.channel !== row.label ? row.channel : "", row.version ? `v${row.version.replace(/^v/, "")}` : "", runnerStateText(row)].filter(Boolean).join(" · ");
  const state = row.health !== "online" ? runnerStateText(row) : "";
  return (
    <div className="runner-group-header">
      <button type="button" className="runner-group-toggle" aria-expanded={!folded} title={`${folded ? "Show" : "Fold"} ${row.label}\n${detail}${row.listing?.locator ? `\n${row.listing.locator}` : ""}`} onClick={onToggle}>
        <Icon name={folded ? "chevronRight" : "chevron"} size={11} />
        <RunnerHealthDot health={row.health} />
        <span className="runner-group-name">{row.label}</span>
        {row.channel && row.channel !== row.label && <span className="runner-group-channel">{row.channel}</span>}
        {row.version && <span className="runner-group-version">{row.version.replace(/^v/, "")}</span>}
        {state && <span className="runner-group-state">{state}</span>}
      </button>
      {waiting > 0 && <b className="runner-group-count urgent" title={`${waiting} waiting for you on ${row.label}`}>{waiting}</b>}
      {!waiting && review > 0 && <b className="runner-group-count" title={`${review} to review on ${row.label}`}>{review}</b>}
      {onNew && (
        <button type="button" className="runner-group-new" aria-label={`New workspace on ${row.label}`} disabled={!row.state || row.health !== "online"}
          title={row.health === "online" ? `New workspace on ${row.label}\nOpens New task on ${row.label}: repositories, an agent and your first message` : `${row.label}: ${runnerStateText(row)}`} onClick={onNew}>
          <Icon name="plus" size={14} />
        </button>
      )}
      {active
        ? <span className="runner-group-showing" title="The page shows this Runner">Showing</span>
        : <button type="button" className="runner-group-show" disabled={!row.state || row.health === "stopped"} onClick={onShow} title={`Show ${row.label}'s workspaces here`}>Show</button>}
    </div>
  );
}

function WorkspaceList({
  target,
  onTarget,
  collections,
  error,
  loaded = true,
  refresh,
  chooseRepositoryFolder,
  onNewConversation,
  viewState,
  embedded = false,
}: ListProps & {
  /** Under its Runner's header: no heading of its own. */
  embedded?: boolean;
}) {
  const { info, api, error: connectionError } = useRunner();
  const index = useConversationIndex();
  // An agent finishing (or starting) a turn changes its worktree: refresh the rows' git state soon after.
  const git = useGitSummaries(index.sessions.map(s => `${s.id}:${s.busy ? 1 : 0}:${s.status}`).join(","));
  // The tracked pull requests: each row's one badge is its most urgent (pull-requests-model.ts).
  const tracked = useTrackedItems();
  const [deletingConversation, setDeletingConversation] = useState<Session>();
  // Each workspace lists every conversation under it; a fold keeps one quiet.
  const [folded, setFolded] = useState<string[]>(() => { try { const value: unknown = JSON.parse(readPreference(viewState, "workspaces.folded") || "[]"); return Array.isArray(value) ? value.filter((id): id is string => typeof id === "string").slice(0, 256) : []; } catch { return []; } });
  const toggleFold = (id: string) => {
    const next = folded.includes(id) ? folded.filter((value) => value !== id) : [...folded, id];
    setFolded(next);
    try { writePreference(viewState, "workspaces.folded", JSON.stringify(next)); } catch { /* a convenience */ }
  };
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
  // The person's order, kept by the Runner (ArrangeWorkspaces): drag a row, or ⌥⇧↑ / ⌥⇧↓ on it.
  const reorder = useListReorder({
    items: collections, grip: ".workspace-link", name: (collection) => collection.name || collection.id,
    arrange: async (collectionIds) => { await api.arrangeWorkspaces({ collectionIds }, { timeoutMs: 10000 }); refresh(); },
    onError: (body) => showToast({ tone: "failed", title: "Workspaces not reordered", body }),
  });
  const choose = (collection: Collection) => {
    const rows = index.sessions.filter((s) => s.collectionId === collection.id && isWorkConversation(s) && !isArchivedAway(s));
    const sessionId =
      index.complete && rows.length === 1
        ? rows[0].id
        : target.collectionId === collection.id
          ? target.sessionId
          : rows[0]?.id || "";
    onTarget({ collectionId: collection.id, sessionId, commandId: "" });
  };
  return (
    <section ref={panel} className={embedded ? "collection-panel runner-page-workspaces" : "collection-panel"}>
      {!embedded && <div className="explorer-heading">
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
      </div>}
      {error && !connectionError && (
        <p className="error" role="alert">
          Workspaces unavailable: {error}
        </p>
      )}
      {/* Never an empty list for a Runner this window cannot reach: an absent
          answer is not "no workspaces", and the last list stays until the next. */}
      {!loaded && (
        <p className="module-empty collection-status" role="status">
          {error || connectionError
            ? "Reconnecting to the Runner… Your workspaces appear as soon as it answers."
            : "Loading workspaces…"}
        </p>
      )}
      {loaded && (error || connectionError) && collections.length > 0 && (
        <p className="collection-status" role="status">
          Reconnecting to the Runner… These are the workspaces it last listed.
        </p>
      )}
      {index.unsupported&&<p className="module-empty">Live conversations · saved history requires a Runner update.</p>}
      {index.error&&!connectionError&&<p className="error" role="alert">Could not refresh conversation history. Previously loaded entries are retained. <button type="button" onClick={index.refresh}>Retry history refresh</button></p>}
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
      <nav className="collection-list" aria-label="Workspaces" aria-keyshortcuts="Alt+Shift+ArrowUp Alt+Shift+ArrowDown" {...reorder.listProps}>
        <span className="sr-only" role="status" aria-live="polite">{reorder.announcement}</span>
        {reorder.items.map((collection) => {
          const rows = index.sessions.filter(
            (s) => s.collectionId === collection.id && isWorkConversation(s) && !isArchivedAway(s),
          );
          const single =
            index.complete && rows.length === 1 ? rows[0] : undefined;
          return (
            <div key={collection.id} className="collection-group" {...reorder.itemProps(collection.id)}>
              <WorkspaceRow
                conversation={single}
                featured={single || (index.complete ? featuredConversation(rows) : undefined)}
                git={rowGit(git.byWorkspace.get(collection.id), rowPull(tracked.links, collection.id, Date.now()))}
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
                fold={rows.length > 1 ? { folded: folded.includes(collection.id), count: rows.length, toggle: () => toggleFold(collection.id) } : undefined}
                hover={{ sessions: rows, summary: git.byWorkspace.get(collection.id), links: tracked.links, onOpenConversation: (sessionId) => onTarget({ collectionId: collection.id, sessionId, commandId: "" }) }}
              />
              {rows.length > 1 && !folded.includes(collection.id) && (
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
      {loaded && !error && !connectionError && !collections.length && (
        <div className="module-empty collection-start">
          <p>Start a task: its repositories, an agent and your first message, in a workspace of its own.</p>
          <NewTaskStart compact />
        </div>
      )}
      <ConversationList
        sessions={index.sessions.filter(isWorkConversation)}
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
      {loaded && target.collectionId && !selected && !error && !connectionError && (
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
