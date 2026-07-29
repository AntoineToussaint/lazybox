import { describe, expect, it } from "vitest";
import {
  requestResyncCommand,
  resizeCommand,
  spawnAgentCommand,
  writeCommand,
} from "./protocol";

describe("IPC command JSON", () => {
  it("matches the serde shape for terminal commands", () => {
    expect(spawnAgentCommand("github:owner/repo#1", "codex")).toEqual({
      Spawn: {
        session_key: "github:owner/repo#1",
        session_id: null,
        kind: { Agent: "codex" },
        cwd: null,
        initial_prompt: null,
        on_main: false,
        model_alias: null,
      },
    });
    expect(writeCommand(7, [27, 91, 65])).toEqual({
      Write: { terminal_id: 7, bytes: [27, 91, 65] },
    });
    expect(resizeCommand(7, 120, 36)).toEqual({
      Resize: { terminal_id: 7, cols: 120, rows: 36 },
    });
    expect(requestResyncCommand(7, 12)).toEqual({
      RequestTerminalResync: { terminal_id: 7, required_seq: 12 },
    });
  });
});
