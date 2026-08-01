// @vitest-environment happy-dom

import { describe, expect, it, vi } from "vitest";
import html from "../index.html?raw";

// A single `main` import per file keeps one set of window listeners, so
// keyboard-driven assertions (`f`, Escape) aren't double-dispatched by a
// second module instance. The whole filter/search surface is therefore
// one sequential integration test rather than several `resetModules`
// re-imports (which race each other on the shared DOM).
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

describe("desktop filter menu and search", () => {
  it("builds the menu from shared metadata, composes toggles, and wires search", async () => {
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
        return Promise.resolve({ workspaces: [], warnings: [] });
      }
      // The terminal reader loops on this — park it so it never resolves.
      if (command === "read_terminal_data") {
        return new Promise<never>(() => {});
      }
      return Promise.resolve();
    });

    loadDocument();
    await import("./main");
    await vi.waitFor(() =>
      expect(element("connection-label").textContent).toBe("Live"),
    );
    pushInbox([{ number: 1 }, { number: 2 }]);
    await vi.waitFor(() =>
      expect(document.querySelectorAll(".workspace-row")).toHaveLength(2),
    );

    // `f` opens a menu grouped by axis, built entirely from the shared
    // `filter_menu` metadata (State / Role / Kind), with live counts.
    press("f");
    expect(element("filter-menu").classList.contains("hidden")).toBe(false);
    expect(
      [...document.querySelectorAll(".filter-section-heading")].map(
        (node) => node.textContent,
      ),
    ).toEqual(["State", "Role", "Kind"]);
    expect(
      filterRow("PR")?.querySelector(".filter-row-count")?.textContent,
    ).toBe("2");

    // Opening moved focus into the menu; Escape closes and returns it to
    // the trigger rather than stranding it on a hidden node (finding #3).
    // Checked before any toggle, which re-renders the menu body.
    expect(element("filter-menu").contains(document.activeElement)).toBe(true);
    press("Escape");
    expect(element("filter-menu").classList.contains("hidden")).toBe(true);
    expect(document.activeElement).toBe(element("filter-button"));

    // `f` toggles: reopen, then a second `f` closes (finding #4).
    press("f");
    expect(element("filter-menu").classList.contains("hidden")).toBe(false);
    press("f");
    expect(element("filter-menu").classList.contains("hidden")).toBe(true);

    // A single toggle sends just that predicate.
    press("f");
    toggleRow("PR");
    expect(lastCall("set_filters")).toEqual({ filters: ["Pr"] });

    // Two toggles with no intervening view push must COMPOSE — a
    // view-derived active set would drop the first (finding #1).
    toggleRow("author");
    expect(lastCall("set_filters")).toEqual({ filters: ["Pr", "Author"] });
    expect(element("filter-button").textContent).toBe("Filter (2)");

    // A view carrying an active filter renders a removable chip; clicking
    // it clears the filter.
    inboxChannel().onmessage({
      ...(inboxViewFor([{ number: 1 }]) as Record<string, unknown>),
      filter_menu: [
        { filter: "Pr", axis: "Kind", label: "PR", count: 1, active: true },
      ],
      filter_chips: ["PR"],
    });
    await vi.waitFor(() =>
      expect(document.querySelector(".filter-chip")).not.toBeNull(),
    );
    expect(element("filter-button").textContent).toBe("Filter (1)");
    document.querySelector<HTMLButtonElement>(".filter-chip")?.click();
    expect(lastCall("set_filters")).toEqual({ filters: [] });

    // The search box feeds the shared pipeline.
    const search = element("inbox-search") as HTMLInputElement;
    search.value = "widget";
    search.dispatchEvent(new Event("input", { bubbles: true }));
    await vi.waitFor(() => expect(lastCall("set_search")).toBeDefined());
    expect(lastCall("set_search")).toEqual({ query: "widget" });
  });
});

function press(key: string): void {
  window.dispatchEvent(new KeyboardEvent("keydown", { key }));
}

function filterRow(label: string): Element | undefined {
  return [...document.querySelectorAll(".filter-row")].find(
    (row) => row.querySelector(".filter-row-label")?.textContent === label,
  );
}

function toggleRow(label: string): void {
  filterRow(label)
    ?.querySelector("input")
    ?.dispatchEvent(new Event("change", { bubbles: true }));
}

function lastCall(command: string): unknown {
  return harness.invoke.mock.calls
    .filter(([name]) => name === command)
    .at(-1)?.[1];
}

function inboxChannel(): { onmessage: (message: unknown) => void } {
  const channel = harness.channels.at(-1);
  if (channel === undefined) {
    throw new Error("missing desktop inbox channel");
  }
  return channel;
}

function pushInbox(rows: Array<{ number: number }>): void {
  inboxChannel().onmessage(inboxViewFor(rows));
}

function inboxViewFor(rows: Array<{ number: number }>): unknown {
  return {
    rows: [
      { RepoHeader: "o/r" },
      { KindHeader: "Pr" },
      ...rows.map((row) => ({ Workspace: `github-o-r-${row.number}` })),
    ],
    workspaces: Object.fromEntries(
      rows.map((row) => [`github-o-r-${row.number}`, workspaceRowFor(row)]),
    ),
    summaries: { "o/r": { active: rows.length, attention: 0, unread: 0 } },
    sort_mode: "ByRoleSplit",
    sort_label: "split",
    collapsed: [],
    total: rows.length,
    unread_total: 0,
    filter_menu: [
      { filter: "Unread", axis: "State", label: "unread", count: 0, active: false },
      { filter: "Author", axis: "Role", label: "author", count: rows.length, active: false },
      { filter: "Pr", axis: "Kind", label: "PR", count: rows.length, active: false },
      { filter: "Issue", axis: "Kind", label: "issue", count: 0, active: false },
    ],
    filter_chips: [],
  };
}

function workspaceRowFor(row: { number: number }): unknown {
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
    unread_count: 0,
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

function loadDocument(): void {
  document.open();
  document.write(html.replace(/<script type="module"[\s\S]*?<\/script>/, ""));
  document.close();
}

function element(id: string): HTMLElement {
  const value = document.getElementById(id);
  if (value === null) {
    throw new Error(`missing #${id}`);
  }
  return value;
}
