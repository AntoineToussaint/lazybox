import { describe, expect, it } from "vitest";
import {
  commandsForWorkspaceIntent,
  createWorkspaceCommand,
  spawnAgentCommand,
} from "./protocol";

describe("IPC command JSON", () => {
  it("matches the generated serde shape for control commands", () => {
    expect(spawnAgentCommand("github:owner/repo#1", "codex")).toEqual({
      SpawnAgent: {
        session_key: "github:owner/repo#1",
        agent: "codex",
      },
    });
    expect(
      createWorkspaceCommand("first workspace", "github-owner-repo", "codex"),
    ).toEqual({
      CreateWorkspace: {
        name: "first workspace",
        project_key: "github-owner-repo",
        agent: "codex",
      },
    });
  });

  it("maps every desktop workflow intent without exposing internal commands", () => {
    const key = "github:owner/repo#1";
    expect(
      commandsForWorkspaceIntent(key, { type: "spawn-shell" }),
    ).toEqual([{ SpawnShell: { session_key: key } }]);
    expect(commandsForWorkspaceIntent(key, { type: "mark-read" })).toEqual([
      { MarkRead: { session_key: key } },
    ]);
    expect(
      commandsForWorkspaceIntent(key, {
        type: "reply",
        body: "  Ready for review.  ",
      }),
    ).toEqual([
      {
        PostReply: {
          session_key: key,
          body: "Ready for review.",
        },
      },
    ]);
  });

  it("does not emit mutations without a workspace or reply body", () => {
    expect(
      commandsForWorkspaceIntent(null, { type: "spawn-agent", agent: "codex" }),
    ).toEqual([]);
    expect(
      commandsForWorkspaceIntent("github:owner/repo#1", {
        type: "reply",
        body: "   ",
      }),
    ).toEqual([]);
  });
});
