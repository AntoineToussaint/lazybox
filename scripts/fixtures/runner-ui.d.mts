// Types for the fake Runner (runner-ui.mjs), for TypeScript callers such as
// the scenarios (scenarios). They name the parts a caller reaches for most
// and leave the rest open: the fixture is JavaScript and grows with the tests,
// so an untyped field is reachable rather than an error.

/** One Runner journal event, as the fixture pushes it to WatchEvents. */
export interface JournalEvent {
  type: string;
  text?: string;
  status?: string;
  operationId?: string;
  requestId?: string;
  turnId?: string;
  agentId?: string;
  parentAgentId?: string;
  name?: string;
  /** base64 of a JSON value, as the Runner sends provider data. */
  providerDataJson?: string;
  receipt?: { operationId: string; delivery: string };
  [key: string]: unknown;
}

/** A conversation's journal (openEvents): what was pushed, and the readers following it. */
export interface EventChannel {
  events: { cursor: string; event: JournalEvent; capturedAt?: string }[];
  /** Appends an event; `capturedAt` (ISO time) is when the Runner recorded it. */
  push(event: JournalEvent, capturedAt?: string): void;
  /** Ends the journal like a saved conversation's (EOF). */
  complete(): void;
  /** Ends the open streams like a restarting Runner. */
  cut(): void;
  /** Replays in batches of `pace` records, `paceMs` apart, like the Runner's stream. */
  pace?: number;
  paceMs?: number;
}

/** A session (an agent's execution) as Info, GetSession and ListHistory report it. */
export interface FixtureSession {
  id: string;
  collectionId?: string;
  workId?: string;
  profileId?: string;
  provider?: string;
  model?: string;
  modelReported?: boolean;
  title?: string;
  status?: string;
  busy?: boolean;
  closed?: boolean;
  permission?: string;
  workingDirectory?: string;
  capabilities?: Record<string, boolean>;
  pending?: JournalEvent[];
  agents?: Record<string, unknown>[];
  queuedMessages?: { operationId: string; text: string; queuedAtUnixMillis: string }[];
  [key: string]: any;
}

/** A workspace (a collection of repositories). */
export interface FixtureCollection {
  id: string;
  name: string;
  root: string;
  state: string;
  members: { spec: { name: string; repositoryId: string; baseRef?: string; branch?: string }; path: string; baseCommit?: string; ready: boolean }[];
  [key: string]: any;
}

/** A managed command whose output streams while it is watched (streamCommand). */
export interface StreamedCommand {
  id: string;
  sessionId: string;
  title: string;
  done: boolean;
  /** What its agent received (COMMAND_OUTPUT.md): exit status, sizes, filter, full-output path. */
  report?: Record<string, unknown>;
  write(text: string): void;
  finish(exitCode?: number): void;
  [key: string]: any;
}

export interface RunnerFixture {
  readonly url: string;
  readonly token: string;
  /** Every RPC received, in order. */
  calls: { method: string; body: any; device?: string }[];
  /** Every RPC that changed something. */
  mutations: { method: string; body: any; device?: string }[];
  collections: FixtureCollection[];
  sessions: FixtureSession[];
  library: { id: string; name?: string; path: string; gitIdentity?: string; sourceUrl?: string; [key: string]: any }[];
  commands: any[];
  processes: any[];
  profiles: any[];
  githubRepositories: any[];
  github: Record<string, any>;
  runner: Record<string, any>;
  remote: Record<string, any>;
  tracked: { links: any[]; events: any[]; trackingError: string; refusal: string; requests: any[]; sent: any[] };
  items: { issues: any[]; searches: any[]; handle(method: string, body: any): unknown };
  missions: Record<string, any>;
  artifacts: { artifacts: Map<string, any>; agentPut(input: Record<string, unknown>): any; fileChanged(collectionId: string, path: string, text: string): void; [key: string]: any };
  workspaceFiles: Map<string, { path: string; text: string; version: string; binary: boolean; truncated: boolean }>;
  /** Opens (or replaces) a conversation's journal. */
  openEvents(sessionId: string): EventChannel;
  /** A conversation's open journal, if it has one, without replacing it. */
  eventsOf(sessionId: string): EventChannel | undefined;
  streamCommand(sessionId: string, title: string, argv?: string[]): StreamedCommand;
  startRun(sessionId: string, title: string, report: Record<string, unknown>): any;
  shipState(collectionId: string, member: string): Record<string, any>;
  setCheckpoints(sessionId: string, value: { turns: unknown[] }): unknown;
  completeGitHub(): void;
  close(): Promise<void>;
  [key: string]: any;
}

export function startRunnerFixture(): Promise<RunnerFixture>;
export function defaultModelPrices(): unknown[];
