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
    options: Record<string, unknown> = {};
    loadAddon(): void {}
    open(): void {}
    focus(): void {}
    reset(): void {}
    dispose(): void {}
    attachCustomKeyEventHandler(): void {}
    onData(): { dispose: () => void } {
      return { dispose: () => {} };
    }
    onResize(): { dispose: () => void } {
      return { dispose: () => {} };
    }
    write(): void {}
  },
}));

describe("desktop daily-driver parity", () => {
  beforeEach(() => {
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
    harness.invoke.mockReset();
    harness.channels.length = 0;
  });

  it("preserves marks and broadcasts across agent, shell, and session-less targets", async () => {
    const workspaces = [
      workspace("agent"),
      workspace("shell"),
      workspace("new"),
    ];
    const terminals = [
      terminal("agent", 7, { Agent: "codex" }),
      terminal("shell", 8, "Shell"),
    ];
    await boot(workspaces);
    stream().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces, terminals, recent_snippets: [] } },
    });
    stream().onmessage(inbox(["agent", "shell", "new"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(3),
    );

    for (const key of ["agent", "shell", "new"]) {
      row(key).querySelector<HTMLElement>(".workspace-mark-toggle")?.click();
    }
    stream().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: workspace("agent") },
    });
    stream().onmessage(inbox(["agent", "shell", "new"]));
    expect(document.querySelectorAll(".workspace-row.marked")).toHaveLength(3);

    button("broadcast-button").click();
    textarea("broadcast-body").value = "Run the focused verification.";
    form("broadcast-form").dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    // "new" is session-less, so the broadcast gates behind a "start N
    // agents?" confirm before spawning.
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    expect(element("confirm-title").textContent).toContain("Start 1 agent");
    button("confirm-accept").click();
    await vi.waitFor(() => expect(commandCalls()).toHaveLength(3));
    expect(commandCalls()).toContainEqual({
      InjectPrompt: { terminal_id: 7, body: "Run the focused verification." },
    });
    expect(commandCalls()).toContainEqual({
      WriteShell: { terminal_id: 8, body: "Run the focused verification." },
    });
    expect(commandCalls()).toContainEqual({
      SpawnAgent: {
        session_key: "new",
        agent: "codex",
        initial_prompt: "Context for new.\n\nRun the focused verification.",
        model_alias: null,
        on_main: false,
      },
    });
    expect(element("status-message").textContent).toContain(
      "agent, shell, new",
    );
  });

  it("reports a target removed mid-broadcast instead of silently dropping it", async () => {
    const workspaces = [workspace("a"), workspace("b")];
    const terminals = [
      terminal("a", 1, { Agent: "codex" }),
      terminal("b", 2, { Agent: "codex" }),
    ];
    await boot(workspaces);
    stream().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces, terminals, recent_snippets: [] } },
    });
    stream().onmessage(inbox(["a", "b"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(2),
    );
    for (const key of ["a", "b"]) {
      row(key).querySelector<HTMLElement>(".workspace-mark-toggle")?.click();
    }

    button("broadcast-button").click();
    textarea("broadcast-body").value = "go";
    // Submit suspends at the first target's await; the poll that removes
    // "b" lands before the loop reaches it. A live-Set iteration would
    // skip "b" entirely; the snapshot still accounts for it.
    form("broadcast-form").dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
    stream().onmessage({ type: "Frame", payload: { WorkspaceRemoved: "b" } });

    await vi.waitFor(() =>
      expect(element("status-message").textContent).toContain(
        "no longer available",
      ),
    );
    expect(element("status-message").textContent).toContain("sent: a");
  });

  it("paginates a large activity feed explicitly instead of rendering all rows", async () => {
    const target = workspace("big");
    target.activity = Array.from({ length: 120 }, (_, index) =>
      activity(index),
    );
    await boot([target]);
    stream().onmessage({
      type: "Frame",
      payload: {
        Snapshot: { workspaces: [target], terminals: [], recent_snippets: [] },
      },
    });
    stream().onmessage(inbox(["big"]));
    // First page is bounded; the rest are behind an explicit control.
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".activity-card")).toHaveLength(50),
    );
    expect(element("activity-count").textContent).toBe("120");
    const more = document.querySelector<HTMLButtonElement>(
      ".activity-show-more",
    )!;
    expect(more.textContent).toContain("70");

    more.click();
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".activity-card")).toHaveLength(100),
    );
    document.querySelector<HTMLButtonElement>(".activity-show-more")!.click();
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".activity-card")).toHaveLength(120),
    );
    expect(document.querySelector(".activity-show-more")).toBeNull();
  });

  it("renders the full activity feed, works on stable selection, and confirms cleanup", async () => {
    const target = workspace("activity");
    target.activity = Array.from({ length: 35 }, (_, index) => activity(index));
    const live = terminal("activity", 9, { Agent: "codex" });
    await boot([target]);
    stream().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [target],
          terminals: [live],
          recent_snippets: [],
        },
      },
    });
    stream().onmessage(inbox(["activity"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".activity-card")).toHaveLength(35),
    );

    const last = document.querySelectorAll<HTMLElement>(".activity-card")[34]!;
    last
      .querySelector<HTMLButtonElement>(".activity-card-actions button")
      ?.click();
    button("work-activity-button").click();
    await vi.waitFor(() =>
      expect(harness.invoke).toHaveBeenCalledWith("resolve_work_prompt", {
        sessionKey: "activity",
        selectedActivity: [34],
        agent: "codex",
      }),
    );

    stream().onmessage({
      type: "Frame",
      payload: {
        WorkspaceCleanupRequested: {
          workspace_key: "activity",
          label: "o/r#activity",
          reason: "Merged",
          active_terminal_count: 0,
          has_local_work: false,
        },
      },
    });
    await vi.waitFor(() => expect(dialog("cleanup-dialog").open).toBe(true));
    button("cleanup-remove").click();
    await vi.waitFor(() =>
      expect(commandCalls()).toContainEqual({
        RemoveMergedWorkspace: { session_key: "activity" },
      }),
    );
  });

  it("surfaces the asking badge and jump target when an agent needs input", async () => {
    const target = workspace("asking");
    const live = terminal("asking", 11, { Agent: "codex" });
    await boot([target]);
    stream().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [target],
          terminals: [live],
          recent_snippets: [],
        },
      },
    });
    stream().onmessage(inbox(["asking"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(1),
    );
    // A working agent reads as "running", never "asking".
    expect(row("asking").textContent).toContain("running");
    expect(row("asking").textContent).not.toContain("asking");

    // The agent asks for input. It travels as AgentState "InputNeeded",
    // which formatAgentState lowercases to "inputneeded" (no space) — the
    // value the badge/jump matchers must compare against.
    stream().onmessage({
      type: "Frame",
      payload: {
        AgentState: {
          session_key: "asking",
          terminal_id: 11,
          state: "InputNeeded",
        },
      },
    });
    await vi.waitFor(() =>
      expect(row("asking").textContent).toContain("asking"),
    );

    // Jump-to-asking locates it instead of reporting "No agent is asking."
    button("jump-asking-button").click();
    expect(element("status-message").textContent).not.toContain(
      "No agent is asking",
    );
  });

  it("does not offer cleanup merely because the last terminal exited", async () => {
    const target = workspace("healthy");
    const live = terminal("healthy", 21, { Agent: "codex" });
    await boot([target]);
    stream().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [target],
          terminals: [live],
          recent_snippets: [],
        },
      },
    });
    stream().onmessage(inbox(["healthy"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(1),
    );

    stream().onmessage({
      type: "Frame",
      payload: {
        TerminalExited: { terminal_id: 21, exit_code: 0, last_output: null },
      },
    });
    // The daemon owns the keep/remove decision; a bare terminal exit on a
    // healthy workspace must never pop the modal (nor risk archiving it).
    await Promise.resolve();
    expect(dialog("cleanup-dialog").open).toBe(false);
    expect(commandCalls()).not.toContainEqual({
      KeepWorkspace: { session_key: "healthy" },
    });
  });

  it("dismisses a pending cleanup prompt when the daemon cancels it", async () => {
    const target = workspace("reopened");
    await boot([target]);
    stream().onmessage({
      type: "Frame",
      payload: {
        Snapshot: { workspaces: [target], terminals: [], recent_snippets: [] },
      },
    });
    stream().onmessage(inbox(["reopened"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(1),
    );

    stream().onmessage({
      type: "Frame",
      payload: {
        WorkspaceCleanupRequested: {
          workspace_key: "reopened",
          label: "o/r#reopened",
          reason: "Closed",
          active_terminal_count: 0,
          has_local_work: false,
        },
      },
    });
    await vi.waitFor(() => expect(dialog("cleanup-dialog").open).toBe(true));
    stream().onmessage({
      type: "Frame",
      payload: { WorkspaceCleanupCancelled: { workspace_key: "reopened" } },
    });
    await vi.waitFor(() => expect(dialog("cleanup-dialog").open).toBe(false));
  });
});

async function boot(workspaces: WorkspaceFixture[]): Promise<void> {
  harness.invoke.mockImplementation((command: string, args?: unknown) => {
    if (command === "desktop_setup_state") {
      return Promise.resolve({
        authority: "embedded",
        providers: ["github"],
        first_run: false,
        selected_scopes: ["github:o/r"],
        agents: [
          {
            id: "codex",
            label: "Codex",
            available: true,
            models: [],
            default_tier: null,
          },
        ],
        default_agent: "codex",
        analytics_enabled: false,
        diagnostics_path: "/tmp/lazybox",
        theme: null,
        themes: [],
        keymap_preset: null,
        collapsed_repos: [],
        terminal_new_layout: "split",
        activity_pane_default: "full",
      });
    }
    if (command === "desktop_info") {
      return Promise.resolve({
        protocol_version: 2,
        max_terminal_frame_bytes: 2048,
        max_terminal_write_bytes: 1024,
        agents: ["codex"],
        default_agent: "codex",
        repositories: [{ project_key: "github-o-r", label: "o/r" }],
        protocol_notice: null,
      });
    }
    if (command === "github_auth_status") {
      return Promise.resolve({
        authenticated: true,
        account: "fixture",
        message: "GitHub credential verified",
      });
    }
    if (command === "list_github_repositories") return Promise.resolve([]);
    if (command === "list_workspaces")
      return Promise.resolve({ workspaces, warnings: [] });
    if (command === "read_terminal_data")
      return new Promise<Uint8Array>(() => {});
    if (command === "resolve_work_prompt") {
      const key = (args as { sessionKey: string }).sessionKey;
      return Promise.resolve(`Context for ${key}.`);
    }
    return Promise.resolve();
  });
  vi.resetModules();
  await import("./main");
  await vi.waitFor(() => expect(harness.channels[0]).not.toBeUndefined());
}

type WorkspaceFixture = Record<string, unknown>;

function workspace(key: string): WorkspaceFixture {
  const template = structuredClone(fixture.events[0]) as {
    Snapshot: { workspaces: WorkspaceFixture[] };
  };
  const ws = template.Snapshot.workspaces[0];
  if (ws === undefined) {
    throw new Error("fixture snapshot is missing a template workspace");
  }
  ws.key = key;
  ws.name = key;
  ws.branch = key;
  ws.sessions = [];
  ws.activity = [];
  return ws;
}

function terminal(
  sessionKey: string,
  terminalId: number,
  kind: unknown,
): Record<string, unknown> {
  return {
    terminal_id: terminalId,
    session_key: sessionKey,
    kind,
    last_seq: 0,
    agent_state: kind === "Shell" ? null : "Working",
    model_label: kind === "Shell" ? null : "Large",
    prompt_history: [],
  };
}

function activity(index: number): Record<string, unknown> {
  return {
    author: `user-${index}`,
    body: `Activity ${index}`,
    created_at: `2026-08-09T00:${String(index).padStart(2, "0")}:00Z`,
    kind: "Comment",
    node_id: `node-${index}`,
    path: null,
    line: null,
    diff_hunk: null,
    thread_id: null,
  };
}

function inbox(keys: string[]): Record<string, unknown> {
  return {
    type: "Inbox",
    payload: {
      outcome: {
        visible: [
          { RepoHeader: "o/r" },
          ...keys.map((key) => ({ Workspace: key })),
        ],
        summaries: { "o/r": { active: keys.length, attention: 0 } },
      },
      sort_mode: "ByRoleSplit",
      mailbox: "Inbox",
      filter_menu: [],
      filter_chips: [],
    },
  };
}

function stream(): { onmessage: (message: unknown) => void } {
  return harness.channels[0]!;
}

function commandCalls(): unknown[] {
  return harness.invoke.mock.calls
    .filter(([command]) => command === "send_command")
    .map(([, args]) => (args as { command: unknown }).command);
}

function row(key: string): HTMLElement {
  return document.querySelector<HTMLElement>(
    `.workspace-row[data-key="${key}"]`,
  )!;
}

function element(id: string): HTMLElement {
  return document.getElementById(id)!;
}

function button(id: string): HTMLButtonElement {
  return element(id) as HTMLButtonElement;
}

function textarea(id: string): HTMLTextAreaElement {
  return element(id) as HTMLTextAreaElement;
}

function form(id: string): HTMLFormElement {
  return element(id) as HTMLFormElement;
}

function dialog(id: string): HTMLDialogElement {
  return element(id) as HTMLDialogElement;
}
