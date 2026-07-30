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
            ? {
                first_run: true,
                selected_scopes: [],
                agents: [
                  {
                    id: "cursor-agent",
                    label: "Cursor Agent",
                    available: true,
                  },
                ],
                default_agent: "cursor-agent",
                analytics_enabled: false,
                diagnostics_path: "/tmp/lazybox-crashes",
              }
            : {
                first_run: false,
                selected_scopes: ["github:acme/widget"],
                agents: [
                  { id: "codex", label: "Codex", available: true },
                ],
                default_agent: "codex",
                analytics_enabled: false,
                diagnostics_path: "/tmp/lazybox-crashes",
              },
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
        settings: {
          github_scopes: ["github:acme"],
          default_agent: "cursor-agent",
          analytics_enabled: false,
        },
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
      expect(settingsCalls().at(-1)).toEqual({
        github_scopes: [],
        default_agent: "codex",
        analytics_enabled: false,
      }),
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
});

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

function replyCommands(): unknown[] {
  return commandCalls().filter(
    (command) =>
      typeof command === "object" &&
      command !== null &&
      "PostReply" in command,
  );
}

function settingsCalls(): unknown[] {
  return harness.invoke.mock.calls
    .filter(([command]) => command === "save_desktop_settings")
    .map(([, args]) => (args as { settings: unknown }).settings);
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
