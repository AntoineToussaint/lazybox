import type {
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
  LazyboxCommand,
  LazyboxEvent,
  Task,
  TerminalKind,
  TerminalSnapshot,
  Workspace,
  WorkspacesResponse,
} from "./generated";

export type {
  DesktopInfo,
  DesktopRepository,
  DesktopStreamMessage,
  LazyboxCommand,
  LazyboxEvent,
  Task,
  TerminalKind,
  TerminalSnapshot,
  Workspace,
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

export function spawnShellCommand(sessionKey: string): LazyboxCommand {
  return { SpawnShell: { session_key: sessionKey } };
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

export function markReadCommand(sessionKey: string): LazyboxCommand {
  return { MarkRead: { session_key: sessionKey } };
}

export function postReplyCommand(
  sessionKey: string,
  body: string,
): LazyboxCommand {
  return { PostReply: { session_key: sessionKey, body } };
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
