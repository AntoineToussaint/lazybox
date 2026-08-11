// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";

// vitest strips `?raw` CSS imports to an empty string, so read the stylesheet
// off disk to assert on its actual tracks. `node:fs` isn't typed in this
// DOM-only tsconfig; the cast specifier keeps TS from resolving it.
const { readFileSync } = (await import("node:fs" as string)) as {
  readFileSync: (path: string, encoding: string) => string;
};

const css = readFileSync("src/style.css", "utf8");

const harness = vi.hoisted(() => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: harness.invoke,
  Channel: class {
    onmessage = (_message: unknown): void => {};
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

describe("workspace column splitter", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
    harness.invoke.mockReset();
    harness.invoke.mockResolvedValue(undefined);
    loadDocument();
  });

  it("keeps the grid tracks content-independent", () => {
    // A px minimum on an `fr` track lets content grow the column; the sidebar
    // must be a definite width and the right pane a `minmax(0, …)` so poll
    // data / long lines can never re-distribute widths — including at the
    // responsive breakpoint.
    const rules = gridColumnsRules();
    expect(rules).not.toHaveLength(0);
    for (const rule of rules) {
      expect(rule).toContain("minmax(0, 1fr)");
      expect(rule).not.toMatch(/minmax\(\s*\d+px/);
    }
    expect(rules.some((rule) => rule.includes("var(--sidebar-width"))).toBe(
      true,
    );
  });

  it("restores a persisted sidebar width on load", async () => {
    localStorage.setItem("lazybox.sidebarWidth", "420");
    await import("./main");
    expect(grid().style.getPropertyValue("--sidebar-width")).toBe("420px");
  });

  it("resizes and persists only on drag, clamped to bounds", async () => {
    await import("./main");
    stubGridRect(1400);

    drag(520);
    expect(grid().style.getPropertyValue("--sidebar-width")).toBe("520px");
    expect(localStorage.getItem("lazybox.sidebarWidth")).toBe("520");

    // Below the sidebar minimum → clamped up.
    drag(40);
    expect(grid().style.getPropertyValue("--sidebar-width")).toBe("240px");
    expect(localStorage.getItem("lazybox.sidebarWidth")).toBe("240");

    // Past the point that would starve the right pane → clamped down
    // (1400 - 360 right-min = 1040).
    drag(1300);
    expect(grid().style.getPropertyValue("--sidebar-width")).toBe("1040px");
    expect(localStorage.getItem("lazybox.sidebarWidth")).toBe("1040");
  });

  it("clamps a persisted width wider than the viewport on load", async () => {
    // Restore must not apply a width persisted on a larger monitor verbatim —
    // it would starve the right pane before the user touches anything.
    const original = HTMLElement.prototype.getBoundingClientRect;
    HTMLElement.prototype.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 1000, height: 800 }) as DOMRect;
    try {
      localStorage.setItem("lazybox.sidebarWidth", "5000");
      await import("./main");
      // Displayed width clamps to viewport (1000 - 360 right-min = 640)…
      expect(grid().style.getPropertyValue("--sidebar-width")).toBe("640px");
      // …but the preferred width is preserved for a later, larger window.
      expect(localStorage.getItem("lazybox.sidebarWidth")).toBe("5000");
    } finally {
      HTMLElement.prototype.getBoundingClientRect = original;
    }
  });

  it("re-clamps a too-wide sidebar when the window shrinks, without persisting", async () => {
    await import("./main");
    // A wide sidebar set on a large window (e.g. a prior drag while maximized).
    grid().style.setProperty("--sidebar-width", "900px");
    stubGridRect(1000);

    window.dispatchEvent(new Event("resize"));

    // Shrinking the window alone re-clamps the live width so the right pane
    // keeps its minimum (1000 - 360 = 640)…
    expect(grid().style.getPropertyValue("--sidebar-width")).toBe("640px");
    // …and this transient clamp never overwrites the stored preference.
    expect(localStorage.getItem("lazybox.sidebarWidth")).toBeNull();
  });

  it("exposes splitter values and resizes both panes from the keyboard", async () => {
    await import("./main");
    stubGridRect(1400);
    splitter().dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }),
    );
    expect(splitter().getAttribute("aria-valuenow")).toBe("376");
    expect(localStorage.getItem("lazybox.sidebarWidth")).toBe("376");

    const activity = document.getElementById("right-pane-splitter")!;
    activity.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }),
    );
    expect(activity.getAttribute("aria-valuenow")).toBe("136");
    expect(localStorage.getItem("lazybox.activityHeight")).toBe("136");
  });

  it("clamps a saved activity height after moving to a shorter monitor", async () => {
    const original = HTMLElement.prototype.getBoundingClientRect;
    HTMLElement.prototype.getBoundingClientRect = function () {
      return this.classList.contains("right-pane")
        ? ({ left: 0, top: 0, width: 800, height: 500 } as DOMRect)
        : ({ left: 0, top: 0, width: 1000, height: 500 } as DOMRect);
    };
    try {
      localStorage.setItem("lazybox.activityHeight", "4000");
      await import("./main");
      expect(
        document
          .querySelector<HTMLElement>(".right-pane")!
          .style.getPropertyValue("--activity-height"),
      ).toBe("340px");
      expect(localStorage.getItem("lazybox.activityHeight")).toBe("4000");
    } finally {
      HTMLElement.prototype.getBoundingClientRect = original;
    }
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

function grid(): HTMLElement {
  return document.querySelector<HTMLElement>(".workspace-grid")!;
}

function splitter(): HTMLElement {
  return document.getElementById("column-splitter")!;
}

// Every `grid-template-columns` declared for `.workspace-grid`, base rule and
// responsive breakpoint alike (the `.focus-mode` single-column override is
// excluded — it collapses to one track by design).
function gridColumnsRules(): string[] {
  return Array.from(
    css.matchAll(
      /\.workspace-grid(?!\.focus-mode)[^{]*\{[^}]*?grid-template-columns:\s*([^;]+);/g,
    ),
    (match: RegExpMatchArray) => (match[1] ?? "").trim(),
  );
}

function stubGridRect(width: number): void {
  grid().getBoundingClientRect = () =>
    ({ left: 0, top: 0, width, height: 800 }) as DOMRect;
}

function drag(clientX: number): void {
  splitter().dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
  window.dispatchEvent(new MouseEvent("mousemove", { buttons: 1, clientX }));
  window.dispatchEvent(new MouseEvent("mouseup"));
}
