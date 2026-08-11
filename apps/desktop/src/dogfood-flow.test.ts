import { describe, expect, it } from "vitest";
import {
  TERMINAL_CLIENT_FRAME_LAYOUT,
  TERMINAL_WRITE_PAYLOAD_LAYOUT,
} from "./generated/terminal-wire";
import { loadPreview } from "./preview";
import {
  applyWorkspaceEvent,
  orderedWorkspaceKeys,
  primaryTask,
} from "./model";
import { commandsForWorkspaceIntent } from "./protocol";
import {
  discardTerminalView,
  requiredTerminalResyncSequence,
  resyncTerminalFrame,
  writeTerminalFrame,
} from "./terminal";

describe("desktop inbox-to-terminal workflow mapping", () => {
  it("triages, replies, starts work, and reconnects terminal replay", () => {
    const fixture = loadPreview();
    // The order comes from the shared view-model, not a TS sort; pick
    // the first row it placed.
    const [firstKey] = orderedWorkspaceKeys(fixture.inboxView);
    const workspace =
      firstKey === undefined ? undefined : fixture.workspaces.get(firstKey);
    expect(workspace).toBeDefined();
    expect(primaryTask(workspace!)?.needs_reply).toBe(true);

    const key = workspace!.key;
    expect(commandsForWorkspaceIntent(key, { type: "mark-read" })).toHaveLength(
      1,
    );
    expect(
      commandsForWorkspaceIntent(key, {
        type: "reply",
        body: "Replay is covered by the reconnect fixture.",
      }),
    ).toEqual([
      {
        PostReply: {
          session_key: key,
          body: "Replay is covered by the reconnect fixture.",
        },
      },
    ]);
    expect(
      commandsForWorkspaceIntent(key, {
        type: "spawn-agent",
        agent: fixture.defaultAgent,
      }),
    ).toHaveLength(1);
    expect(
      commandsForWorkspaceIntent(key, { type: "spawn-shell" }),
    ).toHaveLength(1);

    const terminal = [...fixture.terminals.values()].find(
      (candidate) => candidate.sessionKey === key,
    );
    expect(terminal).toBeDefined();
    expect(new TextDecoder().decode(terminal!.replay)).toContain(
      "Waiting for your next instruction",
    );
    const input = new TextEncoder().encode("status\n");
    const inputFrame = writeTerminalFrame(terminal!.id, input);
    const inputOffset =
      TERMINAL_CLIENT_FRAME_LAYOUT.payloadOffset +
      TERMINAL_WRITE_PAYLOAD_LAYOUT.bytesOffset;
    expect(inputFrame.slice(inputOffset)).toEqual(input);

    discardTerminalView(terminal!);
    const requiredSequence = requiredTerminalResyncSequence(
      terminal!.lastSeq,
      terminal!.replayAvailable,
    );
    const resync = new DataView(
      resyncTerminalFrame(terminal!.id, requiredSequence).buffer,
    );
    expect(resync.getBigUint64(13)).toBe(BigInt(terminal!.lastSeq));

    const updated = {
      ...workspace!,
      seen_count: workspace!.activity.length,
    };
    const next = applyWorkspaceEvent(fixture.workspaces, {
      WorkspaceUpserted: updated,
    });
    expect(next.get(key)?.seen_count).toBe(workspace!.activity.length);
  });
});
