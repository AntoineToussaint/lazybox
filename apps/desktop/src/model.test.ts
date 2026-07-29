import { describe, expect, it } from "vitest";
import type { Task, Workspace } from "./protocol";
import {
  applyWorkspaceEvent,
  primaryTask,
  sortedWorkspaces,
  unreadCount,
} from "./model";

function task(title: string, unread = 0, updatedAt = "2026-01-01"): Task {
  return {
    id: { source: "github", key: `owner/repo#${title.length}` },
    title,
    body: null,
    state: "Open",
    role: "Author",
    ci: "Passing",
    review: "Approved",
    unread_count: unread,
    url: "https://example.test/task",
    repo: "owner/repo",
    updated_at: updatedAt,
    kind: "Pr",
  };
}

function workspace(key: string, pr: Task | null): Workspace {
  return {
    key,
    name: key,
    branch: "main",
    pr,
    gh_issues: [],
    linear_issues: [],
    activity: [],
    seen_count: 0,
    read_indices: [],
    sessions: [],
  };
}

describe("workspace model", () => {
  it("prefers the pull request as the primary task", () => {
    const item = workspace("one", task("PR"));
    item.gh_issues.push(task("issue"));
    expect(primaryTask(item)?.title).toBe("PR");
  });

  it("orders unread work before newer read work", () => {
    const unread = workspace("unread", task("unread", 2, "2026-01-01"));
    const newer = workspace("newer", task("newer", 0, "2026-02-01"));
    expect(sortedWorkspaces([newer, unread]).map((item) => item.key)).toEqual([
      "unread",
      "newer",
    ]);
  });

  it("uses unseen activity when it exceeds task unread counts", () => {
    const item = workspace("activity", task("activity", 1));
    item.activity = [
      { author: "a", body: "one", created_at: "", kind: "Comment" },
      { author: "b", body: "two", created_at: "", kind: "Comment" },
      { author: "c", body: "three", created_at: "", kind: "Comment" },
    ];
    item.seen_count = 1;
    expect(unreadCount(item)).toBe(2);
  });

  it("replaces the baseline and then applies live upserts and removals", () => {
    const first = workspace("first", task("first"));
    const second = workspace("second", task("second"));
    let state = applyWorkspaceEvent(
      new Map([["stale", workspace("stale", null)]]),
      { Snapshot: { workspaces: [first], terminals: [] } },
    );
    state = applyWorkspaceEvent(state, { WorkspaceUpserted: second });
    state = applyWorkspaceEvent(state, { WorkspaceRemoved: "first" });
    expect([...state.keys()]).toEqual(["second"]);
  });
});
