import type {
  LazyboxEvent,
  Task,
  TerminalKind,
  Workspace,
} from "./protocol";

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
