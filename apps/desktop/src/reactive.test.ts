// @vitest-environment happy-dom

import { describe, expect, it } from "vitest";
import { MutationLedger, RequestGenerations } from "./reactive";

describe("reactive request generations", () => {
  it("rejects a deferred response after a newer request starts", () => {
    const requests = new RequestGenerations();
    const older = requests.begin("snippets");
    const newer = requests.begin("snippets");
    expect(requests.isCurrent("snippets", older)).toBe(false);
    expect(requests.isCurrent("snippets", newer)).toBe(true);
  });
});

describe("mutation ledger", () => {
  it("rolls a rejected optimistic operation back while retaining history", () => {
    const ledger = new MutationLedger<string>();
    ledger.begin(
      "ws",
      "auto_merge",
      "SetAutoMergeOnGreen",
      "false",
      "true",
      "Arming…",
    );
    expect(ledger.optimistic("ws", "auto_merge")).toBe("true");

    ledger.rejectCommand("SetAutoMergeOnGreen", "queue full");
    expect(ledger.optimistic("ws", "auto_merge")).toBeUndefined();
    expect(ledger.history("ws")).toMatchObject([
      { state: "rejected", message: "queue full", requested: "true" },
    ]);
  });

  it("rolls back only the oldest workspace for a rejection that names no workspace", () => {
    const ledger = new MutationLedger<string>();
    ledger.begin(
      "ws-a",
      "auto_merge",
      "SetAutoMergeOnGreen",
      "false",
      "true",
      "Arming…",
    );
    ledger.begin(
      "ws-b",
      "auto_merge",
      "SetAutoMergeOnGreen",
      "false",
      "true",
      "Arming…",
    );

    const rejected = ledger.rejectCommand("SetAutoMergeOnGreen", "queue full");
    expect(rejected).toHaveLength(1);
    expect(rejected[0]?.workspaceKey).toBe("ws-a");
    // ws-a (the oldest, first-sent) rolls back; ws-b's optimistic arm survives
    // because its command was not the one this rejection referred to.
    expect(ledger.optimistic("ws-a", "auto_merge")).toBeUndefined();
    expect(ledger.optimistic("ws-b", "auto_merge")).toBe("true");
  });

  it("caps retained history so a long session cannot grow it without bound", () => {
    const ledger = new MutationLedger<string>();
    for (let index = 0; index < 500; index += 1) {
      ledger.recordOutcome("ws", `op-${index}`, "confirmed", `#${index}`, "");
    }
    const history = ledger.history();
    expect(history.length).toBe(200);
    // The newest record is retained; the oldest are dropped.
    expect(history.at(-1)?.message).toBe("#499");
    expect(history.some((record) => record.message === "#0")).toBe(false);
  });

  it("confirms only the matching authoritative value", () => {
    const ledger = new MutationLedger<string>();
    ledger.begin(
      "ws",
      "track_main",
      "SetTrackMain",
      "false",
      "true",
      "Arming…",
    );
    expect(ledger.confirmValue("ws", "track_main", "false")).toBe(false);
    expect(ledger.optimistic("ws", "track_main")).toBe("true");
    expect(ledger.confirmValue("ws", "track_main", "true")).toBe(true);
    expect(ledger.history("ws")[0]?.state).toBe("confirmed");
  });
});
