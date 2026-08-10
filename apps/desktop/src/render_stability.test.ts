// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";
import fixture from "./generated/compatibility.json";

const harness = vi.hoisted(() => ({
  invoke: vi.fn(),
  channels: [] as Array<{ onmessage: (message: unknown) => void }>,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: harness.invoke,
  Channel: class {
    onmessage = (_message: unknown): void => {};

    constructor() {
      harness.channels.push(this);
    }
  },
}));

vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class {
    fit(): void {}
  },
}));

vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    loadAddon(): void {}
    open(): void {}
    focus(): void {}
    reset(): void {}
    dispose(): void {}
    onData(): { dispose: () => void } {
      return { dispose: () => {} };
    }
    onResize(): { dispose: () => void } {
      return { dispose: () => {} };
    }
    write(): void {}
  },
}));

// The workspace list must not fully remount on every poll (#877): a
// re-render that leaves a row's data untouched has to reuse the existing
// DOM node, so scroll position, keyboard focus, and text selection all
// survive the update.
describe("workspace list render stability (#877)", () => {
  beforeEach(() => {
    loadDocument();
    harness.invoke.mockReset();
    harness.channels.length = 0;
  });

  it("reuses row nodes across a no-op re-render", async () => {
    await boot([prWorkspace(), issueWorkspace()]);
    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );

    const before = rowNodes();
    const repoBefore = document.querySelector(".repo-header");

    // A second identical view push (what a poll that changed nothing
    // produces) must not swap any node out.
    channel().onmessage(splitView());
    await Promise.resolve();

    const after = rowNodes();
    expect(after.get("github-o-r-42")).toBe(before.get("github-o-r-42"));
    expect(after.get("github-o-r-7")).toBe(before.get("github-o-r-7"));
    expect(document.querySelector(".repo-header")).toBe(repoBefore);
  });

  it("rebuilds only the row whose data changed", async () => {
    await boot([prWorkspace(), issueWorkspace()]);
    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );
    const before = rowNodes();

    const changed = prWorkspace();
    (changed.pr as Record<string, unknown>).title = "PR o/r#42 (updated)";
    channel().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: changed },
    });
    await vi.waitFor(() =>
      expect(
        rowNodes()
          .get("github-o-r-42")
          ?.querySelector(".workspace-row-title")?.textContent,
      ).toBe("PR o/r#42 (updated)"),
    );

    const after = rowNodes();
    // The mutated row is a fresh node; its untouched sibling is reused.
    expect(after.get("github-o-r-42")).not.toBe(before.get("github-o-r-42"));
    expect(after.get("github-o-r-7")).toBe(before.get("github-o-r-7"));
  });

  it("refreshes a row's relative time as the clock advances", async () => {
    // A render whose row data is otherwise unchanged must still update
    // the displayed age — before, the signature keyed on the raw stamp
    // only, so "30 seconds ago" froze while other rows churned.
    const stamp = "2026-01-01T00:00:00.000Z";
    const base = Date.parse(stamp) + 30_000;
    const clock = vi.spyOn(Date, "now").mockReturnValue(base);
    try {
      const pr = prWorkspace();
      (pr.pr as Record<string, unknown>).updated_at = stamp;
      await boot([pr, issueWorkspace()]);
      channel().onmessage(splitView());
      await vi.waitFor(() =>
        expect(document.querySelectorAll(".workspace-row").length).toBe(2),
      );

      const before = rowNodes().get("github-o-r-42");
      const ageBefore = before?.querySelector("time")?.textContent ?? "";
      expect(ageBefore).not.toBe("");

      clock.mockReturnValue(base + 5 * 60_000);
      channel().onmessage(splitView());
      await vi.waitFor(() =>
        expect(
          rowNodes().get("github-o-r-42")?.querySelector("time")?.textContent,
        ).not.toBe(ageBefore),
      );
      // The refresh happens by rebuilding exactly that row.
      expect(rowNodes().get("github-o-r-42")).not.toBe(before);
    } finally {
      clock.mockRestore();
    }
  });

  it("keeps a node-id-less activity card stable across a prepend", async () => {
    const pr = prWorkspace();
    pr.activity = [activityEntry("2026-01-01T00:01:00.000Z", "alice", "first")];
    pr.seen_count = 0;
    await boot([pr]);
    channel().onmessage(singleView("github-o-r-42"));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(1),
    );
    rowNodes().get("github-o-r-42")?.click();
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".activity-card").length).toBe(1),
    );
    const original = document.querySelector(".activity-card");

    // A newer activity is prepended (newest-first). With an index-based
    // key the original card below it would remount; a content key keeps
    // it the same node.
    const updated = {
      ...pr,
      activity: [
        activityEntry("2026-01-01T00:02:00.000Z", "bob", "second"),
        ...(pr.activity as Array<Record<string, unknown>>),
      ],
    };
    channel().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: updated },
    });
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".activity-card").length).toBe(2),
    );
    const cards = [...document.querySelectorAll(".activity-card")];
    expect(cards[1]).toBe(original);
  });

  it("reuses nodes when only the order changes", async () => {
    await boot([prWorkspace(), issueWorkspace()]);
    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );
    const before = rowNodes();

    // A recompute that reorders (issue first) must move nodes, not
    // rebuild them.
    channel().onmessage(recentView());
    await vi.waitFor(() =>
      expect(rowKeyOrder()).toEqual(["github-o-r-7", "github-o-r-42"]),
    );

    const after = rowNodes();
    expect(after.get("github-o-r-42")).toBe(before.get("github-o-r-42"));
    expect(after.get("github-o-r-7")).toBe(before.get("github-o-r-7"));
  });
});

function rowNodes(): Map<string, HTMLElement> {
  const map = new Map<string, HTMLElement>();
  for (const row of document.querySelectorAll<HTMLElement>(".workspace-row")) {
    const key = row.dataset.key;
    if (key !== undefined) {
      map.set(key, row);
    }
  }
  return map;
}

function rowKeyOrder(): string[] {
  return [...document.querySelectorAll<HTMLElement>(".workspace-row")]
    .map((row) => row.dataset.key)
    .filter((key): key is string => key !== undefined);
}

async function boot(workspaces: Array<Record<string, unknown>>): Promise<void> {
  harness.invoke.mockImplementation((command: string) => {
    if (command === "desktop_setup_state") {
      return Promise.resolve({
        first_run: false,
        selected_scopes: ["github:o/r"],
        agents: [{ id: "codex", label: "Codex", available: true }],
        default_agent: "codex",
        analytics_enabled: false,
        diagnostics_path: "/tmp/lazybox-crashes",
        theme: null,
        themes: [],
        keymap_preset: null,
        terminal_new_layout: "split",
        activity_pane_default: "full",
      });
    }
    if (command === "desktop_info") {
      return Promise.resolve({
        protocol_version: 1,
        max_terminal_frame_bytes: 2048,
        max_terminal_write_bytes: 1024,
        agents: ["codex"],
        default_agent: "codex",
        repositories: [{ project_key: "github-o-r", label: "o/r" }],
      });
    }
    if (command === "list_workspaces") {
      return Promise.resolve({ workspaces, warnings: [] });
    }
    if (command === "read_terminal_data") {
      return new Promise<Uint8Array>(() => {});
    }
    return Promise.resolve();
  });

  vi.resetModules();
  await import("./main");
  await vi.waitFor(() => expect(harness.channels.at(-1)).not.toBeUndefined());
}

function prWorkspace(): Record<string, unknown> {
  const workspace = template();
  workspace.key = "github-o-r-42";
  workspace.branch = "github-o-r-42";
  workspace.name = "PR o/r#42";
  const task = workspace.pr as Record<string, unknown>;
  (task.id as { key: string }).key = "o/r#42";
  task.title = "PR o/r#42";
  task.ci = "Failure";
  task.review = "ChangesRequested";
  task.kind = "Pr";
  return workspace;
}

function issueWorkspace(): Record<string, unknown> {
  const workspace = template();
  workspace.key = "github-o-r-7";
  workspace.branch = "github-o-r-7";
  workspace.name = "Issue o/r#7";
  const task = workspace.pr as Record<string, unknown>;
  (task.id as { key: string }).key = "o/r#7";
  task.title = "Issue o/r#7";
  task.ci = "None";
  task.review = "None";
  task.url = "https://github.com/o/r/issues/7";
  task.kind = "Issue";
  workspace.pr = null;
  workspace.gh_issues = [task];
  return workspace;
}

function template(): Record<string, unknown> {
  const snapshot = structuredClone(fixture.events[0]) as {
    Snapshot: { workspaces: Array<Record<string, unknown>> };
  };
  const workspace = snapshot.Snapshot.workspaces[0];
  if (workspace === undefined) {
    throw new Error("fixture snapshot is missing a template workspace");
  }
  return workspace;
}

function splitView(): Record<string, unknown> {
  return {
    type: "Inbox",
    payload: {
      outcome: {
        visible: [
          { RepoHeader: "o/r" },
          { KindHeader: "Pr" },
          { Workspace: "github-o-r-42" },
          { KindHeader: "Issue" },
          { Workspace: "github-o-r-7" },
        ],
        summaries: { "o/r": { active: 2, attention: 1 } },
      },
      sort_mode: "ByRoleSplit",
      filter_menu: [],
      filter_chips: [],
    },
  };
}

function singleView(key: string): Record<string, unknown> {
  return {
    type: "Inbox",
    payload: {
      outcome: {
        visible: [{ RepoHeader: "o/r" }, { Workspace: key }],
        summaries: { "o/r": { active: 1, attention: 0 } },
      },
      sort_mode: "Recent",
      filter_menu: [],
      filter_chips: [],
    },
  };
}

function activityEntry(
  createdAt: string,
  author: string,
  body: string,
): Record<string, unknown> {
  return {
    author,
    body,
    created_at: createdAt,
    kind: "Comment",
    node_id: null,
    path: null,
    line: null,
    diff_hunk: null,
    thread_id: null,
  };
}

function recentView(): Record<string, unknown> {
  return {
    type: "Inbox",
    payload: {
      outcome: {
        visible: [
          { RepoHeader: "o/r" },
          { Workspace: "github-o-r-7" },
          { Workspace: "github-o-r-42" },
        ],
        summaries: { "o/r": { active: 2, attention: 1 } },
      },
      sort_mode: "Recent",
      filter_menu: [],
      filter_chips: [],
    },
  };
}

function channel(): { onmessage: (message: unknown) => void } {
  const value = harness.channels.at(-1);
  if (value === undefined) {
    throw new Error("missing desktop event channel");
  }
  return value;
}

function loadDocument(): void {
  document.open();
  document.write(html.replace(/<script type="module"[\s\S]*?<\/script>/, ""));
  document.close();
  Object.defineProperty(globalThis, "Option", {
    configurable: true,
    value: function Option(text = "", value = "") {
      const option = document.createElement("option");
      option.textContent = text;
      option.value = value;
      return option;
    },
  });
}
