import type {
  ComputeOutcome,
  FilterAxis,
  FilterMenuItem,
  LazyboxEvent,
  Mailbox,
  SortMode,
  Task,
  TerminalKind,
  VisibleRow,
  Workspace,
} from "./protocol";

/** A run of filter-menu items sharing one axis (State / Role / Kind). */
export interface FilterMenuGroup {
  axis: FilterAxis;
  items: FilterMenuItem[];
}

/**
 * Group the flat filter menu into consecutive axis runs (#733). The menu
 * arrives in `Filter::ALL` order, which is already axis-contiguous, so a
 * single linear pass reproduces the TUI's State / Role / Kind sections
 * without re-sorting or hardcoding the predicate list.
 */
export function filterMenuGroups(menu: FilterMenuItem[]): FilterMenuGroup[] {
  const groups: FilterMenuGroup[] = [];
  for (const item of menu) {
    const last = groups[groups.length - 1];
    if (last === undefined || last.axis !== item.axis) {
      groups.push({ axis: item.axis, items: [item] });
    } else {
      last.items.push(item);
    }
  }
  return groups;
}

/** The active filters, in menu order — the removable header chips (#733). */
export function activeFilters(menu: FilterMenuItem[]): FilterMenuItem[] {
  return menu.filter((item) => item.active);
}

/**
 * A rendered badge pill. `tone` colours it (attention / success);
 * otherwise it renders neutral. Kept as data (not DOM) so the badge
 * set for the detail pane and the list rows is one shared, testable
 * definition — no grouping/sort logic, which lives only in the shared
 * `tui-core` view-model (#732).
 */
export interface TaskSignal {
  label: string;
  tone?: "attention" | "success";
}

export function ciSignal(task: Task): TaskSignal | null {
  if (task.ci === "None") {
    return null;
  }
  return {
    label: `CI ${task.ci.toLowerCase()}`,
    tone:
      task.ci === "Success"
        ? "success"
        : task.ci === "Failure" || task.ci === "Mixed"
          ? "attention"
          : undefined,
  };
}

export function reviewSignal(task: Task): TaskSignal | null {
  if (task.review === "None") {
    return null;
  }
  return {
    label: `Review ${task.review
      .replace(/([A-Z])/g, " $1")
      .trim()
      .toLowerCase()}`,
    tone:
      task.review === "Approved"
        ? "success"
        : task.review === "ChangesRequested"
          ? "attention"
          : undefined,
  };
}

/** Full badge set for the detail pane header. */
export function detailSignals(task: Task): TaskSignal[] {
  const signals: TaskSignal[] = [{ label: task.state }];
  const ci = ciSignal(task);
  if (ci !== null) {
    signals.push(ci);
  }
  const review = reviewSignal(task);
  if (review !== null) {
    signals.push(review);
  }
  if (task.needs_reply) {
    signals.push({
      label:
        task.last_commenter === null
          ? "Reply needed"
          : `Reply to @${task.last_commenter}`,
      tone: "attention",
    });
  }
  if (task.additions > 0 || task.deletions > 0) {
    signals.push({ label: `+${task.additions} −${task.deletions}` });
  }
  for (const label of task.labels.slice(0, 4)) {
    signals.push({ label: label.name });
  }
  return signals;
}

/** Compact badge set for a list row: CI, review, reply, unread. */
export function rowSignals(task: Task | null, unread: number): TaskSignal[] {
  const signals: TaskSignal[] = [];
  if (task !== null) {
    const ci = ciSignal(task);
    if (ci !== null) {
      signals.push(ci);
    }
    const review = reviewSignal(task);
    if (review !== null) {
      signals.push(review);
    }
    if (task.needs_reply) {
      signals.push({ label: "reply", tone: "attention" });
    }
  }
  if (unread > 0) {
    signals.push({ label: `${unread} unread`, tone: "attention" });
  }
  return signals;
}

/** Human label for the current sort mode, matching the TUI chip. */
export function sortModeLabel(mode: SortMode): string {
  switch (mode) {
    case "Recent":
      return "recent";
    case "ByRole":
      return "by-role";
    case "ByRoleSplit":
      return "split";
  }
}

/** Short label for the mailbox control, matching `Mailbox::chip_label`. */
export function mailboxLabel(mailbox: Mailbox): string {
  switch (mailbox) {
    case "Inbox":
      return "inbox";
    case "Inactive":
      return "inactive";
    case "Snoozed":
      return "snoozed";
  }
}

/** Next mailbox in the cycle, matching `Mailbox::next`. */
export function nextMailbox(mailbox: Mailbox): Mailbox {
  return mailbox === "Inbox"
    ? "Inactive"
    : mailbox === "Inactive"
      ? "Snoozed"
      : "Inbox";
}

/** Header label for a PR/Issue/Other section, matching the TUI. */
export function kindHeaderLabel(kind: "Pr" | "Issue" | "Other"): string {
  switch (kind) {
    case "Pr":
      return "PRs";
    case "Issue":
      return "Issues";
    case "Other":
      return "Other";
  }
}

/**
 * The workspace keys the shared view-model placed, in order. Pure
 * projection over the computed rows — it does no ordering itself, so
 * keyboard navigation follows exactly the grouping/sort tui-core
 * produced. Session sub-rows and headers are skipped.
 */
export function orderedWorkspaceKeys(view: ComputeOutcome): string[] {
  const keys: string[] = [];
  for (const row of view.visible) {
    if ("Workspace" in row) {
      keys.push(row.Workspace);
    }
  }
  return keys;
}

/** The repo label a `RepoHeader` row carries, else null. */
export function repoHeaderLabel(row: VisibleRow): string | null {
  return "RepoHeader" in row ? row.RepoHeader : null;
}

export class ReplyDrafts {
  private readonly drafts = new Map<string, string>();

  save(workspaceKey: string, body: string): void {
    if (body.length === 0) {
      this.drafts.delete(workspaceKey);
    } else {
      this.drafts.set(workspaceKey, body);
    }
  }

  get(workspaceKey: string): string {
    return this.drafts.get(workspaceKey) ?? "";
  }

  clear(workspaceKey: string): void {
    this.drafts.delete(workspaceKey);
  }
}

export class InboxConnection<T> {
  private subscribed = false;
  private inFlight: Promise<T> | null = null;

  constructor(
    private readonly load: () => Promise<T>,
    private readonly subscribe: () => Promise<void>,
  ) {}

  connect(): Promise<T> {
    if (this.inFlight !== null) {
      return this.inFlight;
    }
    this.inFlight = this.connectOnce().finally(() => {
      this.inFlight = null;
    });
    return this.inFlight;
  }

  private async connectOnce(): Promise<T> {
    const value = await this.load();
    if (!this.subscribed) {
      await this.subscribe();
      this.subscribed = true;
    }
    return value;
  }
}

export function primaryTask(workspace: Workspace): Task | null {
  return (
    workspace.pr ??
    workspace.gh_issues[0] ??
    workspace.linear_issues[0] ??
    null
  );
}

export function unreadCount(workspace: Workspace): number {
  const unseenCount = Math.max(
    0,
    workspace.activity.length - workspace.seen_count,
  );
  const readIndices = new Set(workspace.read_indices);
  let unread = 0;
  for (let index = 0; index < unseenCount; index += 1) {
    if (!readIndices.has(index)) {
      unread += 1;
    }
  }
  return unread;
}

/**
 * Total unread across the workspaces the shared view-model actually
 * placed as rows. Scoped to the view's mailbox — workspaces
 * `compute_visible` filtered out (e.g. inactive) contribute rows to
 * neither the list nor this count, so the header total always matches
 * what is shown rather than being inflated by hidden workspaces. A key
 * that has no map entry yet (a row that briefly precedes its
 * `WorkspaceUpserted` echo) is skipped, exactly as the renderer skips
 * the row itself.
 */
export function visibleUnreadCount(
  view: ComputeOutcome,
  workspaces: Map<string, Workspace>,
): number {
  return orderedWorkspaceKeys(view).reduce((sum, key) => {
    const workspace = workspaces.get(key);
    return workspace === undefined ? sum : sum + unreadCount(workspace);
  }, 0);
}

interface WorkspaceTerminal {
  id: number;
  sessionKey: string;
  kind: TerminalKind;
  state: string;
}

export function preferredTerminal<T extends WorkspaceTerminal>(
  terminals: Iterable<T>,
  sessionKey: string,
  kind: "agent" | "shell",
  agentId?: string,
): T | undefined {
  return [...terminals]
    .filter((terminal) => {
      if (terminal.sessionKey !== sessionKey) {
        return false;
      }
      return kind === "shell"
        ? terminal.kind === "Shell"
        : typeof terminal.kind === "object" &&
            "Agent" in terminal.kind &&
            (agentId === undefined || terminal.kind.Agent === agentId);
    })
    .sort((left, right) => {
      const leftExited = left.state.startsWith("exited");
      const rightExited = right.state.startsWith("exited");
      if (leftExited !== rightExited) {
        return leftExited ? 1 : -1;
      }
      return right.id - left.id;
    })[0];
}

export function canReplyToTask(task: Task | null): boolean {
  return task?.id.source === "github";
}

export function shouldHandleWorkspaceEnter(
  selectedWorkspace: boolean,
  editableTarget: boolean,
  interactiveTarget: boolean,
): boolean {
  return selectedWorkspace && !editableTarget && !interactiveTarget;
}

export function applyWorkspaceEvent(
  workspaces: Map<string, Workspace>,
  event: LazyboxEvent,
): Map<string, Workspace> {
  const next = new Map(workspaces);
  if ("Snapshot" in event) {
    next.clear();
    for (const workspace of event.Snapshot.workspaces) {
      next.set(workspace.key, workspace);
    }
  } else if ("WorkspaceUpserted" in event) {
    next.set(event.WorkspaceUpserted.key, event.WorkspaceUpserted);
  } else if ("WorkspaceRemoved" in event) {
    next.delete(event.WorkspaceRemoved);
  }
  return next;
}

export function taskReference(task: Task | null): string {
  if (task === null) {
    return "local workspace";
  }
  return task.id.key;
}

/**
 * Human-readable name derived from a ProjectKey alone, for renders that
 * lack an upstream repo name. `github-<owner>-<repo>` → `<owner>/<repo>`;
 * other recognized prefixes return the suffix; an unprefixed key returns
 * itself. Mirrors the daemon's `ProjectKey::display_name`, including its
 * first-`-` owner/repo split (a hyphenated owner is a rare, accepted miss
 * for a fallback label).
 */
export function projectKeyLabel(key: string): string {
  const dash = key.indexOf("-");
  if (dash === -1) {
    return key;
  }
  const prefix = key.slice(0, dash);
  const rest = key.slice(dash + 1);
  if (prefix === "github") {
    const slash = rest.indexOf("-");
    return slash === -1
      ? rest
      : `${rest.slice(0, slash)}/${rest.slice(slash + 1)}`;
  }
  return rest;
}
