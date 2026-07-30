import type { LazyboxEvent, Task, Workspace } from "./protocol";

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
