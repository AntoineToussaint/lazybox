import { Channel, invoke } from "@tauri-apps/api/core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import {
  CommandOutcomeTracker,
  applyWorkspaceEvent,
  filteredWorkspaces,
  primaryTask,
  taskReference,
  unreadCount,
  type WorkspaceFilter,
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
  commandsForWorkspaceIntent,
  spawnAgentCommand,
  terminalKindLabel,
} from "./protocol";
import {
  TerminalFrameDecoder,
  type TerminalBinaryFrame,
  type TerminalInputIntent,
  type TerminalReplayState,
  decodeTerminalStreamItem,
  discardTerminalView,
  resizeTerminalFrame,
  requiredTerminalResyncSequence,
  resyncTerminalFrame,
  sendTerminalFramesSequentially,
  writeTerminalFrames,
} from "./terminal";

export interface TerminalRecord extends TerminalReplayState {
  id: number;
  sessionKey: string;
  kind: TerminalKind;
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

interface DesktopAgentOption {
  id: string;
  label: string;
  available: boolean;
}

interface DesktopSetupState {
  first_run: boolean;
  selected_repositories: string[];
  agents: DesktopAgentOption[];
  default_agent: string;
  analytics_enabled: boolean;
  diagnostics_path: string;
}

interface GithubAuthStatus {
  authenticated: boolean;
  account: string | null;
  message: string;
}

interface GithubRepositoryOption {
  id: string;
  label: string;
  owner: string;
}

type AnalyticsEvent =
  | "app_opened"
  | "workspace_opened"
  | "agent_started"
  | "shell_started"
  | "reply_posted";

const workspaceList = element<HTMLDivElement>("workspace-list");
const workspaceCount = element<HTMLSpanElement>("workspace-count");
const unreadTotal = element<HTMLSpanElement>("unread-count");
const workspaceSearch = element<HTMLInputElement>("workspace-search");
const workspaceFilter = element<HTMLSelectElement>("workspace-filter");
const workspaceEmpty = element<HTMLDivElement>("workspace-empty");
const workspaceDetail = element<HTMLDivElement>("workspace-detail");
const taskKicker = element<HTMLParagraphElement>("task-kicker");
const taskTitle = element<HTMLHeadingElement>("task-title");
const taskMeta = element<HTMLParagraphElement>("task-meta");
const taskDescription = element<HTMLDivElement>("task-description");
const taskSignals = element<HTMLDivElement>("task-signals");
const activityCount = element<HTMLSpanElement>("activity-count");
const activityList = element<HTMLDivElement>("activity-list");
const agentLabel = element<HTMLElement>("agent-label");
const spawnButton = element<HTMLButtonElement>("spawn-button");
const shellButton = element<HTMLButtonElement>("shell-button");
const markReadButton = element<HTMLButtonElement>("mark-read-button");
const replyForm = element<HTMLFormElement>("reply-form");
const replyBody = element<HTMLTextAreaElement>("reply-body");
const refreshButton = element<HTMLButtonElement>("refresh-button");
const settingsButton = element<HTMLButtonElement>("settings-button");
const terminalHost = element<HTMLDivElement>("terminal");
const terminalEmpty = element<HTMLDivElement>("terminal-empty");
const terminalTitle = element<HTMLHeadingElement>("terminal-title");
const terminalState = element<HTMLSpanElement>("terminal-state");
const connectionDot = element<HTMLSpanElement>("connection-dot");
const connectionLabel = element<HTMLSpanElement>("connection-label");
const statusMessage = element<HTMLSpanElement>("status-message");
const setupDialog = element<HTMLDialogElement>("setup-dialog");
const setupForm = element<HTMLFormElement>("setup-form");
const setupTitle = element<HTMLHeadingElement>("setup-title");
const setupClose = element<HTMLButtonElement>("setup-close");
const githubAuthMessage = element<HTMLParagraphElement>("github-auth-message");
const githubAuthBadge = element<HTMLSpanElement>("github-auth-badge");
const githubLoginButton = element<HTMLButtonElement>("github-login-button");
const githubCheckButton = element<HTMLButtonElement>("github-check-button");
const discoverReposButton = element<HTMLButtonElement>("discover-repos-button");
const repositorySearch = element<HTMLInputElement>("repository-search");
const repositoryList = element<HTMLDivElement>("repository-list");
const repositorySelectionCount = element<HTMLParagraphElement>(
  "repository-selection-count",
);
const defaultAgentSelect =
  element<HTMLSelectElement>("default-agent-select");
const analyticsEnabled = element<HTMLInputElement>("analytics-enabled");
const setupError = element<HTMLParagraphElement>("setup-error");
const diagnosticsPath = element<HTMLSpanElement>("diagnostics-path");
const saveSettingsButton =
  element<HTMLButtonElement>("save-settings-button");
const confirmDialog = element<HTMLDialogElement>("confirm-dialog");
const confirmTitle = element<HTMLHeadingElement>("confirm-title");
const confirmMessage = element<HTMLParagraphElement>("confirm-message");
const confirmPreview = element<HTMLPreElement>("confirm-preview");
const confirmAccept = element<HTMLButtonElement>("confirm-accept");

let workspaces = new Map<string, Workspace>();
let terminals = new Map<number, TerminalRecord>();
let selectedKey: string | null = null;
let visibleWorkspaces: Workspace[] = [];
let defaultAgent = "claude";
let previewMode = false;
let inboxLoading = true;
let inboxError: string | null = null;
let setupState: DesktopSetupState | null = null;
let discoveredRepositories: GithubRepositoryOption[] = [];
let selectedRepositories = new Set<string>();
let setupRequired = false;
let lastSubmittedReply: string | null = null;
const commandOutcomes = new CommandOutcomeTracker();
let focusRequestedSession: string | null = null;
let activeTerminal: ActiveTerminal | null = null;
let resizeTimer: number | undefined;
interface PendingTerminalInput {
  bytes: number[];
  intent: TerminalInputIntent;
}

const pendingInput = new Map<number, PendingTerminalInput[]>();
const inputTimers = new Map<number, number>();
const inputSending = new Set<number>();
const encoder = new TextEncoder();
let terminalDecoder = new TerminalFrameDecoder(2 * 1024 * 1024 + 25);
let maxTerminalWriteBytes = 128 * 1024;
const pendingTerminalFrames = new Map<number, TerminalBinaryFrame[]>();

refreshButton.addEventListener("click", () => {
  void runCommands(["Refresh"], "Refreshing providers…", "Refresh requested.");
});

spawnButton.addEventListener("click", () => {
  void startAgent();
});

shellButton.addEventListener("click", () => {
  void startShell();
});

markReadButton.addEventListener("click", () => {
  void markSelectedRead();
});

replyForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void reviewReply();
});

workspaceSearch.addEventListener("input", renderInbox);
workspaceFilter.addEventListener("change", renderInbox);
settingsButton.addEventListener("click", () => void openSettings());
setupClose.addEventListener("click", closeSettings);
githubCheckButton.addEventListener("click", () => void refreshGithubAuth());
githubLoginButton.addEventListener("click", () => void startGithubLogin());
discoverReposButton.addEventListener("click", () => void discoverRepositories());
repositorySearch.addEventListener("input", renderRepositories);
setupForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void saveSettings();
});
setupDialog.addEventListener("cancel", (event) => {
  if (setupRequired) {
    event.preventDefault();
  }
});

window.addEventListener("resize", () => scheduleResize());
window.addEventListener("keydown", handleKeyboard);

void boot();

async function boot(): Promise<void> {
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("preview")) {
    const { loadPreview } = await import("./preview");
    const preview = loadPreview();
    previewMode = true;
    defaultAgent = preview.defaultAgent;
    workspaces = preview.workspaces;
    terminals = preview.terminals;
    inboxLoading = false;
    setConnection(true, "Preview data");
    selectWorkspace(preview.selectedKey);
    render();
    return;
  }

  try {
    setupState = await invoke<DesktopSetupState>("desktop_setup_state");
    applySetupState(setupState);
    if (setupState.first_run) {
      openSetupDialog(true);
      void refreshGithubAuth();
    }
    const info = await invoke<DesktopInfo>("desktop_info");
    terminalDecoder = new TerminalFrameDecoder(info.max_terminal_frame_bytes);
    maxTerminalWriteBytes = info.max_terminal_write_bytes;
    defaultAgent = info.default_agent;
    agentLabel.textContent = defaultAgent;
    const initial = await invoke<WorkspacesResponse>("list_workspaces");
    workspaces = new Map(
      initial.workspaces.map((workspace) => [workspace.key, workspace]),
    );
    inboxLoading = false;
    if (initial.warnings.length > 0) {
      setStatus(initial.warnings[0] ?? "Some workspaces could not be decoded.");
    }
    chooseInitialWorkspace();
    render();

    const events = new Channel<DesktopStreamMessage>();
    events.onmessage = handleStreamMessage;
    await invoke("subscribe_events", { onEvent: events });
    void readTerminalData();
    recordAnalytics("app_opened");
  } catch (error) {
    inboxLoading = false;
    inboxError = String(error);
    renderInbox();
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
  handleEvent(message.payload);
}

function handleEvent(event: LazyboxEvent): void {
  const workspaceChanged =
    "Snapshot" in event ||
    "WorkspaceUpserted" in event ||
    "WorkspaceRemoved" in event;
  const terminalChanged =
    "Snapshot" in event ||
    "TerminalSpawned" in event ||
    "TerminalExited" in event ||
    "AgentState" in event;
  if (workspaceChanged) {
    workspaces = applyWorkspaceEvent(workspaces, event);
    if (selectedKey !== null && !workspaces.has(selectedKey)) {
      selectedKey = null;
    }
    if (
      lastSubmittedReply !== null &&
      selectedKey !== null &&
      workspaces
        .get(selectedKey)
        ?.activity.some((activity) => activity.body.trim() === lastSubmittedReply)
    ) {
      lastSubmittedReply = null;
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
    for (const terminalId of terminals.keys()) {
      applyPendingTerminalFrames(terminalId);
    }
    chooseInitialWorkspace();
    attachSelectedTerminal();
    setStatus("Inbox and terminal replay are synchronized.");
  } else if ("TerminalSpawned" in event) {
    const payload = event.TerminalSpawned;
    terminals.set(payload.terminal_id, {
      id: payload.terminal_id,
      sessionKey: payload.session_key,
      kind: payload.kind,
      replay: new Uint8Array(),
      lastSeq: 0,
      replayAvailable: true,
      dirty: false,
      state: "running",
    });
    if (payload.session_key === selectedKey) {
      attachTerminal(payload.terminal_id);
      if (focusRequestedSession === payload.session_key) {
        activeTerminal?.terminal.focus();
        focusRequestedSession = null;
      }
    }
    applyPendingTerminalFrames(payload.terminal_id);
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
    commandOutcomes.recordFailure();
    setStatus(`${event.ProviderError.source}: ${event.ProviderError.message}`);
    if (event.ProviderError.source === "reply" && lastSubmittedReply !== null) {
      replyBody.value = lastSubmittedReply;
      lastSubmittedReply = null;
      replyBody.focus();
    }
  } else if ("CommandRejected" in event) {
    commandOutcomes.recordFailure();
    setStatus(`${event.CommandRejected.command}: ${event.CommandRejected.message}`);
  } else if ("PollProgress" in event) {
    setStatus(event.PollProgress.message);
  } else if ("PollCompleted" in event) {
    setStatus(
      `${event.PollCompleted.source}: ${event.PollCompleted.count} tasks synchronized.`,
    );
  } else if ("WorktreeProgress" in event) {
    const status = event.WorktreeProgress.status;
    if (typeof status === "object" && "Failed" in status) {
      commandOutcomes.recordFailure();
      setStatus(`Workspace error: ${status.Failed}`);
    } else if (typeof status === "object" && "Warned" in status) {
      setStatus(`Workspace warning: ${status.Warned}`);
    } else if (typeof status === "object" && "Progress" in status) {
      setStatus(status.Progress);
    } else {
      setStatus(
        `Preparing workspace: ${event.WorktreeProgress.step.toLowerCase()}`,
      );
    }
  }

  if (workspaceChanged || terminalChanged) {
    render();
  }
}

async function readTerminalData(): Promise<void> {
  while (!previewMode) {
    try {
      const chunk = await invoke<ArrayBuffer>("read_terminal_data");
      const item = decodeTerminalStreamItem(chunk);
      if (item.kind === "reset") {
        terminalDecoder.reset();
        continue;
      }
      for (const frame of terminalDecoder.push(item.payload)) {
        if (frame.kind === "output") {
          handleTerminalOutput(frame);
        } else if (frame.kind === "resync-unavailable") {
          handleTerminalResyncUnavailable(frame.terminalId);
        } else {
          handleTerminalReplay(frame);
        }
      }
    } catch (error) {
      setStatus(String(error));
      await new Promise((resolve) => window.setTimeout(resolve, 750));
    }
  }
}

function queuePendingTerminalFrame(frame: TerminalBinaryFrame): void {
  const pending = pendingTerminalFrames.get(frame.terminalId) ?? [];
  if (frame.kind !== "output") {
    pending.length = 0;
  }
  pending.push(frame);
  if (pending.length > 32) {
    pending.splice(0, pending.length - 32);
  }
  pendingTerminalFrames.set(frame.terminalId, pending);
}

function applyPendingTerminalFrames(terminalId: number): void {
  const pending = pendingTerminalFrames.get(terminalId);
  if (pending === undefined) {
    return;
  }
  pendingTerminalFrames.delete(terminalId);
  for (const frame of pending) {
    if (frame.kind === "output") {
      handleTerminalOutput(frame);
    } else if (frame.kind === "resync-unavailable") {
      handleTerminalResyncUnavailable(frame.terminalId);
    } else {
      handleTerminalReplay(frame);
    }
  }
}

function render(): void {
  renderInbox();
  renderWorkspace();
  agentLabel.textContent = defaultAgent;
}

function renderInbox(): void {
  const items = filteredWorkspaces(workspaces.values(), {
    query: workspaceSearch.value,
    filter: workspaceFilter.value as WorkspaceFilter,
  });
  visibleWorkspaces = items;
  workspaceList.replaceChildren();
  workspaceList.setAttribute("aria-busy", String(inboxLoading));
  const allItems = [...workspaces.values()];
  const unread = allItems.reduce(
    (sum, workspace) => sum + unreadCount(workspace),
    0,
  );
  workspaceCount.textContent =
    items.length === allItems.length
      ? `${items.length} workspace${items.length === 1 ? "" : "s"}`
      : `${items.length} of ${allItems.length}`;
  unreadTotal.textContent = `${unread} unread`;

  if (inboxLoading) {
    renderInboxMessage("Loading persisted workspaces…");
    return;
  }
  if (inboxError !== null) {
    renderInboxMessage(inboxError, true);
    return;
  }
  if (items.length === 0) {
    renderInboxMessage(
      allItems.length === 0
        ? "Your inbox is empty. Refresh after setup to fetch GitHub work."
        : "No workspaces match this search and filter.",
    );
    return;
  }

  for (const workspace of items) {
    const task = primaryTask(workspace);
    const button = document.createElement("button");
    button.className = "workspace-row";
    button.classList.toggle("selected", workspace.key === selectedKey);
    button.type = "button";
    button.role = "option";
    button.ariaSelected = String(workspace.key === selectedKey);
    button.tabIndex = workspace.key === selectedKey ? 0 : -1;
    const count = unreadCount(workspace);
    button.setAttribute(
      "aria-label",
      `${task?.title ?? workspace.name}, ${task?.repo ?? workspace.branch}, ${
        count === 0 ? "read" : `${count} unread`
      }`,
    );
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
    repo.textContent = [
      task?.repo ?? workspace.branch,
      task?.needs_reply ? "reply needed" : null,
      task?.ci === "Failure" || task?.ci === "Mixed" ? "CI failing" : null,
    ]
      .filter(Boolean)
      .join(" · ");
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
    task?.role === undefined ? null : `you are ${task.role.toLowerCase()}`,
    workspace.branch,
  ]
    .filter((value): value is string => Boolean(value))
    .join(" · ");
  taskSignals.replaceChildren();
  if (task !== null) {
    addTaskSignal(task.state);
    if (task.ci !== "None") {
      addTaskSignal(
        `CI ${task.ci.toLowerCase()}`,
        task.ci === "Success"
          ? "success"
          : task.ci === "Failure" || task.ci === "Mixed"
            ? "attention"
            : undefined,
      );
    }
    if (task.review !== "None") {
      addTaskSignal(
        `Review ${task.review.replace(/([A-Z])/g, " $1").trim().toLowerCase()}`,
        task.review === "Approved"
          ? "success"
          : task.review === "ChangesRequested"
            ? "attention"
            : undefined,
      );
    }
    if (task.needs_reply) {
      addTaskSignal(
        task.last_commenter === null
          ? "Reply needed"
          : `Reply to @${task.last_commenter}`,
        "attention",
      );
    }
    if (task.additions > 0 || task.deletions > 0) {
      addTaskSignal(`+${task.additions} −${task.deletions}`);
    }
    for (const label of task.labels.slice(0, 4)) {
      addTaskSignal(label.name);
    }
  }
  taskDescription.textContent =
    task?.body?.trim() || "No description was provided for this workspace.";
  markReadButton.disabled = unreadCount(workspace) === 0;
  replyBody.disabled = task === null;

  const agentTerminal = terminalForWorkspace(
    workspace.key,
    "agent",
    defaultAgent,
  );
  const existingAgentSession = workspace.sessions.some(
    (session) =>
      typeof session.kind === "object" &&
      "Agent" in session.kind &&
      session.kind.Agent.agent_id === defaultAgent,
  );
  const spawnVerb =
    agentTerminal !== undefined && !agentTerminal.state.startsWith("exited")
      ? "Open"
      : existingAgentSession
        ? "Resume"
        : "Start";
  spawnButton.querySelector("span")!.textContent = spawnVerb;
  shellButton.textContent =
    terminalForWorkspace(workspace.key, "shell") === undefined
      ? "Shell"
      : "Open shell";

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
    card.setAttribute(
      "aria-label",
      `${activity.kind} by ${activity.author}, ${relativeTime(activity.created_at)}`,
    );
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

function renderInboxMessage(message: string, error = false): void {
  const empty = document.createElement("p");
  empty.className = `inbox-empty${error ? " error" : ""}`;
  empty.textContent = message;
  if (error) {
    empty.role = "alert";
  }
  workspaceList.append(empty);
}

function addTaskSignal(
  label: string,
  tone?: "attention" | "success",
): void {
  const signal = document.createElement("span");
  signal.className = `signal-pill${tone === undefined ? "" : ` ${tone}`}`;
  signal.textContent = label;
  taskSignals.append(signal);
}

function selectWorkspace(key: string): void {
  const changed = selectedKey !== key;
  selectedKey = key;
  render();
  attachSelectedTerminal();
  void sendCommand({ FocusWorkspace: { session_key: key } });
  if (changed) {
    const workspace = workspaces.get(key);
    setStatus(`Opened workspace: ${primaryTask(workspace!)?.title ?? workspace?.name ?? key}`);
    recordAnalytics("workspace_opened");
  }
}

function chooseInitialWorkspace(): void {
  if (selectedKey !== null && workspaces.has(selectedKey)) {
    return;
  }
  selectedKey =
    filteredWorkspaces(workspaces.values(), { query: "", filter: "all" })[0]
      ?.key ?? null;
}

function attachSelectedTerminal(): void {
  if (selectedKey === null) {
    detachTerminal();
    return;
  }
  const record =
    terminalForWorkspace(selectedKey, "agent") ??
    terminalForWorkspace(selectedKey, "shell");
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
    terminal.write(record.replay);
    record.replay = new Uint8Array();
  }

  const inputDisposable = terminal.onData((data) => {
    queueTerminalInput(id, [...encoder.encode(data)], terminalInputIntent(data));
  });
  const resizeDisposable = terminal.onResize(({ cols, rows }) => {
    void sendTerminalFrame(resizeTerminalFrame(id, cols, rows));
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
  if (record.dirty || !record.replayAvailable) {
    requestTerminalResync(record);
  }
}

function terminalForWorkspace(
  sessionKey: string,
  kind: "agent" | "shell",
  agentId?: string,
): TerminalRecord | undefined {
  return [...terminals.values()].find((terminal) => {
    if (terminal.sessionKey !== sessionKey) {
      return false;
    }
    return kind === "shell"
      ? terminal.kind === "Shell"
      : typeof terminal.kind === "object" &&
          "Agent" in terminal.kind &&
          (agentId === undefined || terminal.kind.Agent === agentId);
  });
}

function detachTerminal(): void {
  if (activeTerminal !== null) {
    const record = terminals.get(activeTerminal.id);
    if (record !== undefined) {
      discardTerminalView(record);
    }
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

function handleTerminalOutput(frame: TerminalBinaryFrame): void {
  const record = terminals.get(frame.terminalId);
  if (record === undefined) {
    queuePendingTerminalFrame(frame);
    return;
  }
  if (activeTerminal?.id !== frame.terminalId) {
    record.dirty = true;
    return;
  }
  if (activeTerminal.resyncing || frame.seq <= record.lastSeq) {
    return;
  }
  if (
    frame.firstSeq !== record.lastSeq + 1 &&
    !(record.lastSeq === 0 && frame.firstSeq === 0)
  ) {
    requestTerminalResync(
      record,
      requiredTerminalResyncSequence(record.lastSeq, true),
    );
    return;
  }
  activeTerminal.terminal.write(frame.payload);
  record.lastSeq = frame.seq;
  record.dirty = false;
}

function handleTerminalReplay(frame: TerminalBinaryFrame): void {
  const record = terminals.get(frame.terminalId);
  if (record === undefined) {
    queuePendingTerminalFrame(frame);
    return;
  }
  record.replay = frame.payload;
  record.lastSeq = frame.seq;
  record.replayAvailable = true;
  record.dirty = false;
  if (activeTerminal?.id === frame.terminalId) {
    activeTerminal.terminal.reset();
    activeTerminal.terminal.write(frame.payload);
    activeTerminal.resyncing = false;
    setTerminalState(record.state);
  }
}

function handleTerminalResyncUnavailable(terminalId: number): void {
  if (activeTerminal?.id === terminalId) {
    activeTerminal.resyncing = false;
    setTerminalState("waiting for replay");
  }
}

function requestTerminalResync(
  record: TerminalRecord,
  requiredSeq = requiredTerminalResyncSequence(
    record.lastSeq,
    record.replayAvailable,
  ),
): void {
  if (activeTerminal?.id === record.id && !activeTerminal.resyncing) {
    activeTerminal.resyncing = true;
    setTerminalState("resyncing");
    void sendTerminalFrame(resyncTerminalFrame(record.id, requiredSeq));
  }
}

function queueTerminalInput(
  id: number,
  bytes: number[],
  intent: TerminalInputIntent,
): void {
  const pending = pendingInput.get(id) ?? [];
  const tail = pending.at(-1);
  if (tail?.intent === intent) {
    tail.bytes.push(...bytes);
  } else {
    pending.push({ bytes, intent });
  }
  pendingInput.set(id, pending);
  if (inputTimers.has(id)) {
    return;
  }
  const timer = window.setTimeout(() => {
    inputTimers.delete(id);
    void flushTerminalInput(id);
  }, 12);
  inputTimers.set(id, timer);
}

async function flushTerminalInput(id: number): Promise<void> {
  if (inputSending.has(id)) {
    return;
  }
  inputSending.add(id);
  try {
    while (true) {
      const buffered = pendingInput.get(id) ?? [];
      pendingInput.delete(id);
      if (buffered.length === 0) {
        return;
      }
      for (const input of buffered) {
        await sendTerminalFramesSequentially(
          writeTerminalFrames(
            id,
            Uint8Array.from(input.bytes),
            maxTerminalWriteBytes,
            input.intent,
          ),
          sendTerminalFrame,
        );
      }
    }
  } finally {
    inputSending.delete(id);
  }
}

function terminalInputIntent(data: string): TerminalInputIntent {
  if (data === "\r" || data === "\n") {
    return "submit";
  }
  if (/^\x1b\[<\d+;\d+;\d+[mM]$/.test(data)) {
    return "view";
  }
  return "compose";
}

function scheduleResize(): void {
  window.clearTimeout(resizeTimer);
  resizeTimer = window.setTimeout(() => {
    if (activeTerminal === null) {
      return;
    }
    activeTerminal.fit.fit();
    void sendTerminalFrame(
      resizeTerminalFrame(
        activeTerminal.id,
        activeTerminal.terminal.cols,
        activeTerminal.terminal.rows,
      ),
    );
  }, 80);
}

async function startAgent(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const existing = terminalForWorkspace(selectedKey, "agent", defaultAgent);
  if (existing !== undefined && !existing.state.startsWith("exited")) {
    attachTerminal(existing.id);
    activeTerminal?.terminal.focus();
    setStatus(`Opened ${defaultAgent}.`);
    return;
  }
  focusRequestedSession = selectedKey;
  const succeeded = await runCommands(
    [spawnAgentCommand(selectedKey, defaultAgent)],
    `${
      existing === undefined
        ? workspaces.get(selectedKey)?.sessions.length === 0
          ? "Creating workspace and starting"
          : "Starting"
        : "Resuming"
    } ${defaultAgent}…`,
    `${defaultAgent} launch requested.`,
  );
  if (succeeded) {
    recordAnalytics("agent_started");
  } else {
    focusRequestedSession = null;
  }
}

async function startShell(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const existing = terminalForWorkspace(selectedKey, "shell");
  if (existing !== undefined && !existing.state.startsWith("exited")) {
    attachTerminal(existing.id);
    activeTerminal?.terminal.focus();
    setStatus("Opened shell.");
    return;
  }
  focusRequestedSession = selectedKey;
  const succeeded = await runCommands(
    commandsForWorkspaceIntent(selectedKey, { type: "spawn-shell" }),
    "Starting workspace shell…",
    "Shell launch requested.",
  );
  if (succeeded) {
    recordAnalytics("shell_started");
  } else {
    focusRequestedSession = null;
  }
}

async function markSelectedRead(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  await runCommands(
    commandsForWorkspaceIntent(selectedKey, { type: "mark-read" }),
    "Marking workspace read…",
    "Workspace marked read.",
  );
}

async function reviewReply(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const body = replyBody.value.trim();
  if (body.length === 0) {
    setStatus("Write a reply before submitting.");
    replyBody.focus();
    return;
  }
  const accepted = await confirmUserAction(
    "Post this reply?",
    "This comment will be visible to everyone with access to the GitHub task.",
    "Post reply",
    body,
  );
  if (!accepted) {
    return;
  }
  lastSubmittedReply = body;
  const succeeded = await runCommands(
    commandsForWorkspaceIntent(selectedKey, { type: "reply", body }),
    "Posting reply to GitHub…",
    "Reply posted. Refreshing activity…",
  );
  if (succeeded) {
    replyBody.value = "";
    recordAnalytics("reply_posted");
  } else {
    replyBody.value = body;
    replyBody.focus();
  }
}

async function runCommands(
  commands: LazyboxCommand[],
  pendingMessage: string,
  successMessage: string,
): Promise<boolean> {
  const checkpoint = commandOutcomes.checkpoint();
  setStatus(pendingMessage);
  for (const command of commands) {
    if (!(await sendCommand(command))) {
      return false;
    }
  }
  if (!commandOutcomes.succeededSince(checkpoint)) {
    return false;
  }
  setStatus(successMessage);
  return true;
}

async function sendCommand(
  command: LazyboxCommand,
): Promise<boolean> {
  if (previewMode) {
    return true;
  }
  try {
    await invoke("send_command", { command });
    return true;
  } catch (error) {
    setStatus(String(error));
    return false;
  }
}

async function openSettings(): Promise<void> {
  if (previewMode) {
    setupState = {
      first_run: false,
      selected_repositories: ["github:acme/relay"],
      agents: [
        { id: "codex", label: "Codex", available: true },
        { id: "claude", label: "Claude Code", available: true },
      ],
      default_agent: defaultAgent,
      analytics_enabled: false,
      diagnostics_path: "~/.lazybox/v2/desktop-crashes",
    };
  } else {
    try {
      setupState = await invoke<DesktopSetupState>("desktop_setup_state");
    } catch (error) {
      setStatus(String(error));
      return;
    }
  }
  applySetupState(setupState);
  openSetupDialog(false);
  void refreshGithubAuth();
}

function applySetupState(state: DesktopSetupState): void {
  selectedRepositories = new Set(state.selected_repositories);
  defaultAgentSelect.replaceChildren();
  for (const agent of state.agents) {
    const option = document.createElement("option");
    option.value = agent.id;
    option.textContent = `${agent.label}${agent.available ? "" : " — not installed"}`;
    option.disabled = !agent.available;
    option.selected = agent.id === state.default_agent;
    defaultAgentSelect.append(option);
  }
  if (
    !state.agents.some(
      (agent) => agent.id === defaultAgentSelect.value && agent.available,
    )
  ) {
    defaultAgentSelect.value =
      state.agents.find((agent) => agent.available)?.id ?? "";
  }
  analyticsEnabled.checked = state.analytics_enabled;
  diagnosticsPath.textContent = `Crash reports: ${state.diagnostics_path}`;
  renderRepositories();
}

function openSetupDialog(required: boolean): void {
  setupRequired = required;
  setupTitle.textContent = required ? "Set up lazybox" : "Desktop settings";
  setupClose.classList.toggle("hidden", required);
  setupError.classList.add("hidden");
  if (!setupDialog.open) {
    setupDialog.showModal();
  }
  githubCheckButton.focus();
}

function closeSettings(): void {
  if (!setupRequired) {
    setupDialog.close();
  }
}

async function refreshGithubAuth(): Promise<void> {
  githubCheckButton.disabled = true;
  githubAuthBadge.textContent = "Checking";
  githubAuthBadge.classList.remove("ready");
  githubAuthMessage.textContent = "Checking the existing credential chain…";
  let status: GithubAuthStatus;
  try {
    status = previewMode
      ? {
          authenticated: true,
          account: "preview-user",
          message: "GitHub credential verified",
        }
      : await invoke<GithubAuthStatus>("github_auth_status");
  } catch (error) {
    githubCheckButton.disabled = false;
    githubAuthBadge.textContent = "Unavailable";
    githubAuthMessage.textContent = String(error);
    discoverReposButton.disabled = true;
    return;
  }
  githubCheckButton.disabled = false;
  githubAuthBadge.textContent = status.authenticated ? "Connected" : "Action needed";
  githubAuthBadge.classList.toggle("ready", status.authenticated);
  githubAuthMessage.textContent = status.authenticated
    ? `${status.message} for @${status.account ?? "unknown"}`
    : status.message;
  discoverReposButton.disabled = !status.authenticated;
  if (
    status.authenticated &&
    setupRequired &&
    discoveredRepositories.length === 0
  ) {
    void discoverRepositories();
  }
}

async function startGithubLogin(): Promise<void> {
  const accepted = await confirmUserAction(
    "Start GitHub sign-in?",
    "lazybox will open GitHub CLI's browser-based login. The resulting credential stays in GitHub CLI or your OS credential store.",
    "Start sign-in",
  );
  if (!accepted) {
    return;
  }
  try {
    if (!previewMode) {
      await invoke("begin_github_login");
    }
    githubAuthMessage.textContent =
      "Complete sign-in in the browser, then choose Check again.";
    githubAuthBadge.textContent = "Waiting";
    githubAuthBadge.classList.remove("ready");
  } catch (error) {
    showSetupError(String(error));
  }
}

async function discoverRepositories(): Promise<void> {
  repositoryList.setAttribute("aria-busy", "true");
  discoverReposButton.disabled = true;
  repositoryList.replaceChildren();
  const loading = document.createElement("p");
  loading.textContent = "Discovering repositories…";
  repositoryList.append(loading);
  try {
    discoveredRepositories = previewMode
      ? [
          { id: "github:acme/relay", label: "acme/relay", owner: "acme" },
          { id: "github:acme/api", label: "acme/api", owner: "acme" },
        ]
      : await invoke<GithubRepositoryOption[]>("list_github_repositories");
    renderRepositories();
  } catch (error) {
    repositoryList.replaceChildren();
    const message = document.createElement("p");
    message.textContent = String(error);
    message.role = "alert";
    repositoryList.append(message);
  } finally {
    repositoryList.setAttribute("aria-busy", "false");
    discoverReposButton.disabled = false;
  }
}

function renderRepositories(): void {
  const query = repositorySearch.value.trim().toLocaleLowerCase();
  const byId = new Map(
    discoveredRepositories.map((repository) => [repository.id, repository]),
  );
  for (const id of selectedRepositories) {
    if (!byId.has(id)) {
      const label = id.replace(/^github:/, "");
      byId.set(id, {
        id,
        label,
        owner: label.split("/")[0] ?? "",
      });
    }
  }
  const repositories = [...byId.values()]
    .filter((repository) => repository.label.toLocaleLowerCase().includes(query))
    .sort((left, right) => left.label.localeCompare(right.label));
  repositoryList.replaceChildren();
  if (repositories.length === 0) {
    const message = document.createElement("p");
    message.textContent =
      byId.size === 0
        ? "Verify GitHub access, then discover repositories."
        : "No repositories match this filter.";
    repositoryList.append(message);
  }
  for (const repository of repositories) {
    const label = document.createElement("label");
    label.className = "repository-option";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.value = repository.id;
    checkbox.checked = selectedRepositories.has(repository.id);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) {
        selectedRepositories.add(repository.id);
      } else {
        selectedRepositories.delete(repository.id);
      }
      updateRepositorySelectionCount();
    });
    const name = document.createElement("span");
    name.textContent = repository.label;
    label.append(checkbox, name);
    repositoryList.append(label);
  }
  updateRepositorySelectionCount();
}

function updateRepositorySelectionCount(): void {
  repositorySelectionCount.textContent = `${selectedRepositories.size} selected`;
}

async function saveSettings(): Promise<void> {
  setupError.classList.add("hidden");
  if (selectedRepositories.size === 0) {
    showSetupError("Select at least one GitHub repository.");
    return;
  }
  if (defaultAgentSelect.value.length === 0) {
    showSetupError("Install and select a default agent.");
    return;
  }
  const accepted = await confirmUserAction(
    "Save desktop settings?",
    "lazybox will update the shared configuration and restart so provider scope and agent changes take effect.",
    "Save and restart",
  );
  if (!accepted) {
    return;
  }
  saveSettingsButton.disabled = true;
  saveSettingsButton.textContent = "Saving…";
  try {
    if (!previewMode) {
      await invoke("save_desktop_settings", {
        settings: {
          repositories: [...selectedRepositories],
          default_agent: defaultAgentSelect.value,
          analytics_enabled: analyticsEnabled.checked,
        },
      });
    }
    setStatus("Settings saved. Restarting lazybox…");
    if (previewMode) {
      setupDialog.close();
    }
  } catch (error) {
    showSetupError(String(error));
    saveSettingsButton.disabled = false;
    saveSettingsButton.textContent = "Save and restart";
  }
}

function showSetupError(message: string): void {
  setupError.textContent = message;
  setupError.classList.remove("hidden");
}

function confirmUserAction(
  title: string,
  message: string,
  acceptLabel: string,
  preview?: string,
): Promise<boolean> {
  confirmTitle.textContent = title;
  confirmMessage.textContent = message;
  confirmAccept.textContent = acceptLabel;
  confirmPreview.textContent = preview ?? "";
  confirmPreview.classList.toggle("hidden", preview === undefined);
  confirmDialog.returnValue = "";
  confirmDialog.showModal();
  confirmAccept.focus();
  return new Promise((resolve) => {
    confirmDialog.addEventListener(
      "close",
      () => resolve(confirmDialog.returnValue === "confirm"),
      { once: true },
    );
  });
}

function recordAnalytics(event: AnalyticsEvent): void {
  if (previewMode) {
    return;
  }
  void invoke("record_analytics", { event }).catch((error) => {
    setStatus(`Analytics event could not be recorded: ${String(error)}`);
  });
}

function handleKeyboard(event: KeyboardEvent): void {
  if ((event.metaKey || event.ctrlKey) && event.key === ",") {
    event.preventDefault();
    void openSettings();
    return;
  }
  const target = event.target;
  const editable =
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement ||
    (target instanceof HTMLElement && target.isContentEditable);
  if (
    target === replyBody &&
    (event.metaKey || event.ctrlKey) &&
    event.key === "Enter"
  ) {
    event.preventDefault();
    replyForm.requestSubmit();
    return;
  }
  if (editable || setupDialog.open || confirmDialog.open) {
    return;
  }
  if (event.key === "/") {
    event.preventDefault();
    workspaceSearch.focus();
    return;
  }
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    navigateWorkspaces(event.key === "ArrowDown" ? 1 : -1);
    return;
  }
  if (event.key === "Enter" && selectedKey !== null) {
    event.preventDefault();
    selectWorkspace(selectedKey);
    return;
  }
  if (event.key === "r") {
    event.preventDefault();
    replyBody.focus();
  } else if (event.key === "a") {
    event.preventDefault();
    void startAgent();
  } else if (event.key === "s") {
    event.preventDefault();
    void startShell();
  } else if (event.key === "m") {
    event.preventDefault();
    void markSelectedRead();
  } else if (event.key === "R") {
    event.preventDefault();
    void runCommands(["Refresh"], "Refreshing providers…", "Refresh requested.");
  }
}

function navigateWorkspaces(delta: number): void {
  if (visibleWorkspaces.length === 0) {
    return;
  }
  const current = visibleWorkspaces.findIndex(
    (workspace) => workspace.key === selectedKey,
  );
  const next =
    current < 0
      ? 0
      : Math.max(0, Math.min(visibleWorkspaces.length - 1, current + delta));
  const key = visibleWorkspaces[next]?.key;
  if (key !== undefined) {
    selectWorkspace(key);
    workspaceList
      .querySelector<HTMLButtonElement>('[aria-selected="true"]')
      ?.focus();
  }
}

async function sendTerminalFrame(frame: Uint8Array): Promise<void> {
  if (previewMode) {
    return;
  }
  try {
    await invoke("send_terminal_frame", frame);
  } catch (error) {
    setStatus(String(error));
  }
}

function terminalFromSnapshot(snapshot: TerminalSnapshot): TerminalRecord {
  return {
    id: snapshot.terminal_id,
    sessionKey: snapshot.session_key,
    kind: snapshot.kind,
    replay: new Uint8Array(),
    lastSeq: snapshot.last_seq,
    replayAvailable: false,
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
