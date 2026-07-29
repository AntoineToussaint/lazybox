export interface TaskId {
  source: string;
  key: string;
}

export interface Task {
  id: TaskId;
  title: string;
  body: string | null;
  state: string;
  role: string;
  ci: string;
  review: string;
  unread_count: number;
  url: string;
  repo: string | null;
  updated_at: string;
  kind: "Pr" | "Issue" | null;
}

export interface Activity {
  author: string;
  body: string;
  created_at: string;
  kind: string;
}

export interface Workspace {
  key: string;
  name: string;
  branch: string;
  pr: Task | null;
  gh_issues: Task[];
  linear_issues: Task[];
  activity: Activity[];
  seen_count: number;
  read_indices: number[];
  sessions: unknown[];
}

export type TerminalKind =
  | { Agent: string }
  | "Shell"
  | { LogTail: { path: string } };

export interface TerminalSnapshot {
  terminal_id: number;
  session_key: string;
  kind: TerminalKind;
  replay: number[];
  last_seq: number;
  replay_available: boolean;
  agent_state: string | { Exited: { code: number | null } } | null;
}

export type LazyboxEvent =
  | {
      Snapshot: {
        workspaces: Workspace[];
        terminals: TerminalSnapshot[];
      };
    }
  | { WorkspaceUpserted: Workspace }
  | { WorkspaceRemoved: string }
  | {
      TerminalSpawned: {
        terminal_id: number;
        session_key: string;
        kind: TerminalKind;
      };
    }
  | {
      TerminalOutput: {
        terminal_id: number;
        bytes: number[];
        first_seq: number;
        seq: number;
      };
    }
  | {
      TerminalResync: {
        terminal_id: number;
        replay: number[];
        seq: number;
      };
    }
  | { TerminalResyncUnavailable: { terminal_id: number } }
  | {
      TerminalExited: {
        terminal_id: number;
        exit_code: number | null;
        last_output: string | null;
      };
    }
  | { TerminalFocusRequested: { terminal_id: number } }
  | {
      AgentState: {
        session_key: string;
        terminal_id: number;
        state: string | { Exited: { code: number | null } };
      };
    }
  | {
      ProviderError: {
        source: string;
        message: string;
        detail: string;
        kind: string;
      };
    }
  | { CommandRejected: { command: string; reason: string } }
  | { PollProgress: { source: string; message: string } }
  | { PollCompleted: { source: string; count: number } }
  | {
      WorktreeProgress: {
        session_key: string;
        step: string;
        status: string | { Failed: { message: string } };
      };
    };

export interface JsonServerFrame {
  type: "Event";
  payload: LazyboxEvent;
}

export type DesktopStreamMessage =
  | { type: "Connected" }
  | { type: "Disconnected"; payload: { message: string } }
  | { type: "Frame"; payload: JsonServerFrame };

export interface DesktopInfo {
  agents: string[];
  default_agent: string;
}

export interface WorkspacesResponse {
  workspaces: Workspace[];
  warnings: string[];
}

export type LazyboxCommand = string | Record<string, unknown>;

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

export function writeCommand(
  terminalId: number,
  bytes: number[],
): LazyboxCommand {
  return { Write: { terminal_id: terminalId, bytes } };
}

export function resizeCommand(
  terminalId: number,
  cols: number,
  rows: number,
): LazyboxCommand {
  return { Resize: { terminal_id: terminalId, cols, rows } };
}

export function requestResyncCommand(
  terminalId: number,
  requiredSeq: number,
): LazyboxCommand {
  return {
    RequestTerminalResync: {
      terminal_id: terminalId,
      required_seq: requiredSeq,
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
