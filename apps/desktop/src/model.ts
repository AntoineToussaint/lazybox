import type {
  ComputeOutcome,
  Activity,
  ActivityFingerprint,
  FilterAxis,
  FilterMenuItem,
  LazyboxEvent,
  Mailbox,
  SortMode,
  Task,
  TerminalKind,
  VisibleRow,
  Workspace,
  WorkspaceDiffTarget,
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
    if (typeof row === "object" && "Workspace" in row) {
      keys.push(row.Workspace);
    }
  }
  return keys;
}

/** The repo label a `RepoHeader` row carries, else null. */
export function repoHeaderLabel(row: VisibleRow): string | null {
  return typeof row === "object" && "RepoHeader" in row ? row.RepoHeader : null;
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

/**
 * A snooze-duration choice, mirroring the TUI's `mount_snooze_picker`
 * presets (#512). `until` resolves an absolute deadline from `now` so
 * the value is a pure function of its inputs — the desktop computes the
 * timestamp client-side and sends it as the daemon's UTC deadline.
 */
export interface SnoozePreset {
  readonly label: string;
  readonly until: (now: Date) => Date;
}

function afterSeconds(now: Date, seconds: number): Date {
  return new Date(now.getTime() + seconds * 1000);
}

/** Next calendar day at 9am in the viewer's local wall-clock time. */
function tomorrowMorning(now: Date): Date {
  const target = new Date(now);
  target.setDate(target.getDate() + 1);
  target.setHours(9, 0, 0, 0);
  return target;
}

export const SNOOZE_PRESETS: readonly SnoozePreset[] = [
  { label: "1 hour", until: (now) => afterSeconds(now, 3600) },
  { label: "4 hours", until: (now) => afterSeconds(now, 4 * 3600) },
  { label: "Tomorrow 9am", until: tomorrowMorning },
  { label: "1 week", until: (now) => afterSeconds(now, 7 * 24 * 3600) },
  { label: "1 month", until: (now) => afterSeconds(now, 30 * 24 * 3600) },
  { label: "Forever", until: (now) => afterSeconds(now, 365 * 24 * 3600) },
];

export function primaryTask(workspace: Workspace): Task | null {
  return (
    workspace.pr ?? workspace.gh_issues[0] ?? workspace.linear_issues[0] ?? null
  );
}

/**
 * Whether the workspace tracks a GitHub repository — i.e. it has a
 * shared main checkout an on-main spawn can target. Mirrors
 * `Workspace::repo_slug().is_some()`: a `repo` on the primary task, or a
 * `github-*` project key. Local (`local-*`) and repo-less workspaces
 * return false, so the desktop never offers "on main" where it has no
 * meaning (#816).
 */
export function hasRepoScope(workspace: Workspace): boolean {
  if (primaryTask(workspace)?.repo != null) {
    return true;
  }
  return workspace.project_key?.startsWith("github-") ?? false;
}

/**
 * Whether a task is in a state where merge / update-branch / close /
 * delete still make sense. Terminal states (Merged / Closed) are no-ops
 * the desktop shouldn't offer; a Draft PR can't be merged. Never hides a
 * genuinely-actionable item — the daemon remains the authority and
 * reports any rejection via `WorkspaceActionOutcome` (#816).
 */
export function isTerminalTaskState(task: Task): boolean {
  return task.state === "Merged" || task.state === "Closed";
}

/**
 * Whether "track main" (#535) can apply to this workspace, mirroring
 * `Workspace::supports_track_main` on the daemon: a GitHub upstream and a
 * lazybox-provisioned worktree, and **no PR** (a PR branch is
 * simultaneously ahead of and behind `main`, so a fast-forward can never
 * apply). Repo-less rows and linked checkouts have no `origin/<default>`
 * to fast-forward against.
 */
export function supportsTrackMain(workspace: Workspace): boolean {
  return (
    workspace.pr === null &&
    workspace.linked_checkout === null &&
    projectKeySource(workspace.project_key) === "github"
  );
}

/**
 * The source prefix of a ProjectKey (`github` / `linear` / `local`, or
 * `""` when unprefixed), matching `ProjectKey::source_prefix` — the
 * substring before the first `-`.
 */
function projectKeySource(key: string | null): string {
  if (key === null) {
    return "";
  }
  const dash = key.indexOf("-");
  return dash === -1 ? "" : key.slice(0, dash);
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

export type BroadcastDisposition =
  | { type: "agent"; terminalId: number }
  | { type: "shell"; terminalId: number }
  | { type: "spawn" }
  | { type: "skip"; reason: string };

export function broadcastDisposition(
  workspace: Workspace,
  terminals: Iterable<WorkspaceTerminal>,
): BroadcastDisposition {
  const live = [...terminals]
    .filter(
      (terminal) =>
        terminal.sessionKey === workspace.key &&
        !terminal.state.startsWith("exited"),
    )
    .sort((left, right) => right.id - left.id);
  const agent = live.find(
    (terminal) => typeof terminal.kind === "object" && "Agent" in terminal.kind,
  );
  if (agent !== undefined) {
    return { type: "agent", terminalId: agent.id };
  }
  const shell = live.find((terminal) => terminal.kind === "Shell");
  if (shell !== undefined) {
    return { type: "shell", terminalId: shell.id };
  }
  if (workspace.sessions.length === 0) {
    return { type: "spawn" };
  }
  return { type: "skip", reason: "no running agent or shell" };
}

export function activityFingerprint(activity: Activity): ActivityFingerprint {
  if (typeof activity.node_id === "string" && activity.node_id.length > 0) {
    return { NodeId: activity.node_id };
  }
  return {
    Content: {
      author: activity.author,
      created_at: activity.created_at,
      body_prefix: [...(activity.body ?? "")].slice(0, 64).join(""),
    },
  };
}

export function activityFingerprintKey(activity: Activity): string {
  return JSON.stringify(activityFingerprint(activity));
}

export function isActivityUnread(workspace: Workspace, index: number): boolean {
  const unseen = Math.max(0, workspace.activity.length - workspace.seen_count);
  return index < unseen && !workspace.read_indices.includes(index);
}

export function cycleMatchingKey(
  orderedKeys: string[],
  current: string | null,
  matches: (key: string) => boolean,
): string | null {
  const candidates = orderedKeys.filter(matches);
  if (candidates.length === 0) {
    return null;
  }
  const currentIndex = candidates.indexOf(current ?? "");
  return candidates[(currentIndex + 1) % candidates.length] ?? null;
}

export function workspaceRuntimeSignals(
  terminals: Iterable<WorkspaceTerminal>,
  sessionKey: string,
): TaskSignal[] {
  const live = [...terminals].filter(
    (terminal) =>
      terminal.sessionKey === sessionKey &&
      !terminal.state.startsWith("exited"),
  );
  if (live.some((terminal) => terminal.state === "inputneeded")) {
    return [{ label: "asking", tone: "attention" }];
  }
  if (live.some((terminal) => terminal.state === "working")) {
    return [{ label: "running", tone: "success" }];
  }
  if (live.length > 0) {
    return [{ label: "agent ready" }];
  }
  return [];
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

/**
 * The exact checkout whose worktree diff the desktop should inspect
 * (#843), or null when the workspace has nothing on disk to review.
 * Mirrors the TUI's `ViewDiff` target resolution: the newest session's
 * worktree (`Workspace::default_session`, the max-`created_at` session),
 * else the workspace's linked checkout. A pure tracking row (no sessions,
 * no linked checkout) has no diff to show.
 */
export function workspaceDiffTarget(
  workspace: Workspace,
): WorkspaceDiffTarget | null {
  let newest: { id: string; at: number } | null = null;
  for (const session of workspace.sessions) {
    const at = Date.parse(session.created_at);
    if (newest === null || at > newest.at) {
      newest = { id: session.id, at };
    }
  }
  if (newest !== null) {
    return { Session: newest.id };
  }
  if (workspace.linked_checkout !== null) {
    return "LinkedCheckout";
  }
  return null;
}

export function canReplyToTask(task: Task | null): boolean {
  return task?.id.source === "github" || task?.id.source === "linear";
}

export function shouldHandleWorkspaceEnter(
  selectedWorkspace: boolean,
  editableTarget: boolean,
  interactiveTarget: boolean,
): boolean {
  return selectedWorkspace && !editableTarget && !interactiveTarget;
}

export type DesktopShortcut =
  | "settings"
  | "snippets"
  | "navigate-down"
  | "navigate-up"
  | "open-workspace"
  | "sort"
  | "reply"
  | "start-agent"
  | "start-shell"
  | "mark-read"
  | "filter"
  | "search"
  | "focus-mode"
  | "refresh";

type ShortcutEvent = Pick<
  KeyboardEvent,
  "key" | "metaKey" | "ctrlKey" | "altKey" | "shiftKey"
>;

export function resolveDesktopShortcut(
  event: ShortcutEvent,
  keyboardOwned: boolean,
): DesktopShortcut | null {
  if (keyboardOwned) {
    return null;
  }
  const primaryModifier =
    event.metaKey !== event.ctrlKey && !event.altKey && !event.shiftKey;
  if (primaryModifier && event.key === ",") {
    return "settings";
  }
  if (primaryModifier && (event.key === "j" || event.key === "J")) {
    return "snippets";
  }
  if (event.metaKey || event.ctrlKey || event.altKey) {
    return null;
  }
  if (event.shiftKey) {
    return event.key === "R" ? "refresh" : null;
  }
  const shortcuts: Record<string, DesktopShortcut | undefined> = {
    ArrowDown: "navigate-down",
    ArrowUp: "navigate-up",
    Enter: "open-workspace",
    o: "sort",
    r: "reply",
    a: "start-agent",
    s: "start-shell",
    m: "mark-read",
    f: "filter",
    "/": "search",
    ".": "focus-mode",
  };
  return shortcuts[event.key] ?? null;
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
