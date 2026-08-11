// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";

const harness = vi.hoisted(() => ({ invoke: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: harness.invoke,
  Channel: class {
    onmessage = (_message: unknown): void => {};
  },
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    setBadgeCount: vi.fn(),
    isFocused: vi.fn().mockResolvedValue(true),
    unminimize: vi.fn(),
    show: vi.fn(),
    setFocus: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: vi.fn().mockResolvedValue(false),
  requestPermission: vi.fn().mockResolvedValue("denied"),
  sendNotification: vi.fn(),
  onAction: vi.fn().mockResolvedValue({ unregister: vi.fn() }),
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

describe("desktop keyboard and ARIA patterns", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
    document.open();
    document.write(html.replace(/<script type="module"[\s\S]*?<\/script>/, ""));
    document.close();
    window.history.replaceState(null, "", "/?preview");
    Object.defineProperty(globalThis, "Option", {
      configurable: true,
      value: function Option(text = "", value = "") {
        const option = document.createElement("option");
        option.textContent = text;
        option.value = value;
        return option;
      },
    });
  });

  it("makes repository expanders and terminal tabs keyboard focusable", async () => {
    await import("./main");
    await vi.waitFor(() =>
      expect(document.querySelector(".repo-header")).not.toBeNull(),
    );
    const repo = document.querySelector<HTMLButtonElement>(".repo-header")!;
    expect(repo.tagName).toBe("BUTTON");
    expect(repo.getAttribute("aria-expanded")).toBe("true");
    repo.click();
    expect(
      document
        .querySelector<HTMLButtonElement>(".repo-header")
        ?.getAttribute("aria-expanded"),
    ).toBe("false");

    repo.click();
    const tab = document.querySelector<HTMLElement>('[role="tab"]')!;
    expect(tab.tabIndex).toBe(0);
    expect(tab.getAttribute("aria-controls")).toMatch(/^terminal-panel-/);
    expect(
      document
        .getElementById(tab.getAttribute("aria-controls")!)
        ?.getAttribute("role"),
    ).toBe("tabpanel");
  });

  it("supports roving theme radio focus and restores dialog focus", async () => {
    await import("./main");
    const settings = document.getElementById(
      "settings-button",
    ) as HTMLButtonElement;
    settings.focus();
    settings.click();
    await vi.waitFor(() =>
      expect(
        (document.getElementById("setup-dialog") as HTMLDialogElement).open,
      ).toBe(true),
    );
    const selected = document.querySelector<HTMLButtonElement>(
      '.theme-swatch[aria-checked="true"]',
    )!;
    selected.focus();
    selected.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }),
    );
    const next = document.querySelector<HTMLButtonElement>(
      '.theme-swatch[aria-checked="true"]',
    )!;
    expect(next.textContent).toContain("Lazybox Light");
    expect(document.activeElement).toBe(next);
    expect(document.documentElement.style.colorScheme).toBe("light");

    (document.getElementById("setup-close") as HTMLButtonElement).click();
    expect(document.activeElement).toBe(settings);
  });

  it("moves through action menus with arrows and returns focus on Escape", async () => {
    await import("./main");
    // Wait for boot to settle (preview data rendered + a workspace selected)
    // before opening the menu — it only populates for a selected workspace.
    await vi.waitFor(() =>
      expect(document.querySelector(".repo-header")).not.toBeNull(),
    );
    const trigger = document.getElementById(
      "actions-button",
    ) as HTMLButtonElement;
    trigger.click();
    const items = [
      ...document
        .getElementById("actions-menu")!
        .querySelectorAll<HTMLButtonElement>('[role="menuitem"]'),
    ];
    expect(document.activeElement).toBe(items[0]);
    items[0]?.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }),
    );
    expect(document.activeElement).toBe(items[1]);
    items[1]?.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
    );
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(trigger);
  });
});
