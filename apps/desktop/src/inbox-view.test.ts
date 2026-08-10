// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";
import fixture from "./generated/compatibility.json";
import mainSource from "./main.ts?raw";
import modelSource from "./model.ts?raw";

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

describe("grouped inbox renderer (#732)", () => {
  beforeEach(() => {
    loadDocument();
    harness.invoke.mockReset();
    harness.channels.length = 0;
  });

  it("renders repo groups → PR/Issue sections → rows with real badges", async () => {
    await boot([prWorkspace(), issueWorkspace()]);

    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );

    // Repo header names the repo; PR + Issue section headers appear.
    expect(document.querySelector(".repo-header")?.textContent).toContain(
      "o/r",
    );
    const sections = [
      ...document.querySelectorAll<HTMLElement>(".kind-header"),
    ].map((node) => node.textContent);
    expect(sections).toEqual(["PRs", "Issues"]);

    // The PR row carries a real failing-CI badge (not plain text).
    const rows = [...document.querySelectorAll<HTMLElement>(".workspace-row")];
    const prRow = rows.find((row) => row.dataset.key === "github-o-r-42");
    const badges = [...(prRow?.querySelectorAll(".signal-pill") ?? [])].map(
      (pill) => pill.textContent,
    );
    expect(badges).toContain("CI failure");
    expect(prRow?.querySelector(".signal-pill.attention")).not.toBeNull();

    // Order follows the view-model exactly: PR then Issue.
    expect(rows.map((row) => row.dataset.key)).toEqual([
      "github-o-r-42",
      "github-o-r-7",
    ]);
    expect(document.getElementById("sort-label")?.textContent).toBe("split");
  });

  it("reorders and relabels when the sort mode changes", async () => {
    await boot([prWorkspace(), issueWorkspace()]);
    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );

    // A recompute in `recent` mode: a flat list, issue first, no sections.
    channel().onmessage(recentView());
    await vi.waitFor(() =>
      expect(
        [...document.querySelectorAll<HTMLElement>(".workspace-row")].map(
          (row) => row.dataset.key,
        ),
      ).toEqual(["github-o-r-7", "github-o-r-42"]),
    );
    expect(document.querySelectorAll(".kind-header").length).toBe(0);
    expect(document.getElementById("sort-label")?.textContent).toBe("recent");

    // The sort control delegates ordering to the daemon-side model.
    button("sort-button").click();
    await vi.waitFor(() =>
      expect(harness.invoke).toHaveBeenCalledWith("set_sort_mode"),
    );
  });

  it("does not reimplement grouping or sort in TypeScript", () => {
    // Ordering + grouping come only from the shared tui-core view-model.
    expect(modelSource).not.toContain("sortedWorkspaces");
    expect(modelSource).not.toContain("filteredWorkspaces");
    expect(mainSource).not.toContain("sortedWorkspaces");
    expect(mainSource).not.toContain("filteredWorkspaces");
    // The inbox render is driven by the emitted view-model, and sorting
    // is delegated to the daemon-side command.
    expect(mainSource).toContain("orderedWorkspaceKeys");
    expect(mainSource).toContain('invoke("set_sort_mode")');
    // The renderer walks the pre-ordered view rows directly.
    expect(mainSource).toContain("inboxView?.visible");
    // No workspace unread/recency comparator survives (the old TS sort).
    expect(mainSource).not.toContain("unreadCount(right)");
    expect(mainSource).not.toContain("updated_at ??");
    expect(modelSource).not.toContain("unreadCount(right)");
  });

  it("expands a TUI-collapsed repo from complete rows and persists the shared state", async () => {
    const workspaces = [prWorkspace(), issueWorkspace()];
    await boot(workspaces, ["o/r"]);
    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".repo-header")).toHaveLength(1),
    );
    expect(document.querySelectorAll(".workspace-row")).toHaveLength(0);

    element(".repo-header").click();
    await vi.waitFor(() =>
      expect(harness.invoke).toHaveBeenCalledWith("set_repo_collapsed", {
        repo: "o/r",
        collapsed: false,
      }),
    );
    expect(document.querySelectorAll(".workspace-row")).toHaveLength(2);

    loadDocument();
    harness.channels.length = 0;
    await boot(workspaces, []);
    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(2),
    );
  });

  it("a stale failed collapse persist does not clobber a newer re-collapse", async () => {
    // #971 review: three quick toggles where the FIRST persist fails late,
    // and the third re-collapses to the *same* value the first tried. A
    // value-based rollback guard would see "state still collapsed, must be
    // mine" and revert, re-expanding against the successful third toggle;
    // sequence-based supersession must leave the newest toggle intact.
    const workspaces = [prWorkspace(), issueWorkspace()];
    await boot(workspaces, []);
    channel().onmessage(splitView());
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(2),
    );

    const deferred: Array<{
      collapsed: boolean;
      resolve: () => void;
      reject: (error: unknown) => void;
    }> = [];
    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "set_repo_collapsed") {
        const collapsed = (args as { collapsed: boolean }).collapsed;
        return new Promise<void>((resolve, reject) => {
          deferred.push({ collapsed, resolve, reject });
        });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    element(".repo-header").click(); // 1: collapse (persist will FAIL)
    element(".repo-header").click(); // 2: expand   (persist will succeed)
    element(".repo-header").click(); // 3: collapse (persist will succeed)
    expect(deferred.map((call) => call.collapsed)).toEqual([true, false, true]);
    expect(document.querySelectorAll(".workspace-row")).toHaveLength(0);
    const [first, second, third] = deferred;
    if (!first || !second || !third) throw new Error("expected three persists");

    second.resolve();
    third.resolve();
    first.reject(new Error("save failed"));

    // The stale rejection must NOT re-expand the repo the third click
    // collapsed; rows stay hidden.
    await Promise.resolve();
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(0),
    );
  });
});

async function boot(
  workspaces: Array<Record<string, unknown>>,
  collapsedRepos: string[] = [],
): Promise<void> {
  harness.invoke.mockImplementation((command: string) => {
    if (command === "desktop_setup_state") {
      return Promise.resolve({
        authority: "embedded",
        providers: ["github"],
        first_run: false,
        selected_scopes: ["github:o/r"],
        agents: [{ id: "codex", label: "Codex", available: true }],
        default_agent: "codex",
        analytics_enabled: false,
        diagnostics_path: "/tmp/lazybox-crashes",
        log_path: "/tmp/lazybox.log",
        theme: null,
        themes: [],
        keymap_preset: null,
        collapsed_repos: collapsedRepos,
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
  await vi.waitFor(() => expect(channelMaybe()).not.toBeUndefined());
}

function element(selector: string): HTMLElement {
  const value = document.querySelector<HTMLElement>(selector);
  if (value === null) {
    throw new Error(`missing ${selector}`);
  }
  return value;
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

function channelMaybe(): { onmessage: (message: unknown) => void } | undefined {
  return harness.channels.at(-1);
}

function channel(): { onmessage: (message: unknown) => void } {
  const value = channelMaybe();
  if (value === undefined) {
    throw new Error("missing desktop event channel");
  }
  return value;
}

function button(id: string): HTMLButtonElement {
  const value = document.getElementById(id);
  if (value === null) {
    throw new Error(`missing #${id}`);
  }
  return value as HTMLButtonElement;
}
