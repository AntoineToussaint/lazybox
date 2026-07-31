import { Channel, invoke } from "@tauri-apps/api/core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import {
  InboxConnection,
  ReplyDrafts,
  applyWorkspaceEvent,
  canReplyToTask,
  preferredTerminal,
  primaryTask,
  projectKeyLabel,
  shouldHandleWorkspaceEnter,
  taskReference,
  unreadCount,
} from "./model";
import {
  type DesktopInfo,
  type DesktopRepository,
  type DesktopStreamMessage,
  type InboxView,
  type LazyboxCommand,
  type LazyboxEvent,
  type PickerRow,
  type SnippetPickerView,
  type TerminalKind,
  type TerminalSnapshot,
  type Workspace,
  type WorkspacesResponse,
  commandsForWorkspaceIntent,
  createWorkspaceCommand,
  deliverSnippetCommand,
  spawnAgentCommand,
  terminalKindLabel,
} from "./protocol";
import {
  autoSubmitRow,
  clampCursor,
  flattenRows,
  renderSnippetList,
  renderSnippetPreview,
} from "./snippet_picker";
import {
  relativeTime,
  renderInboxList,
  workspaceKeysInOrder,
} from "./inbox_view";
import { terminalTheme } from "./theme";
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

interface DesktopModelTier {
  alias: string;
  label: string;
}

interface DesktopAgentOption {
  id: string;
  label: string;
  available: boolean;
  models: DesktopModelTier[];
  default_tier: string | null;
}

interface DesktopThemeColors {
  accent: string;
  hover: string;
  success: string;
  warn: string;
  error: string;
  text_strong: string;
  text_dim: string;
  chrome: string;
  fill: string;
  surface: string;
}

interface DesktopThemeOption {
  name: string;
  colors: DesktopThemeColors;
}

interface DesktopSetupState {
  first_run: boolean;
  selected_scopes: string[];
  agents: DesktopAgentOption[];
  default_agent: string;
  analytics_enabled: boolean;
  diagnostics_path: string;
  theme: string | null;
  themes: DesktopThemeOption[];
  keymap_preset: string | null;
  terminal_new_layout: string;
  activity_pane_default: string;
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

// Two poles for preview mode only; the live app reads the full catalog
// from `desktop_setup_state` (sourced from the shared Rust palette).
const PREVIEW_THEMES: DesktopThemeOption[] = [
  {
    name: "Lazybox Dark",
    colors: {
      accent: "#7dcfff",
      hover: "#f7768e",
      success: "#9ece6a",
      warn: "#e0af68",
      error: "#f7768e",
      text_strong: "#c0caf5",
      text_dim: "#7a82a7",
      chrome: "#3a4060",
      fill: "#292e42",
      surface: "#1a1d2e",
    },
  },
  {
    name: "Lazybox Light",
    colors: {
      accent: "#1a6ec4",
      hover: "#c13574",
      success: "#23864e",
      warn: "#9f6a00",
      error: "#c13574",
      text_strong: "#1c2030",
      text_dim: "#606880",
      chrome: "#c4c9d6",
      fill: "#dadfe9",
      surface: "#f7f8fa",
    },
  },
];

const workspaceList = element<HTMLDivElement>("workspace-list");
const workspaceCount = element<HTMLSpanElement>("workspace-count");
const unreadTotal = element<HTMLSpanElement>("unread-count");
const sortButton = element<HTMLButtonElement>("sort-button");
const sortLabel = element<HTMLElement>("sort-label");
const newWorkspaceButton =
  element<HTMLButtonElement>("new-workspace-button");
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
const replyButton = element<HTMLButtonElement>("reply-button");
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
const defaultModelSelect =
  element<HTMLSelectElement>("default-model-select");
const defaultModelField = element<HTMLLabelElement>("default-model-field");
const themeList = element<HTMLDivElement>("theme-list");
const keymapPresetLabel = element<HTMLSpanElement>("keymap-preset-label");
const terminalLayoutSelect =
  element<HTMLSelectElement>("terminal-layout-select");
const activityPaneSelect =
  element<HTMLSelectElement>("activity-pane-select");
const analyticsEnabled = element<HTMLInputElement>("analytics-enabled");
const setupError = element<HTMLParagraphElement>("setup-error");
const diagnosticsPath = element<HTMLSpanElement>("diagnostics-path");
const saveSettingsButton =
  element<HTMLButtonElement>("save-settings-button");
const newWorkspaceDialog =
  element<HTMLDialogElement>("new-workspace-dialog");
const newWorkspaceForm = element<HTMLFormElement>("new-workspace-form");
const newWorkspaceProject =
  element<HTMLSelectElement>("new-workspace-project");
const newWorkspaceName = element<HTMLInputElement>("new-workspace-name");
const newWorkspaceAgent =
  element<HTMLInputElement>("new-workspace-agent");
const newWorkspaceError =
  element<HTMLParagraphElement>("new-workspace-error");
const newWorkspaceCancel =
  element<HTMLButtonElement>("new-workspace-cancel");
const confirmDialog = element<HTMLDialogElement>("confirm-dialog");
const confirmTitle = element<HTMLHeadingElement>("confirm-title");
const confirmMessage = element<HTMLParagraphElement>("confirm-message");
const confirmPreview = element<HTMLPreElement>("confirm-preview");
const confirmAccept = element<HTMLButtonElement>("confirm-accept");
const snippetButton = element<HTMLButtonElement>("snippet-button");
const snippetDialog = element<HTMLDialogElement>("snippet-dialog");
const snippetFilter = element<HTMLInputElement>("snippet-filter");
const snippetList = element<HTMLDivElement>("snippet-list");
const snippetPreview = element<HTMLDivElement>("snippet-preview");
const snippetCount = element<HTMLSpanElement>("snippet-count");

let workspaces = new Map<string, Workspace>();
let terminals = new Map<number, TerminalRecord>();
let selectedKey: string | null = null;
let inboxView: InboxView | null = null;
let defaultAgent = "claude";
let previewMode = false;
let inboxLoading = true;
let inboxError: string | null = null;
let setupState: DesktopSetupState | null = null;
let discoveredRepositories: GithubRepositoryOption[] = [];
let availableThemes: DesktopThemeOption[] = [];
let selectedTheme: string | null = null;
let currentThemeColors: DesktopThemeColors | null = null;
let selectedScopes = new Set<string>();
let configuredRepositories: DesktopRepository[] = [];
let setupRequired = false;
const replySubmitting = new Set<string>();
let creatingWorkspace = false;
const replyDrafts = new ReplyDrafts();
const pendingLaunches = new Set<string>();
let focusRequestedSession: string | null = null;
let activeTerminal: ActiveTerminal | null = null;
let resizeTimer: number | undefined;
let snippetViewState: SnippetPickerView | null = null;
let snippetCursor = 0;
let snippetQuery = "";
let snippetTargetTerminal: number | null = null;
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
let desktopMetadataLoaded = false;
let terminalReaderStarted = false;
let eventChannel: Channel<DesktopStreamMessage> | null = null;
let inboxChannel: Channel<InboxView> | null = null;
const inboxConnection = new InboxConnection(
  () => invoke<WorkspacesResponse>("list_workspaces"),
  async () => {
    eventChannel = new Channel<DesktopStreamMessage>();
    eventChannel.onmessage = handleStreamMessage;
    inboxChannel = new Channel<InboxView>();
    inboxChannel.onmessage = applyInboxView;
    await invoke("subscribe_events", {
      onEvent: eventChannel,
      onInbox: inboxChannel,
    });
    if (!terminalReaderStarted) {
      terminalReaderStarted = true;
      void readTerminalData();
    }
  },
);

refreshButton.addEventListener("click", () => {
  void refreshInbox(true);
});

newWorkspaceButton.addEventListener("click", openNewWorkspaceDialog);
newWorkspaceCancel.addEventListener("click", () => newWorkspaceDialog.close());
newWorkspaceForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void createWorkspace();
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

sortButton.addEventListener("click", () => void cycleSortMode());
settingsButton.addEventListener("click", () => void openSettings());
snippetButton.addEventListener("click", () => void openSnippetPicker());
snippetFilter.addEventListener("input", () => void onSnippetFilterInput());
snippetDialog.addEventListener("keydown", handleSnippetKey);
snippetDialog.addEventListener("close", onSnippetDialogClose);
setupClose.addEventListener("click", closeSettings);
githubCheckButton.addEventListener("click", () => void refreshGithubAuth());
githubLoginButton.addEventListener("click", () => void startGithubLogin());
discoverReposButton.addEventListener("click", () => void discoverRepositories());
repositorySearch.addEventListener("input", renderRepositories);
defaultAgentSelect.addEventListener("change", () =>
  renderModelOptions(defaultAgentSelect.value),
);
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
    inboxView = preview.inboxView;
    inboxLoading = false;
    setConnection(true, "Preview data");
    selectWorkspace(preview.selectedKey);
    render();
    return;
  }

  try {
    await initializeDesktopMetadata();
    if (await refreshInbox(false)) {
      recordAnalytics("app_opened");
    }
  } catch (error) {
    showInboxFailure(error);
  }
}

async function initializeDesktopMetadata(): Promise<void> {
  if (desktopMetadataLoaded) {
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
    configuredRepositories = info.repositories;
    agentLabel.textContent = defaultAgent;
    desktopMetadataLoaded = true;
  } catch (error) {
    desktopMetadataLoaded = false;
    throw error;
  }
}

async function refreshInbox(requestProviderRefresh: boolean): Promise<boolean> {
  inboxLoading = true;
  inboxError = null;
  renderInbox();
  try {
    await initializeDesktopMetadata();
    const initial = await inboxConnection.connect();
    workspaces = new Map(
      initial.workspaces.map((workspace) => [workspace.key, workspace]),
    );
    inboxLoading = false;
    if (initial.warnings.length > 0) {
      setStatus(initial.warnings[0] ?? "Some workspaces could not be decoded.");
    }
    chooseInitialWorkspace();
    render();
    setConnection(true, "Live");
    if (requestProviderRefresh) {
      return runCommands(
        ["Refresh"],
        "Refreshing providers…",
        "Refresh requested.",
      );
    }
    return true;
  } catch (error) {
    showInboxFailure(error);
    return false;
  }
}

function showInboxFailure(error: unknown): void {
  inboxLoading = false;
  inboxError = String(error);
  renderInbox();
  setConnection(false, "Daemon unavailable");
  setStatus(`${String(error)} Select Refresh to retry.`);
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
      changeSelectedWorkspace(null);
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
    setStatus(`${event.ProviderError.source}: ${event.ProviderError.message}`);
  } else if ("CommandRejected" in event) {
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
  updateNewWorkspaceButton();
  agentLabel.textContent = defaultAgent;
}

function updateNewWorkspaceButton(): void {
  newWorkspaceButton.disabled = availableRepositories().length === 0;
}

function applyInboxView(view: InboxView): void {
  inboxView = view;
  inboxLoading = false;
  const hadSelection = selectedKey !== null && workspaces.has(selectedKey);
  chooseInitialWorkspace();
  if (!hadSelection && selectedKey !== null) {
    render();
    attachSelectedTerminal();
  } else {
    renderInbox();
  }
}

async function cycleSortMode(): Promise<void> {
  if (previewMode) {
    return;
  }
  try {
    await invoke("cycle_sort_mode");
  } catch (error) {
    setStatus(String(error));
  }
}

async function toggleRepoCollapsed(label: string): Promise<void> {
  if (previewMode) {
    return;
  }
  try {
    await invoke("toggle_repo_collapsed", { label });
  } catch (error) {
    setStatus(String(error));
  }
}

function renderInbox(): void {
  workspaceList.replaceChildren();
  workspaceList.setAttribute("aria-busy", String(inboxLoading));
  sortLabel.textContent = inboxView?.sort_label ?? "split";
  const total = inboxView?.total ?? 0;
  const unread = inboxView?.unread_total ?? 0;
  workspaceCount.textContent = `${total} workspace${total === 1 ? "" : "s"}`;
  unreadTotal.textContent = `${unread} unread`;

  if (inboxError !== null) {
    renderInboxMessage(inboxError, true);
    return;
  }
  // `null` means "connected but the first view hasn't arrived yet" — keep
  // showing loading rather than flashing "empty" before the daemon's
  // opening snapshot lands. An empty inbox is a non-null view with no rows.
  if (inboxLoading || inboxView === null) {
    renderInboxMessage("Loading persisted workspaces…");
    return;
  }
  if (inboxView.rows.length === 0) {
    renderInboxMessage(
      "Your inbox is empty. Refresh after setup to fetch GitHub work.",
    );
    return;
  }

  renderInboxList(workspaceList, inboxView, {
    selectedKey,
    onSelectWorkspace: selectWorkspace,
    onToggleRepo: (label) => void toggleRepoCollapsed(label),
  });
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
  replyBody.disabled = !canReplyToTask(task);
  replyButton.disabled =
    replySubmitting.has(workspace.key) || !canReplyToTask(task);
  replyForm.classList.toggle("hidden", !canReplyToTask(task));

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
  spawnButton.disabled = pendingLaunches.has(
    launchKey(workspace.key, "agent", defaultAgent),
  );
  shellButton.textContent =
    terminalForWorkspace(workspace.key, "shell") === undefined
      ? "Shell"
      : "Open shell";
  shellButton.disabled = pendingLaunches.has(launchKey(workspace.key, "shell"));

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
  const changed = changeSelectedWorkspace(key);
  render();
  attachSelectedTerminal();
  void sendCommand({ FocusWorkspace: { session_key: key } });
  if (changed) {
    const workspace = workspaces.get(key);
    setStatus(`Opened workspace: ${primaryTask(workspace!)?.title ?? workspace?.name ?? key}`);
    recordAnalytics("workspace_opened");
  }
}

function changeSelectedWorkspace(key: string | null): boolean {
  if (selectedKey === key) {
    return false;
  }
  if (selectedKey !== null) {
    replyDrafts.save(selectedKey, replyBody.value);
  }
  selectedKey = key;
  replyBody.value = key === null ? "" : replyDrafts.get(key);
  return true;
}

function chooseInitialWorkspace(): void {
  if (selectedKey !== null && workspaces.has(selectedKey)) {
    return;
  }
  const keys = inboxView === null ? [] : workspaceKeysInOrder(inboxView);
  changeSelectedWorkspace(keys.find((key) => workspaces.has(key)) ?? null);
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
    theme: currentThemeColors
      ? terminalTheme(currentThemeColors)
      : {
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
  // Intercept the snippet shortcut before xterm forwards it to the PTY,
  // so ⌘/Ctrl-J opens the picker instead of reaching the agent.
  terminal.attachCustomKeyEventHandler((event) => {
    if (event.type === "keydown" && isSnippetShortcut(event)) {
      void openSnippetPicker();
      return false;
    }
    return true;
  });
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
  snippetButton.disabled = false;
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
  return preferredTerminal(terminals.values(), sessionKey, kind, agentId);
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
  snippetButton.disabled = true;
  if (snippetDialog.open) {
    closeSnippetPicker();
  }
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

function isSnippetShortcut(event: KeyboardEvent): boolean {
  return (
    (event.metaKey || event.ctrlKey) &&
    !event.altKey &&
    !event.shiftKey &&
    (event.key === "j" || event.key === "J")
  );
}

async function openSnippetPicker(): Promise<void> {
  if (snippetDialog.open) {
    return;
  }
  if (activeTerminal === null) {
    setStatus("Open an agent or shell to send a snippet.");
    return;
  }
  snippetTargetTerminal = activeTerminal.id;
  snippetQuery = "";
  snippetFilter.value = "";
  const view = await fetchSnippetView("");
  if (view === null) {
    return;
  }
  snippetViewState = view;
  snippetCursor = 0;
  renderSnippet();
  if (!snippetDialog.open) {
    snippetDialog.showModal();
  }
  snippetFilter.focus();
}

function onSnippetDialogClose(): void {
  snippetViewState = null;
  snippetTargetTerminal = null;
  activeTerminal?.terminal.focus();
}

function closeSnippetPicker(): void {
  if (snippetDialog.open) {
    snippetDialog.close();
  }
}

async function fetchSnippetView(
  filter: string,
): Promise<SnippetPickerView | null> {
  if (previewMode) {
    return previewSnippetView();
  }
  try {
    return await invoke<SnippetPickerView>("snippet_view", { filter });
  } catch (error) {
    setStatus(String(error));
    return null;
  }
}

async function onSnippetFilterInput(): Promise<void> {
  const query = snippetFilter.value;
  const grew = query.length > snippetQuery.length;
  snippetQuery = query;
  const view = await fetchSnippetView(query);
  if (view === null || !snippetDialog.open) {
    return;
  }
  snippetViewState = view;
  snippetCursor = 0;
  renderSnippet();
  // Auto-submit only when a character was added — parity with the TUI,
  // where Backspace never fires the `]]srev` fast path.
  if (grew) {
    const row = autoSubmitRow(view);
    if (row !== null) {
      await deliverSnippet(row);
    }
  }
}

function renderSnippet(): void {
  const view = snippetViewState;
  if (view === null) {
    return;
  }
  snippetCursor = clampCursor(flattenRows(view).length, snippetCursor);
  snippetCount.textContent = `${view.visible_count}/${view.total}`;
  renderSnippetList(snippetList, view, snippetCursor, {
    onPick: (index) => {
      const picked = flattenRows(view)[index];
      if (picked !== undefined) {
        void deliverSnippet(picked);
      }
    },
    onHover: (index) => {
      snippetCursor = index;
      renderSnippet();
    },
  });
  renderSnippetPreview(snippetPreview, view, snippetCursor);
  snippetList
    .querySelector<HTMLElement>('[aria-selected="true"]')
    ?.scrollIntoView({ block: "nearest" });
}

function handleSnippetKey(event: KeyboardEvent): void {
  if (event.key === "ArrowDown") {
    event.preventDefault();
    moveSnippetCursor(1);
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    moveSnippetCursor(-1);
  } else if (event.key === "Enter") {
    event.preventDefault();
    if (snippetViewState !== null) {
      const row = flattenRows(snippetViewState)[snippetCursor];
      if (row !== undefined) {
        void deliverSnippet(row);
      }
    }
  }
  // Esc falls through to the dialog's native cancel/close.
}

function moveSnippetCursor(delta: number): void {
  if (snippetViewState === null) {
    return;
  }
  const length = flattenRows(snippetViewState).length;
  snippetCursor = clampCursor(length, snippetCursor + delta);
  renderSnippet();
}

async function deliverSnippet(row: PickerRow): Promise<void> {
  const terminalId = snippetTargetTerminal;
  closeSnippetPicker();
  if (terminalId === null) {
    setStatus("The target terminal is no longer attached.");
    return;
  }
  if (await sendCommand(deliverSnippetCommand(terminalId, row))) {
    setStatus(`Sent ]${row.key} to the terminal.`);
  }
}

// A small fixed view for the dev preview harness (no daemon to query).
function previewSnippetView(): SnippetPickerView {
  const row = (
    key: string,
    description: string,
    category: string,
    body: string,
  ): PickerRow => ({ key, description, category, body, origin: "built-in" });
  const groups = [
    {
      category: "Review",
      label: "Review",
      rows: [row("rev", "Review the current diff", "Review", "Review the current diff…")],
    },
    {
      category: "Git & PR",
      label: "Git & PR",
      rows: [row("pr", "Open a PR", "Git & PR", "Open a PR with gh…")],
    },
  ];
  return { filter: "", groups, auto_submit: null, visible_count: 2, total: 2 };
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
  const pendingKey = launchKey(selectedKey, "agent", defaultAgent);
  if (pendingLaunches.has(pendingKey)) {
    return;
  }
  pendingLaunches.add(pendingKey);
  renderWorkspace();
  focusRequestedSession = selectedKey;
  try {
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
  } finally {
    pendingLaunches.delete(pendingKey);
    renderWorkspace();
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
  const pendingKey = launchKey(selectedKey, "shell");
  if (pendingLaunches.has(pendingKey)) {
    return;
  }
  pendingLaunches.add(pendingKey);
  renderWorkspace();
  focusRequestedSession = selectedKey;
  try {
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
  } finally {
    pendingLaunches.delete(pendingKey);
    renderWorkspace();
  }
}

function launchKey(
  workspaceKey: string,
  kind: "agent" | "shell",
  agent = "",
): string {
  return `${workspaceKey}\0${kind}\0${agent}`;
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
  if (selectedKey === null || replySubmitting.has(selectedKey)) {
    return;
  }
  const workspaceKey = selectedKey;
  const workspace = workspaces.get(workspaceKey);
  const task = workspace === undefined ? null : primaryTask(workspace);
  if (!canReplyToTask(task)) {
    setStatus("Replies are available for GitHub tasks only.");
    return;
  }
  const body = replyBody.value.trim();
  if (body.length === 0) {
    setStatus("Write a reply before submitting.");
    replyBody.focus();
    return;
  }
  replySubmitting.add(workspaceKey);
  replyButton.disabled = true;
  try {
    const accepted = await confirmUserAction(
      "Post this reply?",
      "This comment will be visible to everyone with access to the GitHub task.",
      "Post reply",
      body,
    );
    if (!accepted) {
      return;
    }
    replyDrafts.save(workspaceKey, body);
    const succeeded = await runCommands(
      commandsForWorkspaceIntent(workspaceKey, { type: "reply", body }),
      "Posting reply to GitHub…",
      "Reply posted. Refreshing activity…",
    );
    if (succeeded) {
      replyDrafts.clear(workspaceKey);
      if (selectedKey === workspaceKey) {
        replyBody.value = "";
      }
      recordAnalytics("reply_posted");
    } else if (selectedKey === workspaceKey) {
      replyBody.value = body;
      replyBody.focus();
    }
  } finally {
    replySubmitting.delete(workspaceKey);
    renderWorkspace();
  }
}

function availableRepositories(): DesktopRepository[] {
  const repositories = new Map(
    configuredRepositories.map((repository) => [
      repository.project_key,
      repository,
    ]),
  );
  for (const workspace of workspaces.values()) {
    const projectKey = workspace.project_key;
    if (projectKey === null) {
      continue;
    }
    const label =
      primaryTask(workspace)?.repo ??
      repositories.get(projectKey)?.label ??
      projectKeyLabel(projectKey);
    repositories.set(projectKey, { project_key: projectKey, label });
  }
  return [...repositories.values()].sort((left, right) =>
    left.label.localeCompare(right.label),
  );
}

function openNewWorkspaceDialog(): void {
  const repositories = availableRepositories();
  if (repositories.length === 0) {
    setStatus("Configure a GitHub repository in Settings first.");
    return;
  }
  const selectedProject =
    selectedKey === null ? null : workspaces.get(selectedKey)?.project_key;
  newWorkspaceProject.replaceChildren(
    ...repositories.map((repository) => {
      const option = new Option(repository.label, repository.project_key);
      option.selected = repository.project_key === selectedProject;
      return option;
    }),
  );
  newWorkspaceName.value = "";
  newWorkspaceAgent.checked = true;
  newWorkspaceError.classList.add("hidden");
  newWorkspaceDialog.showModal();
  newWorkspaceName.focus();
}

async function createWorkspace(): Promise<void> {
  if (creatingWorkspace) {
    return;
  }
  const name = newWorkspaceName.value.trim();
  if (newWorkspaceProject.value === "") {
    newWorkspaceError.textContent = "Choose a repository.";
    newWorkspaceError.classList.remove("hidden");
    return;
  }
  if (name === "") {
    newWorkspaceError.textContent = "Name the workspace.";
    newWorkspaceError.classList.remove("hidden");
    newWorkspaceName.focus();
    return;
  }
  creatingWorkspace = true;
  try {
    const succeeded = await sendCommand(
      createWorkspaceCommand(
        name,
        newWorkspaceProject.value,
        newWorkspaceAgent.checked ? defaultAgent : null,
      ),
    );
    if (succeeded) {
      newWorkspaceDialog.close();
      setStatus(`Creating ${name}…`);
    }
  } finally {
    creatingWorkspace = false;
  }
}

async function runCommands(
  commands: LazyboxCommand[],
  pendingMessage: string,
  successMessage: string,
): Promise<boolean> {
  setStatus(pendingMessage);
  for (const command of commands) {
    if (!(await sendCommand(command))) {
      return false;
    }
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
      selected_scopes: ["github:acme/relay"],
      agents: [
        { id: "codex", label: "Codex", available: true, models: [], default_tier: null },
        {
          id: "claude",
          label: "Claude Code",
          available: true,
          models: [
            { alias: "S", label: "Haiku" },
            { alias: "M", label: "Sonnet" },
            { alias: "L", label: "Opus" },
          ],
          default_tier: "L",
        },
      ],
      default_agent: defaultAgent,
      analytics_enabled: false,
      diagnostics_path: "~/.lazybox/v2/desktop-crashes",
      theme: null,
      themes: PREVIEW_THEMES,
      keymap_preset: null,
      terminal_new_layout: "split",
      activity_pane_default: "full",
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
  selectedScopes = new Set(state.selected_scopes);
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
  renderModelOptions(defaultAgentSelect.value);

  availableThemes = state.themes;
  selectedTheme = state.theme;
  renderThemeList();
  applyThemeByName(selectedTheme);
  keymapPresetLabel.textContent = `Keymap: ${state.keymap_preset ?? "default"}`;

  terminalLayoutSelect.value = state.terminal_new_layout;
  activityPaneSelect.value = state.activity_pane_default;

  analyticsEnabled.checked = state.analytics_enabled;
  diagnosticsPath.textContent = `Crash reports: ${state.diagnostics_path}`;
  renderRepositories();
}

function renderModelOptions(agentId: string): void {
  const agent = setupState?.agents.find((option) => option.id === agentId);
  const tiers = agent?.models ?? [];
  defaultModelField.classList.toggle("hidden", tiers.length === 0);
  defaultModelSelect.replaceChildren();
  // When the agent has no configured default tier, offer an explicit
  // "agent default" entry (empty value → saved as null) rather than
  // letting the first tier auto-select and silently become the new
  // persisted default.
  if (agent?.default_tier == null) {
    const none = document.createElement("option");
    none.value = "";
    none.textContent = "Agent default";
    defaultModelSelect.append(none);
  }
  for (const tier of tiers) {
    const option = document.createElement("option");
    option.value = tier.alias;
    option.textContent = tier.label;
    defaultModelSelect.append(option);
  }
  defaultModelSelect.value = agent?.default_tier ?? "";
}

function renderThemeList(): void {
  themeList.replaceChildren();
  for (const theme of availableThemes) {
    const swatch = document.createElement("button");
    swatch.type = "button";
    swatch.className = "theme-swatch";
    swatch.role = "radio";
    swatch.setAttribute(
      "aria-checked",
      String(theme.name === selectedTheme),
    );
    swatch.classList.toggle("selected", theme.name === selectedTheme);
    swatch.style.setProperty("--swatch-surface", theme.colors.surface);
    swatch.style.setProperty("--swatch-accent", theme.colors.accent);
    swatch.style.setProperty("--swatch-text", theme.colors.text_strong);
    const dots = document.createElement("span");
    dots.className = "theme-swatch-dots";
    for (const color of [
      theme.colors.accent,
      theme.colors.success,
      theme.colors.warn,
      theme.colors.error,
    ]) {
      const dot = document.createElement("span");
      dot.style.background = color;
      dots.append(dot);
    }
    const name = document.createElement("span");
    name.className = "theme-swatch-name";
    name.textContent = theme.name;
    swatch.append(dots, name);
    swatch.addEventListener("click", () => {
      selectedTheme = theme.name;
      renderThemeList();
      applyThemeColors(theme.colors);
    });
    themeList.append(swatch);
  }
}

// Resolve a theme name to its palette and apply it. An unset (or
// unknown) name resolves to the first catalog entry — the shared default
// theme, exactly what the TUI shows when `ui.theme` is unset — so the two
// clients agree on the default rather than the desktop keeping a bespoke
// palette of its own.
function applyThemeByName(name: string | null): void {
  const theme =
    availableThemes.find((option) => option.name === name) ??
    availableThemes[0];
  if (theme !== undefined) {
    applyThemeColors(theme.colors);
  }
}

// Live theme application: drive the app chrome through CSS custom
// properties and re-skin the active xterm terminal so a theme change is
// visible without a restart.
function applyThemeColors(colors: DesktopThemeColors): void {
  currentThemeColors = colors;
  const root = document.documentElement;
  root.style.setProperty("--theme-accent", colors.accent);
  root.style.setProperty("--theme-hover", colors.hover);
  root.style.setProperty("--theme-success", colors.success);
  root.style.setProperty("--theme-warn", colors.warn);
  root.style.setProperty("--theme-error", colors.error);
  root.style.setProperty("--theme-text-strong", colors.text_strong);
  root.style.setProperty("--theme-text-dim", colors.text_dim);
  root.style.setProperty("--theme-chrome", colors.chrome);
  root.style.setProperty("--theme-fill", colors.fill);
  root.style.setProperty("--theme-surface", colors.surface);
  if (activeTerminal) {
    activeTerminal.terminal.options.theme = terminalTheme(colors);
  }
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
  for (const id of [...selectedScopes].filter((scope) =>
    scope.replace(/^github:/, "").includes("/"),
  )) {
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
  const owners = [
    ...new Set([
      ...repositories.map((repository) => repository.owner),
      ...[...selectedScopes]
        .map((scope) => scope.replace(/^github:/, ""))
        .filter((scope) => !scope.includes("/")),
    ]),
  ].sort();
  for (const owner of owners) {
    const ownerScope = `github:${owner}`;
    const label = document.createElement("label");
    label.className = "repository-option";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.value = ownerScope;
    checkbox.checked = selectedScopes.has(ownerScope);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) {
        selectedScopes.add(ownerScope);
        for (const scope of selectedScopes) {
          if (scope.startsWith(`${ownerScope}/`)) {
            selectedScopes.delete(scope);
          }
        }
      } else {
        selectedScopes.delete(ownerScope);
      }
      renderRepositories();
    });
    const name = document.createElement("span");
    name.textContent = `All repositories in ${owner}`;
    label.append(checkbox, name);
    repositoryList.append(label);
  }
  for (const repository of repositories) {
    const label = document.createElement("label");
    label.className = "repository-option";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.value = repository.id;
    checkbox.checked = selectedScopes.has(repository.id);
    checkbox.disabled = selectedScopes.has(`github:${repository.owner}`);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) {
        selectedScopes.delete(`github:${repository.owner}`);
        selectedScopes.add(repository.id);
      } else {
        selectedScopes.delete(repository.id);
      }
      renderRepositories();
    });
    const name = document.createElement("span");
    name.textContent = repository.label;
    label.append(checkbox, name);
    repositoryList.append(label);
  }
  updateRepositorySelectionCount();
}

function updateRepositorySelectionCount(): void {
  repositorySelectionCount.textContent =
    selectedScopes.size === 0
      ? setupRequired
        ? "No scope selected"
        : "All accessible repositories"
      : `${selectedScopes.size} selected`;
}

async function saveSettings(): Promise<void> {
  setupError.classList.add("hidden");
  if (setupRequired && selectedScopes.size === 0) {
    showSetupError("Select a GitHub organization or repository.");
    return;
  }
  if (defaultAgentSelect.value.length === 0) {
    showSetupError("Install and select a default agent.");
    return;
  }
  const accepted = await confirmUserAction(
    "Save desktop settings?",
    "lazybox will update the shared configuration. Provider scope and default-agent changes restart the app; theme and workspace changes apply immediately.",
    "Save settings",
  );
  if (!accepted) {
    return;
  }
  saveSettingsButton.disabled = true;
  saveSettingsButton.textContent = "Saving…";
  try {
    let restart = false;
    if (!previewMode) {
      restart = await invoke<boolean>("save_desktop_settings", {
        settings: {
          github_scopes: [...selectedScopes],
          default_agent: defaultAgentSelect.value,
          analytics_enabled: analyticsEnabled.checked,
          theme: selectedTheme,
          terminal_new_layout: terminalLayoutSelect.value,
          activity_pane_default: activityPaneSelect.value,
          default_model_tier:
            defaultModelField.classList.contains("hidden") ||
            defaultModelSelect.value.length === 0
              ? null
              : defaultModelSelect.value,
        },
      });
    }
    if (restart) {
      setStatus("Settings saved. Restarting lazybox…");
    } else {
      setStatus("Settings saved.");
      saveSettingsButton.disabled = false;
      saveSettingsButton.textContent = "Save settings";
      setupDialog.close();
    }
  } catch (error) {
    showSetupError(String(error));
    saveSettingsButton.disabled = false;
    saveSettingsButton.textContent = "Save settings";
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
    console.warn("lazybox desktop analytics write failed", error);
  });
}

function handleKeyboard(event: KeyboardEvent): void {
  if ((event.metaKey || event.ctrlKey) && event.key === ",") {
    event.preventDefault();
    if (!setupRequired && !setupDialog.open) {
      void openSettings();
    }
    return;
  }
  if (isSnippetShortcut(event)) {
    event.preventDefault();
    void openSnippetPicker();
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
  if (
    editable ||
    setupDialog.open ||
    confirmDialog.open ||
    newWorkspaceDialog.open ||
    snippetDialog.open
  ) {
    return;
  }
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    navigateWorkspaces(event.key === "ArrowDown" ? 1 : -1);
    return;
  }
  if (
    event.key === "Enter" &&
    selectedKey !== null &&
    shouldHandleWorkspaceEnter(
      true,
      editable,
      target instanceof Element && target.closest("button, a") !== null,
    )
  ) {
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
  } else if (event.key === "o") {
    event.preventDefault();
    void cycleSortMode();
  } else if (event.key === "R") {
    event.preventDefault();
    void refreshInbox(true);
  }
}

function navigateWorkspaces(delta: number): void {
  const keys = inboxView === null ? [] : workspaceKeysInOrder(inboxView);
  if (keys.length === 0) {
    return;
  }
  const current = keys.findIndex((key) => key === selectedKey);
  const next =
    current < 0
      ? 0
      : Math.max(0, Math.min(keys.length - 1, current + delta));
  const key = keys[next];
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

function element<T extends HTMLElement>(id: string): T {
  const value = document.getElementById(id);
  if (value === null) {
    throw new Error(`missing #${id}`);
  }
  return value as T;
}
