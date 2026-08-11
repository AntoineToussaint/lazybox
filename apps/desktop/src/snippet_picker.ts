import type { PickerRow, SnippetGroup, SnippetPickerView } from "./protocol";

/// The flat, in-display-order row list the cursor moves over. Group rows
/// are concatenated exactly as they render, so a snippet floated into the
/// "Recent" group appears twice — matching the TUI, where Recent is a
/// shortcut duplicate, not a move.
export function flattenRows(view: SnippetPickerView): PickerRow[] {
  return view.groups.flatMap((group) => group.rows);
}

/// Clamp a cursor position into `[0, length)`, collapsing an empty list
/// to `0`.
export function clampCursor(length: number, cursor: number): number {
  if (length <= 0) {
    return 0;
  }
  return Math.max(0, Math.min(length - 1, cursor));
}

/// The row the picker should auto-submit, when the shared logic flagged a
/// unique-prefix key match (`]]srev`). Resolved against the visible rows
/// so the caller sends the exact row (key + body) the daemon expects.
export function autoSubmitRow(view: SnippetPickerView): PickerRow | null {
  if (view.auto_submit === null) {
    return null;
  }
  for (const group of view.groups) {
    for (const row of group.rows) {
      if (row.key === view.auto_submit) {
        return row;
      }
    }
  }
  return null;
}

interface RenderOptions {
  onPick: (index: number) => void;
  onHover: (index: number) => void;
}

/// Draw the category-grouped list: one header per group, then its rows.
/// The row at `cursor` (a flat index across all groups) is marked
/// selected. Each row wires click → `onPick` and hover → `onHover`, so
/// the caller owns delivery and cursor tracking.
export function renderSnippetList(
  listEl: HTMLElement,
  view: SnippetPickerView,
  cursor: number,
  options: RenderOptions,
): void {
  listEl.replaceChildren();
  if (view.groups.length === 0) {
    const empty = document.createElement("p");
    empty.className = "snippet-empty";
    empty.textContent =
      view.total === 0
        ? "No snippets configured — see ~/.lazybox/snippets.yaml"
        : "No matches";
    listEl.append(empty);
    return;
  }

  let index = 0;
  for (const group of view.groups) {
    listEl.append(groupHeader(group));
    for (const row of group.rows) {
      listEl.append(snippetRow(row, index, cursor, options));
      index += 1;
    }
  }
}

function groupHeader(group: SnippetGroup): HTMLElement {
  const header = document.createElement("div");
  header.className = "snippet-group";
  header.dataset.category = group.category;
  const label = document.createElement("span");
  label.className = "snippet-group-label";
  label.textContent = group.label;
  const count = document.createElement("span");
  count.className = "snippet-group-count";
  count.textContent = String(group.rows.length);
  header.append(label, count);
  return header;
}

function snippetRow(
  row: PickerRow,
  index: number,
  cursor: number,
  options: RenderOptions,
): HTMLElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "snippet-row";
  button.id = `snippet-option-${index}`;
  button.dataset.index = String(index);
  button.setAttribute("role", "option");
  button.setAttribute("aria-selected", String(index === cursor));
  button.tabIndex = -1;
  const key = document.createElement("span");
  key.className = "snippet-key";
  key.textContent = `]${row.key}`;
  const description = document.createElement("span");
  description.className = "snippet-description";
  description.textContent = row.description;
  button.append(key, description);
  button.addEventListener("click", () => options.onPick(index));
  button.addEventListener("mouseenter", () => options.onHover(index));
  return button;
}

/// Draw the preview pane for the row under the cursor: its key +
/// description, category + origin, and the full body that will be sent.
export function renderSnippetPreview(
  previewEl: HTMLElement,
  view: SnippetPickerView,
  cursor: number,
): void {
  previewEl.replaceChildren();
  const rows = flattenRows(view);
  const row = rows[cursor];
  if (row === undefined) {
    const empty = document.createElement("p");
    empty.className = "snippet-preview-empty";
    empty.textContent = "No snippet selected";
    previewEl.append(empty);
    return;
  }

  const title = document.createElement("p");
  title.className = "snippet-preview-title";
  const key = document.createElement("span");
  key.className = "snippet-key";
  key.textContent = `]${row.key}`;
  title.append(key);
  if (row.description.length > 0) {
    const description = document.createElement("span");
    description.className = "snippet-preview-description";
    description.textContent = row.description;
    title.append(description);
  }

  const meta = document.createElement("p");
  meta.className = "snippet-preview-meta";
  meta.textContent = [row.category === "" ? "Other" : row.category, row.origin]
    .filter((part) => part.length > 0)
    .join(" · ");

  const body = document.createElement("pre");
  body.className = "snippet-preview-body";
  body.textContent = row.body;

  previewEl.append(title, meta, body);
}
