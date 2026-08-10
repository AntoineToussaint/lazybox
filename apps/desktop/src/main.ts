import { Channel, invoke } from "@tauri-apps/api/core";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import {
  InboxConnection,
  ReplyDrafts,
  activeFilters,
  applyWorkspaceEvent,
  canReplyToTask,
  detailSignals,
  filterMenuGroups,
  hasRepoScope,
  isTerminalTaskState,
  kindHeaderLabel,
  mailboxLabel,
  nextMailbox,
  orderedWorkspaceKeys,
  preferredTerminal,
  primaryTask,
  projectKeyLabel,
  rowSignals,
  SNOOZE_PRESETS,
  shouldHandleWorkspaceEnter,
  sortModeLabel,
  supportsTrackMain,
  taskReference,
  type TaskSignal,
  unreadCount,
  visibleUnreadCount,
  workspaceDiffTarget,
} from "./model";
import {
  type Activity,
  type ComputeOutcome,
  type DesktopInfo,
  type DesktopRepository,
  type DesktopStreamMessage,
  type Filter,
  type FilterMenuItem,
  type LazyboxCommand,
  type LazyboxEvent,
  type Mailbox,
  type PickerRow,
  type SnippetPickerView,
  type SortMode,
  type Task,
  type TerminalKind,
  type TerminalSnapshot,
  type PolicyArm,
  type Workspace,
  type WorkspacesResponse,
  type WorkspaceDiffDto,
  archiveCommand,
  closeIssueCommand,
  commandsForWorkspaceIntent,
  createWorkspaceCommand,
  deleteOrCloseCommand,
  deliverSnippetCommand,
  inspectWorkspaceDiffCommand,
  mergePrCommand,
  renameWorkspaceCommand,
  setAutoFixPoliciesCommand,
  setAutoMergeOnGreenCommand,
  setNotesCommand,
  setTrackMainCommand,
  snoozeCommand,
  spawnAgentCommand,
  syncWorkspaceCommand,
  terminalKindLabel,
  unsnoozeCommand,
  updateBranchCommand,
} from "./protocol";
import {
  autoSubmitRow,
  clampCursor,
  flattenRows,
  renderSnippetList,
  renderSnippetPreview,
} from "./snippet_picker";
import { buildDiffView } from "./diff_view";
import { terminalTheme } from "./theme";
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
  /** Tile in the tiles container that hosts this terminal's xterm. */
  tile: HTMLDivElement;
  /** Tab-strip chip (`role="tab"`) for this terminal. */
  tab: HTMLDivElement;
  /** The tab's live-state pill, updated as the runner's state changes. */
  stateEl: HTMLSpanElement;
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
const mailboxButton = element<HTMLButtonElement>("mailbox-button");
const mailboxLabelElement = element<HTMLElement>("mailbox-label");
const inboxSearch = element<HTMLInputElement>("inbox-search");
const filterButton = element<HTMLButtonElement>("filter-button");
const filterMenu = element<HTMLDivElement>("filter-menu");
const filterMenuBody = element<HTMLDivElement>("filter-menu-body");
const filterClear = element<HTMLButtonElement>("filter-clear");
const filterChips = element<HTMLDivElement>("filter-chips");
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
const actionsButton = element<HTMLButtonElement>("actions-button");
const actionsMenu = element<HTMLDivElement>("actions-menu");
const renameDialog = element<HTMLDialogElement>("rename-dialog");
const renameForm = element<HTMLFormElement>("rename-form");
const renameNameInput = element<HTMLInputElement>("rename-name");
const renameError = element<HTMLParagraphElement>("rename-error");
const renameCancel = element<HTMLButtonElement>("rename-cancel");
const replyForm = element<HTMLFormElement>("reply-form");
const replyBody = element<HTMLTextAreaElement>("reply-body");
const replyButton = element<HTMLButtonElement>("reply-button");
const autoMergeToggle = element<HTMLInputElement>("auto-merge-toggle");
const trackMainToggle = element<HTMLInputElement>("track-main-toggle");
const autoFixCiSelect = element<HTMLSelectElement>("auto-fix-ci-select");
const autoFixConflictSelect = element<HTMLSelectElement>(
  "auto-fix-conflict-select",
);
const snoozeSelect = element<HTMLSelectElement>("snooze-select");
const snoozeButton = element<HTMLButtonElement>("snooze-button");
const unsnoozeButton = element<HTMLButtonElement>("unsnooze-button");
const snoozeStatus = element<HTMLSpanElement>("snooze-status");
const syncButton = element<HTMLButtonElement>("sync-button");
const notesForm = element<HTMLFormElement>("notes-form");
const notesBody = element<HTMLTextAreaElement>("notes-body");
const notesSaveButton = element<HTMLButtonElement>("notes-save-button");
const refreshButton = element<HTMLButtonElement>("refresh-button");
const settingsButton = element<HTMLButtonElement>("settings-button");
const terminalTiles = element<HTMLDivElement>("terminal");
const terminalTabs = element<HTMLDivElement>("terminal-tabs");
const terminalEmpty = element<HTMLDivElement>("terminal-empty");
const terminalTitle = element<HTMLHeadingElement>("terminal-title");
const terminalState = element<HTMLSpanElement>("terminal-state");
const workspaceGrid = document.querySelector<HTMLElement>(".workspace-grid")!;
const rightPane = document.querySelector<HTMLElement>(".right-pane")!;
const rightPaneSplitter = element<HTMLDivElement>("right-pane-splitter");
const columnSplitter = element<HTMLDivElement>("column-splitter");
const ACTIVITY_MIN_PX = 120;
const TERMINAL_MIN_PX = 160;
const ACTIVITY_HEIGHT_KEY = "lazybox.activityHeight";
const SIDEBAR_MIN_PX = 240;
const RIGHT_MIN_PX = 360;
const SIDEBAR_WIDTH_KEY = "lazybox.sidebarWidth";
const connectionDot = element<HTMLSpanElement>("connection-dot");
const connectionLabel = element<HTMLSpanElement>("connection-label");
const statusMessage = element<HTMLSpanElement>("status-message");
const protocolNotice = element<HTMLSpanElement>("protocol-notice");
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
const diffDialog = element<HTMLDialogElement>("diff-dialog");
const diffBody = element<HTMLDivElement>("diff-body");
const diffTitle = element<HTMLHeadingElement>("diff-title");
const diffClose = element<HTMLButtonElement>("diff-close");

let workspaces = new Map<string, Workspace>();
let terminals = new Map<number, TerminalRecord>();
let selectedKey: string | null = null;
// The grouped inbox view-model computed by the shared `tui-core` logic
// in `src-tauri` and pushed over the event channel (#732). The frontend
// never computes grouping or sort — it only renders this structure.
let inboxView: ComputeOutcome | null = null;
let sortMode: SortMode = "ByRoleSplit";
let mailbox: Mailbox = "Inbox";
// The filter menu (#733) — every predicate with its live count and
// active flag — carried on the pushed `DesktopInboxView` alongside the
// outcome. The frontend renders it; it never derives the predicate list.
let inboxFilterMenu: FilterMenuItem[] = [];
// Repos the user has visually collapsed in this session. Display-only:
// it hides already-grouped rows, it does not regroup anything.
const collapsedRepos = new Set<string>();
let defaultAgent = "claude";
let previewMode = false;
let inboxLoading = true;
let inboxError: string | null = null;
let filterMenuOpen = false;
let actionsMenuOpen = false;
// The workspace whose diff the user is currently waiting on (#843). The
// `WorkspaceDiffInspected` reply is only shown while it still matches, so
// a diff for a since-reselected workspace is dropped.
let pendingDiffKey: string | null = null;
let searchTimer: number | undefined;
// Optimistic active-filter set: updated synchronously on each toggle so
// rapid clicks compose instead of racing the server round-trip. The
// pushed `Inbox` view reconciles it (see `handleStreamMessage`).
let activeFilterSet = new Set<Filter>();
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
/**
 * Unsaved notes edits, keyed by workspace. Distinct from `ReplyDrafts`
 * because an empty string is a valid saved state for notes, so absence
 * (fall back to `Workspace.notes`) must be distinguishable from a
 * deliberately-cleared draft. Only *dirty* edits are stored (see
 * `saveNotesDraft`), so an untouched workspace always reflects the latest
 * broadcast `Workspace.notes` rather than a pinned stale copy.
 */
const notesDrafts = new Map<string, string>();
/**
 * In-flight automation-policy mutations, keyed by
 * `policyKey(workspaceKey, field)` → the serialized *requested* value.
 * A control with a pending entry stays optimistic across renders (an
 * unrelated `WorkspaceUpserted` echo can't revert it); `renderAutomation`
 * clears the entry once the broadcast state matches the request.
 */
const pendingPolicies = new Map<string, string>();
const pendingLaunches = new Set<string>();
let focusRequestedSession: string | null = null;
// Every terminal of the selected workspace is mounted concurrently as a
// tile (mirrors the TUI's `TerminalStack`), keyed by terminal id. Switching
// workspace swaps the whole set; within a workspace an agent + a shell (or
// several agents) stay live side by side without teardown.
const liveTerminals = new Map<number, ActiveTerminal>();
// The tile that owns keyboard focus and is the target for snippets. In
// focus mode it is the only tile shown.
let focusedTerminalId: number | null = null;
// Focus mode expands the focused tile across the workspace, hiding the
// inbox and activity panels (the `.` chord; TUI focus mode parity).
let focusMode = false;
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
const inboxConnection = new InboxConnection(
  () => invoke<WorkspacesResponse>("list_workspaces"),
  async () => {
    eventChannel = new Channel<DesktopStreamMessage>();
    eventChannel.onmessage = handleStreamMessage;
    await invoke("subscribe_events", { onEvent: eventChannel });
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

actionsButton.addEventListener("click", () => {
  // No stopPropagation: the document click handler still runs and closes
  // any other open menu (e.g. the filter menu), while its actions-menu
  // guard excludes clicks on this button so it can't self-close here.
  toggleActionsMenu();
});
renameCancel.addEventListener("click", () => renameDialog.close());
renameForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void submitRename();
});

replyForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void reviewReply();
});

for (const [index, preset] of SNOOZE_PRESETS.entries()) {
  snoozeSelect.append(new Option(preset.label, String(index)));
}

autoMergeToggle.addEventListener("change", () => {
  void setAutoMerge(autoMergeToggle.checked);
});
trackMainToggle.addEventListener("change", () => {
  void setTrackMain(trackMainToggle.checked);
});
autoFixCiSelect.addEventListener("change", () => void applyAutoFixPolicies());
autoFixConflictSelect.addEventListener(
  "change",
  () => void applyAutoFixPolicies(),
);
snoozeButton.addEventListener("click", () => void snoozeSelected());
unsnoozeButton.addEventListener("click", () => void unsnoozeSelected());
syncButton.addEventListener("click", () => void syncSelected());
notesForm.addEventListener("submit", (event) => {
  event.preventDefault();
  void saveNotes();
});

sortButton.addEventListener("click", () => void cycleSortMode());
mailboxButton.addEventListener("click", () => void cycleMailbox());
filterButton.addEventListener("click", toggleFilterMenu);
filterClear.addEventListener("click", () => {
  activeFilterSet.clear();
  renderFilterControls();
  void applyFilters([]);
});
inboxSearch.addEventListener("input", () => {
  if (searchTimer !== undefined) {
    window.clearTimeout(searchTimer);
  }
  searchTimer = window.setTimeout(() => {
    void applySearch(inboxSearch.value.trim());
  }, 120);
});
document.addEventListener("click", (event) => {
  const target = event.target;
  if (
    actionsMenuOpen &&
    !(
      target instanceof Node &&
      (actionsMenu.contains(target) || actionsButton.contains(target))
    )
  ) {
    closeActionsMenu();
  }
  if (!filterMenuOpen) {
    return;
  }
  if (
    target instanceof Node &&
    (filterMenu.contains(target) || filterButton.contains(target))
  ) {
    return;
  }
  closeFilterMenu();
});
settingsButton.addEventListener("click", () => void openSettings());
snippetButton.addEventListener("click", () => void openSnippetPicker());
snippetFilter.addEventListener("input", () => void onSnippetFilterInput());
snippetDialog.addEventListener("keydown", handleSnippetKey);
snippetDialog.addEventListener("close", onSnippetDialogClose);
diffClose.addEventListener("click", () => diffDialog.close());
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

window.addEventListener("resize", () => {
  reclampSidebarWidth();
  scheduleResize();
});
window.addEventListener("keydown", handleKeyboard);
initColumnSplitter();
initActivitySplitter();

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
    sortMode = preview.sortMode;
    inboxFilterMenu = preview.filterMenu;
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
    setProtocolNotice(info.protocol_notice);
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
  if (message.type === "Inbox") {
    inboxView = message.payload.outcome;
    sortMode = message.payload.sort_mode;
    mailbox = message.payload.mailbox;
    inboxFilterMenu = message.payload.filter_menu;
    inboxLoading = false;
    // The server is authoritative: reconcile the optimistic filter set to
    // the computed view (#733). Each `set_filters` sends the full local
    // set, so the final push after a burst of toggles carries the
    // complete result — convergence is guaranteed, flicker at worst.
    activeFilterSet = new Set(
      activeFilters(message.payload.filter_menu).map((item) => item.filter),
    );
    // "Had a *live* selection" — a `selectedKey` that still points at a
    // present workspace. A stale key (its workspace was just removed)
    // must count as no selection so the terminal for the workspace
    // `chooseInitialWorkspace` auto-picks in its place gets attached.
    const hadSelection = hasLiveSelection();
    chooseInitialWorkspace();
    render();
    if (!hadSelection && selectedKey !== null) {
      syncWorkspaceTerminals();
    }
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
      // Drop the removed workspace's tiles now; the following Inbox push
      // (or a later selection) mounts the replacement.
      syncWorkspaceTerminals();
    }
  }

  if ("Snapshot" in event) {
    unmountAllTerminals();
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
    syncWorkspaceTerminals();
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
      const focusNew = focusRequestedSession === payload.session_key;
      syncWorkspaceTerminals();
      if (focusNew) {
        focusTerminal(payload.terminal_id);
        focusRequestedSession = null;
      }
    }
    applyPendingTerminalFrames(payload.terminal_id);
  } else if ("TerminalExited" in event) {
    const record = terminals.get(event.TerminalExited.terminal_id);
    if (record !== undefined) {
      record.state = `exited ${event.TerminalExited.exit_code ?? ""}`.trim();
    }
    const live = liveTerminals.get(event.TerminalExited.terminal_id);
    if (live !== undefined && record !== undefined) {
      setLiveState(live, record.state);
      if (event.TerminalExited.last_output !== null) {
        live.terminal.write(`\r\n${event.TerminalExited.last_output}`);
      }
    }
  } else if ("TerminalFocusRequested" in event) {
    focusTerminalById(event.TerminalFocusRequested.terminal_id);
  } else if ("AgentState" in event) {
    const record = terminals.get(event.AgentState.terminal_id);
    if (record !== undefined) {
      record.state = formatAgentState(event.AgentState.state);
    }
    const live = liveTerminals.get(event.AgentState.terminal_id);
    if (live !== undefined) {
      setLiveState(live, record?.state ?? "running");
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
  } else if ("WorkspaceActionOutcome" in event) {
    setStatus(event.WorkspaceActionOutcome.message);
  } else if ("WorkspaceDiffInspected" in event) {
    const payload = event.WorkspaceDiffInspected;
    if (pendingDiffKey === payload.workspace_key) {
      pendingDiffKey = null;
      if (payload.diff !== null) {
        showWorkspaceDiff(payload.workspace_key, payload.diff);
      } else {
        setStatus(`Couldn't read diff: ${payload.error ?? "unknown error"}`);
      }
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

function currentFilterMenu(): FilterMenuItem[] {
  return inboxFilterMenu;
}

/**
 * The shared menu with each row's `active` flag taken from the local
 * optimistic set, so a just-clicked toggle shows immediately (and
 * composes) rather than waiting for the server round-trip.
 */
function localizedFilterMenu(): FilterMenuItem[] {
  return currentFilterMenu().map((item) => ({
    ...item,
    active: activeFilterSet.has(item.filter),
  }));
}

async function applyFilters(filters: Filter[]): Promise<void> {
  if (previewMode) {
    return;
  }
  try {
    await invoke("set_filters", { filters });
  } catch (error) {
    setStatus(String(error));
  }
}

/** Toggle one predicate in the active set and re-request the view. */
function toggleFilter(filter: Filter): void {
  // Mutate the local set (not the last-pushed view) so back-to-back
  // toggles compose instead of each reading a stale `active` flag.
  if (!activeFilterSet.delete(filter)) {
    activeFilterSet.add(filter);
  }
  renderFilterControls();
  void applyFilters([...activeFilterSet]);
}

async function applySearch(query: string): Promise<void> {
  if (previewMode) {
    return;
  }
  try {
    await invoke("set_search", { query });
  } catch (error) {
    setStatus(String(error));
  }
}

function openFilterMenu(): void {
  filterMenuOpen = true;
  filterButton.setAttribute("aria-expanded", "true");
  filterMenu.classList.remove("hidden");
  renderFilterMenuBody();
  filterMenu.querySelector<HTMLInputElement>("input")?.focus();
}

function closeFilterMenu(): void {
  filterMenuOpen = false;
  filterButton.setAttribute("aria-expanded", "false");
  // Rescue focus back to the trigger before hiding, so an Escape/`f`
  // close from inside the menu doesn't strand it on a display:none node.
  // An outside click has already moved focus, so leave that case alone.
  if (
    document.activeElement instanceof Node &&
    filterMenu.contains(document.activeElement)
  ) {
    filterButton.focus();
  }
  filterMenu.classList.add("hidden");
}

function toggleFilterMenu(): void {
  if (filterMenuOpen) {
    closeFilterMenu();
  } else {
    openFilterMenu();
  }
}

function renderFilterControls(): void {
  const menu = localizedFilterMenu();
  const active = activeFilters(menu);
  filterButton.textContent =
    active.length === 0 ? "Filter" : `Filter (${active.length})`;
  filterButton.classList.toggle("active", active.length > 0);
  renderFilterChips(active);
  if (filterMenuOpen) {
    renderFilterMenuBody();
  }
}

function renderFilterChips(active: FilterMenuItem[]): void {
  filterChips.replaceChildren();
  filterChips.classList.toggle("empty", active.length === 0);
  for (const item of active) {
    const chip = document.createElement("button");
    chip.type = "button";
    chip.className = "filter-chip";
    chip.setAttribute("role", "listitem");
    chip.setAttribute("aria-label", `Remove ${item.label} filter`);
    chip.title = `Remove ${item.label} filter`;
    const label = document.createElement("span");
    label.textContent = item.label;
    const remove = document.createElement("span");
    remove.className = "filter-chip-remove";
    remove.setAttribute("aria-hidden", "true");
    remove.textContent = "×";
    chip.append(label, remove);
    chip.addEventListener("click", () => toggleFilter(item.filter));
    filterChips.append(chip);
  }
}

function renderFilterMenuBody(): void {
  filterMenuBody.replaceChildren();
  for (const group of filterMenuGroups(localizedFilterMenu())) {
    const section = document.createElement("div");
    section.className = "filter-section";
    const heading = document.createElement("p");
    heading.className = "filter-section-heading";
    heading.textContent = group.axis;
    section.append(heading);
    for (const item of group.items) {
      const row = document.createElement("label");
      row.className = "filter-row";
      const checkbox = document.createElement("input");
      checkbox.type = "checkbox";
      checkbox.checked = item.active;
      checkbox.addEventListener("change", () => toggleFilter(item.filter));
      const label = document.createElement("span");
      label.className = "filter-row-label";
      label.textContent = item.label;
      const count = document.createElement("span");
      count.className = "filter-row-count";
      count.textContent = String(item.count);
      row.append(checkbox, label, count);
      section.append(row);
    }
    filterMenuBody.append(section);
  }
}

interface RowEntry {
  key: string;
  sig: string;
  build: () => HTMLElement;
}

/**
 * Reconcile `container`'s children against `entries` by key, reusing a
 * node whose stored signature is unchanged instead of rebuilding it.
 * Poll-driven renders then touch only the rows whose data actually
 * changed — the list no longer fully remounts on every event. Scroll
 * position always survives (the container itself is never replaced);
 * keyboard focus and text selection survive on any row left unchanged,
 * i.e. every row except the one whose data this render rebuilt (#877).
 */
function reconcileList(container: HTMLElement, entries: RowEntry[]): void {
  const existing = new Map<string, HTMLElement>();
  for (const child of Array.from(container.children)) {
    const key = (child as HTMLElement).dataset.rowKey;
    if (key !== undefined) {
      existing.set(key, child as HTMLElement);
    }
  }
  const desired: HTMLElement[] = [];
  const keep = new Set<HTMLElement>();
  for (const entry of entries) {
    const prev = existing.get(entry.key);
    let node: HTMLElement;
    if (prev !== undefined && prev.dataset.rowSig === entry.sig) {
      node = prev;
    } else {
      node = entry.build();
      node.dataset.rowKey = entry.key;
      node.dataset.rowSig = entry.sig;
    }
    keep.add(node);
    desired.push(node);
  }
  for (const child of Array.from(container.children)) {
    if (!keep.has(child as HTMLElement)) {
      child.remove();
    }
  }
  desired.forEach((node, index) => {
    if (container.children[index] !== node) {
      container.insertBefore(node, container.children[index] ?? null);
    }
  });
}

/**
 * Signature of everything `renderWorkspaceRow` draws, so a poll that
 * leaves a row's rendered output untouched reuses the existing node
 * verbatim. This is the reuse-correctness contract: every value the row
 * renders — and every mutable value its event handlers close over — must
 * appear here, or a stale node will be served after that value changes.
 */
function workspaceRowSig(workspace: Workspace): string {
  const task = primaryTask(workspace);
  const count = unreadCount(workspace);
  const updated = task === null ? null : task.updated_at;
  return JSON.stringify({
    selected: workspace.key === selectedKey,
    reference: taskReference(task),
    role: task?.role ?? null,
    updated,
    // The rendered relative-time string, not just its raw stamp, so a
    // render still refreshes "2m ago" → "3m ago" on a row whose data is
    // otherwise unchanged as wall-clock time advances (#877).
    age: updated === null ? null : relativeTime(updated),
    title: task?.title ?? workspace.name,
    subtitle: task?.repo ?? workspace.branch,
    signals: rowSignals(task, count),
    state: task?.state ?? null,
  });
}

function renderInbox(): void {
  workspaceList.setAttribute("aria-busy", String(inboxLoading));
  sortLabel.textContent = sortModeLabel(sortMode);
  mailboxLabelElement.textContent = mailboxLabel(mailbox);

  const allItems = [...workspaces.values()];
  // Scope the header total to the workspaces the view-model actually
  // placed — never the raw map, which also holds workspaces filtered out
  // of this mailbox (e.g. inactive) and would inflate the count past
  // what the list shows.
  const unread =
    inboxView === null ? 0 : visibleUnreadCount(inboxView, workspaces);
  unreadTotal.textContent = `${unread} unread`;
  renderFilterControls();

  if (inboxLoading) {
    workspaceCount.textContent = "Loading…";
    renderInboxMessage("Loading persisted workspaces…");
    return;
  }
  if (inboxError !== null) {
    workspaceCount.textContent = "";
    renderInboxMessage(inboxError, true);
    return;
  }

  const rows = inboxView?.visible ?? [];
  const workspaceRowCount = rows.filter(
    (row) => typeof row === "object" && "Workspace" in row,
  ).length;
  workspaceCount.textContent = `${workspaceRowCount} workspace${
    workspaceRowCount === 1 ? "" : "s"
  }`;

  if (workspaceRowCount === 0) {
    renderInboxMessage(
      allItems.length === 0
        ? "Your inbox is empty. Refresh after setup to fetch GitHub work."
        : "No workspaces to show.",
    );
    return;
  }

  // The Rust view-model already ordered and grouped everything (#732):
  // walk it top-to-bottom, drawing repo headers, PR/Issue/Other section
  // headers, and workspace rows. Collapse only hides already-grouped
  // rows; it never reorders.
  const entries: RowEntry[] = [];
  let collapsed = false;
  let group = "";
  for (const row of rows) {
    if (row === "FocusedHeader") {
      // The synthetic `★ Focused` section leads the list, above every
      // repo group, and never collapses (it holds starred workspaces
      // lifted out of their repos).
      collapsed = false;
      group = "focused";
      entries.push({
        key: "focused-header",
        sig: "focused",
        build: renderFocusedHeader,
      });
    } else if ("RepoHeader" in row) {
      const repo = row.RepoHeader;
      collapsed = collapsedRepos.has(repo);
      group = `repo:${repo}`;
      const summary = inboxView?.summaries[repo];
      entries.push({
        key: group,
        sig: JSON.stringify({
          collapsed,
          active: summary?.active ?? 0,
          attention: summary?.attention ?? 0,
        }),
        build: () => renderRepoHeader(repo),
      });
    } else if ("KindHeader" in row) {
      if (!collapsed) {
        const kind = row.KindHeader;
        entries.push({
          key: `kind:${group}:${kind}`,
          sig: kind,
          build: () => renderKindHeader(kind),
        });
      }
    } else if ("Workspace" in row) {
      if (collapsed) {
        continue;
      }
      const workspace = workspaces.get(row.Workspace);
      // A workspace row can briefly precede its map entry (the view is
      // recomputed a frame ahead of the WorkspaceUpserted echo). Skip
      // it; the next view refresh fills it in.
      if (workspace !== undefined) {
        entries.push({
          key: `ws:${workspace.key}`,
          sig: workspaceRowSig(workspace),
          build: () => renderWorkspaceRow(workspace),
        });
      }
    }
    // `Session` sub-rows are represented by their workspace row for now.
  }
  reconcileList(workspaceList, entries);
}

function renderRepoHeader(repo: string): HTMLDivElement {
  const header = document.createElement("div");
  header.className = "repo-header";
  header.setAttribute("role", "presentation");
  const collapsed = collapsedRepos.has(repo);
  header.classList.toggle("collapsed", collapsed);
  const twisty = document.createElement("span");
  twisty.className = "repo-twisty";
  twisty.setAttribute("aria-hidden", "true");
  twisty.textContent = collapsed ? "▸" : "▾";
  const label = document.createElement("span");
  label.className = "repo-label";
  label.textContent = repo;
  const summary = inboxView?.summaries[repo];
  const meta = document.createElement("span");
  meta.className = "repo-meta";
  const active = summary?.active ?? 0;
  const attention = summary?.attention ?? 0;
  meta.textContent = attention > 0 ? `${active} · ${attention} ⚑` : `${active}`;
  header.append(twisty, label, meta);
  header.addEventListener("click", () => {
    if (collapsedRepos.has(repo)) {
      collapsedRepos.delete(repo);
    } else {
      collapsedRepos.add(repo);
    }
    renderInbox();
  });
  return header;
}

function renderFocusedHeader(): HTMLDivElement {
  const header = document.createElement("div");
  header.className = "repo-header focused-header";
  header.setAttribute("role", "presentation");
  const label = document.createElement("span");
  label.className = "repo-label";
  label.textContent = "★ Focused";
  header.append(label);
  return header;
}

function renderKindHeader(kind: "Pr" | "Issue" | "Other"): HTMLDivElement {
  const header = document.createElement("div");
  header.className = `kind-header kind-${kind.toLowerCase()}`;
  header.setAttribute("role", "presentation");
  header.textContent = kindHeaderLabel(kind);
  return header;
}

// Any value this row renders, or that its click handler closes over,
// must be reflected in `workspaceRowSig` — the reconciler reuses an
// unchanged node by signature and would otherwise serve a stale row.
function renderWorkspaceRow(workspace: Workspace): HTMLButtonElement {
  const task = primaryTask(workspace);
  const button = document.createElement("button");
  button.className = "workspace-row";
  button.dataset.key = workspace.key;
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
  top.append(reference);
  if (task?.role !== undefined) {
    const role = document.createElement("span");
    role.className = "workspace-role";
    role.textContent = task.role.toLowerCase();
    top.append(role);
  }
  const updatedAt = task?.updated_at;
  if (updatedAt !== undefined && updatedAt !== null) {
    const time = document.createElement("time");
    time.className = "workspace-time";
    time.dateTime = updatedAt;
    time.textContent = relativeTime(updatedAt);
    top.append(time);
  }

  const title = document.createElement("strong");
  title.className = "workspace-row-title";
  title.textContent = task?.title ?? workspace.name;

  const bottom = document.createElement("span");
  bottom.className = "workspace-row-bottom";
  renderTaskBadges(bottom, rowSignals(task, count));
  if (bottom.childElementCount === 0) {
    const state = document.createElement("span");
    state.className = `task-state task-state-${(task?.state ?? "local").toLowerCase()}`;
    state.textContent = task?.state ?? "local";
    bottom.append(state);
  }

  button.append(top, title, bottom);
  return button;
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
    renderTaskBadges(taskSignals, detailSignals(task));
  }
  taskDescription.textContent =
    task?.body?.trim() || "No description was provided for this workspace.";
  markReadButton.disabled = unreadCount(workspace) === 0;
  replyBody.disabled = !canReplyToTask(task);
  replyButton.disabled =
    replySubmitting.has(workspace.key) || !canReplyToTask(task);
  replyForm.classList.toggle("hidden", !canReplyToTask(task));
  renderAutomation(workspace, task);

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

  activityCount.textContent = String(workspace.activity.length);
  if (workspace.activity.length === 0) {
    const empty = document.createElement("p");
    empty.className = "activity-empty";
    empty.textContent = "No activity yet.";
    activityList.replaceChildren(empty);
    return;
  }

  const entries = workspace.activity.slice(0, 30).map(
    (activity): RowEntry => ({
      // Key on the GitHub node id when present; otherwise on content, so
      // a prepended activity doesn't shift index-based keys and remount
      // every card below it. The `id:` / `c:` prefixes keep a node id
      // that happens to be numeric from colliding with a content key.
      key:
        activity.node_id !== null
          ? `act:id:${activity.node_id}`
          : `act:c:${activity.created_at} ${activity.author} ${activity.kind} ${activity.body}`,
      sig: JSON.stringify({
        author: activity.author,
        body: activity.body,
        created_at: activity.created_at,
        kind: activity.kind,
        age: relativeTime(activity.created_at),
      }),
      build: () => renderActivityCard(activity),
    }),
  );
  reconcileList(activityList, entries);
}

function renderActivityCard(activity: Activity): HTMLElement {
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
  return card;
}

function policyKey(workspaceKey: string, field: string): string {
  return `${workspaceKey}\0${field}`;
}

/**
 * Set a checkbox from committed state, but keep the user's optimistic
 * value while a mutation is in flight. Clears the pending marker once the
 * broadcast state confirms the request, so an interleaving upsert can
 * never revert the toggle mid-flight.
 */
function renderPolicyToggle(
  toggle: HTMLInputElement,
  workspaceKey: string,
  field: string,
  committed: boolean,
): void {
  const key = policyKey(workspaceKey, field);
  const requested = pendingPolicies.get(key);
  if (requested === undefined) {
    toggle.checked = committed;
    return;
  }
  if (requested === String(committed)) {
    pendingPolicies.delete(key);
    toggle.checked = committed;
  } else {
    toggle.checked = requested === "true";
  }
}

/**
 * Reflect the workspace's persisted automation state onto the detail
 * pane's controls. Reads only the fields the daemon already broadcasts
 * on `Workspace` — the controls send commands back through the handlers
 * below. Notes are handled separately (draft-aware) in
 * `changeSelectedWorkspace`, so this never touches `notesBody`.
 */
function renderAutomation(workspace: Workspace, task: Task | null): void {
  const hasPr = workspace.pr !== null;
  renderPolicyToggle(
    autoMergeToggle,
    workspace.key,
    "auto_merge",
    workspace.auto_merge_on_green,
  );
  autoMergeToggle.disabled = !hasPr;
  renderPolicyToggle(
    trackMainToggle,
    workspace.key,
    "track_main",
    workspace.track_main,
  );
  trackMainToggle.disabled = !supportsTrackMain(workspace);
  renderAutoFixPolicies(workspace);
  // Auto-fix targets a PR's CI failures and merge conflicts.
  autoFixCiSelect.disabled = !hasPr;
  autoFixConflictSelect.disabled = !hasPr;

  const snoozed = workspace.snoozed_until !== null;
  unsnoozeButton.classList.toggle("hidden", !snoozed);
  snoozeStatus.textContent = snoozed
    ? `Snoozed until ${formatSnoozeUntil(workspace.snoozed_until as string)}`
    : "";

  syncButton.disabled = task === null;
}

/**
 * Render the atomic auto-fix pair, keeping both selects on the requested
 * values while a `SetAutoFixPolicies` is in flight so an interleaving
 * upsert can't revert one arm — which would otherwise let the next edit
 * to the other arm clobber the in-flight change.
 */
function renderAutoFixPolicies(workspace: Workspace): void {
  const key = policyKey(workspace.key, "auto_fix");
  const requested = pendingPolicies.get(key);
  const committed = `${workspace.policies.auto_fix_ci}\0${workspace.policies.auto_fix_conflict}`;
  if (requested === undefined || requested === committed) {
    if (requested === committed) {
      pendingPolicies.delete(key);
    }
    autoFixCiSelect.value = workspace.policies.auto_fix_ci;
    autoFixConflictSelect.value = workspace.policies.auto_fix_conflict;
    return;
  }
  const [ci, conflict] = requested.split("\0");
  autoFixCiSelect.value = ci as PolicyArm;
  autoFixConflictSelect.value = conflict as PolicyArm;
}

function formatSnoozeUntil(iso: string): string {
  const when = new Date(iso);
  return Number.isNaN(when.getTime())
    ? iso
    : when.toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
      });
}

function renderInboxMessage(message: string, error = false): void {
  const empty = document.createElement("p");
  empty.className = `inbox-empty${error ? " error" : ""}`;
  empty.textContent = message;
  if (error) {
    empty.role = "alert";
  }
  workspaceList.replaceChildren(empty);
}

/**
 * Render a set of badge pills into `container`. Shared by the detail
 * pane and the inbox list rows so CI / review / reply / unread badges
 * look and read the same everywhere. The badge *set* is derived by the
 * pure helpers in `model.ts`; this only turns them into DOM.
 */
function renderTaskBadges(container: HTMLElement, signals: TaskSignal[]): void {
  for (const signal of signals) {
    const pill = document.createElement("span");
    pill.className = `signal-pill${
      signal.tone === undefined ? "" : ` ${signal.tone}`
    }`;
    pill.textContent = signal.label;
    container.append(pill);
  }
}

function selectWorkspace(key: string): void {
  const changed = changeSelectedWorkspace(key);
  render();
  syncWorkspaceTerminals();
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
    saveNotesDraft(selectedKey, notesBody.value);
  }
  closeActionsMenu();
  selectedKey = key;
  replyBody.value = key === null ? "" : replyDrafts.get(key);
  notesBody.value = key === null ? "" : notesValueFor(key);
  return true;
}

/**
 * Retain a notes draft only when it diverges from the saved value.
 * Storing an untouched value would pin it and shadow later broadcast
 * updates to `Workspace.notes` (e.g. an edit made from the TUI).
 */
function saveNotesDraft(key: string, value: string): void {
  if (value === savedNotes(key)) {
    notesDrafts.delete(key);
  } else {
    notesDrafts.set(key, value);
  }
}

function savedNotes(key: string): string {
  return workspaces.get(key)?.notes ?? "";
}

/** The notes text to show: an unsaved draft if present, else the saved value. */
function notesValueFor(key: string): string {
  return notesDrafts.get(key) ?? savedNotes(key);
}

/** True when the current selection still points at a present workspace. */
function hasLiveSelection(): boolean {
  return selectedKey !== null && workspaces.has(selectedKey);
}

function chooseInitialWorkspace(): void {
  if (hasLiveSelection()) {
    return;
  }
  const ordered = inboxView === null ? [] : orderedWorkspaceKeys(inboxView);
  changeSelectedWorkspace(
    ordered.find((key) => workspaces.has(key)) ?? null,
  );
}

/** Workspace keys of the rows currently rendered (skips collapsed
 * repos), in the shared view-model's order. Drives keyboard nav. */
function navigableWorkspaceKeys(): string[] {
  return [...workspaceList.querySelectorAll<HTMLButtonElement>(".workspace-row")]
    .map((row) => row.dataset.key)
    .filter((key): key is string => key !== undefined);
}

async function cycleSortMode(): Promise<void> {
  if (previewMode) {
    sortMode = nextSortMode(sortMode);
    sortLabel.textContent = sortModeLabel(sortMode);
    return;
  }
  // The daemon-side model owns the sort; it recomputes and pushes a new
  // Inbox view, which updates the label and re-renders.
  try {
    await invoke("set_sort_mode");
  } catch (error) {
    setStatus(String(error));
  }
}

function nextSortMode(mode: SortMode): SortMode {
  return mode === "Recent"
    ? "ByRole"
    : mode === "ByRole"
      ? "ByRoleSplit"
      : "Recent";
}

async function cycleMailbox(): Promise<void> {
  if (previewMode) {
    mailbox = nextMailbox(mailbox);
    mailboxLabelElement.textContent = mailboxLabel(mailbox);
    return;
  }
  // The daemon-side model owns the mailbox; it recomputes and pushes a
  // new Inbox view, which updates the label and re-renders (#816).
  try {
    await invoke("set_mailbox");
  } catch (error) {
    setStatus(String(error));
  }
}

/** The selected workspace's terminals, in stable id order. */
function workspaceTerminalRecords(): TerminalRecord[] {
  if (selectedKey === null) {
    return [];
  }
  return [...terminals.values()]
    .filter((record) => record.sessionKey === selectedKey)
    .sort((left, right) => left.id - right.id);
}

/**
 * Reconcile the mounted tiles with the selected workspace's terminals:
 * unmount anything that left (workspace switch, exit + removal), mount
 * anything new, and keep the focus on a still-present tile. Every
 * terminal of the workspace stays live at once — no teardown on switch.
 */
function syncWorkspaceTerminals(): void {
  const wanted = workspaceTerminalRecords();
  const wantedIds = new Set(wanted.map((record) => record.id));
  for (const id of [...liveTerminals.keys()]) {
    if (!wantedIds.has(id)) {
      unmountTerminal(id);
    }
  }
  for (const record of wanted) {
    if (!liveTerminals.has(record.id)) {
      mountTerminal(record);
    }
  }
  if (focusedTerminalId === null || !liveTerminals.has(focusedTerminalId)) {
    focusedTerminalId = defaultFocusTerminalId() ?? wanted[0]?.id ?? null;
  }
  layoutTiles();
}

/**
 * The terminal to focus by default: the same preference the pane used
 * before tiling — a non-exited agent, else a non-exited shell, else the
 * newest of its kind — so a reconnect never lands focus on a stale
 * exited terminal when a live one exists.
 */
function defaultFocusTerminalId(): number | undefined {
  if (selectedKey === null) {
    return undefined;
  }
  const preferred =
    terminalForWorkspace(selectedKey, "agent") ??
    terminalForWorkspace(selectedKey, "shell");
  return preferred?.id;
}

function mountTerminal(record: TerminalRecord): void {
  const id = record.id;
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
  // Intercept the chord shortcuts before xterm forwards them to the PTY,
  // so ⌘/Ctrl-J opens the picker and ⌘/Ctrl-. toggles focus mode instead
  // of reaching the agent.
  terminal.attachCustomKeyEventHandler((event) => {
    if (event.type === "keydown" && isSnippetShortcut(event)) {
      void openSnippetPicker();
      return false;
    }
    if (event.type === "keydown" && isFocusModeShortcut(event)) {
      toggleFocusMode();
      return false;
    }
    return true;
  });

  const host = document.createElement("div");
  host.className = "terminal-tile-host";
  const tile = document.createElement("div");
  tile.className = "terminal-tile";
  tile.dataset.terminalId = String(id);
  tile.append(host);
  tile.addEventListener("mousedown", () => focusTerminal(id));
  terminalTiles.append(tile);

  const tabLabel = document.createElement("span");
  tabLabel.className = "terminal-tab-label";
  tabLabel.textContent = terminalKindLabel(record.kind);
  const stateEl = document.createElement("span");
  stateEl.className = "terminal-tab-state";
  const close = document.createElement("button");
  close.type = "button";
  close.className = "terminal-tab-close";
  close.textContent = "×";
  close.setAttribute("aria-label", "Close terminal");
  close.addEventListener("click", (event) => {
    event.stopPropagation();
    void sendTerminalFrame(closeTerminalFrame(id));
  });
  // A `role="tab"` div, not a <button>, so the close <button> can nest
  // inside it without producing invalid button-in-button markup.
  const tab = document.createElement("div");
  tab.className = "terminal-tab";
  tab.dataset.terminalId = String(id);
  tab.setAttribute("role", "tab");
  tab.append(tabLabel, stateEl, close);
  tab.addEventListener("click", () => focusTerminal(id));
  terminalTabs.append(tab);

  terminal.open(host);
  // Sizing is left to `layoutTiles` → `scheduleResize`, which fits only
  // visible tiles: fitting here would measure a still-hidden container
  // (empty-state class, or a focus-mode `display:none` tile) as zero.

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
  const live: ActiveTerminal = {
    id,
    terminal,
    fit,
    tile,
    tab,
    stateEl,
    disposeInput: () => inputDisposable.dispose(),
    disposeResize: () => resizeDisposable.dispose(),
    resyncing: false,
  };
  liveTerminals.set(id, live);
  setLiveState(live, record.state);
  if (record.dirty || !record.replayAvailable) {
    requestTerminalResync(record);
  }
}

function unmountTerminal(id: number): void {
  const live = liveTerminals.get(id);
  if (live === undefined) {
    return;
  }
  const record = terminals.get(id);
  if (record !== undefined) {
    discardTerminalView(record);
  }
  live.disposeInput();
  live.disposeResize();
  live.terminal.dispose();
  live.tile.remove();
  live.tab.remove();
  liveTerminals.delete(id);
}

function unmountAllTerminals(): void {
  for (const id of [...liveTerminals.keys()]) {
    unmountTerminal(id);
  }
  focusedTerminalId = null;
}

/** Reflect a runner's display state on its tab (and the panel header if
 * it is the focused terminal). Transient states like "resyncing" live
 * here only until the next replay resets them to the record state. */
function setLiveState(live: ActiveTerminal, state: string): void {
  live.stateEl.textContent = state;
  live.stateEl.dataset.state = state;
  if (focusedTerminalId === live.id) {
    setTerminalState(state);
  }
}

/** Move keyboard focus to a mounted tile and, in focus mode, make it the
 * single visible one. Re-lays-out only when the focused tile actually
 * changes — a repeated focus (e.g. a mousedown inside the already-focused
 * tile to select text) must not reflow the DOM and drop the selection. */
function focusTerminal(id: number): void {
  const live = liveTerminals.get(id);
  if (live === undefined) {
    return;
  }
  if (focusedTerminalId !== id) {
    focusedTerminalId = id;
    layoutTiles();
  }
  live.terminal.focus();
}

/** Focus a terminal that may belong to another workspace (daemon-driven
 * `TerminalFocusRequested`): switch to its workspace the same way a click
 * would — through `selectWorkspace`, so the sidebar, draft, and daemon
 * `FocusWorkspace` all stay consistent — then focus the tile. */
function focusTerminalById(id: number): void {
  const record = terminals.get(id);
  if (record === undefined) {
    return;
  }
  if (record.sessionKey !== selectedKey) {
    selectWorkspace(record.sessionKey);
  }
  focusTerminal(id);
}

function toggleFocusMode(): void {
  focusMode = !focusMode;
  layoutTiles();
  if (focusMode && focusedTerminalId !== null) {
    liveTerminals.get(focusedTerminalId)?.terminal.focus();
  }
}

/**
 * Apply the current tile set to the DOM: order tabs/tiles by id, mark the
 * focused one, toggle focus-mode (single tile, panels hidden), update the
 * panel header, and refit. Pure projection of `liveTerminals` +
 * `focusedTerminalId` + `focusMode` — safe to call after any change.
 */
function layoutTiles(): void {
  const wanted = workspaceTerminalRecords();
  const hasTerminals = wanted.length > 0;
  // Reconcile tab/tile order to id order, moving only nodes that are out
  // of place. Re-appending an in-place node detaches and re-inserts it,
  // which would collapse an in-progress text selection in that terminal.
  wanted.forEach((record, index) => {
    const live = liveTerminals.get(record.id);
    if (live === undefined) {
      return;
    }
    if (terminalTabs.children[index] !== live.tab) {
      terminalTabs.insertBefore(live.tab, terminalTabs.children[index] ?? null);
    }
    if (terminalTiles.children[index] !== live.tile) {
      terminalTiles.insertBefore(live.tile, terminalTiles.children[index] ?? null);
    }
  });
  terminalEmpty.classList.toggle("hidden", hasTerminals);
  terminalTabs.classList.toggle("hidden", !hasTerminals);
  terminalTiles.classList.toggle("hidden", !hasTerminals);
  const focusActive = focusMode && hasTerminals;
  workspaceGrid.classList.toggle("focus-mode", focusActive);
  terminalTiles.classList.toggle("focus-only", focusActive);
  for (const live of liveTerminals.values()) {
    const focused = live.id === focusedTerminalId;
    live.tile.classList.toggle("focused", focused);
    live.tab.classList.toggle("active", focused);
    live.tab.setAttribute("aria-selected", String(focused));
  }
  const focusedRecord =
    focusedTerminalId === null ? undefined : terminals.get(focusedTerminalId);
  if (focusedRecord !== undefined) {
    terminalTitle.textContent = `${terminalKindLabel(focusedRecord.kind)} · ${focusedRecord.sessionKey}`;
    const live = liveTerminals.get(focusedRecord.id);
    setTerminalState(live?.stateEl.textContent ?? focusedRecord.state);
  } else {
    terminalTitle.textContent = "No terminal attached";
    setTerminalState("idle");
  }
  snippetButton.disabled = !hasTerminals;
  if (!hasTerminals && snippetDialog.open) {
    closeSnippetPicker();
  }
  scheduleResize();
}

/** A tile is laid out (and worth fitting) unless focus mode hid it. */
function isTileVisible(live: ActiveTerminal): boolean {
  return !terminalTiles.classList.contains("focus-only") || live.id === focusedTerminalId;
}

function terminalForWorkspace(
  sessionKey: string,
  kind: "agent" | "shell",
  agentId?: string,
): TerminalRecord | undefined {
  return preferredTerminal(terminals.values(), sessionKey, kind, agentId);
}

function handleTerminalOutput(frame: TerminalBinaryFrame): void {
  const record = terminals.get(frame.terminalId);
  if (record === undefined) {
    queuePendingTerminalFrame(frame);
    return;
  }
  const live = liveTerminals.get(frame.terminalId);
  if (live === undefined) {
    record.dirty = true;
    return;
  }
  if (live.resyncing || frame.seq <= record.lastSeq) {
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
  live.terminal.write(frame.payload);
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
  const live = liveTerminals.get(frame.terminalId);
  if (live !== undefined) {
    live.terminal.reset();
    live.terminal.write(frame.payload);
    live.resyncing = false;
    setLiveState(live, record.state);
  }
}

function handleTerminalResyncUnavailable(terminalId: number): void {
  const live = liveTerminals.get(terminalId);
  if (live !== undefined) {
    live.resyncing = false;
    setLiveState(live, "waiting for replay");
  }
}

function requestTerminalResync(
  record: TerminalRecord,
  requiredSeq = requiredTerminalResyncSequence(
    record.lastSeq,
    record.replayAvailable,
  ),
): void {
  const live = liveTerminals.get(record.id);
  if (live !== undefined && !live.resyncing) {
    live.resyncing = true;
    setLiveState(live, "resyncing");
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
    for (const live of liveTerminals.values()) {
      if (!isTileVisible(live)) {
        continue;
      }
      live.fit.fit();
      void sendTerminalFrame(
        resizeTerminalFrame(live.id, live.terminal.cols, live.terminal.rows),
      );
    }
  }, 80);
}

/**
 * Clamp a desired sidebar width so the right pane always keeps at least
 * RIGHT_MIN_PX. When the grid has no measured width yet (some environments
 * report 0 before first layout) only the lower bound applies; the resize
 * handler re-clamps once the grid has a real width.
 */
function clampSidebarWidth(width: number): number {
  const gridWidth = workspaceGrid.getBoundingClientRect().width;
  const max =
    gridWidth > 0
      ? Math.max(gridWidth - RIGHT_MIN_PX, SIDEBAR_MIN_PX)
      : Infinity;
  return Math.round(Math.min(Math.max(width, SIDEBAR_MIN_PX), max));
}

/**
 * Re-apply the clamp to the live (dragged or restored) width. A window that
 * shrinks below the persisted sidebar width would otherwise starve the right
 * pane, since neither restore nor a bare `resize` runs the drag-time clamp.
 * Only the live CSS value is adjusted — the stored preference is left intact so
 * a later, larger window restores it.
 */
function reclampSidebarWidth(): void {
  const current = workspaceGrid.style.getPropertyValue("--sidebar-width");
  if (current.endsWith("px")) {
    workspaceGrid.style.setProperty(
      "--sidebar-width",
      `${clampSidebarWidth(parseInt(current, 10))}px`,
    );
  }
}

/**
 * Drag the divider between the inbox and the right pane to resize the sidebar
 * column. The width persists as `--sidebar-width` on `.workspace-grid` and in
 * localStorage so columns move only on user drag — never from content growth
 * or a terminal fit pass.
 */
function initColumnSplitter(): void {
  const stored = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY));
  if (Number.isFinite(stored) && stored >= SIDEBAR_MIN_PX) {
    workspaceGrid.style.setProperty(
      "--sidebar-width",
      `${clampSidebarWidth(stored)}px`,
    );
  }
  columnSplitter.addEventListener("mousedown", (event) => {
    event.preventDefault();
    columnSplitter.classList.add("dragging");
    const onMove = (move: MouseEvent) => {
      if (move.buttons === 0) {
        onUp();
        return;
      }
      const rect = workspaceGrid.getBoundingClientRect();
      const width = clampSidebarWidth(move.clientX - rect.left);
      workspaceGrid.style.setProperty("--sidebar-width", `${width}px`);
      scheduleResize();
    };
    const onUp = () => {
      columnSplitter.classList.remove("dragging");
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      const current = workspaceGrid.style.getPropertyValue("--sidebar-width");
      if (current.endsWith("px")) {
        localStorage.setItem(SIDEBAR_WIDTH_KEY, String(parseInt(current, 10)));
      }
      scheduleResize();
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  });
}

/**
 * Drag the divider between the activity pane and the terminal to resize the
 * split, mirroring the TUI RightPane. The chosen activity height persists as a
 * CSS pixel value on `.right-pane` and in localStorage across launches.
 */
function initActivitySplitter(): void {
  const stored = Number(localStorage.getItem(ACTIVITY_HEIGHT_KEY));
  if (Number.isFinite(stored) && stored >= ACTIVITY_MIN_PX) {
    rightPane.style.setProperty("--activity-height", `${stored}px`);
  }
  rightPaneSplitter.addEventListener("mousedown", (event) => {
    event.preventDefault();
    rightPaneSplitter.classList.add("dragging");
    const onMove = (move: MouseEvent) => {
      if (move.buttons === 0) {
        onUp();
        return;
      }
      const rect = rightPane.getBoundingClientRect();
      const max = Math.max(rect.height - TERMINAL_MIN_PX, ACTIVITY_MIN_PX);
      const height = Math.min(
        Math.max(move.clientY - rect.top, ACTIVITY_MIN_PX),
        max,
      );
      rightPane.style.setProperty("--activity-height", `${Math.round(height)}px`);
      scheduleResize();
    };
    const onUp = () => {
      rightPaneSplitter.classList.remove("dragging");
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      const current = rightPane.style.getPropertyValue("--activity-height");
      if (current.endsWith("px")) {
        localStorage.setItem(ACTIVITY_HEIGHT_KEY, String(parseInt(current, 10)));
      }
      scheduleResize();
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  });
}

function isFocusModeShortcut(event: KeyboardEvent): boolean {
  return (
    (event.metaKey || event.ctrlKey) &&
    !event.altKey &&
    !event.shiftKey &&
    event.key === "."
  );
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
  if (focusedTerminalId === null) {
    setStatus("Open an agent or shell to send a snippet.");
    return;
  }
  snippetTargetTerminal = focusedTerminalId;
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
  if (focusedTerminalId !== null) {
    liveTerminals.get(focusedTerminalId)?.terminal.focus();
  }
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
    focusTerminal(existing.id);
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
    focusTerminal(existing.id);
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

function toggleActionsMenu(): void {
  if (actionsMenuOpen) {
    closeActionsMenu();
  } else {
    openActionsMenu();
  }
}

function openActionsMenu(): void {
  if (selectedKey === null) {
    return;
  }
  renderActionsMenu();
  actionsMenu.classList.remove("hidden");
  actionsButton.setAttribute("aria-expanded", "true");
  actionsMenuOpen = true;
}

function closeActionsMenu(): void {
  actionsMenu.classList.add("hidden");
  actionsButton.setAttribute("aria-expanded", "false");
  actionsMenuOpen = false;
}

/** One act-on-work row: a label, whether it's destructive, and its run. */
interface ActionMenuItem {
  label: string;
  danger?: boolean;
  run: () => void;
}

/** Build the contextual act-on-work menu for the selected workspace. */
function renderActionsMenu(): void {
  actionsMenu.replaceChildren();
  if (selectedKey === null) {
    return;
  }
  const key = selectedKey;
  const workspace = workspaces.get(key);
  const task = workspace === undefined ? null : primaryTask(workspace);
  // Classify by the filled task slot, mirroring `WorkspaceKind::classify`
  // — `task.kind` can be null on older data, so keying off it would drop
  // Merge/Close entirely for a real PR/issue.
  const isPr = workspace?.pr != null;
  const isIssue =
    workspace !== undefined &&
    workspace.pr == null &&
    (workspace.gh_issues.length > 0 || workspace.linear_issues.length > 0);
  // Terminal PRs/issues (Merged / Closed) are no-ops for the mutations,
  // and a Draft PR can't be merged; suppress those rows so the menu never
  // offers an action that can only fail (#816). The daemon still gates
  // and reports, so this never hides a genuinely-actionable item.
  const terminal = task !== null && isTerminalTaskState(task);
  const canMerge = isPr && !terminal && task?.state !== "Draft";
  const canUpdateBranch = isPr && !terminal;
  const availableAgents =
    setupState?.agents.filter((agent) => agent.available) ?? [];
  const defaultTiers =
    availableAgents.find((agent) => agent.id === defaultAgent)?.models ?? [];

  const items: ActionMenuItem[] = [];
  // Spawn variants the primary Start button doesn't cover: per-tier and
  // on-main spawns, plus any non-default agent (#816).
  for (const tier of defaultTiers) {
    items.push({
      label: `Start ${defaultAgent} · ${tier.label}`,
      run: () => void spawnFromMenu(key, defaultAgent, tier.alias, false),
    });
  }
  for (const agent of availableAgents) {
    if (agent.id === defaultAgent) {
      continue;
    }
    items.push({
      label: `Start ${agent.label}`,
      run: () => void spawnFromMenu(key, agent.id, null, false),
    });
  }
  if (workspace !== undefined && hasRepoScope(workspace)) {
    items.push({
      label: `Start ${defaultAgent} on main checkout`,
      danger: true,
      run: () => void spawnFromMenu(key, defaultAgent, null, true),
    });
    items.push({
      label: "Start shell on main checkout",
      danger: true,
      run: () => void spawnShellFromMenu(key, true),
    });
  }
  if (task?.url) {
    const url = task.url;
    items.push({ label: "Open in browser", run: () => void openTaskUrl(url) });
  }
  if (workspace !== undefined && workspaceDiffTarget(workspace) !== null) {
    items.push({ label: "View diff", run: () => void requestWorkspaceDiff(key) });
  }
  if (canMerge) {
    items.push({
      label: "Merge PR",
      danger: true,
      run: () =>
        void confirmedMutation(
          key,
          mergePrCommand(key),
          "Merge this PR?",
          "lazybox will merge the PR through GitHub once you confirm.",
          "Merge PR",
          "Merging PR…",
          "Merge requested.",
        ),
    });
  }
  if (canUpdateBranch) {
    items.push({
      label: "Update branch",
      run: () =>
        void runWorkspaceMutation(
          updateBranchCommand(key),
          "Updating branch…",
          "Branch update requested.",
        ),
    });
  }
  items.push({ label: "Rename…", run: () => openRenameDialog(key) });
  items.push({
    label: "Archive workspace",
    danger: true,
    run: () =>
      void confirmedMutation(
        key,
        archiveCommand(key),
        "Archive this workspace?",
        "Its sessions are killed and the row leaves the inbox. The upstream PR/issue is untouched.",
        "Archive",
        "Archiving workspace…",
        "Workspace archived.",
      ),
  });
  if (isIssue && !terminal) {
    items.push({
      label: "Close issue",
      danger: true,
      run: () =>
        void confirmedMutation(
          key,
          closeIssueCommand(key),
          "Close this issue?",
          "The GitHub issue is closed as not-planned.",
          "Close issue",
          "Closing issue…",
          "Issue close requested.",
        ),
    });
  }
  if (task !== null && !terminal) {
    items.push({
      label: isPr ? "Close PR (no merge)" : "Delete or close",
      danger: true,
      run: () =>
        void confirmedMutation(
          key,
          deleteOrCloseCommand(key),
          isPr ? "Close this PR without merging?" : "Delete or close this item?",
          isPr
            ? "The pull request is closed on GitHub without merging."
            : "The issue is deleted when your token has admin rights, else closed as not-planned.",
          isPr ? "Close PR" : "Delete / close",
          "Requesting…",
          "Delete/close requested.",
        ),
    });
  }

  for (const item of items) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = item.danger
      ? "actions-menu-item danger"
      : "actions-menu-item";
    button.setAttribute("role", "menuitem");
    button.textContent = item.label;
    button.addEventListener("click", () => {
      closeActionsMenu();
      item.run();
    });
    actionsMenu.append(button);
  }
}

async function spawnFromMenu(
  workspaceKey: string,
  agent: string,
  modelAlias: string | null,
  onMain: boolean,
): Promise<void> {
  if (onMain) {
    const accepted = await confirmUserAction(
      "Start on the main checkout?",
      "The agent runs on the repo's shared main checkout — edits land on the default branch, not an isolated worktree.",
      "Start on main",
    );
    if (!accepted) {
      return;
    }
  }
  focusRequestedSession = workspaceKey;
  const succeeded = await runCommands(
    commandsForWorkspaceIntent(workspaceKey, {
      type: "spawn-agent",
      agent,
      modelAlias,
      onMain,
    }),
    `Starting ${agent}…`,
    `${agent} launch requested.`,
  );
  if (succeeded) {
    recordAnalytics("agent_started");
  } else {
    focusRequestedSession = null;
  }
}

async function spawnShellFromMenu(
  workspaceKey: string,
  onMain: boolean,
): Promise<void> {
  if (onMain) {
    const accepted = await confirmUserAction(
      "Start a shell on the main checkout?",
      "The shell runs on the repo's shared main checkout — edits land on the default branch, not an isolated worktree.",
      "Start on main",
    );
    if (!accepted) {
      return;
    }
  }
  focusRequestedSession = workspaceKey;
  const succeeded = await runCommands(
    commandsForWorkspaceIntent(workspaceKey, { type: "spawn-shell", onMain }),
    "Starting shell…",
    "Shell launch requested.",
  );
  if (succeeded) {
    recordAnalytics("shell_started");
  } else {
    focusRequestedSession = null;
  }
}

async function openTaskUrl(url: string): Promise<void> {
  if (previewMode) {
    setStatus(`Would open ${url}.`);
    return;
  }
  try {
    await invoke("open_url", { url });
    setStatus(`Opening ${url}…`);
  } catch (error) {
    setStatus(String(error));
  }
}

async function runWorkspaceMutation(
  command: LazyboxCommand,
  pendingMessage: string,
  successMessage: string,
): Promise<void> {
  await runCommands([command], pendingMessage, successMessage);
}

/**
 * Ask the daemon for the workspace's worktree diff (#843). Read-only: the
 * diff arrives asynchronously as `WorkspaceDiffInspected`, which opens the
 * reader. Records the pending workspace so a reply for a since-reselected
 * one is dropped.
 */
async function requestWorkspaceDiff(workspaceKey: string): Promise<void> {
  const workspace = workspaces.get(workspaceKey);
  if (workspace === undefined) {
    return;
  }
  const target = workspaceDiffTarget(workspace);
  if (target === null) {
    setStatus("This workspace has no worktree to review.");
    return;
  }
  if (previewMode) {
    setStatus("Would read the worktree diff.");
    return;
  }
  pendingDiffKey = workspaceKey;
  setStatus("Reading worktree diff…");
  if (!(await sendCommand(inspectWorkspaceDiffCommand(workspaceKey, target)))) {
    pendingDiffKey = null;
  }
}

/** Render the received diff into the modal and show it. */
function showWorkspaceDiff(workspaceKey: string, diff: WorkspaceDiffDto): void {
  diffBody.replaceChildren(buildDiffView(diff));
  diffBody.scrollTop = 0;
  const workspace = workspaces.get(workspaceKey);
  const task = workspace === undefined ? null : primaryTask(workspace);
  diffTitle.textContent =
    task !== null ? `Worktree diff · ${taskReference(task)}` : "Worktree diff";
  if (!diffDialog.open) {
    diffDialog.showModal();
  }
}

async function confirmedMutation(
  workspaceKey: string,
  command: LazyboxCommand,
  title: string,
  message: string,
  acceptLabel: string,
  pendingMessage: string,
  successMessage: string,
): Promise<void> {
  const target = workspaces.get(workspaceKey);
  const accepted = await confirmUserAction(
    title,
    message,
    acceptLabel,
    target?.name,
  );
  if (!accepted) {
    return;
  }
  await runWorkspaceMutation(command, pendingMessage, successMessage);
}

function openRenameDialog(workspaceKey: string): void {
  const workspace = workspaces.get(workspaceKey);
  if (workspace === undefined) {
    return;
  }
  renameNameInput.value = workspace.name;
  renameError.classList.add("hidden");
  renameDialog.dataset.key = workspaceKey;
  renameDialog.showModal();
  renameNameInput.focus();
  renameNameInput.select();
}

async function submitRename(): Promise<void> {
  const workspaceKey = renameDialog.dataset.key;
  const name = renameNameInput.value.trim();
  if (workspaceKey === undefined) {
    renameDialog.close();
    return;
  }
  if (name === "") {
    renameError.textContent = "Name the workspace.";
    renameError.classList.remove("hidden");
    renameNameInput.focus();
    return;
  }
  const succeeded = await sendCommand(renameWorkspaceCommand(workspaceKey, name));
  if (succeeded) {
    renameDialog.close();
    setStatus(`Renamed to ${name}.`);
  } else {
    renameError.textContent = "Rename failed.";
    renameError.classList.remove("hidden");
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

/**
 * Send a policy mutation while marking the control pending so
 * `renderAutomation` keeps it optimistic until the confirming broadcast.
 * On failure the pending marker is dropped — but only if a later edit
 * hasn't already superseded it — and the control reverts to committed
 * state.
 */
async function runPolicyMutation(
  workspaceKey: string,
  field: string,
  requested: string,
  command: LazyboxCommand,
  pendingMessage: string,
  successMessage: string,
): Promise<void> {
  const key = policyKey(workspaceKey, field);
  pendingPolicies.set(key, requested);
  const ok = await runCommands([command], pendingMessage, successMessage);
  if (!ok && pendingPolicies.get(key) === requested) {
    pendingPolicies.delete(key);
    renderWorkspace();
  }
}

async function setAutoMerge(enabled: boolean): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  await runPolicyMutation(
    selectedKey,
    "auto_merge",
    String(enabled),
    setAutoMergeOnGreenCommand(selectedKey, enabled),
    enabled ? "Arming auto-merge on green…" : "Disarming auto-merge…",
    enabled ? "Auto-merge on green armed." : "Auto-merge on green disarmed.",
  );
}

async function setTrackMain(enabled: boolean): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  await runPolicyMutation(
    selectedKey,
    "track_main",
    String(enabled),
    setTrackMainCommand(selectedKey, enabled),
    enabled ? "Arming track main…" : "Disarming track main…",
    enabled ? "Track main armed." : "Track main disarmed.",
  );
}

async function applyAutoFixPolicies(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const ci = autoFixCiSelect.value as PolicyArm;
  const conflict = autoFixConflictSelect.value as PolicyArm;
  await runPolicyMutation(
    selectedKey,
    "auto_fix",
    `${ci}\0${conflict}`,
    setAutoFixPoliciesCommand(selectedKey, ci, conflict),
    "Updating auto-fix policies…",
    "Auto-fix policies updated.",
  );
}

async function snoozeSelected(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const preset =
    SNOOZE_PRESETS[Number(snoozeSelect.value)] ?? SNOOZE_PRESETS[1]!;
  await runCommands(
    [snoozeCommand(selectedKey, preset.until(new Date()))],
    "Snoozing workspace…",
    `Snoozed for ${preset.label.toLowerCase()}.`,
  );
}

async function unsnoozeSelected(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  await runCommands(
    [unsnoozeCommand(selectedKey)],
    "Waking workspace…",
    "Workspace unsnoozed.",
  );
}

async function syncSelected(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  await runCommands(
    [syncWorkspaceCommand(selectedKey)],
    "Syncing workspace…",
    "Workspace sync requested.",
  );
}

async function saveNotes(): Promise<void> {
  if (selectedKey === null) {
    return;
  }
  const workspaceKey = selectedKey;
  const notes = notesBody.value;
  notesSaveButton.disabled = true;
  try {
    const ok = await runCommands(
      [setNotesCommand(workspaceKey, notes)],
      "Saving notes…",
      "Notes saved.",
    );
    if (ok) {
      // Saved: drop the draft so the control follows the broadcast
      // `Workspace.notes` once its echo lands, instead of pinning a copy.
      notesDrafts.delete(workspaceKey);
    }
  } finally {
    notesSaveButton.disabled = false;
  }
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
  for (const live of liveTerminals.values()) {
    live.terminal.options.theme = terminalTheme(colors);
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
  // Escape closes the filter menu or leaves the search box, ahead of
  // the editable guard so it works while the search input has focus.
  if (event.key === "Escape") {
    if (actionsMenuOpen) {
      event.preventDefault();
      closeActionsMenu();
      return;
    }
    if (filterMenuOpen) {
      event.preventDefault();
      closeFilterMenu();
      return;
    }
    if (document.activeElement === inboxSearch) {
      inboxSearch.blur();
      return;
    }
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
    snippetDialog.open ||
    diffDialog.open
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
  if (event.key === "o") {
    event.preventDefault();
    void cycleSortMode();
  } else if (event.key === "r") {
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
  } else if (event.key === "f") {
    event.preventDefault();
    toggleFilterMenu();
  } else if (event.key === "/") {
    event.preventDefault();
    inboxSearch.focus();
  } else if (event.key === ".") {
    event.preventDefault();
    toggleFocusMode();
  } else if (event.key === "R") {
    event.preventDefault();
    void refreshInbox(true);
  }
}

function navigateWorkspaces(delta: number): void {
  const keys = navigableWorkspaceKeys();
  if (keys.length === 0) {
    return;
  }
  const current = keys.indexOf(selectedKey ?? "");
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

// The protocol-skew advisory describes a session-long condition (two builds
// that differ), so it gets its own persistent surface rather than the
// ephemeral status line, which transient warnings and user actions overwrite.
function setProtocolNotice(message: string | null): void {
  if (message) {
    protocolNotice.textContent = message;
    protocolNotice.hidden = false;
  } else {
    protocolNotice.textContent = "";
    protocolNotice.hidden = true;
  }
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
