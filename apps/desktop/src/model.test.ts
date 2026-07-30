import { describe, expect, it } from "vitest";
import type { Task, Workspace } from "./protocol";
import {
  InboxConnection,
  ReplyDrafts,
  applyWorkspaceEvent,
  canReplyToTask,
  filteredWorkspaces,
  preferredTerminal,
  primaryTask,
  shouldHandleWorkspaceEnter,
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
  it("keeps reply drafts scoped to their workspace", () => {
    const drafts = new ReplyDrafts();
    drafts.save("one", "Reply for one");
    drafts.save("two", "Reply for two");

    expect(drafts.get("one")).toBe("Reply for one");
    expect(drafts.get("two")).toBe("Reply for two");
    drafts.clear("one");
    expect(drafts.get("one")).toBe("");
    expect(drafts.get("two")).toBe("Reply for two");
  });

  it("retries a transient inbox load and subscribes once after recovery", async () => {
    let loadAttempts = 0;
    let subscriptions = 0;
    const connection = new InboxConnection(
      async () => {
        loadAttempts += 1;
        if (loadAttempts === 1) {
          throw new Error("daemon warming up");
        }
        return ["workspace"];
      },
      async () => {
        subscriptions += 1;
      },
    );

    await expect(connection.connect()).rejects.toThrow("daemon warming up");
    await expect(connection.connect()).resolves.toEqual(["workspace"]);
    await expect(connection.connect()).resolves.toEqual(["workspace"]);
    expect(loadAttempts).toBe(3);
    expect(subscriptions).toBe(1);
  });

  it("prefers the pull request as the primary task", () => {
    const item = workspace("one", task("PR"));
    item.gh_issues.push(task("issue"));
    expect(primaryTask(item)?.title).toBe("PR");
  });

  it("orders unread work before newer read work", () => {
    const unread = workspace("unread", task("unread", 0, "2026-01-01"));
    unread.activity = [activity("a", "one")];
    const newer = workspace("newer", task("newer", 0, "2026-02-01"));
    expect(sortedWorkspaces([newer, unread]).map((item) => item.key)).toEqual([
      "unread",
      "newer",
    ]);
  });

  it("uses workspace read markers as the authoritative unread state", () => {
    const item = workspace("activity", task("activity", 99));
    item.activity = [
      activity("a", "one"),
      activity("b", "two"),
      activity("c", "three"),
    ];
    item.seen_count = 1;
    item.read_indices = [1];
    expect(unreadCount(item)).toBe(1);

    item.seen_count = item.activity.length;
    item.read_indices = [];
    expect(unreadCount(item)).toBe(0);
  });

  it("searches metadata and filters unread and attention work", () => {
    const failing = workspace("failing", task("Broken release", 0));
    failing.pr!.ci = "Failure";
    const mixed = workspace("mixed", task("Mixed checks", 0));
    mixed.pr!.ci = "Mixed";
    const unread = workspace("unread", task("Provider labels", 2));
    unread.activity = [activity("a", "new")];
    const quiet = workspace("quiet", task("Documentation", 0));

    expect(
      filteredWorkspaces([quiet, failing, unread], {
        query: "labels",
        filter: "all",
      }).map((item) => item.key),
    ).toEqual(["unread"]);
    expect(
      filteredWorkspaces([quiet, failing, unread], {
        query: "",
        filter: "unread",
      }).map((item) => item.key),
    ).toEqual(["unread"]);
    expect(
      filteredWorkspaces([quiet, failing, mixed, unread], {
        query: "",
        filter: "attention",
      }).map((item) => item.key),
    ).toEqual(["failing", "mixed"]);
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

  it("prefers the newest live terminal over stale exited records", () => {
    const records = [
      {
        id: 1,
        sessionKey: "one",
        kind: "Shell" as const,
        state: "exited 0",
      },
      {
        id: 2,
        sessionKey: "one",
        kind: "Shell" as const,
        state: "running",
      },
      {
        id: 3,
        sessionKey: "one",
        kind: "Shell" as const,
        state: "running",
      },
    ];

    expect(preferredTerminal(records, "one", "shell")?.id).toBe(3);
  });

  it("only enables replies for GitHub tasks", () => {
    const github = task("GitHub");
    const linear = task("Linear");
    linear.id.source = "linear";

    expect(canReplyToTask(github)).toBe(true);
    expect(canReplyToTask(linear)).toBe(false);
    expect(canReplyToTask(null)).toBe(false);
  });

  it("leaves Enter activation to native buttons and links", () => {
    expect(shouldHandleWorkspaceEnter(true, false, false)).toBe(true);
    expect(shouldHandleWorkspaceEnter(true, false, true)).toBe(false);
    expect(shouldHandleWorkspaceEnter(true, true, false)).toBe(false);
  });
});
