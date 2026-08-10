import { describe, expect, it } from "vitest";
import {
  archiveCommand,
  closeIssueCommand,
  commandsForWorkspaceIntent,
  createWorkspaceCommand,
  deleteOrCloseCommand,
  inspectWorkspaceDiffCommand,
  injectPromptCommand,
  markActivityReadCommand,
  mergePrCommand,
  renameWorkspaceCommand,
  setAutoFixPoliciesCommand,
  setAutoMergeOnGreenCommand,
  setNotesCommand,
  setTrackMainCommand,
  snoozeCommand,
  spawnAgentCommand,
  syncWorkspaceCommand,
  unsnoozeCommand,
  updateBranchCommand,
  writeShellCommand,
} from "./protocol";

describe("IPC command JSON", () => {
  it("matches the generated serde shape for control commands", () => {
    expect(spawnAgentCommand("github:owner/repo#1", "codex")).toEqual({
      SpawnAgent: {
        session_key: "github:owner/repo#1",
        agent: "codex",
        initial_prompt: null,
        model_alias: null,
        on_main: false,
      },
    });
    expect(
      spawnAgentCommand(
        "github:owner/repo#1",
        "claude",
        "L",
        true,
        "Fix CI with the repository conventions.",
      ),
    ).toEqual({
      SpawnAgent: {
        session_key: "github:owner/repo#1",
        agent: "claude",
        initial_prompt: "Fix CI with the repository conventions.",
        model_alias: "L",
        on_main: true,
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

  it("keeps contextual spawn and live delivery atomic at the desktop wire", () => {
    expect(injectPromptCommand(7, "Review the selected comments.")).toEqual({
      InjectPrompt: { terminal_id: 7, body: "Review the selected comments." },
    });
    expect(writeShellCommand(8, "cargo test")).toEqual({
      WriteShell: { terminal_id: 8, body: "cargo test" },
    });
    expect(
      markActivityReadCommand("github:owner/repo#1", 3, { NodeId: "C_1" }),
    ).toEqual({
      MarkActivityRead: {
        session_key: "github:owner/repo#1",
        index: 3,
        fingerprint: { NodeId: "C_1" },
      },
    });
  });

  it("builds the act-on-work mutations against the workspace key", () => {
    const key = "github:owner/repo#1";
    expect(mergePrCommand(key)).toEqual({ MergePr: { session_key: key } });
    expect(updateBranchCommand(key)).toEqual({
      UpdateBranch: { session_key: key },
    });
    expect(archiveCommand(key)).toEqual({ Archive: { session_key: key } });
    expect(closeIssueCommand(key)).toEqual({
      CloseIssue: { session_key: key },
    });
    expect(deleteOrCloseCommand(key)).toEqual({
      DeleteOrClose: { session_key: key },
    });
    expect(renameWorkspaceCommand(key, "New name")).toEqual({
      RenameWorkspace: { session_key: key, name: "New name" },
    });
  });

  it("maps every desktop workflow intent without exposing internal commands", () => {
    const key = "github:owner/repo#1";
    expect(commandsForWorkspaceIntent(key, { type: "spawn-shell" })).toEqual([
      { SpawnShell: { session_key: key, on_main: false } },
    ]);
    expect(
      commandsForWorkspaceIntent(key, { type: "spawn-shell", onMain: true }),
    ).toEqual([{ SpawnShell: { session_key: key, on_main: true } }]);
    expect(
      commandsForWorkspaceIntent(key, {
        type: "spawn-agent",
        agent: "claude",
        modelAlias: "M",
        onMain: true,
      }),
    ).toEqual([
      {
        SpawnAgent: {
          session_key: key,
          agent: "claude",
          initial_prompt: null,
          model_alias: "M",
          on_main: true,
        },
      },
    ]);
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

  it("builds automation, snooze, sync, and notes commands", () => {
    const key = "github:owner/repo#1";
    expect(setAutoMergeOnGreenCommand(key, true)).toEqual({
      SetAutoMergeOnGreen: { session_key: key, enabled: true },
    });
    expect(setTrackMainCommand(key, false)).toEqual({
      SetTrackMain: { session_key: key, enabled: false },
    });
    expect(setAutoFixPoliciesCommand(key, "Arm", "Disarm")).toEqual({
      SetAutoFixPolicies: { session_key: key, ci: "Arm", conflict: "Disarm" },
    });
    expect(
      snoozeCommand(key, new Date("2026-08-05T09:00:00.000Z")),
    ).toEqual({
      Snooze: { session_key: key, until: "2026-08-05T09:00:00.000Z" },
    });
    expect(unsnoozeCommand(key)).toEqual({ Unsnooze: { session_key: key } });
    expect(syncWorkspaceCommand(key)).toEqual({
      SyncWorkspace: { session_key: key },
    });
    expect(setNotesCommand(key, "flaky job")).toEqual({
      SetNotes: { session_key: key, notes: "flaky job" },
    });
    expect(
      inspectWorkspaceDiffCommand(key, { Session: "session-uuid" }),
    ).toEqual({
      InspectWorkspaceDiff: {
        session_key: key,
        target: { Session: "session-uuid" },
      },
    });
    expect(inspectWorkspaceDiffCommand(key, "LinkedCheckout")).toEqual({
      InspectWorkspaceDiff: { session_key: key, target: "LinkedCheckout" },
    });
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
