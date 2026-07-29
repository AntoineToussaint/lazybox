export const TERMINAL_SERVER_FRAME_HEADER_BYTES = 25;

export type TerminalBinaryFrameKind =
  | "snapshot"
  | "output"
  | "resync"
  | "scrollback"
  | "resync-unavailable";

export interface TerminalBinaryFrame {
  kind: TerminalBinaryFrameKind;
  terminalId: number;
  firstSeq: number;
  seq: number;
  payload: Uint8Array;
}

const serverKinds: Record<number, TerminalBinaryFrameKind | undefined> = {
  1: "snapshot",
  2: "output",
  3: "resync",
  4: "scrollback",
  5: "resync-unavailable",
};

export class TerminalFrameDecoder {
  private buffer = new Uint8Array();

  constructor(private readonly maxFrameBytes: number) {}

  push(chunk: ArrayBuffer | Uint8Array): TerminalBinaryFrame[] {
    const incoming =
      chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
    const joined = new Uint8Array(this.buffer.length + incoming.length);
    joined.set(this.buffer);
    joined.set(incoming, this.buffer.length);
    this.buffer = joined;

    const frames: TerminalBinaryFrame[] = [];
    let offset = 0;
    while (this.buffer.length - offset >= 4) {
      const view = new DataView(
        this.buffer.buffer,
        this.buffer.byteOffset + offset,
      );
      const bodyLength = view.getUint32(0);
      if (
        bodyLength < TERMINAL_SERVER_FRAME_HEADER_BYTES ||
        bodyLength > this.maxFrameBytes
      ) {
        throw new Error(`invalid terminal frame length ${bodyLength}`);
      }
      if (this.buffer.length - offset < 4 + bodyLength) {
        break;
      }
      const kind = serverKinds[view.getUint8(4)];
      if (kind === undefined) {
        throw new Error(`unknown terminal frame kind ${view.getUint8(4)}`);
      }
      const terminalId = readSafeU64(view, 5);
      const firstSeq = readSafeU64(view, 13);
      const seq = readSafeU64(view, 21);
      const payloadStart = offset + 4 + TERMINAL_SERVER_FRAME_HEADER_BYTES;
      const payloadEnd = offset + 4 + bodyLength;
      frames.push({
        kind,
        terminalId,
        firstSeq,
        seq,
        payload: this.buffer.slice(payloadStart, payloadEnd),
      });
      offset += 4 + bodyLength;
    }
    this.buffer = this.buffer.slice(offset);
    return frames;
  }
}

export function writeTerminalFrame(
  terminalId: number,
  payload: Uint8Array,
): Uint8Array {
  return encodeClientFrame(1, terminalId, payload);
}

export function writeTerminalFrames(
  terminalId: number,
  payload: Uint8Array,
  maxWriteBytes: number,
): Uint8Array[] {
  if (!Number.isInteger(maxWriteBytes) || maxWriteBytes < 1) {
    throw new Error("terminal write limit must be a positive integer");
  }
  const frames: Uint8Array[] = [];
  for (let offset = 0; offset < payload.length; offset += maxWriteBytes) {
    frames.push(
      writeTerminalFrame(
        terminalId,
        payload.slice(offset, offset + maxWriteBytes),
      ),
    );
  }
  return frames;
}

export function resizeTerminalFrame(
  terminalId: number,
  cols: number,
  rows: number,
): Uint8Array {
  const payload = new Uint8Array(4);
  const view = new DataView(payload.buffer);
  view.setUint16(0, cols);
  view.setUint16(2, rows);
  return encodeClientFrame(2, terminalId, payload);
}

export function resyncTerminalFrame(
  terminalId: number,
  requiredSeq: number,
): Uint8Array {
  const payload = new Uint8Array(8);
  new DataView(payload.buffer).setBigUint64(0, BigInt(requiredSeq));
  return encodeClientFrame(3, terminalId, payload);
}

export function closeTerminalFrame(terminalId: number): Uint8Array {
  return encodeClientFrame(4, terminalId, new Uint8Array());
}

function encodeClientFrame(
  kind: number,
  terminalId: number,
  payload: Uint8Array,
): Uint8Array {
  const bodyLength = 9 + payload.length;
  const frame = new Uint8Array(4 + bodyLength);
  const view = new DataView(frame.buffer);
  view.setUint32(0, bodyLength);
  view.setUint8(4, kind);
  view.setBigUint64(5, BigInt(terminalId));
  frame.set(payload, 13);
  return frame;
}

function readSafeU64(view: DataView, offset: number): number {
  const value = view.getBigUint64(offset);
  if (value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error(`terminal frame integer ${value} exceeds JavaScript precision`);
  }
  return Number(value);
}
