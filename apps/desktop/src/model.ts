import type { LazyboxEvent, Task, Workspace } from "./protocol";

export type WorkspaceFilter = "all" | "unread" | "attention";

export interface WorkspaceQuery {
  query: string;
  filter: WorkspaceFilter;
}

export class CommandOutcomeTracker {
  private failureGeneration = 0;

  checkpoint(): number {
    return this.failureGeneration;
  }

  recordFailure(): void {
    this.failureGeneration += 1;
  }

  succeededSince(checkpoint: number): boolean {
    return checkpoint === this.failureGeneration;
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
  const taskUnread =
    (workspace.pr?.unread_count ?? 0) +
    workspace.gh_issues.reduce((sum, task) => sum + task.unread_count, 0) +
    workspace.linear_issues.reduce((sum, task) => sum + task.unread_count, 0);
  return Math.max(taskUnread, workspace.activity.length - workspace.seen_count);
}

export function sortedWorkspaces(
  workspaces: Iterable<Workspace>,
): Workspace[] {
  return [...workspaces].sort((left, right) => {
    const unreadDelta = unreadCount(right) - unreadCount(left);
    if (unreadDelta !== 0) {
      return unreadDelta;
    }
    const leftUpdated = primaryTask(left)?.updated_at ?? "";
    const rightUpdated = primaryTask(right)?.updated_at ?? "";
    return rightUpdated.localeCompare(leftUpdated);
  });
}

export function filteredWorkspaces(
  workspaces: Iterable<Workspace>,
  options: WorkspaceQuery,
): Workspace[] {
  const query = options.query.trim().toLocaleLowerCase();
  return sortedWorkspaces(workspaces).filter((workspace) => {
    const task = primaryTask(workspace);
    if (options.filter === "unread" && unreadCount(workspace) === 0) {
      return false;
    }
    if (
      options.filter === "attention" &&
      task?.ci !== "Failure" &&
      task?.ci !== "Mixed" &&
      task?.review !== "ChangesRequested" &&
      !task?.needs_reply
    ) {
      return false;
    }
    if (query.length === 0) {
      return true;
    }
    return [
      workspace.name,
      workspace.branch,
      task?.title,
      task?.body,
      task?.repo,
      taskReference(task),
      ...workspace.activity.map((activity) => activity.author),
    ]
      .filter((value): value is string => value !== null && value !== undefined)
      .some((value) => value.toLocaleLowerCase().includes(query));
  });
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
