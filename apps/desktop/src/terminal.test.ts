import { describe, expect, it } from "vitest";
import {
  TerminalFrameDecoder,
  appendTerminalInput,
  decodeTerminalStreamItem,
  discardTerminalView,
  requiredTerminalResyncSequence,
  resizeTerminalFrame,
  resyncTerminalFrame,
  sendTerminalFramesSequentially,
  writeTerminalFrame,
  writeTerminalFrames,
} from "./terminal";

describe("binary terminal protocol", () => {
  it("encodes raw input without JSON number arrays", () => {
    const frame = writeTerminalFrame(7, new Uint8Array([0, 27, 255]));
    const view = new DataView(frame.buffer);

    expect(view.getUint32(0)).toBe(13);
    expect(view.getUint8(4)).toBe(1);
    expect(view.getBigUint64(5)).toBe(7n);
    expect([...frame.slice(13)]).toEqual([0, 0, 27, 255]);
  });

  it("encodes resize and resync metadata in network byte order", () => {
    const resize = new DataView(resizeTerminalFrame(3, 120, 40).buffer);
    expect(resize.getUint8(4)).toBe(2);
    expect(resize.getUint16(13)).toBe(120);
    expect(resize.getUint16(15)).toBe(40);

    const resync = new DataView(resyncTerminalFrame(3, 44).buffer);
    expect(resync.getUint8(4)).toBe(3);
    expect(resync.getBigUint64(13)).toBe(44n);
  });

  it("chunks large writes at the daemon-advertised limit", () => {
    const frames = writeTerminalFrames(7, new Uint8Array([0, 1, 2, 3, 4]), 2);

    expect(frames).toHaveLength(3);
    expect(frames.map((frame) => [...frame.slice(13)])).toEqual([
      [0, 0, 1],
      [0, 2, 3],
      [0, 4],
    ]);
  });

  it("marks only the final chunk of a submitted write as submit", () => {
    const frames = writeTerminalFrames(
      7,
      new Uint8Array([0, 1, 2, 3, 4]),
      2,
      "submit",
    );

    expect(frames.map((frame) => [...frame.slice(13)])).toEqual([
      [0, 0, 1],
      [0, 2, 3],
      [1, 4],
    ]);
  });

  it("decodes fragmented and batched server frames", () => {
    const first = serverFrame(2, 7, 11, 12, new Uint8Array([0, 1, 255]));
    const second = serverFrame(3, 7, 0, 14, new Uint8Array([9, 8]));
    const bytes = new Uint8Array(first.length + second.length);
    bytes.set(first);
    bytes.set(second, first.length);
    const decoder = new TerminalFrameDecoder(2 * 1024 * 1024 + 25);

    expect(decoder.push(bytes.slice(0, 9))).toEqual([]);
    const decoded = decoder.push(bytes.slice(9));

    expect(decoded).toHaveLength(2);
    expect(decoded[0]).toMatchObject({
      kind: "output",
      terminalId: 7,
      firstSeq: 11,
      seq: 12,
    });
    expect([...decoded[0]!.payload]).toEqual([0, 1, 255]);
    expect(decoded[1]).toMatchObject({
      kind: "resync",
      terminalId: 7,
      seq: 14,
    });
  });

  it("decodes an unavailable resync without a JSON control event", () => {
    const frame = serverFrame(5, 7, 0, 0, new Uint8Array());
    const decoder = new TerminalFrameDecoder(2048);

    expect(decoder.push(frame)).toEqual([
      {
        kind: "resync-unavailable",
        terminalId: 7,
        firstSeq: 0,
        seq: 0,
        payload: new Uint8Array(),
      },
    ]);
  });

  it("rejects oversized frames before allocating their payload", () => {
    const bytes = new Uint8Array(4);
    new DataView(bytes.buffer).setUint32(0, 2049);
    const decoder = new TerminalFrameDecoder(2048);

    expect(() => decoder.push(bytes)).toThrow("invalid terminal frame length");
  });

  it("drops a malformed prefix so the following valid frame can render", () => {
    const malformed = new Uint8Array(4);
    new DataView(malformed.buffer).setUint32(0, 2049);
    const fresh = serverFrame(2, 7, 10, 10, new Uint8Array([9]));
    const decoder = new TerminalFrameDecoder(2048);

    expect(() => decoder.push(malformed)).toThrowError(
      "invalid terminal frame length 2049",
    );
    expect(decoder.push(fresh)).toMatchObject([
      { kind: "output", terminalId: 7, firstSeq: 10, seq: 10 },
    ]);
  });

  it("invalidates a discarded view and requests an attainable replay baseline", () => {
    const state = {
      replay: new Uint8Array(),
      lastSeq: 42,
      replayAvailable: true,
      dirty: false,
    };

    discardTerminalView(state);

    expect(state).toEqual({
      replay: new Uint8Array(),
      lastSeq: 42,
      replayAvailable: false,
      dirty: true,
    });
    expect(
      requiredTerminalResyncSequence(state.lastSeq, state.replayAvailable),
    ).toBe(42);
    expect(requiredTerminalResyncSequence(42, true)).toBe(43);
  });

  it("resets an incomplete frame before decoding a reconnected stream", () => {
    const stale = serverFrame(2, 7, 1, 1, new Uint8Array([1, 2, 3]));
    const fresh = serverFrame(1, 7, 0, 9, new Uint8Array([9, 8]));
    const decoder = new TerminalFrameDecoder(2048);

    expect(decoder.push(stale.slice(0, stale.length - 1))).toEqual([]);
    decoder.reset();

    expect(decoder.push(fresh)).toEqual([
      {
        kind: "snapshot",
        terminalId: 7,
        firstSeq: 0,
        seq: 9,
        payload: new Uint8Array([9, 8]),
      },
    ]);
  });

  it("decodes native reset and data items without JSON byte arrays", () => {
    expect(decodeTerminalStreamItem(new Uint8Array([0]))).toEqual({
      kind: "reset",
    });
    expect(decodeTerminalStreamItem(new Uint8Array([1, 4, 5]))).toEqual({
      kind: "data",
      payload: new Uint8Array([4, 5]),
    });
    expect(
      decodeTerminalStreamItem(
        new Uint8Array([2, ...new TextEncoder().encode("connection lost")]),
      ),
    ).toEqual({ kind: "disconnected", message: "connection lost" });
  });

  it("bounds a large coalesced append without variadic array spreading", () => {
    const pending: Array<{
      bytes: number[];
      intent: "compose" | "submit" | "view";
    }> = [{ bytes: [1, 2], intent: "compose" }];
    const bytes = new Uint8Array(300_000).fill(7);

    appendTerminalInput(pending, bytes, "compose", 64 * 1024);

    expect(pending.every((input) => input.bytes.length <= 64 * 1024)).toBe(
      true,
    );
    expect(pending.reduce((sum, input) => sum + input.bytes.length, 0)).toBe(
      300_002,
    );
    expect(pending.at(-1)?.intent).toBe("compose");
  });

  it("waits for each terminal frame before sending the next one", async () => {
    const frames = [
      new Uint8Array([1]),
      new Uint8Array([2]),
      new Uint8Array([3]),
    ];
    const sent: number[] = [];
    let concurrent = 0;
    let maxConcurrent = 0;

    await sendTerminalFramesSequentially(frames, async (frame) => {
      concurrent += 1;
      maxConcurrent = Math.max(maxConcurrent, concurrent);
      await Promise.resolve();
      sent.push(frame[0]!);
      concurrent -= 1;
    });

    expect(sent).toEqual([1, 2, 3]);
    expect(maxConcurrent).toBe(1);
  });
});

function serverFrame(
  kind: number,
  terminalId: number,
  firstSeq: number,
  seq: number,
  payload: Uint8Array,
): Uint8Array {
  const frame = new Uint8Array(4 + 25 + payload.length);
  const view = new DataView(frame.buffer);
  view.setUint32(0, 25 + payload.length);
  view.setUint8(4, kind);
  view.setBigUint64(5, BigInt(terminalId));
  view.setBigUint64(13, BigInt(firstSeq));
  view.setBigUint64(21, BigInt(seq));
  frame.set(payload, 29);
  return frame;
}
