import { describe, expect, it } from "vitest";
import fixture from "./generated/compatibility.json";
import { applyWorkspaceEvent, filteredWorkspaces } from "./model";
import {
  postReplyCommand,
  spawnAgentCommand,
  type LazyboxEvent,
} from "./protocol";
import { canCompleteSetup, type DesktopSetupStatus } from "./setup";
import { TerminalFrameDecoder } from "./terminal";

describe("credential-free desktop workflow fixture", () => {
  it("runs setup, inbox, reply, agent, and terminal output without live services", () => {
    const setup: DesktopSetupStatus = {
      completed: false,
      github: {
        id: "github",
        label: "GitHub",
        available: true,
        detail: "Authenticated as @fixture",
      },
      agents: [
        {
          id: "codex",
          label: "Codex",
          available: true,
          detail: "codex fixture",
        },
      ],
      selected_repositories: [],
      default_agent: null,
      analytics_enabled: false,
      crash_reports_enabled: false,
    };
    expect(
      canCompleteSetup(setup, {
        repositories: ["github:o/r"],
        default_agent: "codex",
        analytics_enabled: false,
        crash_reports_enabled: false,
      }),
    ).toBe(true);

    const snapshot = fixture.events[0] as unknown as LazyboxEvent;
    const workspaces = applyWorkspaceEvent(new Map(), snapshot);
    const selected = filteredWorkspaces(workspaces.values(), "PR o/r", "all")[0];
    expect(selected?.key).toBe("github-o-r-42");

    expect(postReplyCommand(selected!.key, "Ready to ship.")).toEqual({
      PostReply: {
        session_key: "github-o-r-42",
        body: "Ready to ship.",
      },
    });
    expect(spawnAgentCommand(selected!.key, "codex")).toEqual({
      SpawnAgent: {
        session_key: "github-o-r-42",
        agent: "codex",
      },
    });

    const decoder = new TerminalFrameDecoder(2048);
    const output = new TextEncoder().encode("agent ready\r\n");
    const frame = serverOutputFrame(7, 1, 1, output);
    const decoded = decoder.push(frame);
    expect(decoded).toHaveLength(1);
    expect(new TextDecoder().decode(decoded[0]!.payload)).toBe("agent ready\r\n");
  });
});

function serverOutputFrame(
  terminalId: number,
  firstSeq: number,
  lastSeq: number,
  payload: Uint8Array,
): Uint8Array {
  const frame = new Uint8Array(29 + payload.length);
  const view = new DataView(frame.buffer);
  view.setUint32(0, 25 + payload.length);
  view.setUint8(4, 2);
  view.setBigUint64(5, BigInt(terminalId));
  view.setBigUint64(13, BigInt(firstSeq));
  view.setBigUint64(21, BigInt(lastSeq));
  frame.set(payload, 29);
  return frame;
}
