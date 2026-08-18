import { describe, expect, it } from "vitest";
import type { ComputeOutcome, Task, Workspace } from "./protocol";
import {
  InboxConnection,
  ReplyDrafts,
  applyWorkspaceEvent,
  activityFingerprint,
  broadcastDisposition,
  canReplyToTask,
  ciSignal,
  cycleMatchingKey,
  detailSignals,
  hasRepoScope,
  isTerminalTaskState,
  isActivityUnread,
  kindHeaderLabel,
  orderedWorkspaceKeys,
  preferredTerminal,
  primaryTask,
  projectKeyLabel,
  repoHeaderLabel,
  resolveDesktopShortcut,
  reviewSignal,
  rowSignals,
  SNOOZE_PRESETS,
  shouldHandleWorkspaceEnter,
  sortModeLabel,
  supportsTrackMain,
  unreadCount,
  visibleUnreadCount,
  workspaceDiffTarget,
  workspaceRuntimeSignals,
} from "./model";
import type { Session } from "./generated/Session";

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
    reviews: [],
    assignees: [],
    author: "",
    auto_merge_enabled: false,
    is_in_merge_queue: false,
    mergeable: "Unknown",
    is_behind_base: false,
    merge_blocked: false,
    approval_policy: "Default",
    node_id: null,
    needs_reply: false,
    last_commenter: null,
    recent_activity: [],
    additions: 0,
    deletions: 0,
    changed_files: 0,
    closes_issues: [],
    linked_tasks: [],
    kind: "Pr",
    priority: null,
    state_label: null,
  };
}

function workspace(key: string, pr: Task | null): Workspace {
  return {
    schema: 1,
    key,
    project_key: null,
    local: false,
    linked_checkout: null,
    remote: null,
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

function session(id: string, createdAt: string): Session {
  return {
    id,
    workspace_key: "ws",
    name: "claude",
    kind: { Agent: { agent_id: "claude" } },
    state: "Idle",
    worktree_path: `/tmp/${id}`,
    worktree_branch: null,
    created_at: createdAt,
    last_output_at: null,
    layout: { Tabs: { active: 0 } },
    provider_session_ids: {},
  };
}

describe("workspaceDiffTarget", () => {
  it("targets the newest session's worktree", () => {
    const ws = workspace("ws", null);
    ws.sessions = [
      session("older", "2026-01-01T00:00:00Z"),
      session("newest", "2026-01-03T00:00:00Z"),
      session("middle", "2026-01-02T00:00:00Z"),
    ];
    expect(workspaceDiffTarget(ws)).toEqual({ Session: "newest" });
  });

  it("falls back to the linked checkout when there are no sessions", () => {
    const ws = workspace("ws", null);
    ws.linked_checkout = "/home/dev/repo";
    expect(workspaceDiffTarget(ws)).toBe("LinkedCheckout");
  });

  it("prefers a session over a linked checkout", () => {
    const ws = workspace("ws", null);
    ws.sessions = [session("s", "2026-01-01T00:00:00Z")];
    ws.linked_checkout = "/home/dev/repo";
    expect(workspaceDiffTarget(ws)).toEqual({ Session: "s" });
  });

  it("returns null for a pure tracking row", () => {
    expect(workspaceDiffTarget(workspace("ws", null))).toBeNull();
  });
});

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

describe("act-on-work gating", () => {
  it("treats a task-repo or github project as repo-scoped, local as not", () => {
    // A PR with a repo is repo-scoped.
    expect(hasRepoScope(workspace("w1", task("PR")))).toBe(true);
    // A task-less workspace under a github project is still repo-scoped.
    expect(
      hasRepoScope({
        ...workspace("w2", null),
        project_key: "github-acme-widget",
      }),
    ).toBe(true);
    // A local project (or no project) has no shared main checkout.
    expect(
      hasRepoScope({ ...workspace("w3", null), project_key: "local-scratch" }),
    ).toBe(false);
    expect(hasRepoScope(workspace("w4", null))).toBe(false);
  });

  it("flags only Merged/Closed tasks as terminal", () => {
    expect(isTerminalTaskState({ ...task("open"), state: "Open" })).toBe(false);
    expect(isTerminalTaskState({ ...task("draft"), state: "Draft" })).toBe(
      false,
    );
    expect(isTerminalTaskState({ ...task("merged"), state: "Merged" })).toBe(
      true,
    );
    expect(isTerminalTaskState({ ...task("closed"), state: "Closed" })).toBe(
      true,
    );
  });
});

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
    expect(ciSignal(failing)).toEqual({
      label: "CI failure",
      tone: "attention",
    });
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

  it("handles the synthetic `FocusedHeader` string row without treating it as a header object", () => {
    const view: ComputeOutcome = {
      visible: [
        "FocusedHeader",
        { Workspace: "octo/widget#2" },
        { RepoHeader: "octo/widget" },
        { Workspace: "octo/widget#1" },
      ],
      summaries: { "octo/widget": { active: 1, attention: 0 } },
    };
    // The starred workspace under the section is still projected in order.
    expect(orderedWorkspaceKeys(view)).toEqual([
      "octo/widget#2",
      "octo/widget#1",
    ]);
    // `FocusedHeader` is not a repo header — the primitive doesn't crash
    // the `in` narrowing and carries no label.
    expect(repoHeaderLabel("FocusedHeader")).toBeNull();
    expect(repoHeaderLabel({ RepoHeader: "octo/widget" })).toBe("octo/widget");
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

  it("enables replies for the provider-backed GitHub and Linear tasks", () => {
    const github = task("GitHub");
    const linear = task("Linear");
    linear.id.source = "linear";

    expect(canReplyToTask(github)).toBe(true);
    expect(canReplyToTask(linear)).toBe(true);
    expect(canReplyToTask(null)).toBe(false);
  });

  it("leaves Enter activation to native buttons and links", () => {
    expect(shouldHandleWorkspaceEnter(true, false, false)).toBe(true);
    expect(shouldHandleWorkspaceEnter(true, false, true)).toBe(false);
    expect(shouldHandleWorkspaceEnter(true, true, false)).toBe(false);
  });

  it("requires the exact modifier set for desktop shortcuts", () => {
    const key = (overrides: Partial<KeyboardEvent>) => ({
      key: "a",
      metaKey: false,
      ctrlKey: false,
      altKey: false,
      shiftKey: false,
      ...overrides,
    });

    expect(resolveDesktopShortcut(key({}), false)).toBe("start-agent");
    expect(resolveDesktopShortcut(key({ metaKey: true }), false)).toBeNull();
    expect(resolveDesktopShortcut(key({ ctrlKey: true }), false)).toBeNull();
    expect(resolveDesktopShortcut(key({ altKey: true }), false)).toBeNull();
    expect(resolveDesktopShortcut(key({ shiftKey: true }), false)).toBeNull();
    expect(
      resolveDesktopShortcut(key({ key: "j", metaKey: true }), false),
    ).toBe("snippets");
    expect(
      resolveDesktopShortcut(key({ key: "j", ctrlKey: true }), false),
    ).toBe("snippets");
    expect(
      resolveDesktopShortcut(
        key({ key: "j", metaKey: true, ctrlKey: true }),
        false,
      ),
    ).toBeNull();
    expect(
      resolveDesktopShortcut(key({ key: "R", shiftKey: true }), false),
    ).toBe("refresh");
  });

  it("gives modal and editor keyboard ownership precedence", () => {
    const agentKey = {
      key: "a",
      metaKey: false,
      ctrlKey: false,
      altKey: false,
      shiftKey: false,
    };
    const settingsKey = { ...agentKey, key: ",", metaKey: true };

    expect(resolveDesktopShortcut(agentKey, true)).toBeNull();
    expect(resolveDesktopShortcut(settingsKey, true)).toBeNull();
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

  it("allows track-main only for a repo-scoped GitHub worktree without a PR", () => {
    const base = workspace("w", null);
    base.project_key = "github-o-r";

    // GitHub-scoped, no PR, provisioned worktree → supported.
    expect(supportsTrackMain(base)).toBe(true);

    // A PR branch can't fast-forward onto main.
    expect(supportsTrackMain({ ...base, pr: task("PR") })).toBe(false);

    // A linked checkout sits on the user's own clone/branch.
    expect(
      supportsTrackMain({ ...base, linked_checkout: "/home/me/o/r" }),
    ).toBe(false);

    // Non-GitHub / repo-less rows have no origin/<default> to track.
    expect(supportsTrackMain({ ...base, project_key: "linear-team" })).toBe(
      false,
    );
    expect(supportsTrackMain({ ...base, project_key: "local-scratch" })).toBe(
      false,
    );
    expect(supportsTrackMain({ ...base, project_key: null })).toBe(false);
  });

  it("resolves snooze presets to absolute deadlines from a fixed now", () => {
    const now = new Date("2026-08-04T12:00:00.000Z");
    const byLabel = new Map(
      SNOOZE_PRESETS.map((preset) => [preset.label, preset.until(now)]),
    );

    expect(byLabel.get("1 hour")).toEqual(new Date("2026-08-04T13:00:00.000Z"));
    expect(byLabel.get("4 hours")).toEqual(
      new Date("2026-08-04T16:00:00.000Z"),
    );
    expect(byLabel.get("1 week")).toEqual(new Date("2026-08-11T12:00:00.000Z"));
    // "Tomorrow 9am" is anchored to the viewer's local wall clock; assert
    // it lands at 09:00 local and is within the next two days regardless
    // of the test's timezone.
    const tomorrow = byLabel.get("Tomorrow 9am");
    expect(tomorrow?.getHours()).toBe(9);
    expect(tomorrow?.getMinutes()).toBe(0);
    const deltaMs = (tomorrow?.getTime() ?? 0) - now.getTime();
    expect(deltaMs).toBeGreaterThan(0);
    expect(deltaMs).toBeLessThanOrEqual(2 * 24 * 3600 * 1000);
  });
});

describe("desktop daily-driver state", () => {
  it("plans the mixed-target broadcast matrix without losing target identity", () => {
    const runningAgent = workspace("agent", task("agent"));
    const runningShell = workspace("shell", task("shell"));
    const sessionless = workspace("new", task("new"));
    const stopped = workspace("stopped", task("stopped"));
    stopped.sessions = [session("old", "2026-01-01T00:00:00Z")];
    const terminals = [
      {
        id: 7,
        sessionKey: "agent",
        kind: { Agent: "codex" } as const,
        state: "working",
      },
      { id: 8, sessionKey: "shell", kind: "Shell" as const, state: "running" },
    ];

    expect(broadcastDisposition(runningAgent, terminals)).toEqual({
      type: "agent",
      terminalId: 7,
    });
    expect(broadcastDisposition(runningShell, terminals)).toEqual({
      type: "shell",
      terminalId: 8,
    });
    expect(broadcastDisposition(sessionless, terminals)).toEqual({
      type: "spawn",
    });
    expect(broadcastDisposition(stopped, terminals)).toEqual({
      type: "skip",
      reason: "no running agent or shell",
    });
  });

  it("cycles attention targets in view order and reflects live agent badges", () => {
    const keys = ["one", "two", "three"];
    const asking = new Set(["one", "three"]);
    expect(cycleMatchingKey(keys, null, (key) => asking.has(key))).toBe("one");
    expect(cycleMatchingKey(keys, "one", (key) => asking.has(key))).toBe(
      "three",
    );
    expect(cycleMatchingKey(keys, "three", (key) => asking.has(key))).toBe(
      "one",
    );

    const terminal = {
      id: 1,
      sessionKey: "one",
      kind: { Agent: "codex" } as const,
      // The runtime `state` is `formatAgentState(AgentState)`, which
      // lowercases the wire variant — `"InputNeeded"` → `"inputneeded"`,
      // no space. A literal `"input needed"` here is what previously
      // masked the badge/jump never firing in production.
      state: "inputneeded",
    };
    expect(workspaceRuntimeSignals([terminal], "one")).toEqual([
      { label: "asking", tone: "attention" },
    ]);
    terminal.state = "working";
    expect(workspaceRuntimeSignals([terminal], "one")).toEqual([
      { label: "running", tone: "success" },
    ]);
  });

  it("uses stable activity fingerprints while read state remains positional", () => {
    const item = workspace("activity", task("activity"));
    item.activity = [activity("octo", "a".repeat(70)), activity("hub", "next")];
    item.seen_count = 1;
    item.read_indices = [];
    expect(activityFingerprint(item.activity[0]!)).toEqual({
      Content: {
        author: "octo",
        created_at: "2026-01-01T00:00:00Z",
        body_prefix: "a".repeat(64),
      },
    });
    expect(isActivityUnread(item, 0)).toBe(true);
    expect(isActivityUnread(item, 1)).toBe(false);
  });
});
