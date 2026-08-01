// Thin renderer for the grouped inbox. All grouping, sort, and badge
// priority is computed by the shared `tui-core` logic and delivered as
// an `InboxView` (see src-tauri/src/inbox.rs); this module only maps the
// view-model's rows and enums to DOM, classes, and colors.

import type {
  FilterAxis,
  FilterMenuItem,
  InboxView,
  StatusTag,
  VisibleRow,
  WorkspaceKind,
  WorkspaceRow,
} from "./generated";

export type BadgeTone = "neutral" | "attention" | "success" | "info";

export interface BadgeSpec {
  label: string;
  tone: BadgeTone;
}

/** Map a status tag to its pill color tone (palette lives in CSS). */
export function statusTone(status: StatusTag): BadgeTone {
  switch (status) {
    case "Conflict":
    case "CiFailed":
    case "CiMixed":
    case "ChangesRequested":
      return "attention";
    case "Merged":
    case "Ready":
    case "Approved":
    case "CiOk":
      return "success";
    case "Draft":
    case "Queued":
    case "AutoMerge":
    case "ReviewPending":
    case "CiRunning":
    case "Behind":
    case "Closed":
      return "info";
    case "None":
      return "neutral";
  }
}

export interface FilterMenuGroup {
  axis: FilterAxis;
  items: FilterMenuItem[];
}

/**
 * Group the shared filter menu by axis (State / Role / Kind),
 * preserving the order `tui-core` emits. The menu is already ordered so
 * an axis's rows are contiguous, so a single linear pass suffices — the
 * desktop never hardcodes the predicate list or its grouping.
 */
export function filterMenuGroups(menu: FilterMenuItem[]): FilterMenuGroup[] {
  const groups: FilterMenuGroup[] = [];
  for (const item of menu) {
    const last = groups[groups.length - 1];
    if (last === undefined || last.axis !== item.axis) {
      groups.push({ axis: item.axis, items: [item] });
    } else {
      last.items.push(item);
    }
  }
  return groups;
}

/** The active filters, in menu order — the removable header chips. */
export function activeFilters(menu: FilterMenuItem[]): FilterMenuItem[] {
  return menu.filter((item) => item.active);
}

/** Section header label for a workspace-kind band. */
export function kindHeaderLabel(kind: WorkspaceKind): string {
  switch (kind) {
    case "Pr":
      return "PRs";
    case "Issue":
      return "Issues";
    case "Other":
      return "Other";
  }
}

/**
 * The badges a workspace row shows, sourced entirely from the
 * view-model: the single status pill (CI/review priority already
 * resolved by `StatusTag::for_task`) and the +/− diff. Unread count and
 * relative time render in dedicated slots, not as pills.
 */
export function workspaceBadges(row: WorkspaceRow): BadgeSpec[] {
  const badges: BadgeSpec[] = [];
  if (row.status !== "None" && row.status_label.length > 0) {
    badges.push({ label: row.status_label, tone: statusTone(row.status) });
  }
  if (row.needs_reply) {
    badges.push({
      label:
        row.last_commenter === null
          ? "Reply needed"
          : `Reply to @${row.last_commenter}`,
      tone: "attention",
    });
  }
  if (row.additions > 0 || row.deletions > 0) {
    badges.push({
      label: `+${row.additions} −${row.deletions}`,
      tone: "neutral",
    });
  }
  return badges;
}

/** Session keys of the workspace rows, in display order (for keyboard nav). */
export function workspaceKeysInOrder(view: InboxView): string[] {
  const keys: string[] = [];
  for (const row of view.rows) {
    if ("Workspace" in row) {
      keys.push(row.Workspace);
    }
  }
  return keys;
}

export interface InboxListHandlers {
  selectedKey: string | null;
  onSelectWorkspace: (key: string) => void;
  onToggleRepo: (label: string) => void;
}

/**
 * Render the grouped inbox tree into `list`: repo group headers →
 * PR/Issue/Other section headers → workspace rows with real badges.
 * Each repo becomes an ARIA `group` so the `listbox` container only ever
 * holds groups (its rows are the focusable `option`s); the repo/section
 * headers are presentational chrome inside the group.
 */
export function renderInboxList(
  list: HTMLElement,
  view: InboxView,
  handlers: InboxListHandlers,
): void {
  list.replaceChildren();
  const collapsed = new Set(view.collapsed);
  let group: HTMLElement | null = null;
  for (const row of view.rows) {
    if ("RepoHeader" in row) {
      group = document.createElement("div");
      group.className = "repo-group";
      group.setAttribute("role", "group");
      group.setAttribute("aria-label", row.RepoHeader);
      group.append(
        repoHeader(row.RepoHeader, view, collapsed.has(row.RepoHeader), handlers),
      );
      list.append(group);
    } else if ("KindHeader" in row) {
      (group ?? list).append(sectionHeader(kindHeaderLabel(row.KindHeader)));
    } else if ("Workspace" in row) {
      const data = view.workspaces[row.Workspace];
      if (data !== undefined) {
        (group ?? list).append(workspaceRow(data, handlers));
      }
    }
    // Session sub-rows are not surfaced in the desktop v1 (the workspace
    // row already represents the primary session).
  }
}

function repoHeader(
  label: string,
  view: InboxView,
  isCollapsed: boolean,
  handlers: InboxListHandlers,
): HTMLElement {
  const summary = view.summaries[label];
  const header = document.createElement("button");
  header.type = "button";
  header.className = "repo-header";
  header.setAttribute("aria-expanded", String(!isCollapsed));
  header.addEventListener("click", () => handlers.onToggleRepo(label));

  const caret = document.createElement("span");
  caret.className = "repo-caret";
  caret.textContent = isCollapsed ? "▸" : "▾";

  const name = document.createElement("span");
  name.className = "repo-name";
  name.textContent = label;

  const count = document.createElement("span");
  count.className = "repo-count";
  const active = summary?.active ?? 0;
  count.textContent = String(active);

  header.append(caret, name, count);
  if ((summary?.attention ?? 0) > 0) {
    const dot = document.createElement("span");
    dot.className = "repo-attention";
    dot.setAttribute("aria-label", `${summary?.attention} need attention`);
    dot.textContent = "●";
    header.append(dot);
  }
  return header;
}

function sectionHeader(label: string): HTMLElement {
  const header = document.createElement("div");
  header.className = "section-header";
  header.setAttribute("role", "presentation");
  header.textContent = label;
  return header;
}

function workspaceRow(
  row: WorkspaceRow,
  handlers: InboxListHandlers,
): HTMLElement {
  const button = document.createElement("button");
  const selected = row.key === handlers.selectedKey;
  button.className = "workspace-row";
  button.classList.toggle("selected", selected);
  button.type = "button";
  button.setAttribute("role", "option");
  button.setAttribute("aria-selected", String(selected));
  button.tabIndex = selected ? 0 : -1;
  button.setAttribute(
    "aria-label",
    `${row.title}, ${row.repo ?? ""}, ${
      row.unread_count === 0 ? "read" : `${row.unread_count} unread`
    }`,
  );
  button.addEventListener("click", () => handlers.onSelectWorkspace(row.key));

  const top = document.createElement("span");
  top.className = "workspace-row-top";
  const reference = document.createElement("span");
  reference.className = "workspace-reference";
  reference.textContent = row.number === null ? row.reference : `#${row.number}`;
  const state = document.createElement("span");
  const stateName = row.state ?? "local";
  state.className = `task-state task-state-${stateName.toLowerCase()}`;
  state.textContent = stateName;
  top.append(reference, state);
  for (const badge of workspaceBadges(row)) {
    top.append(signalPill(badge));
  }

  const title = document.createElement("strong");
  title.className = "workspace-title";
  title.textContent = row.title;

  const bottom = document.createElement("span");
  bottom.className = "workspace-row-bottom";
  const meta = document.createElement("span");
  meta.className = "workspace-meta";
  meta.textContent = [
    row.repo,
    row.role === null ? null : `you are ${row.role.toLowerCase()}`,
    row.updated_at === null ? null : relativeTime(row.updated_at),
  ]
    .filter((value): value is string => Boolean(value))
    .join(" · ");
  const unread = document.createElement("span");
  unread.className = "unread-badge";
  unread.textContent = row.unread_count > 0 ? String(row.unread_count) : "·";
  bottom.append(meta, unread);

  button.append(top, title, bottom);
  return button;
}

function signalPill(badge: BadgeSpec): HTMLElement {
  const pill = document.createElement("span");
  pill.className = `signal-pill${badge.tone === "neutral" ? "" : ` ${badge.tone}`}`;
  pill.textContent = badge.label;
  return pill;
}

/** Format an RFC 3339 timestamp as a short relative time. */
export function relativeTime(value: string): string {
  const timestamp = Date.parse(value);
  if (!Number.isFinite(timestamp)) {
    return "";
  }
  const seconds = Math.round((timestamp - Date.now()) / 1000);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  if (Math.abs(seconds) < 60) {
    return formatter.format(seconds, "second");
  }
  const minutes = Math.round(seconds / 60);
  if (Math.abs(minutes) < 60) {
    return formatter.format(minutes, "minute");
  }
  const hours = Math.round(minutes / 60);
  if (Math.abs(hours) < 24) {
    return formatter.format(hours, "hour");
  }
  return formatter.format(Math.round(hours / 24), "day");
}

// Re-export the row type for consumers that render from `InboxView`.
export type { VisibleRow };
