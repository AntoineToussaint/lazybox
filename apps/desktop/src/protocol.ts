import type {
  DesktopInfo,
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
    Spawn: {
      session_key: sessionKey,
      session_id: null,
      kind: { Agent: agent },
      cwd: null,
      initial_prompt: null,
      on_main: false,
      model_alias: null,
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
