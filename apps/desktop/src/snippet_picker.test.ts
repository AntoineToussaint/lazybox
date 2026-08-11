// @vitest-environment happy-dom

import { describe, expect, it, vi } from "vitest";
import type { PickerRow, SnippetPickerView } from "./generated";
import {
  autoSubmitRow,
  clampCursor,
  flattenRows,
  renderSnippetList,
  renderSnippetPreview,
} from "./snippet_picker";

function pick(overrides: Partial<PickerRow> & { key: string }): PickerRow {
  return {
    key: overrides.key,
    description: overrides.description ?? `${overrides.key} description`,
    category: overrides.category ?? "Review",
    body: overrides.body ?? `${overrides.key} body`,
    origin: overrides.origin ?? "built-in",
  };
}

// An empty-filter view with a "Recent" group floating `pr` above its
// real category — the shape `tui-core::snippets` emits.
function recentView(): SnippetPickerView {
  return {
    filter: "",
    groups: [
      {
        category: " Recent",
        label: "Recent",
        rows: [pick({ key: "pr", category: "Git & PR" })],
      },
      { category: "Review", label: "Review", rows: [pick({ key: "rev" })] },
      {
        category: "Git & PR",
        label: "Git & PR",
        rows: [pick({ key: "pr", category: "Git & PR" })],
      },
    ],
    auto_submit: null,
    visible_count: 3,
    total: 2,
  };
}

describe("snippet picker helpers", () => {
  it("flattens group rows in display order, keeping the Recent duplicate", () => {
    const rows = flattenRows(recentView());
    expect(rows.map((row) => row.key)).toEqual(["pr", "rev", "pr"]);
  });

  it("clamps the cursor into range", () => {
    expect(clampCursor(3, -1)).toBe(0);
    expect(clampCursor(3, 5)).toBe(2);
    expect(clampCursor(0, 4)).toBe(0);
  });

  it("resolves the auto-submit key to a row, else null", () => {
    const view: SnippetPickerView = {
      filter: "rev",
      groups: [
        { category: "Review", label: "Review", rows: [pick({ key: "rev" })] },
      ],
      auto_submit: "rev",
      visible_count: 1,
      total: 3,
    };
    expect(autoSubmitRow(view)?.key).toBe("rev");
    expect(autoSubmitRow({ ...view, auto_submit: null })).toBeNull();
  });
});

describe("renderSnippetList", () => {
  it("draws category headers, rows, and marks the cursor selected", () => {
    const list = document.createElement("div");
    renderSnippetList(list, recentView(), 1, {
      onPick: vi.fn(),
      onHover: vi.fn(),
    });

    const headers = [...list.querySelectorAll(".snippet-group-label")].map(
      (el) => el.textContent,
    );
    expect(headers).toEqual(["Recent", "Review", "Git & PR"]);

    const rows = list.querySelectorAll(".snippet-row");
    expect(rows).toHaveLength(3);
    // Cursor 1 is the "rev" row (after the Recent "pr").
    expect(rows[1]?.getAttribute("aria-selected")).toBe("true");
    expect(rows[0]?.getAttribute("aria-selected")).toBe("false");
    expect(rows[1]?.querySelector(".snippet-key")?.textContent).toBe("]rev");
  });

  it("clicking a row picks it by its flat index", () => {
    const list = document.createElement("div");
    const onPick = vi.fn();
    renderSnippetList(list, recentView(), 0, { onPick, onHover: vi.fn() });
    list.querySelectorAll<HTMLButtonElement>(".snippet-row")[2]?.click();
    expect(onPick).toHaveBeenCalledWith(2);
  });

  it("shows an empty state when nothing matches", () => {
    const list = document.createElement("div");
    renderSnippetList(
      list,
      {
        filter: "zz",
        groups: [],
        auto_submit: null,
        visible_count: 0,
        total: 3,
      },
      0,
      { onPick: vi.fn(), onHover: vi.fn() },
    );
    expect(list.querySelector(".snippet-empty")?.textContent).toBe(
      "No matches",
    );
  });
});

describe("renderSnippetPreview", () => {
  it("renders the cursor row's key, meta, and full body", () => {
    const preview = document.createElement("div");
    renderSnippetPreview(preview, recentView(), 1);
    expect(preview.querySelector(".snippet-key")?.textContent).toBe("]rev");
    expect(preview.querySelector(".snippet-preview-meta")?.textContent).toBe(
      "Review · built-in",
    );
    expect(preview.querySelector(".snippet-preview-body")?.textContent).toBe(
      "rev body",
    );
  });
});
