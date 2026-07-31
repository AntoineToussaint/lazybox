// @vitest-environment happy-dom

import { describe, expect, it, vi } from "vitest";
import type { InboxView, StatusTag, WorkspaceRow } from "./generated";
import {
  kindHeaderLabel,
  renderInboxList,
  statusTone,
  workspaceBadges,
  workspaceKeysInOrder,
} from "./inbox_view";

function row(overrides: Partial<WorkspaceRow> & { key: string }): WorkspaceRow {
  return {
    key: overrides.key,
    title: overrides.title ?? overrides.key,
    reference: overrides.reference ?? "owner/repo#1",
    number: overrides.number ?? 1,
    repo: overrides.repo ?? "owner/repo",
    kind: overrides.kind ?? "Pr",
    role: overrides.role ?? "Author",
    state: overrides.state ?? "Open",
    status: overrides.status ?? "CiOk",
    status_label: overrides.status_label ?? "CI OK",
    ci: overrides.ci ?? "Success",
    review: overrides.review ?? "None",
    unread_count: overrides.unread_count ?? 0,
    updated_at: overrides.updated_at ?? null,
    additions: overrides.additions ?? 0,
    deletions: overrides.deletions ?? 0,
    labels: overrides.labels ?? [],
    needs_reply: overrides.needs_reply ?? false,
    last_commenter: overrides.last_commenter ?? null,
    session_count: overrides.session_count ?? 0,
    attention: overrides.attention ?? false,
  };
}

function groupedView(): InboxView {
  return {
    rows: [
      { RepoHeader: "owner/repo" },
      { KindHeader: "Pr" },
      { Workspace: "owner/repo#10" },
      { Workspace: "owner/repo#11" },
      { KindHeader: "Issue" },
      { Workspace: "owner/repo#20" },
    ],
    workspaces: {
      "owner/repo#10": row({
        key: "owner/repo#10",
        title: "Green PR",
        number: 10,
        status: "CiOk",
        status_label: "CI OK",
        unread_count: 2,
      }),
      "owner/repo#11": row({
        key: "owner/repo#11",
        title: "Failing PR",
        number: 11,
        status: "CiFailed",
        status_label: "CI FAIL",
        ci: "Failure",
      }),
      "owner/repo#20": row({
        key: "owner/repo#20",
        title: "An issue",
        number: 20,
        kind: "Issue",
        status: "None",
        status_label: "",
      }),
    },
    summaries: { "owner/repo": { active: 3, attention: 1 } },
    sort_mode: "ByRoleSplit",
    sort_label: "split",
    collapsed: [],
    total: 3,
    unread_total: 2,
  };
}

describe("inbox view rendering", () => {
  it("maps status tags to color tones and section labels", () => {
    expect(statusTone("CiFailed")).toBe("attention");
    expect(statusTone("ChangesRequested")).toBe("attention");
    expect(statusTone("CiOk")).toBe("success");
    expect(statusTone("Ready")).toBe("success");
    expect(statusTone("ReviewPending")).toBe("info");
    expect(statusTone("None")).toBe("neutral");
    expect(kindHeaderLabel("Pr")).toBe("PRs");
    expect(kindHeaderLabel("Issue")).toBe("Issues");
    expect(kindHeaderLabel("Other")).toBe("Other");
  });

  it("derives the status pill and diff badges from the view-model", () => {
    const failing = workspaceBadges(
      row({ key: "k", status: "CiFailed", status_label: "CI FAIL" }),
    );
    expect(failing).toEqual([{ label: "CI FAIL", tone: "attention" }]);

    const withDiff = workspaceBadges(
      row({ key: "k", status: "None", status_label: "", additions: 4, deletions: 2 }),
    );
    expect(withDiff).toEqual([{ label: "+4 −2", tone: "neutral" }]);
  });

  it("renders repo groups, PR/Issue sections, and rows with real badges", () => {
    const list = document.createElement("div");
    renderInboxList(list, groupedView(), {
      selectedKey: "owner/repo#10",
      onSelectWorkspace: () => {},
      onToggleRepo: () => {},
    });

    expect(list.querySelector(".repo-header .repo-name")?.textContent).toBe(
      "owner/repo",
    );
    const sections = [...list.querySelectorAll(".section-header")].map(
      (node) => node.textContent,
    );
    expect(sections).toEqual(["PRs", "Issues"]);

    const rows = [...list.querySelectorAll<HTMLButtonElement>(".workspace-row")];
    expect(rows).toHaveLength(3);
    expect(rows[0]?.textContent).toContain("Green PR");
    expect(rows[0]?.textContent).toContain("CI OK");
    expect(rows[0]?.getAttribute("aria-selected")).toBe("true");
    expect(rows[1]?.querySelector(".signal-pill.attention")?.textContent).toBe(
      "CI FAIL",
    );
    // Unread count renders in its own slot, not as a pill.
    expect(rows[0]?.querySelector(".unread-badge")?.textContent).toBe("2");
    // A statusless issue shows no status pill.
    expect(rows[2]?.querySelector(".signal-pill")).toBeNull();
  });

  it("selects a workspace by key when its row is clicked", () => {
    const list = document.createElement("div");
    const onSelectWorkspace = vi.fn();
    renderInboxList(list, groupedView(), {
      selectedKey: null,
      onSelectWorkspace,
      onToggleRepo: () => {},
    });
    list.querySelectorAll<HTMLButtonElement>(".workspace-row")[1]?.click();
    expect(onSelectWorkspace).toHaveBeenCalledWith("owner/repo#11");
  });

  it("shows a collapsed caret and toggles the repo on header click", () => {
    const view = groupedView();
    view.collapsed = ["owner/repo"];
    view.rows = [{ RepoHeader: "owner/repo" }];
    const list = document.createElement("div");
    const onToggleRepo = vi.fn();
    renderInboxList(list, view, {
      selectedKey: null,
      onSelectWorkspace: () => {},
      onToggleRepo,
    });
    const header = list.querySelector<HTMLButtonElement>(".repo-header");
    expect(header?.getAttribute("aria-expanded")).toBe("false");
    expect(header?.querySelector(".repo-caret")?.textContent).toBe("▸");
    expect(list.querySelectorAll(".workspace-row")).toHaveLength(0);
    header?.click();
    expect(onToggleRepo).toHaveBeenCalledWith("owner/repo");
  });

  it("lists workspace keys in display order, skipping headers", () => {
    expect(workspaceKeysInOrder(groupedView())).toEqual([
      "owner/repo#10",
      "owner/repo#11",
      "owner/repo#20",
    ]);
  });
});
