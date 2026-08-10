import type { Activity } from "./generated/Activity";
import type { PolicyArm } from "./generated/PolicyArm";
import type { DesktopCleanupReason } from "./generated/DesktopCleanupReason";
import type {
  ComputeOutcome,
  ActivityFingerprint,
  DesktopInboxView,
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
  DiffFileDto,
  DiffHunkDto,
  DiffLineDto,
  DiffLineKindDto,
  Filter,
  FilterAxis,
  FilterMenuItem,
  LazyboxCommand,
  LazyboxEvent,
  Mailbox,
  PickerRow,
  RepoSummary,
  SnippetGroup,
  SnippetPickerView,
  SortMode,
  Task,
  TerminalKind,
  TerminalSnapshot,
  VisibleRow,
  Workspace,
  WorkspaceDiffDto,
  WorkspaceDiffTarget,
  WorkspaceKind,
  WorkspacesResponse,
  UserPrompt,
} from "./generated";

export type {
  Activity,
  ComputeOutcome,
  ActivityFingerprint,
  DesktopCleanupReason,
  DesktopInboxView,
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
  DiffFileDto,
  DiffHunkDto,
  DiffLineDto,
  DiffLineKindDto,
  Filter,
  FilterAxis,
  FilterMenuItem,
  LazyboxCommand,
  LazyboxEvent,
  Mailbox,
  PickerRow,
  PolicyArm,
  RepoSummary,
  SnippetGroup,
  SnippetPickerView,
  SortMode,
  Task,
  TerminalKind,
  TerminalSnapshot,
  VisibleRow,
  Workspace,
  WorkspaceDiffDto,
  WorkspaceDiffTarget,
  WorkspaceKind,
  WorkspacesResponse,
  UserPrompt,
};

export function spawnAgentCommand(
  sessionKey: string,
  agent: string,
  modelAlias: string | null = null,
  onMain = false,
  initialPrompt: string | null = null,
): LazyboxCommand {
  return {
    SpawnAgent: {
      session_key: sessionKey,
      agent,
      initial_prompt: initialPrompt,
      model_alias: modelAlias,
      on_main: onMain,
    },
  };
}

export function spawnShellCommand(
  sessionKey: string,
  onMain = false,
): LazyboxCommand {
  return {
    SpawnShell: {
      session_key: sessionKey,
      on_main: onMain,
    },
  };
}

export function mergePrCommand(sessionKey: string): LazyboxCommand {
  return { MergePr: { session_key: sessionKey } };
}

export function updateBranchCommand(sessionKey: string): LazyboxCommand {
  return { UpdateBranch: { session_key: sessionKey } };
}

export function archiveCommand(sessionKey: string): LazyboxCommand {
  return { Archive: { session_key: sessionKey } };
}

export function closeIssueCommand(sessionKey: string): LazyboxCommand {
  return { CloseIssue: { session_key: sessionKey } };
}

export function deleteOrCloseCommand(sessionKey: string): LazyboxCommand {
  return { DeleteOrClose: { session_key: sessionKey } };
}

export function renameWorkspaceCommand(
  sessionKey: string,
  name: string,
): LazyboxCommand {
  return { RenameWorkspace: { session_key: sessionKey, name } };
}

export function createWorkspaceCommand(
  name: string,
  projectKey: string,
  agent: string | null,
): LazyboxCommand {
  return {
    CreateWorkspace: {
      name,
      project_key: projectKey,
      agent,
    },
  };
}

export type WorkspaceIntent =
  | {
      type: "spawn-agent";
      agent: string;
      modelAlias?: string | null;
      onMain?: boolean;
      initialPrompt?: string | null;
    }
  | { type: "spawn-shell"; onMain?: boolean }
  | { type: "mark-read" }
  | { type: "reply"; body: string };

export function commandsForWorkspaceIntent(
  sessionKey: string | null,
  intent: WorkspaceIntent,
): LazyboxCommand[] {
  if (sessionKey === null) {
    return [];
  }
  switch (intent.type) {
    case "spawn-agent":
      return [
        spawnAgentCommand(
          sessionKey,
          intent.agent,
          intent.modelAlias ?? null,
          intent.onMain ?? false,
          intent.initialPrompt ?? null,
        ),
      ];
    case "spawn-shell":
      return [spawnShellCommand(sessionKey, intent.onMain ?? false)];
    case "mark-read":
      return [{ MarkRead: { session_key: sessionKey } }];
    case "reply": {
      const body = intent.body.trim();
      if (body.length === 0) {
        return [];
      }
      return [{ PostReply: { session_key: sessionKey, body } }];
    }
  }
}

export function deliverSnippetCommand(
  terminalId: number,
  row: PickerRow,
): LazyboxCommand {
  return {
    DeliverSnippet: {
      terminal_id: terminalId,
      snippet_key: row.key,
      category: row.category,
      body: row.body,
    },
  };
}

export function injectPromptCommand(
  terminalId: number,
  body: string,
): LazyboxCommand {
  return { InjectPrompt: { terminal_id: terminalId, body } };
}

export function writeShellCommand(
  terminalId: number,
  body: string,
): LazyboxCommand {
  return { WriteShell: { terminal_id: terminalId, body } };
}

export function markActivityReadCommand(
  sessionKey: string,
  index: number,
  fingerprint: ActivityFingerprint,
): LazyboxCommand {
  return { MarkActivityRead: { session_key: sessionKey, index, fingerprint } };
}

export function keepWorkspaceCommand(sessionKey: string): LazyboxCommand {
  return { KeepWorkspace: { session_key: sessionKey } };
}

export function removeMergedWorkspaceCommand(
  sessionKey: string,
): LazyboxCommand {
  return { RemoveMergedWorkspace: { session_key: sessionKey } };
}

export function adoptSessionsCommand(
  sourceWorkspaceKey: string,
  targetWorkspaceKey: string,
): LazyboxCommand {
  return {
    AdoptSessions: {
      source_workspace_key: sourceWorkspaceKey,
      target_workspace_key: targetWorkspaceKey,
    },
  };
}

export function requestReviewersCommand(
  workspaceKey: string,
  logins: string[],
): LazyboxCommand {
  return { RequestReviewers: { workspace_key: workspaceKey, logins } };
}

export function setAssigneesCommand(
  workspaceKey: string,
  logins: string[],
): LazyboxCommand {
  return { SetAssignees: { workspace_key: workspaceKey, logins } };
}

export function setLabelsCommand(
  workspaceKey: string,
  names: string[],
): LazyboxCommand {
  return { SetLabels: { workspace_key: workspaceKey, names } };
}

export function setAutoMergeOnGreenCommand(
  sessionKey: string,
  enabled: boolean,
): LazyboxCommand {
  return { SetAutoMergeOnGreen: { session_key: sessionKey, enabled } };
}

export function setTrackMainCommand(
  sessionKey: string,
  enabled: boolean,
): LazyboxCommand {
  return { SetTrackMain: { session_key: sessionKey, enabled } };
}

export function setAutoFixPoliciesCommand(
  sessionKey: string,
  ci: PolicyArm,
  conflict: PolicyArm,
): LazyboxCommand {
  return { SetAutoFixPolicies: { session_key: sessionKey, ci, conflict } };
}

export function snoozeCommand(
  sessionKey: string,
  until: Date,
): LazyboxCommand {
  return { Snooze: { session_key: sessionKey, until: until.toISOString() } };
}

export function unsnoozeCommand(sessionKey: string): LazyboxCommand {
  return { Unsnooze: { session_key: sessionKey } };
}

export function syncWorkspaceCommand(sessionKey: string): LazyboxCommand {
  return { SyncWorkspace: { session_key: sessionKey } };
}

export function setNotesCommand(
  sessionKey: string,
  notes: string,
): LazyboxCommand {
  return { SetNotes: { session_key: sessionKey, notes } };
}

export function inspectWorkspaceDiffCommand(
  sessionKey: string,
  target: WorkspaceDiffTarget,
): LazyboxCommand {
  return { InspectWorkspaceDiff: { session_key: sessionKey, target } };
}

export function terminalKindLabel(kind: TerminalKind): string {
  if (kind === "Shell") {
    return "shell";
  }
  if ("Agent" in kind) {
    return kind.Agent;
  }
  return kind.LogTail.path;
}
