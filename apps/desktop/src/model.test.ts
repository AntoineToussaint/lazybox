import { describe, expect, it } from "vitest";
import type { ComputeOutcome, Task, Workspace } from "./protocol";
import {
  InboxConnection,
  ReplyDrafts,
  applyWorkspaceEvent,
  canReplyToTask,
  ciSignal,
  detailSignals,
  kindHeaderLabel,
  orderedWorkspaceKeys,
  preferredTerminal,
  primaryTask,
  projectKeyLabel,
  reviewSignal,
  rowSignals,
  shouldHandleWorkspaceEnter,
  sortModeLabel,
  unreadCount,
  visibleUnreadCount,
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

  it("totals unread only over workspaces the view placed as rows", () => {
    const shown = workspace("shown", task("shown"));
    shown.activity = [activity("a", "one"), activity("b", "two")];
    expect(unreadCount(shown)).toBe(2);

    // Present in the map but filtered out of the view (e.g. inactive):
    // its unread must not reach the header total.
    const hidden = workspace("hidden", task("hidden"));
    hidden.activity = [activity("c", "three")];
    expect(unreadCount(hidden)).toBe(1);

    const items = new Map([
      ["shown", shown],
      ["hidden", hidden],
    ]);
    const view: ComputeOutcome = {
      visible: [{ RepoHeader: "owner/repo" }, { Workspace: "shown" }],
      summaries: { "owner/repo": { active: 1, attention: 0 } },
    };
    // 2 (only `shown`), never 3 — `hidden` has no row.
    expect(visibleUnreadCount(view, items)).toBe(2);

    // A row whose workspace hasn't landed in the map yet is skipped,
    // exactly as the renderer skips the row — no NaN, no over-count.
    const pending: ComputeOutcome = {
      visible: [{ Workspace: "shown" }, { Workspace: "not-in-map" }],
      summaries: {},
    };
    expect(visibleUnreadCount(pending, items)).toBe(2);
  });

  it("derives CI and review badges with the right tone", () => {
    const failing = task("Broken release");
    failing.ci = "Failure";
    failing.review = "ChangesRequested";
    expect(ciSignal(failing)).toEqual({ label: "CI failure", tone: "attention" });
    expect(reviewSignal(failing)).toEqual({
      label: "Review changes requested",
      tone: "attention",
    });

    const green = task("Ready");
    green.ci = "Success";
    green.review = "Approved";
    expect(ciSignal(green)).toEqual({ label: "CI success", tone: "success" });
    expect(reviewSignal(green)).toEqual({
      label: "Review approved",
      tone: "success",
    });

    const none = task("Draft");
    none.ci = "None";
    none.review = "None";
    expect(ciSignal(none)).toBeNull();
    expect(reviewSignal(none)).toBeNull();
  });

  it("builds compact row badges and fuller detail badges", () => {
    const item = task("Ship it");
    item.ci = "Failure";
    item.review = "Pending";
    item.needs_reply = true;
    item.additions = 12;
    item.deletions = 3;

    const row = rowSignals(item, 2).map((signal) => signal.label);
    expect(row).toEqual(["CI failure", "Review pending", "reply", "2 unread"]);

    const detail = detailSignals(item).map((signal) => signal.label);
    expect(detail).toContain("Open");
    expect(detail).toContain("CI failure");
    expect(detail).toContain("+12 −3");
  });

  it("labels sort modes and section headers like the TUI", () => {
    expect(sortModeLabel("Recent")).toBe("recent");
    expect(sortModeLabel("ByRole")).toBe("by-role");
    expect(sortModeLabel("ByRoleSplit")).toBe("split");
    expect(kindHeaderLabel("Pr")).toBe("PRs");
    expect(kindHeaderLabel("Issue")).toBe("Issues");
  });

  it("projects workspace keys from the view-model in order, skipping headers", () => {
    const view: ComputeOutcome = {
      visible: [
        { RepoHeader: "octo/widget" },
        { KindHeader: "Pr" },
        { Workspace: "octo/widget#2" },
        { KindHeader: "Issue" },
        { Workspace: "octo/widget#1" },
      ],
      summaries: { "octo/widget": { active: 2, attention: 0 } },
    };
    expect(orderedWorkspaceKeys(view)).toEqual([
      "octo/widget#2",
      "octo/widget#1",
    ]);
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
          recent_snippets: [],
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

  it("derives a human label from a project key instead of the raw key", () => {
    expect(projectKeyLabel("github-acme-widget")).toBe("acme/widget");
    expect(projectKeyLabel("github-o-pretty-hackernews")).toBe(
      "o/pretty-hackernews",
    );
    expect(projectKeyLabel("local-scratch")).toBe("scratch");
    expect(projectKeyLabel("linear-team123")).toBe("team123");
    expect(projectKeyLabel("standalone")).toBe("standalone");
  });
});
