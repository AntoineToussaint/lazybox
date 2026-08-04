import type {
  ComputeOutcome,
  DesktopInboxView,
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
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
  WorkspaceKind,
  WorkspacesResponse,
} from "./generated";

export type {
  ComputeOutcome,
  DesktopInboxView,
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
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
  WorkspaceKind,
  WorkspacesResponse,
};

export function spawnAgentCommand(
  sessionKey: string,
  agent: string,
  modelAlias: string | null = null,
  onMain = false,
): LazyboxCommand {
  return {
    SpawnAgent: {
      session_key: sessionKey,
      agent,
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

export function terminalKindLabel(kind: TerminalKind): string {
  if (kind === "Shell") {
    return "shell";
  }
  if ("Agent" in kind) {
    return kind.Agent;
  }
  return kind.LogTail.path;
}
