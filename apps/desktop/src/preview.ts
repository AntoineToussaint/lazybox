import type { TerminalRecord } from "./main";
import type { Workspace } from "./protocol";

export interface PreviewState {
  defaultAgent: string;
  workspaces: Map<string, Workspace>;
  terminals: Map<number, TerminalRecord>;
  selectedKey: string;
}

export function loadPreview(): PreviewState {
  const now = new Date();
  const reviewTime = new Date(now.getTime() - 18 * 60_000).toISOString();
  const ciTime = new Date(now.getTime() - 52 * 60_000).toISOString();
  const selectedKey = "github:acme/relay#641";
  const selected: Workspace = {
    key: selectedKey,
    name: "Desktop app spike",
    branch: "issue-641-desktop-spike",
    pr: {
      id: { source: "github", key: "acme/relay#641" },
      title: "Ship a focused desktop client on the existing daemon",
      body:
        "Prove the client boundary with a live inbox and one interactive agent terminal. Keep the backend source-agnostic and reuse the existing replay contract.",
      state: "InReview",
      role: "Author",
      ci: "Passing",
      review: "ReviewRequired",
      unread_count: 2,
      url: "https://example.test/pull/641",
      repo: "acme/relay",
      updated_at: reviewTime,
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
      },
      {
        author: "ci",
        body: "All 438 checks passed in 8m 12s.",
        created_at: ciTime,
        kind: "CiUpdate",
      },
    ],
    seen_count: 0,
    read_indices: [],
    sessions: [],
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
  return {
    defaultAgent: "codex",
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
          replay: [
            ...new TextEncoder().encode(
              "\u001b[1;36mCodex\u001b[0m  lazybox desktop spike\n\n" +
                "› Inspecting the API gateway and terminal replay contract\n" +
                "  ✓ Inbox snapshot received\n" +
                "  ✓ PTY attached through NDJSON\n" +
                "  • Waiting for your next instruction\n\n",
            ),
          ],
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
