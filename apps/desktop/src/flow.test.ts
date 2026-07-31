// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";
import fixture from "./generated/compatibility.json";

const harness = vi.hoisted(() => ({
  invoke: vi.fn(),
  channels: [] as Array<{ onmessage: (message: unknown) => void }>,
  terminalWrites: [] as string[],
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
    onData(): { dispose: () => void } {
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
        return new Promise<Uint8Array>((resolve) => terminalReads.push(resolve));
      }
      if (command === "send_command") {
        const payload = args as {
          command?: { PostReply?: { body: string } };
        };
        if (payload.command?.PostReply !== undefined && replyAttempt === "fail") {
          return new Promise((_resolve, reject) => {
            rejectReply = reject;
          });
        }
        return Promise.resolve();
      }
      return Promise.resolve();
    });

    await import("./main");
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(true));
    await vi.waitFor(() =>
      expect(document.querySelector('input[value="github:acme"]')).not.toBeNull(),
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
    await import("./main");
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
    pushInbox([{ number: 42 }]);
    await vi.waitFor(() =>
      expect(element("task-title").textContent).toBe("PR o/r#42"),
    );
    expect(button("spawn-button").disabled).toBe(false);
    expect(button("shell-button").disabled).toBe(false);

    button("spawn-button").click();
    button("shell-button").click();
    await vi.waitFor(() => {
      expect(commandCalls()).toContainEqual({
        SpawnAgent: { session_key: "github-o-r-42", agent: "codex" },
      });
      expect(commandCalls()).toContainEqual({
        SpawnShell: { session_key: "github-o-r-42" },
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
    await import("./main");
    // Wait until the initial connect settles (it overwrites the map from
    // the empty list_workspaces) before injecting the empty view + events,
    // so a late refreshInbox can't clobber them.
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );
    pushInbox([]);
    await vi.waitFor(() =>
      expect(element("workspace-list").textContent).toContain("inbox is empty"),
    );

    eventChannel().onmessage({
      type: "Frame",
      payload: { Snapshot: { workspaces: [pr(42), pr(43)], terminals: [] } },
    });
    pushInbox([{ number: 42 }, { number: 43 }]);
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row").length).toBe(2),
    );

    selectWorkspaceRow("PR o/r#42");
    await vi.waitFor(() => expect(element("task-title").textContent).toBe("PR o/r#42"));
    expect(button("reply-button").disabled).toBe(false);

    input("#reply-body").value = "Shipping now.";
    form("reply-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();
    await vi.waitFor(() => expect(replyCommands()).toHaveLength(1));
    expect(button("reply-button").disabled).toBe(true);

    selectWorkspaceRow("PR o/r#43");
    await vi.waitFor(() => expect(element("task-title").textContent).toBe("PR o/r#43"));
    expect(button("reply-button").disabled).toBe(false);
    expect(input("#reply-body").disabled).toBe(false);

    selectWorkspaceRow("PR o/r#42");
    await vi.waitFor(() => expect(element("task-title").textContent).toBe("PR o/r#42"));
    expect(button("reply-button").disabled).toBe(true);

    releaseReply?.();
    await vi.waitFor(() => expect(button("reply-button").disabled).toBe(false));
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
    await import("./main");
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
          repositories: [{ project_key: "github-acme-widget", label: "acme/widget" }],
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
    await import("./main");
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
    await import("./main");
    // Wait until the initial connect settles (it overwrites the map from
    // the empty list_workspaces) before injecting the empty view + events,
    // so a late refreshInbox can't clobber them.
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );
    pushInbox([]);
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
          repositories: [{ project_key: "github-acme-widget", label: "acme/widget" }],
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
    await import("./main");
    await vi.waitFor(() =>
      expect(button("settings-button").disabled).toBe(false),
    );

    button("settings-button").click();
    await vi.waitFor(() => expect(dialog("setup-dialog").open).toBe(true));

    // Theme catalog renders swatches; the model menu and workspace
    // enums seed from config.
    expect(document.querySelectorAll(".theme-swatch")).toHaveLength(2);
    expect(element("keymap-preset-label").textContent).toBe("Keymap: vim");
    expect([...select("default-model-select").options].map((o) => o.value)).toEqual([
      "S",
      "M",
      "L",
    ]);
    expect(select("default-model-select").value).toBe("L");
    expect(select("terminal-layout-select").value).toBe("split");
    expect(select("activity-pane-select").value).toBe("full");

    themeSwatch("Lazybox Light").click();
    setSelect("default-model-select", "M");
    setSelect("terminal-layout-select", "tabs");
    setSelect("activity-pane-select", "hidden");

    form("setup-form").dispatchEvent(submitEvent());
    await vi.waitFor(() => expect(dialog("confirm-dialog").open).toBe(true));
    button("confirm-accept").click();

    await vi.waitFor(() =>
      expect(settingsCalls().at(-1)).toEqual(
        savePayload({
          github_scopes: ["github:acme/widget"],
          default_agent: "claude",
          theme: "Lazybox Light",
          terminal_new_layout: "tabs",
          activity_pane_default: "hidden",
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
          repositories: [{ project_key: "github-acme-widget", label: "acme/widget" }],
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
    await import("./main");
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
});

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

function selectWorkspaceRow(title: string): void {
  const row = [...document.querySelectorAll<HTMLButtonElement>(".workspace-row")].find(
    (candidate) => candidate.textContent?.includes(title),
  );
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

// `subscribe_events` creates the event channel first, then the inbox
// channel — so the last two channels are [event, inbox].
function eventChannel(): { onmessage: (message: unknown) => void } {
  const channel = harness.channels.at(-2);
  if (channel === undefined) {
    throw new Error("missing desktop event channel");
  }
  return channel;
}

function inboxChannel(): { onmessage: (message: unknown) => void } {
  const channel = harness.channels.at(-1);
  if (channel === undefined) {
    throw new Error("missing desktop inbox channel");
  }
  return channel;
}

function pushInbox(rows: Array<{ number: number; unread?: number }>): void {
  inboxChannel().onmessage(inboxViewFor(rows));
}

function inboxViewFor(rows: Array<{ number: number; unread?: number }>): unknown {
  // An empty inbox is a non-null view with no rows — mirrors the daemon's
  // opening snapshot for a repo with nothing in flight.
  const tree =
    rows.length === 0
      ? []
      : [
          { RepoHeader: "o/r" },
          { KindHeader: "Pr" },
          ...rows.map((row) => ({ Workspace: `github-o-r-${row.number}` })),
        ];
  const unread = rows.reduce((sum, row) => sum + (row.unread ?? 0), 0);
  return {
    rows: tree,
    workspaces: Object.fromEntries(
      rows.map((row) => [`github-o-r-${row.number}`, workspaceRowFor(row)]),
    ),
    summaries:
      rows.length === 0
        ? {}
        : { "o/r": { active: rows.length, attention: 0, unread } },
    sort_mode: "ByRoleSplit",
    sort_label: "split",
    collapsed: [],
    total: rows.length,
    unread_total: unread,
  };
}

function workspaceRowFor(row: { number: number; unread?: number }): unknown {
  return {
    key: `github-o-r-${row.number}`,
    title: `PR o/r#${row.number}`,
    reference: `o/r#${row.number}`,
    number: row.number,
    repo: "o/r",
    kind: "Pr",
    role: "Author",
    state: "Open",
    status: "CiOk",
    status_label: "CI OK",
    ci: "Success",
    review: "None",
    unread_count: row.unread ?? 0,
    updated_at: "2026-04-01T12:00:00Z",
    additions: 0,
    deletions: 0,
    labels: [],
    needs_reply: false,
    last_commenter: null,
    session_count: 0,
    attention: false,
  };
}

function commandCalls(): unknown[] {
  return harness.invoke.mock.calls
    .filter(([command]) => command === "send_command")
    .map(([, args]) => (args as { command: unknown }).command);
}

function replyCommands(): unknown[] {
  return commandCalls().filter(
    (command) =>
      typeof command === "object" &&
      command !== null &&
      "PostReply" in command,
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
    text_dim: "#7a82a7",
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
    first_run: false,
    selected_scopes: [],
    agents: [agentOption("codex", "Codex")],
    default_agent: "codex",
    analytics_enabled: false,
    diagnostics_path: "/tmp/lazybox-crashes",
    theme: null,
    themes: [],
    keymap_preset: null,
    terminal_new_layout: "split",
    activity_pane_default: "full",
    ...overrides,
  };
}

// The full save payload the frontend now emits; the extra appearance /
// workspace fields default to their unset values in these flows.
function savePayload(
  overrides: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    github_scopes: [],
    default_agent: "codex",
    analytics_enabled: false,
    theme: null,
    terminal_new_layout: "split",
    activity_pane_default: "full",
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
