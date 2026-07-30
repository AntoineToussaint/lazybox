import { Channel, invoke } from "@tauri-apps/api/core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import {
  applyWorkspaceEvent,
  filteredWorkspaces,
  type InboxFilter,
  primaryTask,
  sortedWorkspaces,
  taskReference,
  unreadCount,
} from "./model";
import {
  type DesktopInfo,
  type DesktopRepository,
  type DesktopStreamMessage,
  type LazyboxCommand,
  type LazyboxEvent,
  type TerminalKind,
  type TerminalSnapshot,
  type Workspace,
  type WorkspacesResponse,
  createWorkspaceCommand,
  markReadCommand,
  postReplyCommand,
  spawnAgentCommand,
  spawnShellCommand,
  terminalKindLabel,
} from "./protocol";
import {
  canCompleteSetup,
  mergeRepositoryScopes,
  type AnalyticsEvent,
  type DesktopScope,
  type DesktopSetupInput,
  type DesktopSetupStatus,
} from "./setup";
import {
  TerminalFrameDecoder,
  type TerminalBinaryFrame,
  type TerminalInputIntent,
  type TerminalReplayState,
  closeTerminalFrame,
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

const workspaceList = element<HTMLDivElement>("workspace-list");
const workspaceCount = element<HTMLSpanElement>("workspace-count");
const unreadTotal = element<HTMLSpanElement>("unread-count");
const inboxLoading = element<HTMLDivElement>("inbox-loading");
const inboxError = element<HTMLDivElement>("inbox-error");
const inboxErrorMessage = element<HTMLSpanElement>("inbox-error-message");
const searchInput = element<HTMLInputElement>("search-input");
const filterSelect = element<HTMLSelectElement>("filter-select");
const workspaceEmpty = element<HTMLDivElement>("workspace-empty");
const workspaceDetail = element<HTMLDivElement>("workspace-detail");
const taskKicker = element<HTMLParagraphElement>("task-kicker");
const taskTitle = element<HTMLHeadingElement>("task-title");
const taskMeta = element<HTMLParagraphElement>("task-meta");
const taskDescription = element<HTMLDivElement>("task-description");
const activityCount = element<HTMLSpanElement>("activity-count");
const activityList = element<HTMLDivElement>("activity-list");
const agentLabel = element<HTMLElement>("agent-label");
const agentSelect = element<HTMLSelectElement>("agent-select");
const spawnActionLabel = element<HTMLSpanElement>("spawn-action-label");
const spawnButton = element<HTMLButtonElement>("spawn-button");
const shellButton = element<HTMLButtonElement>("shell-button");
const markReadButton = element<HTMLButtonElement>("mark-read-button");
const replyButton = element<HTMLButtonElement>("reply-button");
const newWorkspaceButton = element<HTMLButtonElement>("new-workspace-button");
const refreshButton = element<HTMLButtonElement>("refresh-button");
const retryButton = element<HTMLButtonElement>("retry-button");
const settingsButton = element<HTMLButtonElement>("settings-button");
const terminalHost = element<HTMLDivElement>("terminal");
const terminalEmpty = element<HTMLDivElement>("terminal-empty");
const terminalTitle = element<HTMLHeadingElement>("terminal-title");
const terminalState = element<HTMLSpanElement>("terminal-state");
const closeTerminalButton = element<HTMLButtonElement>("close-terminal-button");
const connectionDot = element<HTMLSpanElement>("connection-dot");
const connectionLabel = element<HTMLSpanElement>("connection-label");
const statusMessage = element<HTMLSpanElement>("status-message");
const setupDialog = element<HTMLDialogElement>("setup-dialog");
const setupForm = element<HTMLFormElement>("setup-form");
const setupEyebrow = element<HTMLParagraphElement>("setup-eyebrow");
const setupTitle = element<HTMLHeadingElement>("setup-title");
const setupIntro = element<HTMLParagraphElement>("setup-intro");
const setupCloseButton = element<HTMLButtonElement>("setup-close-button");
const setupCancelButton = element<HTMLButtonElement>("setup-cancel-button");
const setupSubmitButton = element<HTMLButtonElement>("setup-submit-button");
const setupError = element<HTMLParagraphElement>("setup-error");
const githubStatus = element<HTMLParagraphElement>("github-status");
const githubLoginButton = element<HTMLButtonElement>("github-login-button");
const githubRecheckButton = element<HTMLButtonElement>("github-recheck-button");
const organizationSelect = element<HTMLSelectElement>("organization-select");
const repositoryList = element<HTMLFieldSetElement>("repository-list");
const setupAgentSelect = element<HTMLSelectElement>("setup-agent-select");
const analyticsCheckbox = element<HTMLInputElement>("analytics-checkbox");
const crashCheckbox = element<HTMLInputElement>("crash-checkbox");
const replyDialog = element<HTMLDialogElement>("reply-dialog");
const replyForm = element<HTMLFormElement>("reply-form");
const replyBody = element<HTMLTextAreaElement>("reply-body");
const replyError = element<HTMLParagraphElement>("reply-error");
const replyCancelButton = element<HTMLButtonElement>("reply-cancel-button");
const replySubmitButton = element<HTMLButtonElement>("reply-submit-button");
const newWorkspaceDialog = element<HTMLDialogElement>("new-workspace-dialog");
const newWorkspaceForm = element<HTMLFormElement>("new-workspace-form");
const newWorkspaceProject = element<HTMLSelectElement>("new-workspace-project");
const newWorkspaceName = element<HTMLInputElement>("new-workspace-name");
const newWorkspaceAgent = element<HTMLInputElement>("new-workspace-agent");
const newWorkspaceError = element<HTMLParagraphElement>("new-workspace-error");
const newWorkspaceCancelButton = element<HTMLButtonElement>(
  "new-workspace-cancel-button",
);
const closeTerminalDialog = element<HTMLDialogElement>(
  "close-terminal-dialog",
);
const closeTerminalForm = element<HTMLFormElement>("close-terminal-form");
const closeTerminalCancelButton = element<HTMLButtonElement>(
  "close-terminal-cancel-button",
);

let workspaces = new Map<string, Workspace>();
let terminals = new Map<number, TerminalRecord>();
let selectedKey: string | null = null;
let defaultAgent = "claude";
let configuredAgents = ["claude"];
let configuredRepositories: DesktopRepository[] = [];
let inboxFilter: InboxFilter = "all";
let searchQuery = "";
let setupStatus: DesktopSetupStatus | null = null;
let selectedScopeIds = new Set<string>();
let repositoryScopes: DesktopScope[] = [];
let setupBlocking = false;
let clientStarted = false;
let replySubmitting = false;
let previewMode = false;
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
  void sendCommand("Refresh", "Refreshing providers…");
});

spawnButton.addEventListener("click", () => {
  void openOrStartAgent();
});

shellButton.addEventListener("click", () => void openOrStartShell());
markReadButton.addEventListener("click", () => void markSelectedRead());
replyButton.addEventListener("click", openReplyDialog);
newWorkspaceButton.addEventListener("click", openNewWorkspaceDialog);
closeTerminalButton.addEventListener("click", () => closeTerminalDialog.showModal());
retryButton.addEventListener("click", () => void bootClient());
settingsButton.addEventListener("click", () => void openSetup(false));
searchInput.addEventListener("input", () => {
  searchQuery = searchInput.value;
  renderInbox();
});
filterSelect.addEventListener("change", () => {
  inboxFilter = filterSelect.value as InboxFilter;
  renderInbox();
});
agentSelect.addEventListener("change", () => {
  defaultAgent = agentSelect.value;
  renderWorkspace();
});
githubLoginButton.addEventListener("click", () => void beginGithubLogin());
githubRecheckButton.addEventListener("click", () => void refreshSetupStatus());
organizationSelect.addEventListener("change", () => {
  if (organizationSelect.value !== "") {
    void loadRepositories(organizationSelect.value);
  }
});
setupAgentSelect.addEventListener("change", updateSetupSubmit);
analyticsCheckbox.addEventListener("change", updateSetupSubmit);
crashCheckbox.addEventListener("change", updateSetupSubmit);
setupCloseButton.addEventListener("click", closeSetup);
setupCancelButton.addEventListener("click", closeSetup);
setupForm.addEventListener("submit", (event) => void submitSetup(event));
setupDialog.addEventListener("cancel", (event) => {
  if (setupBlocking) {
    event.preventDefault();
  }
});
replyCancelButton.addEventListener("click", () => replyDialog.close());
replyForm.addEventListener("submit", (event) => void submitReply(event));
newWorkspaceCancelButton.addEventListener("click", () =>
  newWorkspaceDialog.close(),
);
newWorkspaceForm.addEventListener("submit", (event) =>
  void submitNewWorkspace(event),
);
closeTerminalCancelButton.addEventListener("click", () =>
  closeTerminalDialog.close(),
);
closeTerminalForm.addEventListener("submit", (event) => {
  event.preventDefault();
  if (activeTerminal !== null) {
    void sendTerminalFrame(closeTerminalFrame(activeTerminal.id));
    setStatus("Closing terminal…");
  }
  closeTerminalDialog.close();
});

window.addEventListener("resize", () => scheduleResize());
window.addEventListener("keydown", handleGlobalKey);

void boot();

async function boot(): Promise<void> {
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("preview")) {
    const { loadPreview } = await import("./preview");
    const preview = loadPreview();
    previewMode = true;
    defaultAgent = preview.defaultAgent;
    configuredAgents = [preview.defaultAgent];
    configuredRepositories = [];
    workspaces = preview.workspaces;
    terminals = preview.terminals;
    configureAgentSelect();
    setInboxReady();
    setConnection(true, "Preview data");
    selectWorkspace(preview.selectedKey);
    render();
    return;
  }

  try {
    const info = await invoke<DesktopInfo>("desktop_info");
    if (!info.setup_completed) {
      setConnection(false, "Setup required");
      setStatus("Complete setup to start the inbox.");
      await openSetup(true);
      return;
    }
    await bootClient(info);
  } catch (error) {
    showInboxError(error);
  }
}

async function bootClient(existingInfo?: DesktopInfo): Promise<void> {
  try {
    const info = existingInfo ?? (await invoke<DesktopInfo>("desktop_info"));
    terminalDecoder = new TerminalFrameDecoder(info.max_terminal_frame_bytes);
    maxTerminalWriteBytes = info.max_terminal_write_bytes;
    configuredAgents = info.agents;
    configuredRepositories = info.repositories;
    defaultAgent = info.default_agent;
    configureAgentSelect();
    const initial = await invoke<WorkspacesResponse>("list_workspaces");
    workspaces = new Map(
      initial.workspaces.map((workspace) => [workspace.key, workspace]),
    );
    if (initial.warnings.length > 0) {
      setStatus(initial.warnings[0] ?? "Some workspaces could not be decoded.");
    } else {
      setStatus("Inbox loaded.");
    }
    chooseInitialWorkspace();
    setInboxReady();
    render();

    if (!clientStarted) {
      const events = new Channel<DesktopStreamMessage>();
      events.onmessage = handleStreamMessage;
      await invoke("subscribe_events", { onEvent: events });
      clientStarted = true;
      void readTerminalData();
    }
  } catch (error) {
    showInboxError(error);
  }
}

function configureAgentSelect(): void {
  agentSelect.replaceChildren();
  for (const agent of configuredAgents) {
    const option = document.createElement("option");
    option.value = agent;
    option.textContent = agent;
    option.selected = agent === defaultAgent;
    agentSelect.append(option);
  }
}

function setInboxReady(): void {
  inboxLoading.classList.add("hidden");
  inboxError.classList.add("hidden");
  workspaceList.classList.remove("hidden");
}

function showInboxError(error: unknown): void {
  setConnection(false, "Daemon unavailable");
  inboxLoading.classList.add("hidden");
  workspaceList.classList.add("hidden");
  inboxError.classList.remove("hidden");
  inboxErrorMessage.textContent = String(error);
  setStatus(String(error));
}

async function openSetup(blocking: boolean): Promise<void> {
  setupBlocking = blocking;
  setupEyebrow.textContent = blocking ? "First run" : "Settings";
  setupTitle.textContent = blocking ? "Set up lazybox" : "Desktop settings";
  setupIntro.textContent = blocking
    ? "Connect GitHub, choose the repositories you want in your inbox, and pick the agent lazybox starts by default."
    : "Update the focused desktop workflow. Saving restarts the embedded daemon so every setting applies together.";
  setupCloseButton.classList.toggle("hidden", blocking);
  setupCancelButton.classList.toggle("hidden", blocking);
  setSetupError(null);
  setupSubmitButton.disabled = true;
  setupSubmitButton.textContent = "Checking setup…";
  if (!setupDialog.open) {
    setupDialog.showModal();
  }
  await refreshSetupStatus();
}

async function refreshSetupStatus(): Promise<void> {
  githubStatus.textContent = "Checking authentication…";
  githubRecheckButton.disabled = true;
  try {
    setupStatus = await invoke<DesktopSetupStatus>("desktop_setup_status");
    selectedScopeIds = new Set(setupStatus.selected_scopes);
    renderSetupStatus(setupStatus);
    if (setupStatus.github.available) {
      await loadOrganizations();
    }
  } catch (error) {
    setSetupError(String(error));
  } finally {
    githubRecheckButton.disabled = false;
    updateSetupSubmit();
  }
}

function renderSetupStatus(status: DesktopSetupStatus): void {
  githubStatus.textContent = status.github.detail;
  githubLoginButton.classList.toggle("hidden", status.github.available);
  analyticsCheckbox.checked = status.analytics_enabled;
  crashCheckbox.checked = status.crash_reports_enabled;
  setupAgentSelect.replaceChildren();
  const selectedAgent =
    status.agents.find(
      (agent) => agent.available && agent.id === status.default_agent,
    )?.id ?? status.agents.find((agent) => agent.available)?.id;
  for (const agent of status.agents) {
    const option = document.createElement("option");
    option.value = agent.id;
    option.disabled = !agent.available;
    option.textContent = `${agent.label} · ${agent.detail}`;
    option.selected = agent.id === selectedAgent;
    setupAgentSelect.append(option);
  }
}

async function loadOrganizations(): Promise<void> {
  organizationSelect.disabled = true;
  organizationSelect.replaceChildren(new Option("Loading organizations…", ""));
  repositoryList.disabled = true;
  repositoryList.replaceChildren(statusParagraph("Loading repositories…"));
  try {
    const organizations = await invoke<DesktopScope[]>(
      "list_github_organizations",
    );
    organizationSelect.replaceChildren(
      new Option("Choose an organization", ""),
    );
    for (const organization of organizations) {
      organizationSelect.append(
        new Option(organization.label, organization.id),
      );
    }
    organizationSelect.disabled = organizations.length === 0;
    const selected = [...selectedScopeIds][0];
    const owner =
      selected === undefined
        ? organizations[0]?.id
        : `github:${selected.slice("github:".length).split("/")[0]}`;
    if (
      owner !== undefined &&
      organizations.some((organization) => organization.id === owner)
    ) {
      organizationSelect.value = owner;
      await loadRepositories(owner);
    } else {
      repositoryList.replaceChildren(
        statusParagraph("Choose an organization to load repositories."),
      );
    }
  } catch (error) {
    setSetupError(String(error));
    organizationSelect.replaceChildren(
      new Option("Could not load organizations", ""),
    );
  }
}

async function loadRepositories(parentId: string): Promise<void> {
  repositoryList.disabled = true;
  repositoryList.replaceChildren(statusParagraph("Loading repositories…"));
  try {
    const repositories = await invoke<DesktopScope[]>(
      "list_github_repositories",
      { parentId },
    );
    repositoryScopes = mergeRepositoryScopes(repositoryScopes, repositories);
    renderRepositoryList(parentId);
  } catch (error) {
    repositoryList.replaceChildren(
      statusParagraph("Repositories could not be loaded."),
    );
    setSetupError(String(error));
  }
}

function renderRepositoryList(parentId: string): void {
  repositoryList.replaceChildren();
  const repositories = repositoryScopes.filter(
    (repository) => repository.parent === parentId,
  );
  repositoryList.disabled = false;
  const organization = document.createElement("label");
  organization.className = "check-row";
  const organizationCheckbox = document.createElement("input");
  organizationCheckbox.type = "checkbox";
  organizationCheckbox.value = parentId;
  organizationCheckbox.checked = selectedScopeIds.has(parentId);
  organizationCheckbox.addEventListener("change", () => {
    if (organizationCheckbox.checked) {
      selectedScopeIds.add(parentId);
      for (const repository of repositories) {
        selectedScopeIds.delete(repository.id);
      }
    } else {
      selectedScopeIds.delete(parentId);
    }
    renderRepositoryList(parentId);
    updateSetupSubmit();
  });
  const organizationText = document.createElement("span");
  organizationText.textContent = "All repositories in this organization";
  organization.append(organizationCheckbox, organizationText);
  repositoryList.append(organization);
  if (repositories.length === 0) {
    repositoryList.append(statusParagraph("No repositories found."));
    return;
  }
  for (const repository of repositories) {
    const label = document.createElement("label");
    label.className = "check-row";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.value = repository.id;
    checkbox.checked = selectedScopeIds.has(repository.id);
    checkbox.disabled = organizationCheckbox.checked;
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) {
        selectedScopeIds.delete(parentId);
        selectedScopeIds.add(repository.id);
      } else {
        selectedScopeIds.delete(repository.id);
      }
      updateSetupSubmit();
    });
    const text = document.createElement("span");
    text.textContent = repository.label;
    label.append(checkbox, text);
    repositoryList.append(label);
  }
}

function setupInput(): DesktopSetupInput {
  return {
    github_scopes: [...selectedScopeIds].sort(),
    default_agent: setupAgentSelect.value,
    analytics_enabled: analyticsCheckbox.checked,
    crash_reports_enabled: crashCheckbox.checked,
  };
}

function updateSetupSubmit(): void {
  if (setupStatus === null) {
    setupSubmitButton.disabled = true;
    return;
  }
  const input = setupInput();
  setupSubmitButton.disabled = setupBlocking
    ? !canCompleteSetup(setupStatus, input)
    : input.default_agent === "";
  setupSubmitButton.textContent = "Save and restart";
}

async function beginGithubLogin(): Promise<void> {
  setSetupError(null);
  githubLoginButton.disabled = true;
  try {
    await invoke("begin_github_login");
    githubStatus.textContent =
      "Authentication opened in Terminal. Finish there, then recheck.";
  } catch (error) {
    setSetupError(String(error));
  } finally {
    githubLoginButton.disabled = false;
  }
}

async function submitSetup(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  if (setupStatus === null || setupSubmitButton.disabled) {
    return;
  }
  setSetupError(null);
  setupSubmitButton.disabled = true;
  setupSubmitButton.textContent = "Saving and restarting…";
  try {
    await invoke("save_desktop_setup", { input: setupInput() });
  } catch (error) {
    setSetupError(String(error));
    updateSetupSubmit();
  }
}

function closeSetup(): void {
  if (!setupBlocking) {
    setupDialog.close();
  }
}

function setSetupError(message: string | null): void {
  setupError.classList.toggle("hidden", message === null);
  setupError.textContent = message ?? "";
}

function statusParagraph(message: string): HTMLParagraphElement {
  const paragraph = document.createElement("p");
  paragraph.textContent = message;
  return paragraph;
}

function recordAnalytics(event: AnalyticsEvent): void {
  if (!previewMode) {
    void invoke("record_analytics_event", { event });
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
    }
    applyPendingTerminalFrames(payload.terminal_id);
  } else if ("TerminalExited" in event) {
    const record = terminals.get(event.TerminalExited.terminal_id);
    if (record !== undefined) {
      record.state = `exited ${event.TerminalExited.exit_code ?? ""}`.trim();
    }
    if (activeTerminal?.id === event.TerminalExited.terminal_id) {
      setTerminalState(record?.state ?? "exited");
      closeTerminalButton.classList.add("hidden");
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
    setStatus(
      `Preparing workspace: ${event.WorktreeProgress.step.toLowerCase()}`,
    );
  }

  if (workspaceChanged) {
    render();
  } else if (
    "TerminalSpawned" in event ||
    "TerminalExited" in event ||
    "AgentState" in event
  ) {
    renderWorkspace();
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
  const items = filteredWorkspaces(workspaces.values(), searchQuery, inboxFilter);
  workspaceList.replaceChildren();
  const unread = [...workspaces.values()].reduce(
    (sum, workspace) => sum + unreadCount(workspace),
    0,
  );
  workspaceCount.textContent =
    items.length === workspaces.size
      ? `${items.length} workspace${items.length === 1 ? "" : "s"}`
      : `${items.length} of ${workspaces.size}`;
  unreadTotal.textContent = `${unread} unread`;

  if (items.length === 0) {
    const empty = document.createElement("p");
    empty.className = "inbox-empty";
    empty.textContent =
      workspaces.size === 0
        ? "No matching GitHub tasks yet. Refresh after selecting repositories in Settings."
        : "No workspaces match this search and filter.";
    workspaceList.append(empty);
    return;
  }

  for (const workspace of items) {
    const task = primaryTask(workspace);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "workspace-row";
    button.classList.toggle("selected", workspace.key === selectedKey);
    button.setAttribute(
      "aria-selected",
      String(workspace.key === selectedKey),
    );
    button.setAttribute("role", "option");
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
    newWorkspaceButton.disabled = availableRepositories().length === 0;
    spawnButton.disabled = true;
    shellButton.disabled = true;
    markReadButton.disabled = true;
    replyButton.disabled = true;
    return;
  }

  const task = primaryTask(workspace);
  const agentTerminal = terminalForWorkspace(
    workspace.key,
    (kind) => kind !== "Shell" && "Agent" in kind,
  );
  const shellTerminal = terminalForWorkspace(
    workspace.key,
    (kind) => kind === "Shell",
  );
  spawnActionLabel.textContent = agentTerminal === undefined ? "Start" : "Resume";
  shellButton.textContent = shellTerminal === undefined ? "Shell" : "Resume shell";
  spawnButton.disabled = false;
  shellButton.disabled = false;
  newWorkspaceButton.disabled = availableRepositories().length === 0;
  markReadButton.disabled = unreadCount(workspace) === 0;
  replyButton.disabled = task === null;
  taskKicker.textContent = task === null ? "Local workspace" : taskReference(task);
  taskTitle.textContent = task?.title ?? workspace.name;
  taskMeta.textContent = [
    task?.repo,
    task?.role?.toLowerCase(),
    task?.ci === undefined ? null : `CI ${task.ci.toLowerCase()}`,
    task?.review === undefined ? null : `review ${task.review.toLowerCase()}`,
    task?.reviewers.length ? `${task.reviewers.length} reviewer${task.reviewers.length === 1 ? "" : "s"}` : null,
    task?.labels.slice(0, 3).map((label) => label.name).join(", "),
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

  for (const [index, activity] of workspace.activity.slice(0, 30).entries()) {
    const card = document.createElement("article");
    card.className = "activity-card";
    card.classList.toggle(
      "unread",
      index < workspace.activity.length - workspace.seen_count &&
        !workspace.read_indices.includes(index),
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

function terminalForWorkspace(
  workspaceKey: string,
  matches: (kind: TerminalKind) => boolean,
): TerminalRecord | undefined {
  return [...terminals.values()].find(
    (terminal) =>
      terminal.sessionKey === workspaceKey &&
      !terminal.state.startsWith("exited") &&
      matches(terminal.kind),
  );
}

async function openOrStartAgent(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const existing = terminalForWorkspace(
    selectedKey,
    (kind) => kind !== "Shell" && "Agent" in kind && kind.Agent === defaultAgent,
  );
  if (existing !== undefined) {
    attachTerminal(existing.id);
    setStatus(`Resumed ${defaultAgent}.`);
    return;
  }
  const sent = await sendCommand(
    spawnAgentCommand(selectedKey, defaultAgent),
    `Preparing a workspace for ${defaultAgent}…`,
  );
  if (sent.ok) {
    recordAnalytics("agent_started");
  }
}

async function openOrStartShell(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const existing = terminalForWorkspace(
    selectedKey,
    (kind) => kind === "Shell",
  );
  if (existing !== undefined) {
    attachTerminal(existing.id);
    setStatus("Resumed shell.");
    return;
  }
  const sent = await sendCommand(
    spawnShellCommand(selectedKey),
    "Preparing workspace shell…",
  );
  if (sent.ok) {
    recordAnalytics("shell_started");
  }
}

async function markSelectedRead(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  await sendCommand(markReadCommand(selectedKey), "Marking workspace read…");
}

function openReplyDialog(): void {
  if (selectedKey === null) {
    return;
  }
  replyError.classList.add("hidden");
  replyError.textContent = "";
  if (!replyDialog.open) {
    replyDialog.showModal();
  }
  replyBody.focus();
}

async function submitReply(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  if (replySubmitting) {
    return;
  }
  const body = replyBody.value.trim();
  if (selectedKey === null || body === "") {
    replyError.textContent = "Write a reply before posting.";
    replyError.classList.remove("hidden");
    return;
  }
  replySubmitting = true;
  replySubmitButton.disabled = true;
  replyCancelButton.disabled = true;
  replyError.classList.add("hidden");
  const result = await sendCommand(postReplyCommand(selectedKey, body), "Posting reply…");
  replySubmitting = false;
  replySubmitButton.disabled = false;
  replyCancelButton.disabled = false;
  if (result.ok) {
    replyDialog.close();
    replyBody.value = "";
    setStatus("Reply posted. Refreshing the task…");
    recordAnalytics("reply_posted");
    await sendCommand("Refresh");
  } else {
    replyError.textContent = result.error;
    replyError.classList.remove("hidden");
    replyBody.focus();
  }
}

function openNewWorkspaceDialog(): void {
  const workspace =
    selectedKey === null ? undefined : workspaces.get(selectedKey);
  const repositories = availableRepositories();
  if (repositories.length === 0) {
    setStatus("Configure a GitHub repository in Settings first.");
    return;
  }
  newWorkspaceError.classList.add("hidden");
  newWorkspaceName.value = "";
  newWorkspaceAgent.checked = true;
  newWorkspaceProject.replaceChildren(
    ...repositories.map((repository) => {
      const option = new Option(repository.label, repository.project_key);
      option.selected = repository.project_key === workspace?.project_key;
      return option;
    }),
  );
  newWorkspaceDialog.showModal();
  newWorkspaceName.focus();
}

async function submitNewWorkspace(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  const name = newWorkspaceName.value.trim();
  if (newWorkspaceProject.value === "") {
    newWorkspaceError.textContent = "Choose a repository.";
    newWorkspaceError.classList.remove("hidden");
    return;
  }
  if (name === "") {
    newWorkspaceError.textContent = "Give the workspace a name.";
    newWorkspaceError.classList.remove("hidden");
    return;
  }
  const sent = await sendCommand(
    createWorkspaceCommand(
      name,
      newWorkspaceProject.value,
      newWorkspaceAgent.checked ? defaultAgent : null,
    ),
    `Creating ${name}…`,
  );
  if (sent.ok) {
    newWorkspaceDialog.close();
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
    if (workspace.project_key === null) {
      continue;
    }
    repositories.set(workspace.project_key, {
      project_key: workspace.project_key,
      label: primaryTask(workspace)?.repo ?? workspace.project_key,
    });
  }
  return [...repositories.values()].sort((left, right) =>
    left.label.localeCompare(right.label),
  );
}

function handleGlobalKey(event: KeyboardEvent): void {
  if (event.metaKey && event.key === ",") {
    event.preventDefault();
    if (!setupBlocking && !setupDialog.open) {
      void openSetup(false);
    }
    return;
  }
  if (event.metaKey && event.key.toLowerCase() === "r") {
    event.preventDefault();
    void sendCommand("Refresh", "Refreshing providers…");
    return;
  }
  if (
    event.key === "/" &&
    !isEditingElement(event.target) &&
    !setupDialog.open &&
    !replyDialog.open &&
    !newWorkspaceDialog.open
  ) {
    event.preventDefault();
    searchInput.focus();
    return;
  }
  if (
    (event.key === "ArrowDown" || event.key === "ArrowUp") &&
    !isEditingElement(event.target) &&
    !document.querySelector("dialog[open]")
  ) {
    const items = filteredWorkspaces(
      workspaces.values(),
      searchQuery,
      inboxFilter,
    );
    if (items.length === 0) {
      return;
    }
    event.preventDefault();
    const index = items.findIndex((workspace) => workspace.key === selectedKey);
    const delta = event.key === "ArrowDown" ? 1 : -1;
    const start = index === -1 ? (delta > 0 ? -1 : 0) : index;
    const next = (start + delta + items.length) % items.length;
    selectWorkspace(items[next]!.key);
  }
}

function isEditingElement(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement ||
    (target instanceof HTMLElement &&
      target.closest(".xterm-helper-textarea") !== null)
  );
}

function selectWorkspace(key: string): void {
  selectedKey = key;
  render();
  attachSelectedTerminal();
  void sendCommand({ FocusWorkspace: { session_key: key } });
  recordAnalytics("workspace_opened");
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
  closeTerminalButton.classList.toggle(
    "hidden",
    record.state.startsWith("exited"),
  );
  scheduleResize();
  terminal.focus();

  if (record.dirty || !record.replayAvailable) {
    requestTerminalResync(record);
  }
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
  closeTerminalButton.classList.add("hidden");
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

async function sendCommand(
  command: LazyboxCommand,
  pendingMessage?: string,
): Promise<{ ok: true } | { ok: false; error: string }> {
  if (pendingMessage !== undefined) {
    setStatus(pendingMessage);
  }
  if (previewMode) {
    return { ok: true };
  }
  try {
    await invoke("send_command", { command });
    return { ok: true };
  } catch (error) {
    const message = String(error);
    setStatus(message);
    return { ok: false, error: message };
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
