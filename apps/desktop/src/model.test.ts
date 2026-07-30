import { describe, expect, it } from "vitest";
import type { Task, Workspace } from "./protocol";
import {
  applyWorkspaceEvent,
  filteredWorkspaces,
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
    ci: "Success",
    review: "Approved",
    checks: [],
    unread_count: unread,
    url: "https://example.test/task",
    repo: "owner/repo",
    branch: null,
    base_branch: null,
    updated_at: updatedAt,
    created_at: null,
    closed_at: null,
    labels: [],
    reviewers: [],
    assignees: [],
    auto_merge_enabled: false,
    is_in_merge_queue: false,
    mergeable: "Unknown",
    is_behind_base: false,
    node_id: null,
    needs_reply: false,
    last_commenter: null,
    recent_activity: [],
    additions: 0,
    deletions: 0,
    closes_issues: [],
    kind: "Pr",
  };
}

function workspace(key: string, pr: Task | null): Workspace {
  return {
    schema: 1,
    key,
    project_key: null,
    local: false,
    linked_checkout: null,
    name: key,
    branch: "main",
    sessions: [],
    pr,
    gh_issues: [],
    linear_issues: [],
    activity: [],
    seen_count: 0,
    read_indices: [],
    snoozed_until: null,
    auto_merge_on_green: false,
    track_main: false,
    base_branch: null,
    track_main_behind: false,
    policies: { auto_fix_ci: "Default", auto_fix_conflict: "Default" },
    notes: "",
    sent_snippets: [],
    cleanup_prompt: "unresolved",
    created_at: "2026-01-01T00:00:00Z",
    last_viewed_at: null,
  };
}

function activity(author: string, body: string) {
  return {
    author,
    body,
    created_at: "2026-01-01T00:00:00Z",
    kind: "Comment" as const,
    node_id: null,
    path: null,
    line: null,
    diff_hunk: null,
    thread_id: null,
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
      activity("a", "one"),
      activity("b", "two"),
      activity("c", "three"),
    ];
    item.seen_count = 1;
    expect(unreadCount(item)).toBe(2);
  });

  it("filters the inbox by text and actionable state", () => {
    const failing = workspace("failure", task("Broken build", 0));
    if (failing.pr !== null) {
      failing.pr.ci = "Failure";
    }
    const review = workspace("review", task("Needs review", 0));
    if (review.pr !== null) {
      review.pr.role = "Reviewer";
      review.pr.review = "Pending";
    }
    const unread = workspace("unread", task("Fresh comment", 2));
    const values = [review, unread, failing];

    expect(filteredWorkspaces(values, "broken", "all")).toEqual([failing]);
    expect(filteredWorkspaces(values, "", "ci")).toEqual([failing]);
    expect(filteredWorkspaces(values, "", "review")).toEqual([review]);
    expect(filteredWorkspaces(values, "", "unread")).toEqual([unread]);
  });

  it("replaces the baseline and then applies live upserts and removals", () => {
    const first = workspace("first", task("first"));
    const second = workspace("second", task("second"));
    let state = applyWorkspaceEvent(
      new Map([["stale", workspace("stale", null)]]),
      {
        Snapshot: {
          workspaces: [first],
          terminals: [],
        },
      },
    );
    state = applyWorkspaceEvent(state, { WorkspaceUpserted: second });
    state = applyWorkspaceEvent(state, { WorkspaceRemoved: "first" });
    expect([...state.keys()]).toEqual(["second"]);
  });
});
