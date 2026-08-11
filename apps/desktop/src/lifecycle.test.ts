// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";

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

describe("desktop lifecycle", () => {
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
    harness.channels.length = 0;
    harness.invoke.mockReset();
    harness.invoke.mockImplementation((command: string) => {
      if (command === "desktop_setup_state") {
        return Promise.resolve({
          authority: "embedded",
          providers: ["github"],
          first_run: false,
          selected_scopes: [],
          agents: [{ id: "codex", label: "Codex", available: true }],
          default_agent: "codex",
          analytics_enabled: false,
          diagnostics_path: "/tmp",
          theme: null,
          themes: [],
          keymap_preset: null,
          collapsed_repos: [],
        });
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
        return new Promise(() => {});
      }
      return Promise.resolve();
    });
  });

  it("has no module-load effects and reinitializes without duplicate listeners or timers", async () => {
    vi.resetModules();
    const clearInterval = vi.spyOn(window, "clearInterval");
    const module = await import("./main");
    expect(harness.invoke).not.toHaveBeenCalled();

    const first = module.init(document);
    await vi.waitFor(() =>
      expect(harness.invoke).toHaveBeenCalledWith(
        "subscribe_events",
        expect.anything(),
      ),
    );
    first.dispose();
    expect(clearInterval).toHaveBeenCalledTimes(1);
    await vi.waitFor(() =>
      expect(harness.invoke).toHaveBeenCalledWith(
        "unsubscribe_events",
        expect.anything(),
      ),
    );

    const second = module.init(document);
    await vi.waitFor(() =>
      expect(
        harness.invoke.mock.calls.filter(
          ([name]) => name === "subscribe_events",
        ),
      ).toHaveLength(2),
    );
    harness.invoke.mockClear();
    document.getElementById("refresh-button")?.click();
    await vi.waitFor(() =>
      expect(
        harness.invoke.mock.calls.filter(
          ([name]) => name === "list_workspaces",
        ),
      ).toHaveLength(1),
    );
    second.dispose();
    expect(clearInterval).toHaveBeenCalledTimes(2);
  });

  it("stops the terminal read loop after dispose instead of looping forever", async () => {
    vi.resetModules();
    let reads = 0;
    let releaseRead: ((chunk: ArrayBuffer) => void) | undefined;
    const nextRead = (): Promise<ArrayBuffer> =>
      new Promise<ArrayBuffer>((resolve) => {
        releaseRead = resolve;
      });
    const base = harness.invoke.getMockImplementation()!;
    harness.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "read_terminal_data") {
        reads += 1;
        return nextRead();
      }
      return base(command, args);
    });

    const module = await import("./main");
    const app = module.init(document);
    // The loop makes its first read as soon as the stream subscribes.
    await vi.waitFor(() => expect(reads).toBe(1));

    // Resolve the in-flight read with a reset frame; the loop should come
    // back for a second read.
    const reset = new Uint8Array([0]).buffer;
    releaseRead?.(reset);
    await vi.waitFor(() => expect(reads).toBe(2));

    // Dispose while a read is parked, then let it resolve. A loop that never
    // checks `disposed` would issue a third read here.
    app.dispose();
    releaseRead?.(reset);
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(reads).toBe(2);
  });
});
