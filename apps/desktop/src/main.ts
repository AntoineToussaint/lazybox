import { Channel, invoke } from "@tauri-apps/api/core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import {
  applyWorkspaceEvent,
  primaryTask,
  sortedWorkspaces,
  taskReference,
  unreadCount,
} from "./model";
import {
  type DesktopInfo,
  type DesktopStreamMessage,
  type LazyboxCommand,
  type LazyboxEvent,
  type TerminalKind,
  type TerminalSnapshot,
  type Workspace,
  type WorkspacesResponse,
  requestResyncCommand,
  resizeCommand,
  spawnAgentCommand,
  terminalKindLabel,
  writeCommand,
} from "./protocol";

export interface TerminalRecord {
  id: number;
  sessionKey: string;
  kind: TerminalKind;
  replay: number[];
  lastSeq: number;
  replayAvailable: boolean;
  dirty: boolean;
  state: string;
}

interface ActiveTerminal {
  id: number;
  terminal: Terminal;
  fit: FitAddon;
  disposeInput: () => void;
  disposeResize: () => void;
  resyncing: boolean;
}

const workspaceList = element<HTMLDivElement>("workspace-list");
const workspaceCount = element<HTMLSpanElement>("workspace-count");
const unreadTotal = element<HTMLSpanElement>("unread-count");
const workspaceEmpty = element<HTMLDivElement>("workspace-empty");
const workspaceDetail = element<HTMLDivElement>("workspace-detail");
const taskKicker = element<HTMLParagraphElement>("task-kicker");
const taskTitle = element<HTMLHeadingElement>("task-title");
const taskMeta = element<HTMLParagraphElement>("task-meta");
const taskDescription = element<HTMLDivElement>("task-description");
const activityCount = element<HTMLSpanElement>("activity-count");
const activityList = element<HTMLDivElement>("activity-list");
const agentLabel = element<HTMLElement>("agent-label");
const spawnButton = element<HTMLButtonElement>("spawn-button");
const refreshButton = element<HTMLButtonElement>("refresh-button");
const terminalHost = element<HTMLDivElement>("terminal");
const terminalEmpty = element<HTMLDivElement>("terminal-empty");
const terminalTitle = element<HTMLHeadingElement>("terminal-title");
const terminalState = element<HTMLSpanElement>("terminal-state");
const connectionDot = element<HTMLSpanElement>("connection-dot");
const connectionLabel = element<HTMLSpanElement>("connection-label");
const statusMessage = element<HTMLSpanElement>("status-message");

let workspaces = new Map<string, Workspace>();
let terminals = new Map<number, TerminalRecord>();
let selectedKey: string | null = null;
let defaultAgent = "claude";
let previewMode = false;
let activeTerminal: ActiveTerminal | null = null;
let resizeTimer: number | undefined;
const pendingInput = new Map<number, number[]>();
const inputTimers = new Map<number, number>();
const encoder = new TextEncoder();

refreshButton.addEventListener("click", () => {
  void sendCommand("Refresh", "Refreshing providers…");
});

spawnButton.addEventListener("click", () => {
  if (selectedKey !== null) {
    void sendCommand(
      spawnAgentCommand(selectedKey, defaultAgent),
      `Starting ${defaultAgent}…`,
    );
  }
});

window.addEventListener("resize", () => scheduleResize());

void boot();

async function boot(): Promise<void> {
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("preview")) {
    const { loadPreview } = await import("./preview");
    const preview = loadPreview();
    previewMode = true;
    defaultAgent = preview.defaultAgent;
    workspaces = preview.workspaces;
    terminals = preview.terminals;
    setConnection(true, "Preview data");
    selectWorkspace(preview.selectedKey);
    render();
    return;
  }

  try {
    const info = await invoke<DesktopInfo>("desktop_info");
    defaultAgent = info.default_agent;
    agentLabel.textContent = defaultAgent;
    const initial = await invoke<WorkspacesResponse>("list_workspaces");
    workspaces = new Map(
      initial.workspaces.map((workspace) => [workspace.key, workspace]),
    );
    if (initial.warnings.length > 0) {
      setStatus(initial.warnings[0] ?? "Some workspaces could not be decoded.");
    }
    chooseInitialWorkspace();
    render();

    const events = new Channel<DesktopStreamMessage>();
    events.onmessage = handleStreamMessage;
    await invoke("subscribe_events", { onEvent: events });
  } catch (error) {
    setConnection(false, "Daemon unavailable");
    setStatus(String(error));
  }
}

function handleStreamMessage(message: DesktopStreamMessage): void {
  if (message.type === "Connected") {
    setConnection(true, "Live");
    return;
  }
  if (message.type === "Disconnected") {
    setConnection(false, "Reconnecting…");
    setStatus(message.payload.message);
    return;
  }
  handleEvent(message.payload.payload);
}

function handleEvent(event: LazyboxEvent): void {
  const workspaceChanged =
    "Snapshot" in event ||
    "WorkspaceUpserted" in event ||
    "WorkspaceRemoved" in event;
  if (workspaceChanged) {
    workspaces = applyWorkspaceEvent(workspaces, event);
    if (selectedKey !== null && !workspaces.has(selectedKey)) {
      selectedKey = null;
    }
  }

  if ("Snapshot" in event) {
    detachTerminal();
    terminals = new Map(
      event.Snapshot.terminals.map((snapshot) => [
        snapshot.terminal_id,
        terminalFromSnapshot(snapshot),
      ]),
    );
    chooseInitialWorkspace();
    attachSelectedTerminal();
    setStatus("Inbox and terminal replay are synchronized.");
  } else if ("TerminalSpawned" in event) {
    const payload = event.TerminalSpawned;
    terminals.set(payload.terminal_id, {
      id: payload.terminal_id,
      sessionKey: payload.session_key,
      kind: payload.kind,
      replay: [],
      lastSeq: 0,
      replayAvailable: true,
      dirty: false,
      state: "running",
    });
    if (payload.session_key === selectedKey) {
      attachTerminal(payload.terminal_id);
    }
  } else if ("TerminalOutput" in event) {
    handleTerminalOutput(event.TerminalOutput);
  } else if ("TerminalResync" in event) {
    handleTerminalResync(event.TerminalResync);
  } else if ("TerminalResyncUnavailable" in event) {
    if (activeTerminal?.id === event.TerminalResyncUnavailable.terminal_id) {
      activeTerminal.resyncing = false;
      setTerminalState("waiting for replay");
    }
  } else if ("TerminalExited" in event) {
    const record = terminals.get(event.TerminalExited.terminal_id);
    if (record !== undefined) {
      record.state = `exited ${event.TerminalExited.exit_code ?? ""}`.trim();
    }
    if (activeTerminal?.id === event.TerminalExited.terminal_id) {
      setTerminalState(record?.state ?? "exited");
      if (event.TerminalExited.last_output !== null) {
        activeTerminal.terminal.write(`\r\n${event.TerminalExited.last_output}`);
      }
    }
  } else if ("TerminalFocusRequested" in event) {
    attachTerminal(event.TerminalFocusRequested.terminal_id);
  } else if ("AgentState" in event) {
    const record = terminals.get(event.AgentState.terminal_id);
    if (record !== undefined) {
      record.state = formatAgentState(event.AgentState.state);
    }
    if (activeTerminal?.id === event.AgentState.terminal_id) {
      setTerminalState(record?.state ?? "running");
    }
  } else if ("ProviderError" in event) {
    setStatus(`${event.ProviderError.source}: ${event.ProviderError.message}`);
  } else if ("CommandRejected" in event) {
    setStatus(`${event.CommandRejected.command}: ${event.CommandRejected.reason}`);
  } else if ("PollProgress" in event) {
    setStatus(event.PollProgress.message);
  } else if ("PollCompleted" in event) {
    setStatus(
      `${event.PollCompleted.source}: ${event.PollCompleted.count} tasks synchronized.`,
    );
  } else if ("WorktreeProgress" in event) {
    setStatus(
      `Preparing workspace: ${event.WorktreeProgress.step.toLowerCase()}`,
    );
  }

  if (workspaceChanged) {
    render();
  }
}

function render(): void {
  renderInbox();
  renderWorkspace();
  agentLabel.textContent = defaultAgent;
}

function renderInbox(): void {
  const items = sortedWorkspaces(workspaces.values());
  workspaceList.replaceChildren();
  const unread = items.reduce((sum, workspace) => sum + unreadCount(workspace), 0);
  workspaceCount.textContent = `${items.length} workspace${items.length === 1 ? "" : "s"}`;
  unreadTotal.textContent = `${unread} unread`;

  if (items.length === 0) {
    const empty = document.createElement("p");
    empty.className = "inbox-empty";
    empty.textContent = "No persisted workspaces yet.";
    workspaceList.append(empty);
    return;
  }

  for (const workspace of items) {
    const task = primaryTask(workspace);
    const button = document.createElement("button");
    button.className = "workspace-row";
    button.classList.toggle("selected", workspace.key === selectedKey);
    button.addEventListener("click", () => selectWorkspace(workspace.key));

    const top = document.createElement("span");
    top.className = "workspace-row-top";
    const reference = document.createElement("span");
    reference.className = "workspace-reference";
    reference.textContent = taskReference(task);
    const state = document.createElement("span");
    state.className = `task-state task-state-${(task?.state ?? "local").toLowerCase()}`;
    state.textContent = task?.state ?? "local";
    top.append(reference, state);

    const title = document.createElement("strong");
    title.textContent = task?.title ?? workspace.name;
    const bottom = document.createElement("span");
    bottom.className = "workspace-row-bottom";
    const repo = document.createElement("span");
    repo.textContent = task?.repo ?? workspace.branch;
    const count = unreadCount(workspace);
    const unreadBadge = document.createElement("span");
    unreadBadge.className = "unread-badge";
    unreadBadge.textContent = count > 0 ? String(count) : "·";
    bottom.append(repo, unreadBadge);
    button.append(top, title, bottom);
    workspaceList.append(button);
  }
}

function renderWorkspace(): void {
  const workspace = selectedKey === null ? undefined : workspaces.get(selectedKey);
  workspaceEmpty.classList.toggle("hidden", workspace !== undefined);
  workspaceDetail.classList.toggle("hidden", workspace === undefined);
  if (workspace === undefined) {
    return;
  }

  const task = primaryTask(workspace);
  taskKicker.textContent = task === null ? "Local workspace" : taskReference(task);
  taskTitle.textContent = task?.title ?? workspace.name;
  taskMeta.textContent = [
    task?.repo,
    task?.role?.toLowerCase(),
    task?.ci === undefined ? null : `CI ${task.ci.toLowerCase()}`,
    workspace.branch,
  ]
    .filter((value): value is string => Boolean(value))
    .join(" · ");
  taskDescription.textContent =
    task?.body?.trim() || "No description was provided for this workspace.";

  activityList.replaceChildren();
  activityCount.textContent = String(workspace.activity.length);
  if (workspace.activity.length === 0) {
    const empty = document.createElement("p");
    empty.className = "activity-empty";
    empty.textContent = "No activity yet.";
    activityList.append(empty);
    return;
  }

  for (const activity of workspace.activity.slice(0, 30)) {
    const card = document.createElement("article");
    card.className = "activity-card";
    const heading = document.createElement("div");
    const author = document.createElement("strong");
    author.textContent = activity.author;
    const time = document.createElement("time");
    time.dateTime = activity.created_at;
    time.textContent = relativeTime(activity.created_at);
    heading.append(author, time);
    const body = document.createElement("p");
    body.textContent = activity.body;
    card.append(heading, body);
    activityList.append(card);
  }
}

function selectWorkspace(key: string): void {
  selectedKey = key;
  render();
  attachSelectedTerminal();
  void sendCommand({ FocusWorkspace: { session_key: key } });
}

function chooseInitialWorkspace(): void {
  if (selectedKey !== null && workspaces.has(selectedKey)) {
    return;
  }
  selectedKey = sortedWorkspaces(workspaces.values())[0]?.key ?? null;
}

function attachSelectedTerminal(): void {
  if (selectedKey === null) {
    detachTerminal();
    return;
  }
  const record = [...terminals.values()].find(
    (terminal) => terminal.sessionKey === selectedKey,
  );
  if (record === undefined) {
    detachTerminal();
    return;
  }
  attachTerminal(record.id);
}

function attachTerminal(id: number): void {
  const record = terminals.get(id);
  if (record === undefined || activeTerminal?.id === id) {
    return;
  }
  detachTerminal();
  const terminal = new Terminal({
    convertEol: false,
    cursorBlink: true,
    fontFamily: '"SFMono-Regular", "Cascadia Code", "Liberation Mono", monospace',
    fontSize: 13,
    lineHeight: 1.18,
    scrollback: 10_000,
    theme: {
      background: "#0a0d12",
      foreground: "#dce5e8",
      cursor: "#82d8bd",
      cursorAccent: "#0a0d12",
      selectionBackground: "#2a554d",
      black: "#0a0d12",
      brightBlack: "#5d6c73",
      green: "#82d8bd",
      brightGreen: "#a5ecd7",
      cyan: "#78c7dc",
      brightCyan: "#a4ddec",
      yellow: "#e3bd75",
      brightYellow: "#f1d49a",
      red: "#e47e77",
      brightRed: "#f2a09b",
      white: "#dce5e8",
      brightWhite: "#ffffff",
    },
  });
  const fit = new FitAddon();
  terminal.loadAddon(fit);
  terminalHost.classList.remove("hidden");
  terminalEmpty.classList.add("hidden");
  terminal.open(terminalHost);
  fit.fit();

  if (record.replayAvailable && record.replay.length > 0) {
    terminal.write(Uint8Array.from(record.replay));
    record.replay = [];
  }

  const inputDisposable = terminal.onData((data) => {
    queueTerminalInput(id, [...encoder.encode(data)]);
  });
  const resizeDisposable = terminal.onResize(({ cols, rows }) => {
    void sendCommand(resizeCommand(id, cols, rows));
  });
  activeTerminal = {
    id,
    terminal,
    fit,
    disposeInput: () => inputDisposable.dispose(),
    disposeResize: () => resizeDisposable.dispose(),
    resyncing: false,
  };
  terminalTitle.textContent = `${terminalKindLabel(record.kind)} · ${record.sessionKey}`;
  setTerminalState(record.state);
  scheduleResize();
  terminal.focus();

  if (record.dirty || !record.replayAvailable) {
    requestTerminalResync(record);
  }
}

function detachTerminal(): void {
  if (activeTerminal !== null) {
    activeTerminal.disposeInput();
    activeTerminal.disposeResize();
    activeTerminal.terminal.dispose();
    activeTerminal = null;
  }
  terminalHost.replaceChildren();
  terminalHost.classList.add("hidden");
  terminalEmpty.classList.remove("hidden");
  terminalTitle.textContent = "No terminal attached";
  setTerminalState("idle");
}

function handleTerminalOutput(payload: {
  terminal_id: number;
  bytes: number[];
  first_seq: number;
  seq: number;
}): void {
  const record = terminals.get(payload.terminal_id);
  if (record === undefined) {
    return;
  }
  if (activeTerminal?.id !== payload.terminal_id) {
    record.dirty = true;
    return;
  }
  if (activeTerminal.resyncing || payload.seq <= record.lastSeq) {
    return;
  }
  if (
    payload.first_seq !== record.lastSeq + 1 &&
    !(record.lastSeq === 0 && payload.first_seq === 0)
  ) {
    requestTerminalResync(record);
    return;
  }
  activeTerminal.terminal.write(Uint8Array.from(payload.bytes));
  record.lastSeq = payload.seq;
  record.dirty = false;
}

function handleTerminalResync(payload: {
  terminal_id: number;
  replay: number[];
  seq: number;
}): void {
  const record = terminals.get(payload.terminal_id);
  if (record === undefined) {
    return;
  }
  record.replay = payload.replay;
  record.lastSeq = payload.seq;
  record.replayAvailable = true;
  record.dirty = false;
  if (activeTerminal?.id === payload.terminal_id) {
    activeTerminal.terminal.reset();
    activeTerminal.terminal.write(Uint8Array.from(payload.replay));
    activeTerminal.resyncing = false;
    setTerminalState(record.state);
  }
}

function requestTerminalResync(record: TerminalRecord): void {
  if (activeTerminal?.id === record.id && !activeTerminal.resyncing) {
    activeTerminal.resyncing = true;
    setTerminalState("resyncing");
    void sendCommand(requestResyncCommand(record.id, record.lastSeq + 1));
  }
}

function queueTerminalInput(id: number, bytes: number[]): void {
  const pending = pendingInput.get(id) ?? [];
  pending.push(...bytes);
  pendingInput.set(id, pending);
  if (inputTimers.has(id)) {
    return;
  }
  const timer = window.setTimeout(() => {
    inputTimers.delete(id);
    const buffered = pendingInput.get(id) ?? [];
    pendingInput.delete(id);
    if (buffered.length > 0) {
      void sendCommand(writeCommand(id, buffered));
    }
  }, 12);
  inputTimers.set(id, timer);
}

function scheduleResize(): void {
  window.clearTimeout(resizeTimer);
  resizeTimer = window.setTimeout(() => {
    if (activeTerminal === null) {
      return;
    }
    activeTerminal.fit.fit();
    void sendCommand(
      resizeCommand(
        activeTerminal.id,
        activeTerminal.terminal.cols,
        activeTerminal.terminal.rows,
      ),
    );
  }, 80);
}

async function sendCommand(
  command: LazyboxCommand,
  pendingMessage?: string,
): Promise<void> {
  if (pendingMessage !== undefined) {
    setStatus(pendingMessage);
  }
  if (previewMode) {
    return;
  }
  try {
    await invoke("send_command", { command });
  } catch (error) {
    setStatus(String(error));
  }
}

function terminalFromSnapshot(snapshot: TerminalSnapshot): TerminalRecord {
  return {
    id: snapshot.terminal_id,
    sessionKey: snapshot.session_key,
    kind: snapshot.kind,
    replay: snapshot.replay,
    lastSeq: snapshot.last_seq,
    replayAvailable: snapshot.replay_available,
    dirty: false,
    state:
      snapshot.agent_state === null
        ? "running"
        : formatAgentState(snapshot.agent_state),
  };
}

function formatAgentState(
  state: string | { Exited: { code: number | null } },
): string {
  if (typeof state === "string") {
    return state.toLowerCase();
  }
  return `exited ${state.Exited.code ?? ""}`.trim();
}

function setConnection(connected: boolean, label: string): void {
  connectionDot.classList.toggle("connected", connected);
  connectionLabel.textContent = label;
}

function setStatus(message: string): void {
  statusMessage.textContent = message;
}

function setTerminalState(state: string): void {
  terminalState.textContent = state;
  terminalState.dataset.state = state;
}

function relativeTime(value: string): string {
  const timestamp = Date.parse(value);
  if (!Number.isFinite(timestamp)) {
    return "";
  }
  const seconds = Math.round((timestamp - Date.now()) / 1000);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  if (Math.abs(seconds) < 60) {
    return formatter.format(seconds, "second");
  }
  const minutes = Math.round(seconds / 60);
  if (Math.abs(minutes) < 60) {
    return formatter.format(minutes, "minute");
  }
  const hours = Math.round(minutes / 60);
  if (Math.abs(hours) < 24) {
    return formatter.format(hours, "hour");
  }
  return formatter.format(Math.round(hours / 24), "day");
}

function element<T extends HTMLElement>(id: string): T {
  const value = document.getElementById(id);
  if (value === null) {
    throw new Error(`missing #${id}`);
  }
  return value as T;
}
