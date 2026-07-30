import { describe, expect, it } from "vitest";
import {
  createWorkspaceCommand,
  markReadCommand,
  postReplyCommand,
  spawnAgentCommand,
  spawnShellCommand,
} from "./protocol";

describe("IPC command JSON", () => {
  it("matches the generated serde shape for control commands", () => {
    expect(spawnAgentCommand("github:owner/repo#1", "codex")).toEqual({
      SpawnAgent: {
        session_key: "github:owner/repo#1",
        agent: "codex",
      },
    });
    expect(spawnShellCommand("github:owner/repo#1")).toEqual({
      SpawnShell: { session_key: "github:owner/repo#1" },
    });
    expect(markReadCommand("github:owner/repo#1")).toEqual({
      MarkRead: { session_key: "github:owner/repo#1" },
    });
    expect(postReplyCommand("github:owner/repo#1", "Looks good.")).toEqual({
      PostReply: {
        session_key: "github:owner/repo#1",
        body: "Looks good.",
      },
    });
    expect(createWorkspaceCommand("investigate", "github-owner-repo", "codex"))
      .toEqual({
        CreateWorkspace: {
          name: "investigate",
          project_key: "github-owner-repo",
          agent: "codex",
        },
      });
  });
});
