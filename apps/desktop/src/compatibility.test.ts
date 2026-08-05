import { describe, expect, it } from "vitest";
import fixture from "./generated/compatibility.json";
import {
  DESKTOP_PROTOCOL_FINGERPRINT,
  DESKTOP_PROTOCOL_VERSION,
} from "./generated";

function variantTag(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  if (typeof value !== "object" || value === null) {
    throw new Error("wire fixture variant must be a string or object");
  }
  const keys = Object.keys(value);
  if (keys.length !== 1 || keys[0] === undefined) {
    throw new Error("wire fixture variant must have exactly one tag");
  }
  return keys[0];
}

describe("generated desktop compatibility fixture", () => {
  it("matches the supported protocol version", () => {
    expect(fixture.protocol_version).toBe(DESKTOP_PROTOCOL_VERSION);
    expect(fixture.protocol_fingerprint).toBe(DESKTOP_PROTOCOL_FINGERPRINT);
  });

  it("covers every exported command and event variant", () => {
    expect(new Set(fixture.commands.map(variantTag)).size).toBe(21);
    expect(new Set(fixture.events.map(variantTag)).size).toBe(14);
  });
});
