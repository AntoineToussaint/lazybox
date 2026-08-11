// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";
import fixture from "./generated/compatibility.json";

const harness = vi.hoisted(() => ({
  invoke: vi.fn(),
  channels: [] as Array<{ onmessage: (message: unknown) => void }>,
  terminalWrites: [] as string[],
  terminalDataHandlers: [] as Array<(data: string) => void>,
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
    onData(handler: (data: string) => void): { dispose: () => void } {
      harness.terminalDataHandlers.push(handler);
      return { dispose: () => {} };
    }
    onResize(): { dispose: () => void } {
      return { dispose: () => {} };
    }
    write(data: string | Uint8Array): void {
      harness.terminalWrites.push(
        typeof data === "string" ? data : new TextDecoder().decode(data),
      );
    }
  },
}));

describe("credential-free desktop workflow", () => {
  beforeEach(() => {
    loadDocument();
    harness.invoke.mockReset();
    harness.channels.length = 0;
    harness.terminalWrites.length = 0;
    harness.terminalDataHandlers.length = 0;
  });

  it("runs setup, empty-inbox creation, mutations, and terminal recovery", async () => {
    let phase: "onboarding" | "workflow" = "onboarding";
    let replyAttempt: "fail" | "succeed" = "fail";
    let rejectReply: ((reason: unknown) => void) | undefined;
    const terminalReads: Array<(bytes: Uint8Array) => void> = [];

    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          phase === "onboarding"
            ? settingsStateFixture({
                first_run: true,
                agents: [agentOption("cursor-agent", "Cursor Agent")],
                default_agent: "cursor-agent",
              })
            : settingsStateFixture({
                selected_scopes: ["github:acme/widget"],
              }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: phase === "onboarding" ? ["cursor-agent"] : ["codex"],
          default_agent: phase === "onboarding" ? "cursor-agent" : "codex",
          repositories:
            phase === "onboarding"
              ? []
              : [{ project_key: "github-acme-widget", label: "acme/widget" }],
        });
      }
      if (command === "github_auth_status") {
        return Promise.resolve({
          authenticated: true,
          account: "fixture",
          message: "GitHub credential verified",
        });
      }
      if (command === "list_github_repositories") {
        return Promise.resolve([
          {
            id: "github:acme/widget",
            label: "acme/widget",
            owner: "acme",
          },
        ]);
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "subscribe_events") {
        return Promise.resolve();
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>((resolve) =>
          terminalReads.push(resolve),
        );
      }
      if (command === "send_command") {
        const payload = args as {
          command?: { PostReply?: { body: string } };
        };
        if (
          payload.command?.PostReply !== undefined &&
          replyAttempt === "fail"
        ) {
          return new Promise((_resolve, reject) => {
            rejectReply = reject;
          });
        }
        return Promise.resolve();
      }
      return Promise.resolve();
    });

    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(true));
    await vi.waitFor(() =>
      expect(
        document.querySelector('input[value="github:acme"]'),
      ).not.toBeNull(),
    );

    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: ",", metaKey: true }),
    );
    await Promise.resolve();
    expect(element("setup-close").classList.contains("hidden")).toBe(true);
    expect(dialog("setup-dialog").open).toBe(true);

    input('input[value="github:acme"]').click();
    form("setup-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();
    await vi.waitFor(() => {
      expect(harness.invoke).toHaveBeenCalledWith("save_desktop_settings", {
        settings: savePayload({
          github_scopes: ["github:acme"],
          default_agent: "cursor-agent",
        }),
      });
    });

    phase = "workflow";
    vi.resetModules();
    loadDocument();
    harness.channels.length = 0;
    terminalReads.length = 0;
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(button("new-workspace-button").disabled).toBe(false),
    );

    button("new-workspace-button").click();
    expect(dialog("new-workspace-dialog").open).toBe(true);
    expect(select("new-workspace-project").value).toBe("github-acme-widget");
    input("#new-workspace-name").value = "first local workspace";
    form("new-workspace-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => {
      expect(commandCalls()).toContainEqual({
        CreateWorkspace: {
          name: "first local workspace",
          project_key: "github-acme-widget",
          agent: "codex",
        },
      });
    });
    await vi.waitFor(() =>
      expect(dialog("new-workspace-dialog").open).toBe(false),
    );

    const snapshot = structuredClone(fixture.events[0]) as {
      Snapshot: {
        workspaces: Array<{ key: string }>;
        terminals: Array<{ session_key: string }>;
      };
    };
    const recoveredTerminals = snapshot.Snapshot.terminals;
    if (recoveredTerminals[0] !== undefined) {
      recoveredTerminals[0].session_key = "github-o-r-42";
    }
    snapshot.Snapshot.terminals = [];
    eventChannel().onmessage({ type: "Frame", payload: snapshot });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );
    expect(button("spawn-button").disabled).toBe(false);
    expect(button("shell-button").disabled).toBe(false);

    button("spawn-button").click();
    button("shell-button").click();
    await vi.waitFor(() => {
      expect(commandCalls()).toContainEqual({
        SpawnAgent: {
          session_key: "github-o-r-42",
          agent: "codex",
          initial_prompt: null,
          model_alias: null,
          on_main: false,
        },
      });
      expect(commandCalls()).toContainEqual({
        SpawnShell: { session_key: "github-o-r-42", on_main: false },
      });
    });

    input("#reply-body").value = "Ready to ship.";
    form("reply-form").dispatchEvent(submitEvent());
    form("reply-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();
    await vi.waitFor(() => expect(replyCommands()).toHaveLength(1));
    rejectReply?.("post failed: permission denied");
    await vi.waitFor(() => {
      expect(input("#reply-body").value).toBe("Ready to ship.");
      expect(element("status-message").textContent).toContain(
        "permission denied",
      );
      expect(button("reply-button").disabled).toBe(false);
    });

    replyAttempt = "succeed";
    form("reply-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();
    await vi.waitFor(() => expect(input("#reply-body").value).toBe(""));
    expect(replyCommands()).toHaveLength(2);

    button("settings-button").click();
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(true));
    input('input[value="github:acme/widget"]').click();
    expect(element("repository-selection-count").textContent).toBe(
      "All accessible repositories",
    );
    form("setup-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();
    await vi.waitFor(() =>
      expect(settingsCalls().at(-1)).toEqual(savePayload()),
    );
    button("setup-close").click();

    snapshot.Snapshot.terminals = recoveredTerminals;
    eventChannel().onmessage({ type: "Frame", payload: snapshot });
    await vi.waitFor(() => expect(terminalReads.length).toBeGreaterThan(0));
    terminalReads.shift()?.(
      nativeTerminalData(serverFrame(3, 7, 0, 42, "recovered terminal\r\n")),
    );
    await vi.waitFor(() =>
      expect(harness.terminalWrites).toContain("recovered terminal\r\n"),
    );
  });

  it("keeps an in-flight reply on one workspace from disabling another", async () => {
    let releaseReply: (() => void) | undefined;

    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:acme/widget"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      if (command === "send_command") {
        const payload = args as { command?: { PostReply?: unknown } };
        if (payload.command?.PostReply !== undefined) {
          return new Promise<void>((resolve) => {
            releaseReply = resolve;
          });
        }
        return Promise.resolve();
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [pr(42), pr(43)], terminals: [] } },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42", "github-o-r-43"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );

    selectWorkspaceRow("PR o/r#42");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );
    expect(button("reply-button").disabled).toBe(false);

    input("#reply-body").value = "Shipping now.";
    form("reply-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();
    await vi.waitFor(() => expect(replyCommands()).toHaveLength(1));
    expect(button("reply-button").disabled).toBe(true);

    selectWorkspaceRow("PR o/r#43");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#43"),
    );
    expect(button("reply-button").disabled).toBe(false);
    expect(input("#reply-body").disabled).toBe(false);

    selectWorkspaceRow("PR o/r#42");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );
    expect(button("reply-button").disabled).toBe(true);

    releaseReply?.();
    await vi.waitFor(() => expect(button("reply-button").disabled).toBe(false));
  });

  it("gates act-on-work items by state/scope and surfaces mutation outcomes", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o/r"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    const openPr = pr(42);
    const mergedPr = pr(43);
    (mergedPr.pr as { state: string }).state = "Merged";
    const localScratch = taskless("local-scratch");
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [openPr, mergedPr, localScratch],
          terminals: [],
        },
      },
    });
    eventChannel().onmessage(
      inboxMessage(["github-o-r-42", "github-o-r-43", "scratch"]),
    );
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(3),
    );

    // Open PR: merge / update-branch / on-main / browser all offered.
    selectWorkspaceRow("PR o/r#42");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );
    let labels = openActionsMenuLabels();
    expect(labels).toContain("Merge PR");
    expect(labels).toContain("Update branch");
    expect(labels).toContain("Open in browser");
    expect(labels).toContain("Start codex on main checkout");

    // Merged PR is terminal: no merge / update-branch / delete offered.
    selectWorkspaceRow("PR o/r#43");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#43"),
    );
    labels = openActionsMenuLabels();
    expect(labels).not.toContain("Merge PR");
    expect(labels).not.toContain("Update branch");
    expect(labels).not.toContain("Close PR (no merge)");
    // A repo-scoped, non-mutating action still shows.
    expect(labels).toContain("Open in browser");

    // Local scratch has no repo: on-main must not be offered.
    selectWorkspaceRow("Scratch");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("Scratch"),
    );
    labels = openActionsMenuLabels();
    expect(labels.some((label) => label.includes("on main checkout"))).toBe(
      false,
    );
    expect(labels).toContain("Rename…");

    // A GitHub-rejected merge must reach the status line, not vanish.
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        WorkspaceActionOutcome: {
          workspace_key: "github-o-r-42",
          ok: false,
          message: "Merge of o/r#42 failed: not mergeable",
        },
      },
    });
    await vi.waitFor(() =>
      expect(element("status-message").textContent).toContain("not mergeable"),
    );
  });

  it("requests and renders a worktree diff (#843)", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o/r"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    // A linked-checkout workspace has an on-disk tree to review, so the
    // target resolves to `LinkedCheckout`.
    const linked = pr(42);
    linked.linked_checkout = "/home/dev/o-r";
    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [linked], terminals: [] } },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(1),
    );

    selectWorkspaceRow("PR o/r#42");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );

    expect(openActionsMenuLabels()).toContain("View diff");

    const viewDiff = [
      ...element("actions-menu").querySelectorAll<HTMLButtonElement>(
        ".actions-menu-item",
      ),
    ].find((item) => item.textContent === "View diff");
    viewDiff?.click();

    await vi.waitFor(() =>
      expect(commandCalls()).toContainEqual({
        InspectWorkspaceDiff: {
          session_key: "github-o-r-42",
          target: "LinkedCheckout",
        },
      }),
    );

    // The daemon replies asynchronously; the reader opens with the diff.
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        WorkspaceDiffInspected: {
          workspace_key: "github-o-r-42",
          diff: {
            status: [" M src/main.rs"],
            stat: [" src/main.rs | 1 +"],
            files: [
              {
                old_path: null,
                path: "src/main.rs",
                headers: [],
                hunks: [
                  {
                    header: "@@ -1 +1 @@",
                    old_start: 1,
                    new_start: 1,
                    lines: [
                      {
                        kind: "Addition",
                        text: "+let x = 1;",
                        old_line: null,
                        new_line: 1,
                      },
                    ],
                  },
                ],
              },
            ],
            truncated: false,
          },
          error: null,
        },
      },
    });

    await vi.waitFor(() => expect(dialog("diff-dialog").open).toBe(true));
    expect(element("diff-body").textContent).toContain("src/main.rs");
    expect(element("diff-body").textContent).toContain("+let x = 1;");

    // While the reader is up it is a modal: global shortcuts must not
    // leak to the inbox behind it. `f` (open filter menu) stays inert.
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "f" }));
    expect(element("filter-menu").classList.contains("hidden")).toBe(true);
  });

  it("does not dispatch modified action keys or shortcuts behind rename", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [pr(42)], terminals: [] } },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );

    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: "a", metaKey: true }),
    );
    expect(commandCalls().some(isSpawnAgentCommand)).toBe(false);

    const rename = [
      ...element("actions-menu").querySelectorAll<HTMLButtonElement>(
        ".actions-menu-item",
      ),
    ].find((item) => item.textContent === "Rename…");
    if (rename === undefined) {
      openActionsMenuLabels();
    }
    const renameAction = [
      ...element("actions-menu").querySelectorAll<HTMLButtonElement>(
        ".actions-menu-item",
      ),
    ].find((item) => item.textContent === "Rename…");
    renameAction?.click();
    expect(dialog("rename-dialog").open).toBe(true);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "a" }));
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "f" }));
    expect(commandCalls().some(isSpawnAgentCommand)).toBe(false);
    expect(element("filter-menu").classList.contains("hidden")).toBe(true);
  });

  it("refuses terminal input across a native disconnect and resyncs on reconnect", async () => {
    const terminalReads: Array<(bytes: Uint8Array) => void> = [];
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>((resolve) =>
          terminalReads.push(resolve),
        );
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [agentTerminal("github-o-r-42", 7)],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(harness.terminalDataHandlers).toHaveLength(1),
    );

    terminalReads.shift()?.(new Uint8Array([0]));
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toBe(
        "terminal live",
      ),
    );
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    terminalReads.shift()?.(
      new Uint8Array([
        2,
        ...new TextEncoder().encode("read terminal stream: connection reset"),
      ]),
    );
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toContain(
        "offline",
      ),
    );

    const writesBefore = terminalCommandKinds().filter(
      (kind) => kind === 1,
    ).length;
    harness.terminalDataHandlers[0]?.("echo lost\r");
    await settle();
    expect(terminalCommandKinds().filter((kind) => kind === 1)).toHaveLength(
      writesBefore,
    );
    expect(element("terminal-stream-message").textContent).toContain(
      "was not sent",
    );

    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    terminalReads.shift()?.(new Uint8Array([0]));
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toContain(
        "recovered",
      ),
    );
    expect(terminalCommandKinds()).toContain(3);

    harness.terminalDataHandlers[0]?.("echo sent\r");
    await vi.waitFor(() =>
      expect(terminalCommandKinds().filter((kind) => kind === 1)).toHaveLength(
        writesBefore + 1,
      ),
    );

    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    const resyncsBeforeMalformed = terminalCommandKinds().filter(
      (kind) => kind === 3,
    ).length;
    const malformed = new Uint8Array(5);
    malformed[0] = 1;
    new DataView(malformed.buffer).setUint32(1, 2049);
    terminalReads.shift()?.(malformed);
    await vi.waitFor(() =>
      expect(element("status-message").textContent).toContain(
        "Malformed terminal data",
      ),
    );
    expect(terminalCommandKinds().filter((kind) => kind === 3)).toHaveLength(
      resyncsBeforeMalformed + 1,
    );

    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    terminalReads.shift()?.(
      nativeTerminalData(serverFrame(3, 7, 0, 9, "fresh replay\r\n")),
    );
    await vi.waitFor(() =>
      expect(harness.terminalWrites).toContain("fresh replay\r\n"),
    );
  });

  it("refuses input quietly before the first connection without a lost-input notice", async () => {
    const terminalReads: Array<(bytes: Uint8Array) => void> = [];
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>((resolve) =>
          terminalReads.push(resolve),
        );
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [agentTerminal("github-o-r-42", 7)],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(harness.terminalDataHandlers).toHaveLength(1),
    );

    // The terminal is mounted but the terminal stream has not sent its
    // first Reset yet: typing must be refused *quietly*, not with the
    // scary "will not be replayed" lost-input notice.
    harness.terminalDataHandlers[0]?.("echo early\r");
    await settle();
    expect(terminalCommandKinds().filter((kind) => kind === 1)).toHaveLength(0);
    expect(element("terminal-stream-notice").hidden).toBe(true);
    expect(element("terminal-stream-message").textContent).toBe("");
    expect(element("status-message").textContent).toContain("connecting");
  });

  it("recovers without claiming dropped input when none was attempted", async () => {
    const terminalReads: Array<(bytes: Uint8Array) => void> = [];
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>((resolve) =>
          terminalReads.push(resolve),
        );
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [agentTerminal("github-o-r-42", 7)],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(harness.terminalDataHandlers).toHaveLength(1),
    );

    terminalReads.shift()?.(new Uint8Array([0]));
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toBe(
        "terminal live",
      ),
    );
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    // Drop and reconnect without ever typing while offline.
    terminalReads.shift()?.(
      new Uint8Array([2, ...new TextEncoder().encode("connection reset")]),
    );
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toContain(
        "offline",
      ),
    );
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    terminalReads.shift()?.(new Uint8Array([0]));
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toContain(
        "recovered",
      ),
    );
    expect(element("terminal-stream-message").textContent).not.toContain(
      "was not sent",
    );
  });

  it("keeps sending input when the daemon advertises a bad write limit", async () => {
    const terminalReads: Array<(bytes: Uint8Array) => void> = [];
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          // A non-positive limit would make appendTerminalInput throw inside
          // the onData callback and break every keystroke without the clamp.
          max_terminal_write_bytes: 0,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>((resolve) =>
          terminalReads.push(resolve),
        );
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [agentTerminal("github-o-r-42", 7)],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(harness.terminalDataHandlers).toHaveLength(1),
    );

    terminalReads.shift()?.(new Uint8Array([0]));
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toBe(
        "terminal live",
      ),
    );
    const writesBefore = terminalCommandKinds().filter(
      (kind) => kind === 1,
    ).length;
    harness.terminalDataHandlers[0]?.("x");
    await vi.waitFor(() =>
      expect(terminalCommandKinds().filter((kind) => kind === 1)).toHaveLength(
        writesBefore + 1,
      ),
    );
  });

  it("stops flushing buffered input when the channel drops mid-flush", async () => {
    const terminalReads: Array<(bytes: Uint8Array) => void> = [];
    const heldSends: Array<() => void> = [];
    let holdNextSend = false;
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>((resolve) =>
          terminalReads.push(resolve),
        );
      }
      if (command === "send_terminal_frame" && holdNextSend) {
        holdNextSend = false;
        return new Promise<void>((resolve) => heldSends.push(resolve));
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [agentTerminal("github-o-r-42", 7)],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(harness.terminalDataHandlers).toHaveLength(1),
    );

    terminalReads.shift()?.(new Uint8Array([0]));
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toBe(
        "terminal live",
      ),
    );

    // Two distinct intents buffer as two entries in one flush batch; hold
    // the first write frame in flight so the disconnect lands between them.
    holdNextSend = true;
    harness.terminalDataHandlers[0]?.("ab");
    harness.terminalDataHandlers[0]?.("\r");
    await vi.waitFor(() => expect(heldSends.length).toBe(1));
    const writesInFlight = terminalCommandKinds().filter(
      (kind) => kind === 1,
    ).length;

    await vi.waitFor(() => expect(terminalReads.length).toBe(1));
    terminalReads.shift()?.(
      new Uint8Array([2, ...new TextEncoder().encode("connection reset")]),
    );
    await vi.waitFor(() =>
      expect(element("terminal-stream-health").textContent).toContain(
        "offline",
      ),
    );

    // Releasing the in-flight frame must not let the second buffered entry
    // go out after the channel is known dead.
    heldSends[0]?.();
    await settle();
    expect(terminalCommandKinds().filter((kind) => kind === 1)).toHaveLength(
      writesInFlight,
    );
  });

  it("sends automation, snooze, sync, and notes commands from the detail pane", async () => {
    mockAutomationHarness();

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [pr(42)], terminals: [] } },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );

    // Controls reflect the workspace's persisted automation state.
    expect(input("#auto-merge-toggle").checked).toBe(false);
    expect(select("auto-fix-ci-select").value).toBe("Default");
    expect(button("unsnooze-button").classList.contains("hidden")).toBe(true);
    expect(button("sync-button").disabled).toBe(false);
    // Track-main can't apply to a PR branch, so it's disabled here.
    expect(input("#track-main-toggle").disabled).toBe(true);

    const autoMerge = input("#auto-merge-toggle");
    autoMerge.checked = true;
    autoMerge.dispatchEvent(new Event("change", { bubbles: true }));

    const ci = select("auto-fix-ci-select");
    ci.value = "Arm";
    ci.dispatchEvent(new Event("change", { bubbles: true }));

    select("snooze-select").value = "0";
    button("snooze-button").click();
    button("sync-button").click();

    input("#notes-body").value = "flaky job";
    form("notes-form").dispatchEvent(submitEvent());

    await vi.waitFor(() => {
      expect(commandCalls()).toContainEqual({
        SetAutoMergeOnGreen: { session_key: "github-o-r-42", enabled: true },
      });
      expect(commandCalls()).toContainEqual({
        SetAutoFixPolicies: {
          session_key: "github-o-r-42",
          ci: "Arm",
          conflict: "Default",
        },
      });
      expect(commandCalls()).toContainEqual({
        SyncWorkspace: { session_key: "github-o-r-42" },
      });
      expect(commandCalls()).toContainEqual({
        SetNotes: { session_key: "github-o-r-42", notes: "flaky job" },
      });
    });

    const snooze = commandCalls().find(
      (
        command,
      ): command is { Snooze: { session_key: string; until: string } } =>
        typeof command === "object" && command !== null && "Snooze" in command,
    );
    expect(snooze?.Snooze.session_key).toBe("github-o-r-42");
    expect(Number.isNaN(Date.parse(snooze?.Snooze.until ?? ""))).toBe(false);
  });

  it("shows notes edited elsewhere instead of pinning an untouched draft", async () => {
    mockAutomationHarness();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    const withNotes = pr(42);
    withNotes.notes = "saved locally";
    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [withNotes, pr(43)], terminals: [] } },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42", "github-o-r-43"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );
    expect(notesField().value).toBe("saved locally");

    // Leave #42 WITHOUT editing its notes.
    selectWorkspaceRow("PR o/r#43");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#43"),
    );

    // Another client edits #42's notes; the daemon broadcasts the change.
    const edited = pr(42);
    edited.notes = "edited in the TUI";
    eventChannel().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: edited },
    });

    // Returning shows the upstream value, not a stale pinned draft.
    selectWorkspaceRow("PR o/r#42");
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );
    expect(notesField().value).toBe("edited in the TUI");
  });

  it("keeps a policy change optimistic when an unrelated upsert interleaves", async () => {
    mockAutomationHarness();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [pr(42)], terminals: [] } },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );

    // Arm auto-merge and auto-fix CI (both in flight, no echo yet).
    const autoMerge = input("#auto-merge-toggle");
    autoMerge.checked = true;
    autoMerge.dispatchEvent(new Event("change", { bubbles: true }));
    const ci = select("auto-fix-ci-select");
    ci.value = "Arm";
    ci.dispatchEvent(new Event("change", { bubbles: true }));
    await vi.waitFor(() =>
      expect(
        commandCalls().some(
          (c) =>
            typeof c === "object" && c !== null && "SetAutoFixPolicies" in c,
        ),
      ).toBe(true),
    );

    // An unrelated poll re-broadcasts the workspace with the OLD (unpersisted)
    // automation state but a changed title, proving a render ran.
    const stale = pr(42);
    (stale.pr as { title: string }).title = "Poll refresh";
    eventChannel().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: stale },
    });
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("Poll refresh"),
    );

    // The in-flight controls must NOT have been reverted to committed state.
    expect(input("#auto-merge-toggle").checked).toBe(true);
    expect(select("auto-fix-ci-select").value).toBe("Arm");

    // Changing the other auto-fix arm must carry the still-optimistic CI arm,
    // not a value the interleaving upsert clobbered back to Default.
    const conflict = select("auto-fix-conflict-select");
    conflict.value = "Arm";
    conflict.dispatchEvent(new Event("change", { bubbles: true }));
    await vi.waitFor(() =>
      expect(lastAutoFixCommand()).toEqual({
        SetAutoFixPolicies: {
          session_key: "github-o-r-42",
          ci: "Arm",
          conflict: "Arm",
        },
      }),
    );
  });

  it("gates PR-only automation controls and enables track-main on a scratch workspace", async () => {
    mockAutomationHarness();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    // A GitHub, repo-scoped, no-PR workspace (issue tracked, scratch branch).
    const scratch = pr(42);
    scratch.gh_issues = [scratch.pr];
    scratch.pr = null;
    scratch.project_key = "github-o-r";
    scratch.linked_checkout = null;
    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [scratch], terminals: [] } },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );

    // Auto-merge / auto-fix target a PR — disabled here.
    expect(input("#auto-merge-toggle").disabled).toBe(true);
    expect(select("auto-fix-ci-select").disabled).toBe(true);
    expect(select("auto-fix-conflict-select").disabled).toBe(true);
    // Track-main applies to a no-PR GitHub worktree — enabled, and sends.
    expect(input("#track-main-toggle").disabled).toBe(false);
    const trackMain = input("#track-main-toggle");
    trackMain.checked = true;
    trackMain.dispatchEvent(new Event("change", { bubbles: true }));
    await vi.waitFor(() =>
      expect(commandCalls()).toContainEqual({
        SetTrackMain: { session_key: "github-o-r-42", enabled: true },
      }),
    );
    // Sync targets the upstream item (the issue) — enabled.
    expect(button("sync-button").disabled).toBe(false);
  });

  it("attaches the replacement terminal when the selected workspace is removed", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    // Two workspaces, each with its own live agent terminal.
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42), pr(43)],
          terminals: [
            agentTerminal("github-o-r-42", 7),
            agentTerminal("github-o-r-43", 8),
          ],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42", "github-o-r-43"]));

    // Initial auto-selection attaches the first workspace's terminal.
    await vi.waitFor(() =>
      expect(element("terminal-title").textContent).toContain("github-o-r-42"),
    );

    // Remove the selected workspace; the recomputed view lists only the
    // second (the daemon sends the removal Frame before the Inbox).
    eventChannel().onmessage({
      type: "Frame",
      payload: { WorkspaceRemoved: "github-o-r-42" },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-43"]));

    // The replacement is auto-selected AND its terminal is attached —
    // the pane must not stay pinned to the removed workspace's terminal.
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#43"),
    );
    expect(element("terminal-title").textContent).toContain("github-o-r-43");
  });

  it("totals unread only over workspaces the view shows, not the raw map", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    // Both workspaces reach the frontend map (from the snapshot), each
    // with unread activity — but the recomputed view lists only #42, as
    // if #43 were filtered out of this mailbox (e.g. inactive).
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [withUnread(pr(42), 3), withUnread(pr(43), 5)],
          terminals: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));

    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(1),
    );
    // Header counts #42's 3 unread only — never 8 (which would leak the
    // hidden #43's unread into the shown total).
    expect(element("unread-count").textContent).toBe("3 unread");
  });

  it("renders the filter menu and delegates filter/search to the daemon (#733)", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42), pr(43)],
          terminals: [],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(
      inboxMessage(["github-o-r-42", "github-o-r-43"], filterMenuFixture()),
    );
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );

    // The menu is built from the shared view-model, grouped by axis with
    // live counts — never a hardcoded predicate list.
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "f" }));
    await vi.waitFor(() =>
      expect(element("filter-menu").classList.contains("hidden")).toBe(false),
    );
    expect(
      document.querySelectorAll("#filter-menu .filter-section").length,
    ).toBe(3); // State / Role / Kind
    expect(document.querySelectorAll("#filter-menu .filter-row").length).toBe(
      4,
    );

    // A single toggle delegates just that predicate to the daemon; the
    // optimistic chip shows immediately.
    toggleFilterRow("PR");
    await vi.waitFor(() => expect(lastFilterCall()).toEqual(["Pr"]));
    expect(element("filter-chips").textContent).toContain("PR");

    // A second toggle with no intervening view push must COMPOSE — a
    // view-derived active set would drop the first.
    toggleFilterRow("author");
    await vi.waitFor(() => expect(lastFilterCall()).toEqual(["Pr", "Author"]));
    expect(element("filter-button").textContent).toBe("Filter (2)");

    // `f` is a toggle: a second press closes the menu.
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "f" }));
    expect(element("filter-menu").classList.contains("hidden")).toBe(true);

    // Search delegates to the daemon's global search.
    const search = input("#inbox-search");
    search.value = "widget";
    search.dispatchEvent(new Event("input", { bubbles: true }));
    await vi.waitFor(() =>
      expect(harness.invoke).toHaveBeenCalledWith("set_search", {
        query: "widget",
        generation: 1,
        controllerId: expect.any(String),
      }),
    );
  });

  it("disables New workspace when no repository is available", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:acme"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(harness.invoke).toHaveBeenCalledWith("desktop_info"),
    );
    await vi.waitFor(() =>
      expect(button("new-workspace-button").disabled).toBe(true),
    );
    button("new-workspace-button").click();
    expect(dialog("new-workspace-dialog").open).toBe(false);
  });

  it("sends only one CreateWorkspace when submitted twice in flight", async () => {
    let releaseCreate: (() => void) | undefined;

    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:acme/widget"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [
            { project_key: "github-acme-widget", label: "acme/widget" },
          ],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      if (command === "send_command") {
        const payload = args as { command?: { CreateWorkspace?: unknown } };
        if (payload.command?.CreateWorkspace !== undefined) {
          return new Promise<void>((resolve) => {
            releaseCreate = resolve;
          });
        }
        return Promise.resolve();
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(button("new-workspace-button").disabled).toBe(false),
    );

    button("new-workspace-button").click();
    expect(dialog("new-workspace-dialog").open).toBe(true);
    input("#new-workspace-name").value = "scratch space";
    form("new-workspace-form").dispatchEvent(submitEvent());
    form("new-workspace-form").dispatchEvent(submitEvent());

    await vi.waitFor(() => expect(createCommands().length).toBe(1));
    // Let any erroneous second dispatch settle before asserting it stayed at one.
    await Promise.resolve();
    await Promise.resolve();
    expect(createCommands()).toHaveLength(1);
    expect(dialog("new-workspace-dialog").open).toBe(true);

    releaseCreate?.();
  });

  it("opens the snippet picker over the focused terminal and delivers a pick", async () => {
    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:acme/widget"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [
            { project_key: "github-acme-widget", label: "acme/widget" },
          ],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      if (command === "snippet_view") {
        return Promise.resolve(
          snippetView((args as { filter: string }).filter),
        );
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    // No terminal yet → the snippet button is inert.
    expect(button("snippet-button").disabled).toBe(true);

    // Attach an agent terminal to the selected workspace, so ⌘/Ctrl-J has
    // a target.
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [
            {
              terminal_id: 7,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
          ],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(button("snippet-button").disabled).toBe(false),
    );

    // Open via the keyboard shortcut and pick a row → it delivers to the
    // focused terminal id.
    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: "j", metaKey: true }),
    );
    await vi.waitFor(() => expect(dialog("snippet-dialog").open).toBe(true));
    const rows = document.querySelectorAll<HTMLButtonElement>(".snippet-row");
    expect(rows.length).toBe(2);
    rows[0]?.click();
    await vi.waitFor(() =>
      expect(deliverCommands()).toContainEqual({
        terminal_id: 7,
        snippet_key: "rev",
        category: "Review",
        body: "Review the current diff.",
      }),
    );
    await vi.waitFor(() => expect(dialog("snippet-dialog").open).toBe(false));

    // Typing a full unique key auto-submits without a click (parity with
    // the TUI `]]srev` fast path).
    window.dispatchEvent(
      new KeyboardEvent("keydown", { key: "j", metaKey: true }),
    );
    await vi.waitFor(() => expect(dialog("snippet-dialog").open).toBe(true));
    const filter = input("#snippet-filter");
    filter.value = "rev";
    filter.dispatchEvent(new Event("input", { bubbles: true }));
    await vi.waitFor(() =>
      expect(
        deliverCommands().filter((c) => c.snippet_key === "rev"),
      ).toHaveLength(2),
    );
    expect(dialog("snippet-dialog").open).toBe(false);
  });

  it("labels a task-less workspace with a friendly name in the picker", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(settingsStateFixture());
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: { workspaces: [taskless("local-scratch")], terminals: [] },
      },
    });
    await vi.waitFor(() =>
      expect(button("new-workspace-button").disabled).toBe(false),
    );

    button("new-workspace-button").click();
    const options = [...select("new-workspace-project").options].map(
      (option) => option.textContent,
    );
    expect(options).toContain("scratch");
    expect(options).not.toContain("local-scratch");
  });

  it("renders theme / agent / workspace settings and saves the full payload", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({
            selected_scopes: ["github:acme/widget"],
            default_agent: "claude",
            agents: [
              {
                id: "claude",
                label: "Claude Code",
                available: true,
                models: [
                  { alias: "S", label: "Haiku" },
                  { alias: "M", label: "Sonnet" },
                  { alias: "L", label: "Opus" },
                ],
                default_tier: "L",
              },
            ],
            theme: "Lazybox Dark",
            themes: [
              { name: "Lazybox Dark", colors: darkColors() },
              { name: "Lazybox Light", colors: lightColors() },
            ],
            keymap_preset: "vim",
          }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["claude"],
          default_agent: "claude",
          repositories: [
            { project_key: "github-acme-widget", label: "acme/widget" },
          ],
        });
      }
      if (command === "github_auth_status") {
        return Promise.resolve({
          authenticated: true,
          account: "fixture",
          message: "GitHub credential verified",
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      // Theme / layout / model changes need no restart.
      if (command === "save_desktop_settings") {
        return Promise.resolve(false);
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(button("settings-button").disabled).toBe(false),
    );

    button("settings-button").click();
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(true));

    // Theme catalog and model menu seed from config.
    expect(document.querySelectorAll(".theme-swatch")).toHaveLength(2);
    expect(element("keymap-preset-label").textContent).toBe("Keymap: vim");
    expect(
      [...select("default-model-select").options].map((o) => o.value),
    ).toEqual(["S", "M", "L"]);
    expect(select("default-model-select").value).toBe("L");

    themeSwatch("Lazybox Light").click();
    setSelect("default-model-select", "M");

    form("setup-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();

    await vi.waitFor(() =>
      expect(settingsCalls().at(-1)).toEqual(
        savePayload({
          github_scopes: ["github:acme/widget"],
          default_agent: "claude",
          theme: "Lazybox Light",
          default_model_tier: "M",
        }),
      ),
    );
    // No restart requested → the dialog closes on its own.
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(false));
  });

  it("does not persist a model tier the user never chose", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({
            selected_scopes: ["github:acme/widget"],
            default_agent: "custombot",
            agents: [
              {
                id: "custombot",
                label: "Custom Bot",
                available: true,
                // Tiers defined, but no configured default.
                models: [
                  { alias: "F", label: "Fast" },
                  { alias: "S", label: "Slow" },
                ],
                default_tier: null,
              },
            ],
          }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["custombot"],
          default_agent: "custombot",
          repositories: [
            { project_key: "github-acme-widget", label: "acme/widget" },
          ],
        });
      }
      if (command === "github_auth_status") {
        return Promise.resolve({
          authenticated: true,
          account: "fixture",
          message: "GitHub credential verified",
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      if (command === "save_desktop_settings") {
        return Promise.resolve(false);
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(button("settings-button").disabled).toBe(false),
    );

    button("settings-button").click();
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(true));

    // The menu leads with an explicit "Agent default" entry so an
    // untouched save keeps the tier unset instead of adopting the first.
    const model = select("default-model-select");
    expect([...model.options].map((o) => o.value)).toEqual(["", "F", "S"]);
    expect(model.value).toBe("");

    form("setup-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();

    await vi.waitFor(() =>
      expect(settingsCalls().at(-1)).toEqual(
        savePayload({
          github_scopes: ["github:acme/widget"],
          default_agent: "custombot",
          default_model_tier: null,
        }),
      ),
    );
  });

  it("boots an empty client from remote daemon authority without local onboarding", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 3,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          providers: ["github", "linear"],
          agents: [],
          default_agent: "remote-bot",
          repositories: [],
          settings: {},
          protocol_notice: null,
        });
      }
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({
            authority: "remote",
            providers: ["github", "linear"],
            selected_scopes: ["github:remote/widget"],
            default_agent: "remote-bot",
            agents: [
              {
                id: "remote-bot",
                label: "Remote Bot",
                available: true,
                models: [{ alias: "R", label: "Remote Large" }],
                default_tier: "R",
              },
            ],
          }),
        );
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      if (command === "save_desktop_settings") {
        return Promise.resolve(false);
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(button("settings-button").disabled).toBe(false),
    );

    expect(dialog("setup-dialog").open).toBe(false);
    expect(harness.invoke).not.toHaveBeenCalledWith("github_auth_status");
    button("settings-button").click();
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(true));

    expect(element("settings-authority").textContent).toContain(
      "github, linear",
    );
    expect(select("default-agent-select").disabled).toBe(true);
    expect(
      [...select("default-agent-select").options].map((option) => option.value),
    ).toEqual(["remote-bot"]);
    expect(
      [...select("default-model-select").options].map((option) => option.value),
    ).toEqual(["R"]);
    expect(harness.invoke).not.toHaveBeenCalledWith("github_auth_status");
    expect(harness.invoke).not.toHaveBeenCalledWith("list_github_repositories");

    input("#analytics-enabled").click();
    form("setup-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();
    await vi.waitFor(() =>
      expect(settingsCalls().at(-1)).toEqual(
        savePayload({
          github_scopes: ["github:remote/widget"],
          default_agent: "remote-bot",
          analytics_enabled: true,
        }),
      ),
    );
  });

  it("surfaces a tolerated protocol-skew notice and keeps it past a workspace warning (#815)", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
          protocol_notice:
            "daemon build 9.9.9+deadbeef differs from desktop build 0.1.9+cafef00d; update one side if anything misbehaves",
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({
          workspaces: [],
          warnings: ["a workspace row could not be decoded"],
        });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    const notice = element("protocol-notice");
    expect(notice.hidden).toBe(false);
    expect(notice.textContent).toContain("9.9.9+deadbeef");
    expect(notice.textContent).toContain("update one side");
    // The workspace warning lands on the ephemeral status line; the
    // persistent skew notice must survive it rather than be overwritten.
    expect(element("status-message").textContent).toContain(
      "could not be decoded",
    );
  });

  it("hides the protocol-skew surface when the daemon build matches (#815)", async () => {
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve(
          settingsStateFixture({ selected_scopes: ["github:o"] }),
        );
      }
      if (command === "desktop_info") {
        return Promise.resolve({
          protocol_version: 1,
          max_terminal_frame_bytes: 2048,
          max_terminal_write_bytes: 1024,
          agents: ["codex"],
          default_agent: "codex",
          repositories: [],
          protocol_notice: null,
        });
      }
      if (command === "list_workspaces") {
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      if (command === "read_terminal_data") {
        return new Promise<Uint8Array>(() => {});
      }
      return Promise.resolve();
    });

    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    expect(element("protocol-notice").hidden).toBe(true);
  });

  it("mounts a tile per workspace terminal and moves focus on tab click", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    // One workspace with two live terminals — an agent and a shell — so
    // both are visible at once without teardown (TerminalStack parity).
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [
            {
              terminal_id: 7,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
            {
              terminal_id: 8,
              session_key: "github-o-r-42",
              kind: "Shell",
              last_seq: 0,
              agent_state: null,
            },
          ],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));

    await vi.waitFor(() =>
      expect(document.querySelectorAll(".terminal-tile")).toHaveLength(2),
    );
    expect(document.querySelectorAll(".terminal-tab")).toHaveLength(2);
    // The lowest-id terminal (the agent) is focused first.
    expect(element("terminal-title").textContent).toContain("codex");
    expect(tiles()[0]?.classList.contains("focused")).toBe(true);

    // Clicking the shell tab moves focus to its tile.
    const shellTab = [
      ...document.querySelectorAll<HTMLElement>(".terminal-tab"),
    ].find((tab) => tab.textContent?.includes("shell"));
    shellTab?.click();
    expect(element("terminal-title").textContent).toContain("shell");
    expect(tiles()[1]?.classList.contains("focused")).toBe(true);
    expect(tiles()[0]?.classList.contains("focused")).toBe(false);
  });

  it("toggles focus mode with `.` and back off again", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [
            {
              terminal_id: 7,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
          ],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".terminal-tile")).toHaveLength(1),
    );

    const grid = document.querySelector(".workspace-grid")!;
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "." }));
    expect(grid.classList.contains("focus-mode")).toBe(true);
    expect(element("terminal").classList.contains("focus-only")).toBe(true);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "." }));
    expect(grid.classList.contains("focus-mode")).toBe(false);
    expect(element("terminal").classList.contains("focus-only")).toBe(false);
  });

  it("stacks activity above the terminal in a resizable right column (#936)", async () => {
    const grid = document.querySelector(".workspace-grid")!;
    const rightPane = document.querySelector<HTMLElement>(".right-pane")!;
    // Two columns — sidebar (inbox) + right pane — not three, with a draggable
    // splitter between them (#958).
    expect([...grid.children].map((el) => el.className)).toEqual([
      "inbox-panel",
      "column-splitter",
      "right-pane",
    ]);
    // The right column stacks activity above the terminal, split by the divider.
    expect([...rightPane.children].map((el) => el.className)).toEqual([
      "activity-panel",
      "right-pane-splitter",
      "terminal-panel",
    ]);

    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    const splitter = element("right-pane-splitter");
    splitter.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(splitter.classList.contains("dragging")).toBe(true);
    window.dispatchEvent(
      new MouseEvent("mousemove", { clientY: 240, buttons: 1 }),
    );
    expect(rightPane.style.getPropertyValue("--activity-height")).toMatch(
      /^\d+px$/,
    );
    window.dispatchEvent(new MouseEvent("mouseup"));
    expect(splitter.classList.contains("dragging")).toBe(false);
    localStorage.removeItem("lazybox.activityHeight");
  });

  it("restores a persisted activity height on launch (#936)", async () => {
    localStorage.setItem("lazybox.activityHeight", "300");
    const rightPane = document.querySelector<HTMLElement>(".right-pane")!;
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );
    expect(rightPane.style.getPropertyValue("--activity-height")).toBe("300px");
    localStorage.removeItem("lazybox.activityHeight");
  });

  it("ends the drag when the mouseup is missed (#936)", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    const splitter = element("right-pane-splitter");
    const rightPane = document.querySelector<HTMLElement>(".right-pane")!;
    splitter.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(splitter.classList.contains("dragging")).toBe(true);
    // A move with no button held means the mouseup was swallowed (alt-tab out
    // mid-drag) — the drag must end rather than keep resizing on hover.
    window.dispatchEvent(
      new MouseEvent("mousemove", { clientY: 200, buttons: 0 }),
    );
    expect(splitter.classList.contains("dragging")).toBe(false);
    rightPane.style.removeProperty("--activity-height");
    window.dispatchEvent(
      new MouseEvent("mousemove", { clientY: 400, buttons: 1 }),
    );
    expect(rightPane.style.getPropertyValue("--activity-height")).toBe("");
    localStorage.removeItem("lazybox.activityHeight");
  });

  it("re-focusing the already-focused tile does no layout/resize work", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [
            {
              terminal_id: 7,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
            {
              terminal_id: 8,
              session_key: "github-o-r-42",
              kind: "Shell",
              last_seq: 0,
              agent_state: null,
            },
          ],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".terminal-tile")).toHaveLength(2),
    );
    // Let the initial layout's debounced resize settle.
    await settle();
    const before = terminalFrameCount();

    // A mousedown inside the already-focused (agent) tile must not relayout —
    // reflowing the DOM here would collapse an in-progress text selection.
    tiles()[0]?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    await settle();
    expect(terminalFrameCount()).toBe(before);

    // Focusing a different tile is a real change, so it does lay out + resize.
    tiles()[1]?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    await settle();
    expect(terminalFrameCount()).toBeGreaterThan(before);
  });

  it("renders each tab as a role=tab div with a non-focusable close affordance", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [
            {
              terminal_id: 7,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
          ],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(document.querySelector(".terminal-tab")).not.toBeNull(),
    );

    const tab = document.querySelector<HTMLElement>(".terminal-tab")!;
    expect(tab.tagName).toBe("DIV");
    expect(tab.getAttribute("role")).toBe("tab");
    // The close affordance lives inside the tab but is NOT a nested interactive
    // control: it is a non-focusable, aria-hidden span, so the tab is the only
    // focus stop (WCAG "nested-interactive").
    const close = tab.querySelector<HTMLElement>(".terminal-tab-close")!;
    expect(close.tagName).toBe("SPAN");
    expect(close.getAttribute("aria-hidden")).toBe("true");
    expect(close.hasAttribute("tabindex")).toBe(false);
    // The Delete close shortcut is announced to assistive tech.
    expect(tab.getAttribute("aria-keyshortcuts")).toBe("Delete");

    // Close works by mouse click on the affordance…
    const beforeClick = terminalFrameCount();
    close.click();
    await settle();
    expect(terminalFrameCount()).toBeGreaterThan(beforeClick);

    // …and by keyboard: Delete on the focused tab closes it too.
    const beforeKey = terminalFrameCount();
    tab.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Delete", bubbles: true }),
    );
    await settle();
    expect(terminalFrameCount()).toBeGreaterThan(beforeKey);

    // Backspace must NOT close: it's a habitual back/delete keystroke and
    // closing kills a running agent's PTY with no confirmation.
    const beforeBackspace = terminalFrameCount();
    tab.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Backspace", bubbles: true }),
    );
    await settle();
    expect(terminalFrameCount()).toBe(beforeBackspace);
  });

  it("defaults focus to a live terminal, not a lower-id exited one", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    // Lowest id (7) is an exited agent; the live agent is id 8.
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [
            {
              terminal_id: 7,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: { Exited: { code: 0 } },
            },
            {
              terminal_id: 8,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
          ],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".terminal-tile")).toHaveLength(2),
    );

    // Focus lands on the running terminal (id 8), not the exited id 7
    // (which `wanted[0]` would have picked before the fix).
    expect(
      document.querySelector<HTMLElement>(".terminal-tile.focused")?.dataset
        .terminalId,
    ).toBe("8");
  });

  it("switches workspace (sending FocusWorkspace) on a cross-workspace focus request", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42), pr(43)],
          terminals: [
            {
              terminal_id: 7,
              session_key: "github-o-r-42",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
            {
              terminal_id: 8,
              session_key: "github-o-r-43",
              kind: { Agent: "codex" },
              last_seq: 0,
              agent_state: "Working",
            },
          ],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42", "github-o-r-43"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );

    // The daemon asks to focus #43's terminal (spawn-of-existing-singleton).
    eventChannel().onmessage({
      type: "Frame",
      payload: { TerminalFocusRequested: { terminal_id: 8 } },
    });

    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#43"),
    );
    expect(
      commandCalls().some(
        (command) =>
          typeof command === "object" &&
          command !== null &&
          "FocusWorkspace" in command &&
          (command as { FocusWorkspace: { session_key: string } })
            .FocusWorkspace.session_key === "github-o-r-43",
      ),
    ).toBe(true);
  });

  it("rebuilds only the bursting row, keeping a sibling focused and scrolled during an agent-state burst", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42), pr(43)],
          terminals: [agentTerminal("github-o-r-42", 7)],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage({
      ...inboxMessage(["github-o-r-42", "github-o-r-43"]),
      payload: {
        ...(inboxMessage(["github-o-r-42", "github-o-r-43"]).payload as object),
        revision: 1,
      },
    });
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(2),
    );

    const list = element("workspace-list");
    // Focus the sibling whose agent state does NOT change during the burst;
    // only workspace 42's live agent badge (running ⇄ ready) is churning.
    const sibling = document.querySelector<HTMLButtonElement>(
      '[data-key="github-o-r-43"]',
    )!;
    sibling.focus();
    list.scrollTop = 41;
    for (let index = 0; index < 10; index += 1) {
      eventChannel().onmessage({
        type: "Frame",
        payload: {
          AgentState: {
            session_key: "github-o-r-42",
            terminal_id: 7,
            state: index % 2 === 0 ? "Working" : "Idle",
          },
        },
      });
    }

    // The keyed reconciler rebuilds only workspace 42's row (its badge
    // changed); the untouched sibling keeps its node identity, keyboard
    // focus, and the list scroll position across the whole burst.
    expect(document.querySelector('[data-key="github-o-r-43"]')).toBe(sibling);
    expect(document.activeElement).toBe(sibling);
    expect(list.scrollTop).toBe(41);
  });

  it("does not rebuild an unchanged inbox row when a sibling row updates (#974)", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42), pr(43)],
          terminals: [],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42", "github-o-r-43"]));
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(2),
    );

    const rowA = document.querySelector<HTMLButtonElement>(
      '[data-key="github-o-r-42"]',
    )!;
    const titleA = rowA.querySelector(".workspace-row-title");

    // Change only row B, which re-renders the whole inbox.
    const updatedB = pr(43);
    (updatedB.pr as { title: string }).title = "changed B title";
    eventChannel().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: updatedB },
    });
    await vi.waitFor(() =>
      expect(
        document.querySelector(
          '[data-key="github-o-r-43"] .workspace-row-title',
        )?.textContent,
      ).toBe("changed B title"),
    );

    // Row A's element AND its inner nodes must be the very same objects — an
    // unchanged row is not rebuilt.
    expect(document.querySelector('[data-key="github-o-r-42"]')).toBe(rowA);
    expect(rowA.querySelector(".workspace-row-title")).toBe(titleA);
  });

  it("drops a lower-revision inbox view arriving out of order (#974)", async () => {
    mockDaemon();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42), pr(43)],
          terminals: [],
          recent_snippets: [],
        },
      },
    });
    const withRevision = (keys: string[], revision: number) => ({
      ...inboxMessage(keys),
      payload: { ...(inboxMessage(keys).payload as object), revision },
    });
    const order = (): Array<string | undefined> =>
      [...document.querySelectorAll<HTMLElement>(".workspace-row")].map(
        (row) => row.dataset.key,
      );

    eventChannel().onmessage(
      withRevision(["github-o-r-42", "github-o-r-43"], 5),
    );
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(2),
    );
    expect(order()).toEqual(["github-o-r-42", "github-o-r-43"]);

    // A stale (lower-revision) push that reorders the rows must be ignored.
    eventChannel().onmessage(
      withRevision(["github-o-r-43", "github-o-r-42"], 3),
    );
    expect(order()).toEqual(["github-o-r-42", "github-o-r-43"]);

    // A genuinely newer revision is applied.
    eventChannel().onmessage(
      withRevision(["github-o-r-43", "github-o-r-42"], 6),
    );
    await vi.waitFor(() =>
      expect(order()).toEqual(["github-o-r-43", "github-o-r-42"]),
    );
  });

  it("does not let a deferred refresh overwrite a newer workspace event", async () => {
    let listCalls = 0;
    let resolveRefresh: ((value: unknown) => void) | undefined;
    mockDaemon();
    const baseImplementation = harness.invoke.getMockImplementation()!;
    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "list_workspaces" && ++listCalls === 2) {
        return new Promise((resolve) => {
          resolveRefresh = resolve;
        });
      }
      return baseImplementation(command, args);
    });
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );

    button("refresh-button").click();
    await vi.waitFor(() => expect(resolveRefresh).toBeTypeOf("function"));
    const newest = pr(42);
    (newest.pr as { title: string }).title = "new streamed title";
    eventChannel().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: newest },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    resolveRefresh?.({ workspaces: [pr(43)], warnings: [] });

    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("new streamed title"),
    );
    expect(element("workspace-list").textContent).not.toContain("PR o/r#43");
  });

  it("preserves a dirty note when a remote edit arrives and marks the conflict", async () => {
    mockAutomationHarness();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );
    const workspace = pr(42);
    workspace.notes = "server base";
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [workspace],
          terminals: [],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() => expect(notesField().value).toBe("server base"));

    notesField().value = "local draft";
    notesField().dispatchEvent(new Event("input", { bubbles: true }));
    const remote = pr(42);
    remote.notes = "remote edit";
    eventChannel().onmessage({
      type: "Frame",
      payload: { WorkspaceUpserted: remote },
    });

    expect(notesField().value).toBe("local draft");
    expect(notesField().dataset.remoteConflict).toBe("true");
    expect(element("status-message").textContent).toContain("changed remotely");
  });

  it("rolls back a rejected policy and retains the outcome in messages", async () => {
    mockAutomationHarness();
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: { workspaces: [pr(42)], terminals: [], recent_snippets: [] },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );

    const toggle = input("#auto-merge-toggle");
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change", { bubbles: true }));
    await vi.waitFor(() => expect(toggle.checked).toBe(true));
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        CommandRejected: {
          command: "SetAutoMergeOnGreen",
          message: "queue full",
        },
      },
    });

    expect(toggle.checked).toBe(false);
    button("messages-button").click();
    expect(element("messages-list").textContent).toContain(
      "auto_merge · rejected",
    );
    expect(element("messages-list").textContent).toContain("queue full");
  });

  it("drops a stale snippet result before it can render or auto-submit", async () => {
    const deferred = new Map<string, (value: unknown) => void>();
    mockDaemon();
    const baseImplementation = harness.invoke.getMockImplementation()!;
    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "snippet_view") {
        const filter = (args as { filter: string }).filter;
        if (filter === "") {
          return Promise.resolve(snippetView(filter));
        }
        return new Promise((resolve) => deferred.set(filter, resolve));
      }
      return baseImplementation(command, args);
    });
    vi.resetModules();
    await import("./main").then(({ init }) => init(document));
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );
    eventChannel().onmessage({
      type: "Frame",
      payload: {
        Snapshot: {
          workspaces: [pr(42)],
          terminals: [agentTerminal("github-o-r-42", 7)],
          recent_snippets: [],
        },
      },
    });
    eventChannel().onmessage(inboxMessage(["github-o-r-42"]));
    await vi.waitFor(() =>
      expect(button("snippet-button").disabled).toBe(false),
    );
    button("snippet-button").click();
    await vi.waitFor(() => expect(dialog("snippet-dialog").open).toBe(true));

    const filter = input("#snippet-filter");
    filter.value = "rev";
    filter.dispatchEvent(new Event("input", { bubbles: true }));
    filter.value = "pr";
    filter.dispatchEvent(new Event("input", { bubbles: true }));
    deferred.get("pr")?.(snippetView("pr"));
    await vi.waitFor(() =>
      expect(element("snippet-list").textContent).toContain("Open a PR"),
    );
    deferred.get("rev")?.(snippetView("rev"));
    await Promise.resolve();
    await Promise.resolve();

    expect(dialog("snippet-dialog").open).toBe(true);
    expect(element("snippet-list").textContent).toContain("Open a PR");
    expect(deliverCommands()).toHaveLength(0);
  });
});

// Count the terminal frames the frontend has sent — resize frames land
// here, so a spurious relayout shows up as extra `send_terminal_frame`s.
function terminalFrameCount(): number {
  return harness.invoke.mock.calls.filter(
    ([command]) => command === "send_terminal_frame",
  ).length;
}

function terminalCommandKinds(): number[] {
  return harness.invoke.mock.calls
    .filter(([command]) => command === "send_terminal_frame")
    .map(([, frame]) => (frame as Uint8Array)[4])
    .filter((kind): kind is number => kind !== undefined);
}

function isSpawnAgentCommand(command: unknown): boolean {
  return (
    typeof command === "object" && command !== null && "SpawnAgent" in command
  );
}

// Wait out the 80ms debounced resize plus any queued microtasks.
function settle(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 120));
}

// A minimal live daemon: authenticated, one repo scope, empty inbox until
// the test pushes a Snapshot.
function mockDaemon(): void {
  harness.invoke.mockImplementation((command: string) => {
    if (command === "desktop_setup_state") {
      return Promise.resolve(
        settingsStateFixture({ selected_scopes: ["github:o"] }),
      );
    }
    if (command === "desktop_info") {
      return Promise.resolve({
        protocol_version: 1,
        max_terminal_frame_bytes: 2048,
        max_terminal_write_bytes: 1024,
        agents: ["codex"],
        default_agent: "codex",
        repositories: [],
      });
    }
    if (command === "list_workspaces") {
      return Promise.resolve({ workspaces: [], warnings: [] });
    }
    if (command === "read_terminal_data") {
      return new Promise<Uint8Array>(() => {});
    }
    return Promise.resolve();
  });
}

function tiles(): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>(".terminal-tile")];
}

// Stand-in for the grouped view `src-tauri` computes and pushes. In
// production the shared tui-core logic orders these rows; the test only
// needs a valid structure listing the given workspace keys so the thin
// renderer draws their rows.
function inboxMessage(
  keys: string[],
  filterMenu: unknown[] = [],
  filterChips: string[] = [],
): Record<string, unknown> {
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
      filter_menu: filterMenu,
      filter_chips: filterChips,
    },
  };
}

// A minimal shared-shape filter menu (#733) so the desktop renders its
// grouped menu; the counts are fixture data.
function filterMenuFixture(): unknown[] {
  return [
    {
      filter: "Unread",
      axis: "State",
      label: "unread",
      count: 0,
      active: false,
    },
    {
      filter: "Author",
      axis: "Role",
      label: "author",
      count: 2,
      active: false,
    },
    { filter: "Pr", axis: "Kind", label: "PR", count: 2, active: false },
    { filter: "Issue", axis: "Kind", label: "issue", count: 0, active: false },
  ];
}

function toggleFilterRow(label: string): void {
  const row = [...document.querySelectorAll(".filter-row")].find(
    (candidate) =>
      candidate.querySelector(".filter-row-label")?.textContent === label,
  );
  row
    ?.querySelector("input")
    ?.dispatchEvent(new Event("change", { bubbles: true }));
}

function lastFilterCall(): unknown {
  const call = harness.invoke.mock.calls
    .filter(([command]) => command === "set_filters")
    .at(-1);
  return (call?.[1] as { filters: unknown } | undefined)?.filters;
}

// A live agent terminal for `sessionKey`, cloned from the contract
// fixture so its shape stays current. `terminalForWorkspace` matches on
// `session_key`, so the key must equal the workspace key.
function agentTerminal(
  sessionKey: string,
  terminalId: number,
): Record<string, unknown> {
  const template = structuredClone(fixture.events[0]) as {
    Snapshot: { terminals: Array<Record<string, unknown>> };
  };
  const terminal = template.Snapshot.terminals[0];
  if (terminal === undefined) {
    throw new Error("fixture snapshot is missing a template terminal");
  }
  terminal.session_key = sessionKey;
  terminal.terminal_id = terminalId;
  return terminal;
}

// Give a workspace `count` unread activity items. `unreadCount` reads
// only `activity.length` against `seen_count`/`read_indices`, so opaque
// placeholders are enough to move the count.
function withUnread(
  workspace: Record<string, unknown>,
  count: number,
): Record<string, unknown> {
  workspace.activity = Array.from({ length: count }, () => ({}));
  workspace.seen_count = 0;
  workspace.read_indices = [];
  return workspace;
}

function pr(number: number): Record<string, unknown> {
  const template = structuredClone(fixture.events[0]) as {
    Snapshot: { workspaces: Array<Record<string, unknown>> };
  };
  const workspace = template.Snapshot.workspaces[0];
  if (workspace === undefined) {
    throw new Error("fixture snapshot is missing a template workspace");
  }
  workspace.key = `github-o-r-${number}`;
  workspace.branch = `github-o-r-${number}`;
  workspace.name = `PR o/r#${number}`;
  const task = workspace.pr as {
    id: { key: string };
    title: string;
    url: string;
  };
  task.id.key = `o/r#${number}`;
  task.title = `PR o/r#${number}`;
  task.url = `https://github.com/o/r/pull/${number}`;
  return workspace;
}

function taskless(projectKey: string): Record<string, unknown> {
  const workspace = pr(1);
  workspace.key = "scratch";
  workspace.branch = "scratch";
  workspace.name = "Scratch";
  workspace.project_key = projectKey;
  workspace.local = true;
  workspace.pr = null;
  workspace.gh_issues = [];
  workspace.linear_issues = [];
  return workspace;
}

// Open the workspace actions menu and return its item labels. Clicking
// the button re-renders the menu from the current selection each time.
function openActionsMenuLabels(): string[] {
  const menu = element("actions-menu");
  if (menu.classList.contains("hidden")) {
    button("actions-button").click();
  }
  return [
    ...menu.querySelectorAll<HTMLButtonElement>(".actions-menu-item"),
  ].map((item) => item.textContent ?? "");
}

function selectWorkspaceRow(title: string): void {
  const row = [
    ...document.querySelectorAll<HTMLButtonElement>(".workspace-row"),
  ].find((candidate) => candidate.textContent?.includes(title));
  if (row === undefined) {
    throw new Error(`missing workspace row for ${title}`);
  }
  row.click();
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

function element(id: string): HTMLElement {
  const value = document.getElementById(id);
  if (value === null) {
    throw new Error(`missing #${id}`);
  }
  return value;
}

function button(id: string): HTMLButtonElement {
  return element(id) as HTMLButtonElement;
}

function dialog(id: string): HTMLDialogElement {
  return element(id) as HTMLDialogElement;
}

function form(id: string): HTMLFormElement {
  return element(id) as HTMLFormElement;
}

function input(selector: string): HTMLInputElement {
  const value = document.querySelector<HTMLInputElement>(selector);
  if (value === null) {
    throw new Error(`missing ${selector}`);
  }
  return value;
}

function select(id: string): HTMLSelectElement {
  return element(id) as HTMLSelectElement;
}

function submitEvent(): SubmitEvent {
  return new SubmitEvent("submit", { bubbles: true, cancelable: true });
}

function eventChannel(): { onmessage: (message: unknown) => void } {
  const channel = harness.channels.at(-1);
  if (channel === undefined) {
    throw new Error("missing desktop event channel");
  }
  return channel;
}

function commandCalls(): unknown[] {
  return harness.invoke.mock.calls
    .filter(([command]) => command === "send_command")
    .map(([, args]) => (args as { command: unknown }).command);
}

// A configured, credential-satisfied daemon with an empty inbox — the
// baseline for detail-pane automation tests, which drive state entirely
// through pushed event Frames.
function mockAutomationHarness(): void {
  harness.invoke.mockImplementation((command: string) => {
    if (command === "desktop_setup_state") {
      return Promise.resolve(
        settingsStateFixture({ selected_scopes: ["github:o"] }),
      );
    }
    if (command === "desktop_info") {
      return Promise.resolve({
        protocol_version: 1,
        max_terminal_frame_bytes: 2048,
        max_terminal_write_bytes: 1024,
        agents: ["codex"],
        default_agent: "codex",
        repositories: [],
      });
    }
    if (command === "list_workspaces") {
      return Promise.resolve({ workspaces: [], warnings: [] });
    }
    if (command === "read_terminal_data") {
      return new Promise<Uint8Array>(() => {});
    }
    return Promise.resolve();
  });
}

function notesField(): HTMLTextAreaElement {
  return element("notes-body") as HTMLTextAreaElement;
}

function lastAutoFixCommand(): unknown {
  return commandCalls()
    .filter(
      (command) =>
        typeof command === "object" &&
        command !== null &&
        "SetAutoFixPolicies" in command,
    )
    .at(-1);
}

function replyCommands(): unknown[] {
  return commandCalls().filter(
    (command) =>
      typeof command === "object" && command !== null && "PostReply" in command,
  );
}

function createCommands(): unknown[] {
  return commandCalls().filter(
    (command) =>
      typeof command === "object" &&
      command !== null &&
      "CreateWorkspace" in command,
  );
}

// The `DeliverSnippet` payloads sent to the gateway, unwrapped.
function deliverCommands(): Array<{
  terminal_id: number;
  snippet_key: string;
  category: string;
  body: string;
}> {
  return commandCalls()
    .filter(
      (command): command is { DeliverSnippet: Record<string, unknown> } =>
        typeof command === "object" &&
        command !== null &&
        "DeliverSnippet" in command,
    )
    .map((command) => command.DeliverSnippet as never);
}

// The grouped view `snippet_view` would return: `rev` and `pr`, with the
// exact-key auto-submit target set only when the filter uniquely matches.
function snippetView(filter: string): unknown {
  return {
    filter,
    groups: [
      {
        category: "Review",
        label: "Review",
        rows: [
          {
            key: "rev",
            description: "Review the current diff",
            category: "Review",
            body: "Review the current diff.",
            origin: "built-in",
          },
        ],
      },
      {
        category: "Git & PR",
        label: "Git & PR",
        rows: [
          {
            key: "pr",
            description: "Open a PR",
            category: "Git & PR",
            body: "Open a PR.",
            origin: "built-in",
          },
        ],
      },
    ],
    auto_submit: filter === "rev" ? "rev" : null,
    visible_count: 2,
    total: 2,
  };
}

function settingsCalls(): unknown[] {
  return harness.invoke.mock.calls
    .filter(([command]) => command === "save_desktop_settings")
    .map(([, args]) => (args as { settings: unknown }).settings);
}

function agentOption(id: string, label: string): Record<string, unknown> {
  return { id, label, available: true, models: [], default_tier: null };
}

function darkColors(): Record<string, string> {
  return {
    accent: "#7dcfff",
    hover: "#f7768e",
    success: "#9ece6a",
    warn: "#e0af68",
    error: "#f7768e",
    text_strong: "#c0caf5",
    text_dim: "#8088ad",
    chrome: "#3a4060",
    fill: "#292e42",
    surface: "#1a1d2e",
  };
}

function lightColors(): Record<string, string> {
  return {
    accent: "#1a6ec4",
    hover: "#c13574",
    success: "#23864e",
    warn: "#9f6a00",
    error: "#c13574",
    text_strong: "#1c2030",
    text_dim: "#606880",
    chrome: "#c4c9d6",
    fill: "#dadfe9",
    surface: "#f7f8fa",
  };
}

function themeSwatch(name: string): HTMLButtonElement {
  const swatch = [
    ...document.querySelectorAll<HTMLButtonElement>(".theme-swatch"),
  ].find((candidate) => candidate.textContent?.includes(name));
  if (swatch === undefined) {
    throw new Error(`missing theme swatch for ${name}`);
  }
  return swatch;
}

function setSelect(id: string, value: string): void {
  const control = select(id);
  control.value = value;
  control.dispatchEvent(new Event("change", { bubbles: true }));
}

function settingsStateFixture(
  overrides: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    authority: "embedded",
    providers: ["github"],
    first_run: false,
    selected_scopes: [],
    agents: [agentOption("codex", "Codex")],
    default_agent: "codex",
    analytics_enabled: false,
    diagnostics_path: "/tmp/lazybox-crashes",
    log_path: "/tmp/lazybox.log",
    theme: null,
    themes: [],
    keymap_preset: null,
    collapsed_repos: [],
    ...overrides,
  };
}

// The full save payload the frontend emits.
function savePayload(
  overrides: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    github_scopes: [],
    default_agent: "codex",
    analytics_enabled: false,
    theme: null,
    default_model_tier: null,
    ...overrides,
  };
}

function nativeTerminalData(frame: Uint8Array): Uint8Array {
  const item = new Uint8Array(frame.length + 1);
  item[0] = 1;
  item.set(frame, 1);
  return item;
}

function serverFrame(
  kind: number,
  terminalId: number,
  firstSeq: number,
  lastSeq: number,
  payload: string,
): Uint8Array {
  const bytes = new TextEncoder().encode(payload);
  const frame = new Uint8Array(29 + bytes.length);
  const view = new DataView(frame.buffer);
  view.setUint32(0, 25 + bytes.length);
  view.setUint8(4, kind);
  view.setBigUint64(5, BigInt(terminalId));
  view.setBigUint64(13, BigInt(firstSeq));
  view.setBigUint64(21, BigInt(lastSeq));
  frame.set(bytes, 29);
  return frame;
}
