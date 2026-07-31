import type { TerminalRecord } from "./main";
import type { InboxView, Workspace, WorkspaceRow } from "./protocol";

export interface PreviewState {
  defaultAgent: string;
  workspaces: Map<string, Workspace>;
  terminals: Map<number, TerminalRecord>;
  inboxView: InboxView;
  selectedKey: string;
}

function previewRow(
  workspace: Workspace,
  overrides: Partial<WorkspaceRow>,
): WorkspaceRow {
  const task = workspace.pr!;
  return {
    key: workspace.key,
    title: task.title,
    reference: task.id.key,
    number: Number(task.id.key.split("#")[1] ?? "0"),
    repo: task.repo,
    kind: "Pr",
    role: task.role,
    state: task.state,
    status: "CiOk",
    status_label: "CI OK",
    ci: task.ci,
    review: task.review,
    unread_count: task.unread_count,
    updated_at: task.updated_at,
    additions: task.additions,
    deletions: task.deletions,
    labels: task.labels,
    needs_reply: task.needs_reply,
    last_commenter: task.last_commenter,
    session_count: workspace.sessions.length,
    attention: task.needs_reply,
    ...overrides,
  };
}

export function loadPreview(): PreviewState {
  const now = new Date();
  const reviewTime = new Date(now.getTime() - 18 * 60_000).toISOString();
  const ciTime = new Date(now.getTime() - 52 * 60_000).toISOString();
  const selectedKey = "github:acme/relay#641";
  const selected: Workspace = {
    schema: 1,
    key: selectedKey,
    project_key: null,
    local: false,
    linked_checkout: null,
    name: "Desktop client boundary",
    branch: "issue-646-desktop-boundary",
    sessions: [],
    pr: {
      id: { source: "github", key: "acme/relay#641" },
      title: "Ship a focused desktop client on the existing daemon",
      body:
        "Prove the client boundary with a live inbox and one interactive agent terminal. Keep the backend source-agnostic and reuse the existing replay contract.",
      state: "InReview",
      role: "Author",
      ci: "Success",
      review: "Pending",
      checks: [],
      unread_count: 2,
      url: "https://example.test/pull/641",
      repo: "acme/relay",
      branch: "issue-646-desktop-boundary",
      base_branch: "main",
      updated_at: reviewTime,
      created_at: null,
      closed_at: null,
      labels: [],
      reviewers: [],
      assignees: [],
      auto_merge_enabled: false,
      is_in_merge_queue: false,
      mergeable: "Mergeable",
      is_behind_base: false,
      node_id: null,
      needs_reply: true,
      last_commenter: "mira",
      recent_activity: [],
      additions: 0,
      deletions: 0,
      closes_issues: [],
      kind: "Pr",
    },
    gh_issues: [],
    linear_issues: [],
    activity: [
      {
        author: "mira",
        body:
          "The daemon boundary looks solid. Can the terminal recover after a slow consumer drops output?",
        created_at: reviewTime,
        kind: "Review",
        node_id: null,
        path: null,
        line: null,
        diff_hunk: null,
        thread_id: null,
      },
      {
        author: "ci",
        body: "All 438 checks passed in 8m 12s.",
        created_at: ciTime,
        kind: "CiUpdate",
        node_id: null,
        path: null,
        line: null,
        diff_hunk: null,
        thread_id: null,
      },
    ],
    seen_count: 0,
    read_indices: [],
    snoozed_until: null,
    auto_merge_on_green: false,
    track_main: false,
    base_branch: "main",
    track_main_behind: false,
    policies: { auto_fix_ci: "Default", auto_fix_conflict: "Default" },
    notes: "",
    sent_snippets: [],
    cleanup_prompt: "unresolved",
    created_at: now.toISOString(),
    last_viewed_at: null,
  };
  const second: Workspace = {
    ...selected,
    key: "github:acme/relay#638",
    name: "Provider labels",
    branch: "fix-provider-labels",
    pr: {
      ...selected.pr!,
      id: { source: "github", key: "acme/relay#638" },
      title: "Preserve hyphenated project labels",
      state: "Open",
      unread_count: 0,
      updated_at: ciTime,
    },
    activity: [],
    seen_count: 0,
  };
  const inboxView: InboxView = {
    rows: [
      { RepoHeader: "acme/relay" },
      { KindHeader: "Pr" },
      { Workspace: selected.key },
      { Workspace: second.key },
    ],
    workspaces: {
      [selected.key]: previewRow(selected, {
        status: "ReviewPending",
        status_label: "REVIEW",
      }),
      [second.key]: previewRow(second, {}),
    },
    summaries: { "acme/relay": { active: 2, attention: 1, unread: 2 } },
    sort_mode: "ByRoleSplit",
    sort_label: "split",
    collapsed: [],
    total: 2,
    unread_total: 2,
  };
  return {
    defaultAgent: "codex",
    inboxView,
    workspaces: new Map([
      [selected.key, selected],
      [second.key, second],
    ]),
    terminals: new Map([
      [
        1,
        {
          id: 1,
          sessionKey: selectedKey,
          kind: { Agent: "codex" },
          replay: new TextEncoder().encode(
              "\u001b[1;36mCodex\u001b[0m  lazybox desktop client\n\n" +
                "› Inspecting the API gateway and terminal replay contract\n" +
                "  ✓ Inbox snapshot received\n" +
                "  ✓ PTY attached through NDJSON\n" +
                "  • Waiting for your next instruction\n\n",
            ),
          lastSeq: 4,
          replayAvailable: true,
          dirty: false,
          state: "done",
        },
      ],
    ]),
    selectedKey,
  };
}
