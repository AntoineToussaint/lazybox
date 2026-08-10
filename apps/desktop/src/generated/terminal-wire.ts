export const DESKTOP_PROTOCOL_VERSION = 3 as const;
export const DESKTOP_PROTOCOL_FINGERPRINT = 1366899356 as const;
export const TERMINAL_SERVER_FRAME_HEADER_BYTES = 25 as const;
export const TERMINAL_CLIENT_FRAME_HEADER_BYTES = 9 as const;
export const TERMINAL_SERVER_FRAME_LAYOUT = {
  lengthOffset: 0,
  lengthPrefixBytes: 4,
  kindOffset: 4,
  terminalIdOffset: 5,
  firstSeqOffset: 13,
  lastSeqOffset: 21,
  payloadOffset: 29,
} as const;
export const TERMINAL_CLIENT_FRAME_LAYOUT = {
  lengthOffset: 0,
  lengthPrefixBytes: 4,
  kindOffset: 4,
  terminalIdOffset: 5,
  payloadOffset: 13,
} as const;
export const TERMINAL_RESIZE_PAYLOAD_LAYOUT = {
  bytes: 4,
  colsOffset: 0,
  rowsOffset: 2,
} as const;
export const TERMINAL_RESYNC_PAYLOAD_LAYOUT = {
  bytes: 8,
  requiredSeqOffset: 0,
} as const;
export const TERMINAL_WRITE_PAYLOAD_LAYOUT = {
  intentOffset: 0,
  bytesOffset: 1,
} as const;
export const TERMINAL_INPUT_INTENTS = {
  compose: 0,
  submit: 1,
  view: 2,
} as const;
export const TERMINAL_SERVER_FRAME_KINDS = {
  snapshot: 1,
  output: 2,
  resync: 3,
  scrollback: 4,
  resyncUnavailable: 5,
} as const;
export const TERMINAL_CLIENT_COMMAND_KINDS = {
  write: 1,
  resize: 2,
  resync: 3,
  close: 4,
  fetchScrollback: 5,
} as const;
export const DESKTOP_TERMINAL_STREAM_ITEMS = {
  reset: 0,
  data: 1,
} as const;
