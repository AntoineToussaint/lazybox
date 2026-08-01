import type {
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
  Filter,
  FilterAxis,
  FilterMenuItem,
  InboxView,
  LazyboxCommand,
  LazyboxEvent,
  PickerRow,
  RepoSummary,
  SnippetGroup,
  SnippetPickerView,
  SortMode,
  StatusTag,
  Task,
  TerminalKind,
  TerminalSnapshot,
  VisibleRow,
  Workspace,
  WorkspaceKind,
  WorkspaceRow,
  WorkspacesResponse,
} from "./generated";

export type {
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
  Filter,
  FilterAxis,
  FilterMenuItem,
  InboxView,
  LazyboxCommand,
  LazyboxEvent,
  PickerRow,
  RepoSummary,
  SnippetGroup,
  SnippetPickerView,
  SortMode,
  StatusTag,
  Task,
  TerminalKind,
  TerminalSnapshot,
  VisibleRow,
  Workspace,
  WorkspaceKind,
  WorkspaceRow,
  WorkspacesResponse,
};

export function spawnAgentCommand(
  sessionKey: string,
  agent: string,
): LazyboxCommand {
  return {
    SpawnAgent: {
      session_key: sessionKey,
      agent,
    },
  };
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
  | { type: "spawn-agent"; agent: string }
  | { type: "spawn-shell" }
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
      return [spawnAgentCommand(sessionKey, intent.agent)];
    case "spawn-shell":
      return [{ SpawnShell: { session_key: sessionKey } }];
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
