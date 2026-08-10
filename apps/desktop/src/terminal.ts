import {
  DESKTOP_TERMINAL_STREAM_ITEMS,
  TERMINAL_INPUT_INTENTS,
  TERMINAL_CLIENT_COMMAND_KINDS,
  TERMINAL_SERVER_FRAME_HEADER_BYTES,
  TERMINAL_CLIENT_FRAME_HEADER_BYTES,
  TERMINAL_CLIENT_FRAME_LAYOUT,
  TERMINAL_RESIZE_PAYLOAD_LAYOUT,
  TERMINAL_RESYNC_PAYLOAD_LAYOUT,
  TERMINAL_SERVER_FRAME_LAYOUT,
  TERMINAL_SERVER_FRAME_KINDS,
  TERMINAL_WRITE_PAYLOAD_LAYOUT,
} from "./generated/terminal-wire";

export { TERMINAL_SERVER_FRAME_HEADER_BYTES };

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

export interface TerminalReplayState {
  replay: Uint8Array;
  lastSeq: number;
  replayAvailable: boolean;
  dirty: boolean;
}

export type TerminalInputIntent = "compose" | "submit" | "view";

export type TerminalStreamItem =
  | { kind: "reset" }
  | { kind: "disconnected"; message: string }
  | { kind: "data"; payload: Uint8Array };

export interface PendingTerminalInput {
  bytes: number[];
  intent: TerminalInputIntent;
}

const serverKinds: Record<number, TerminalBinaryFrameKind | undefined> = {
  [TERMINAL_SERVER_FRAME_KINDS.snapshot]: "snapshot",
  [TERMINAL_SERVER_FRAME_KINDS.output]: "output",
  [TERMINAL_SERVER_FRAME_KINDS.resync]: "resync",
  [TERMINAL_SERVER_FRAME_KINDS.scrollback]: "scrollback",
  [TERMINAL_SERVER_FRAME_KINDS.resyncUnavailable]: "resync-unavailable",
};

export class TerminalFrameDecoder {
  private buffer = new Uint8Array();

  constructor(private readonly maxFrameBytes: number) {}

  reset(): void {
    this.buffer = new Uint8Array();
  }

  push(chunk: ArrayBuffer | Uint8Array): TerminalBinaryFrame[] {
    const incoming =
      chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
    const joined = new Uint8Array(this.buffer.length + incoming.length);
    joined.set(this.buffer);
    joined.set(incoming, this.buffer.length);
    this.buffer = joined;

    const frames: TerminalBinaryFrame[] = [];
    let offset = 0;
    while (
      this.buffer.length - offset >=
      TERMINAL_SERVER_FRAME_LAYOUT.lengthPrefixBytes
    ) {
      const view = new DataView(
        this.buffer.buffer,
        this.buffer.byteOffset + offset,
      );
      const bodyLength = view.getUint32(
        TERMINAL_SERVER_FRAME_LAYOUT.lengthOffset,
      );
      if (
        bodyLength < TERMINAL_SERVER_FRAME_HEADER_BYTES ||
        bodyLength > this.maxFrameBytes
      ) {
        this.reset();
        throw new Error(`invalid terminal frame length ${bodyLength}`);
      }
      if (
        this.buffer.length - offset <
        TERMINAL_SERVER_FRAME_LAYOUT.lengthPrefixBytes + bodyLength
      ) {
        break;
      }
      const kind =
        serverKinds[view.getUint8(TERMINAL_SERVER_FRAME_LAYOUT.kindOffset)];
      if (kind === undefined) {
        this.reset();
        throw new Error(
          `unknown terminal frame kind ${view.getUint8(TERMINAL_SERVER_FRAME_LAYOUT.kindOffset)}`,
        );
      }
      const terminalId = readSafeU64(
        view,
        TERMINAL_SERVER_FRAME_LAYOUT.terminalIdOffset,
      );
      const firstSeq = readSafeU64(
        view,
        TERMINAL_SERVER_FRAME_LAYOUT.firstSeqOffset,
      );
      const seq = readSafeU64(view, TERMINAL_SERVER_FRAME_LAYOUT.lastSeqOffset);
      const payloadStart = offset + TERMINAL_SERVER_FRAME_LAYOUT.payloadOffset;
      const payloadEnd =
        offset + TERMINAL_SERVER_FRAME_LAYOUT.lengthPrefixBytes + bodyLength;
      frames.push({
        kind,
        terminalId,
        firstSeq,
        seq,
        payload: this.buffer.slice(payloadStart, payloadEnd),
      });
      offset += TERMINAL_SERVER_FRAME_LAYOUT.lengthPrefixBytes + bodyLength;
    }
    this.buffer = this.buffer.slice(offset);
    return frames;
  }
}

export function discardTerminalView(state: TerminalReplayState): void {
  state.replay = new Uint8Array();
  state.replayAvailable = false;
  state.dirty = true;
}

export function requiredTerminalResyncSequence(
  lastSeq: number,
  replayAvailable: boolean,
): number {
  return replayAvailable ? lastSeq + 1 : lastSeq;
}

export function decodeTerminalStreamItem(
  chunk: ArrayBuffer | Uint8Array,
): TerminalStreamItem {
  const bytes = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
  if (bytes[0] === DESKTOP_TERMINAL_STREAM_ITEMS.reset && bytes.length === 1) {
    return { kind: "reset" };
  }
  if (bytes[0] === DESKTOP_TERMINAL_STREAM_ITEMS.data) {
    return { kind: "data", payload: bytes.slice(1) };
  }
  if (bytes[0] === DESKTOP_TERMINAL_STREAM_ITEMS.disconnected) {
    return {
      kind: "disconnected",
      message: new TextDecoder().decode(bytes.slice(1)),
    };
  }
  throw new Error("invalid native terminal stream item");
}

export function appendTerminalInput(
  pending: PendingTerminalInput[],
  bytes: Uint8Array,
  intent: TerminalInputIntent,
  maxBufferedBytes: number,
): void {
  if (!Number.isInteger(maxBufferedBytes) || maxBufferedBytes < 1) {
    throw new Error("terminal input buffer limit must be a positive integer");
  }
  let offset = 0;
  const tail = pending.at(-1);
  if (tail?.intent === intent && tail.bytes.length < maxBufferedBytes) {
    const take = Math.min(maxBufferedBytes - tail.bytes.length, bytes.length);
    for (; offset < take; offset += 1) {
      tail.bytes.push(bytes[offset]!);
    }
    if (offset < bytes.length) {
      tail.intent = "compose";
    }
  }
  while (offset < bytes.length) {
    const end = Math.min(offset + maxBufferedBytes, bytes.length);
    const chunk: number[] = [];
    for (; offset < end; offset += 1) {
      chunk.push(bytes[offset]!);
    }
    pending.push({
      bytes: chunk,
      intent: offset === bytes.length ? intent : "compose",
    });
  }
}

export async function sendTerminalFramesSequentially(
  frames: Iterable<Uint8Array>,
  send: (frame: Uint8Array) => Promise<void>,
): Promise<void> {
  for (const frame of frames) {
    await send(frame);
  }
}

export function writeTerminalFrame(
  terminalId: number,
  payload: Uint8Array,
  intent: TerminalInputIntent = "compose",
): Uint8Array {
  const body = new Uint8Array(
    TERMINAL_WRITE_PAYLOAD_LAYOUT.bytesOffset + payload.length,
  );
  body[TERMINAL_WRITE_PAYLOAD_LAYOUT.intentOffset] =
    TERMINAL_INPUT_INTENTS[intent];
  body.set(payload, TERMINAL_WRITE_PAYLOAD_LAYOUT.bytesOffset);
  return encodeClientFrame(
    TERMINAL_CLIENT_COMMAND_KINDS.write,
    terminalId,
    body,
  );
}

export function writeTerminalFrames(
  terminalId: number,
  payload: Uint8Array,
  maxWriteBytes: number,
  intent: TerminalInputIntent = "compose",
): Uint8Array[] {
  if (!Number.isInteger(maxWriteBytes) || maxWriteBytes < 1) {
    throw new Error("terminal write limit must be a positive integer");
  }
  const frames: Uint8Array[] = [];
  for (let offset = 0; offset < payload.length; offset += maxWriteBytes) {
    const nextOffset = offset + maxWriteBytes;
    frames.push(
      writeTerminalFrame(
        terminalId,
        payload.slice(offset, nextOffset),
        nextOffset >= payload.length ? intent : "compose",
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
  const payload = new Uint8Array(TERMINAL_RESIZE_PAYLOAD_LAYOUT.bytes);
  const view = new DataView(payload.buffer);
  view.setUint16(TERMINAL_RESIZE_PAYLOAD_LAYOUT.colsOffset, cols);
  view.setUint16(TERMINAL_RESIZE_PAYLOAD_LAYOUT.rowsOffset, rows);
  return encodeClientFrame(
    TERMINAL_CLIENT_COMMAND_KINDS.resize,
    terminalId,
    payload,
  );
}

export function resyncTerminalFrame(
  terminalId: number,
  requiredSeq: number,
): Uint8Array {
  const payload = new Uint8Array(TERMINAL_RESYNC_PAYLOAD_LAYOUT.bytes);
  new DataView(payload.buffer).setBigUint64(
    TERMINAL_RESYNC_PAYLOAD_LAYOUT.requiredSeqOffset,
    BigInt(requiredSeq),
  );
  return encodeClientFrame(
    TERMINAL_CLIENT_COMMAND_KINDS.resync,
    terminalId,
    payload,
  );
}

export function closeTerminalFrame(terminalId: number): Uint8Array {
  return encodeClientFrame(
    TERMINAL_CLIENT_COMMAND_KINDS.close,
    terminalId,
    new Uint8Array(),
  );
}

export function fetchTerminalScrollbackFrame(terminalId: number): Uint8Array {
  return encodeClientFrame(
    TERMINAL_CLIENT_COMMAND_KINDS.fetchScrollback,
    terminalId,
    new Uint8Array(),
  );
}

function encodeClientFrame(
  kind: number,
  terminalId: number,
  payload: Uint8Array,
): Uint8Array {
  const bodyLength = TERMINAL_CLIENT_FRAME_HEADER_BYTES + payload.length;
  const frame = new Uint8Array(
    TERMINAL_CLIENT_FRAME_LAYOUT.lengthPrefixBytes + bodyLength,
  );
  const view = new DataView(frame.buffer);
  view.setUint32(TERMINAL_CLIENT_FRAME_LAYOUT.lengthOffset, bodyLength);
  view.setUint8(TERMINAL_CLIENT_FRAME_LAYOUT.kindOffset, kind);
  view.setBigUint64(
    TERMINAL_CLIENT_FRAME_LAYOUT.terminalIdOffset,
    BigInt(terminalId),
  );
  frame.set(payload, TERMINAL_CLIENT_FRAME_LAYOUT.payloadOffset);
  return frame;
}

function readSafeU64(view: DataView, offset: number): number {
  const value = view.getBigUint64(offset);
  if (value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error(
      `terminal frame integer ${value} exceeds JavaScript precision`,
    );
  }
  return Number(value);
}
