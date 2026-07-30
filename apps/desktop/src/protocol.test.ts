import { describe, expect, it } from "vitest";
import { spawnAgentCommand } from "./protocol";

describe("IPC command JSON", () => {
  it("matches the generated serde shape for control commands", () => {
    expect(spawnAgentCommand("github:owner/repo#1", "codex")).toEqual({
      SpawnAgent: {
        session_key: "github:owner/repo#1",
        agent: "codex",
      },
    });
  });
});
