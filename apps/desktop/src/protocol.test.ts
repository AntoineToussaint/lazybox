import { describe, expect, it } from "vitest";
import { spawnAgentCommand } from "./protocol";

describe("IPC command JSON", () => {
  it("matches the generated serde shape for control commands", () => {
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
  });
});
