import { describe, expect, it } from "vitest";
import {
  TerminalFrameDecoder,
  resizeTerminalFrame,
  resyncTerminalFrame,
  writeTerminalFrame,
  writeTerminalFrames,
} from "./terminal";

describe("binary terminal protocol", () => {
  it("encodes raw input without JSON number arrays", () => {
    const frame = writeTerminalFrame(7, new Uint8Array([0, 27, 255]));
    const view = new DataView(frame.buffer);

    expect(view.getUint32(0)).toBe(12);
    expect(view.getUint8(4)).toBe(1);
    expect(view.getBigUint64(5)).toBe(7n);
    expect([...frame.slice(13)]).toEqual([0, 27, 255]);
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
    const frames = writeTerminalFrames(
      7,
      new Uint8Array([0, 1, 2, 3, 4]),
      2,
    );

    expect(frames).toHaveLength(3);
    expect(frames.map((frame) => [...frame.slice(13)])).toEqual([
      [0, 1],
      [2, 3],
      [4],
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
