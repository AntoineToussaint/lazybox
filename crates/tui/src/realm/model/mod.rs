//! `Model` — the realm-side replacement for lazybox's `App` struct.
//!
//! ## Architecture
//!
//! Panes (Sidebar / Right / Terminals) are **not** mounted into the
//! tuirealm `Application`. They live as typed fields on `Model` and
//! we drive their `view`/`on_event`/`handle_key` directly. tuirealm's
//! `Application` only owns **modals** — that's where its mount/unmount
//! + Z-stack semantics actually pay off.
//!
//! Why: lazybox's panes are persistently visible, mutate often, and the
//! orchestrator needs typed handles to drain queued commands. Mounting
//! them via `app.mount(id, Box::new(pane))` hides the concrete type
//! behind `dyn AppComponent` and forces awkward attribute-based
//! round-trips for the simplest "give me the queued commands" calls.
//! Holding them as fields is the cleaner shape.
//!
//! ## Modal stack
//!
//! Modals do go through `Application`. We track a `Vec<Id>` so multi-
//! modal stacking (rare) works, and call `app.active(&id)` whenever
//! the top changes. Modal payloads come back as `Msg`s from
//! `app.tick(...)` and `Model::update` decides what to do.

mod dispatch;
mod events;
mod helpers;
mod inputs;
mod keys;
mod modals;
#[cfg(test)]
mod tests;

pub use helpers::{run_loop_with_model, run_with_client};

// Re-export helper free functions so sibling submodules
// (`keys.rs`, etc.) can keep their `super::foo` import shape after
// the helpers moved out of mod.rs.
pub(crate) use helpers::{
    emit_clipboard_copy, find_action_for_chord, key_event_to_chord, paint_selection, rect_contains,
    split_for_footer,
};

use crate::PaneId;
use crate::realm::UserEvent;
use crate::realm::components::right::Right;
use crate::realm::components::sidebar::Sidebar;
use crate::realm::components::splash::Splash;
use crate::realm::components::terminals::Terminals;
use lazybox_ipc::{Client, Command as IpcCommand};
use std::sync::mpsc;
use std::time::Duration;
use tuirealm::application::Application;
use tuirealm::event::Event as RealmEvent;
use tuirealm::listener::{EventListenerCfg, Poll, PortError, PortResult};
use tuirealm::ratatui::layout::Rect;
use tuirealm::terminal::{CrosstermTerminalAdapter, TerminalAdapter};

const SIDEBAR_PID: PaneId = PaneId::new(1);
const RIGHT_PID: PaneId = PaneId::new(2);
const TERMINALS_PID: PaneId = PaneId::new(3);

/// Component IDs for modal-side mounts only. Pane access is via
/// typed fields, so panes don't appear here.
#[derive(Debug, Eq, PartialEq, Clone, Hash)]
pub enum Id {
    Splash,
    Help,
    Error,
    Polling,
    Reply,
    /// Single-line input prompt for naming a brand-new pre-PR
    /// workspace. Submit → `Command::CreateWorkspace { name }`.
    NewWorkspace,
    /// Single-line input prompt for naming a brand-new local
    /// Project. Submit → `Command::CreateProject { name }`.
    NewProject,
    /// Picker for selecting an editor when 2+ are detected.
    /// Submit → `editors::launch(template, worktree)`.
    Editor,
    /// Active setup-wizard step. Each transition unmounts the
    /// previous component at this id and mounts the next; only one
    /// setup step is ever live.
    Setup,
    /// Confirm dialog for removing a workspace — either one that fell
    /// out of scope while having running terminals, or one whose PR
    /// just merged (see `RemovalReason`). The pending key + reason
    /// live in `active_removal_prompt`; `Msg::Confirmed(true)` reads
    /// the reason to pick the right command (`Kill` vs.
    /// `RemoveMergedWorkspace`).
    RemoveOutOfScope,
    /// Confirm dialog asking the user to merge an issue workspace
    /// (that has live sessions) into the PR that closes it. The
    /// (issue, PR) keys live in `active_merge_prompt`; `Msg::Confirmed`
    /// dispatches `Command::ConfirmMerge` back to the daemon.
    MergeConfirm,
    /// Picker for the `Shift-A` ("adopt") flow — pick the target
    /// workspace the source's sessions should move into. Source is
    /// stashed in `pending_adopt_source`; `Msg::ChoicePicked` reads
    /// the picked index out of `adopt_choices` and dispatches
    /// `Command::AdoptSessions`.
    AdoptTarget,
    /// Project picker for the global "start agent" (`Shift-W`) flow.
    /// Choices live in `start_agent_project_choices`; `Msg::ChoicePicked`
    /// resolves the project, then funnels into the new-workspace name
    /// input (which auto-spawns the default agent on submit). Skipped
    /// when only one project exists.
    StartAgentProject,
    /// Single-line input prompt for the reviewer-login(s) to add to
    /// the focused workspace's PR. Submit →
    /// `Command::RequestReviewers { workspace_key, logins }`. The
    /// pending workspace key lives in `pending_review_request`;
    /// `Msg::InputSubmitted` reads it.
    RequestReviewers,
    /// Same shape as `RequestReviewers` but for assignees. Submit
    /// → `Command::AddAssignees { workspace_key, logins }`. Works
    /// on issues too (both PRs and issues are `Assignable`).
    AddAssignees,
    /// Multi-select picker mounted on Shift-L (`ManageLabels`).
    /// Lists the repository's full label set with the currently-
    /// applied labels pre-checked; submit → `Command::SetLabels`.
    /// Works on issues too — both PRs and issues implement GraphQL's
    /// `Labelable` interface.
    ManageLabels,
    /// Duration picker mounted on `z` (ToggleSnooze) when the
    /// workspace is NOT currently snoozed. Single-pick choice
    /// modal with several common snooze durations (1h, today,
    /// tomorrow, next week, 1 month, forever). The pending
    /// workspace key lives in `pending_snooze_workspace`;
    /// `Msg::ChoicePicked` reads it + the picked Duration and
    /// dispatches `Command::Snooze`.
    SnoozeDuration,
    /// Right-click context menu over a sidebar workspace row.
    /// Single-pick `Choice` modal whose items are the workspace's
    /// available actions (spawn claude / shell / mark read /
    /// archive / merge / …). Source row + action list live in
    /// `pending_sidebar_context`; `Msg::ChoicePicked` resolves the
    /// index back to an action and dispatches the same IPC the
    /// keyboard shortcut would.
    SidebarContext,
    /// Confirm dialog before firing `Command::CleanWorktrees`.
    /// Picked from the Settings palette; Yes → dispatch.
    CleanWorktreesConfirm,
    /// Loading screen mounted while we wait for the daemon to reply
    /// to `Command::InspectWorktrees`. Swapped out for `InspectList`
    /// when `Event::WorktreesInspected` arrives.
    InspectLoading,
    /// Choice modal listing every worktree the inspector reported,
    /// with a special first row for "Delete all N safe worktrees".
    /// Picking a row routes through `pending_inspect_rows` →
    /// `InspectConfirm` for a final per-row destructive prompt.
    InspectList,
    /// Confirm for the per-row delete picked from `InspectList`.
    /// The target row lives in `pending_inspect_target`;
    /// `Msg::Confirmed(true)` dispatches `DeleteOrphanedWorktree`.
    InspectConfirm,
    /// Unified confirm modal for any destructive catalog action.
    /// `Model::dispatch_action` routes here when
    /// `ActionDef::is_destructive()` is true; the pending `Action`
    /// lives in `pending_action_confirm` and fires on
    /// `Msg::Confirmed(true)`. Replaces the per-action confirm
    /// modals (MergePrConfirm, the kill latch, …) — one modal id,
    /// one Yes-handler, one place to remember.
    ActionConfirm,
    /// Snippet picker mounted from the terminal pane on `]<key>`.
    /// Filter input + scrollable snippet list. `Msg::ChoicePicked`
    /// resolves the picked row to a snippet body, which the
    /// dispatcher writes to the active terminal followed by `\r`
    /// (auto-submit). See `realm::components::snippet_picker`.
    SnippetPicker,
    /// In-app feature tour (issue #146). Stepped walkthrough card,
    /// launched on first run (gated by `ui.tour_seen`) and on demand
    /// via the tour shortcut. `Msg::TourFinished` marks it seen +
    /// pops. See `realm::components::tour`.
    Tour,
    /// Debug / sync-status window (default `Shift-D`). Read-only,
    /// scrollable view of recent provider-sync outcomes built from
    /// `self.status.sync`. No pending state — dismiss just pops it.
    SyncStatus,
}

/// Why a workspace-removal confirm prompt is being shown. Both
/// reasons share the `Id::RemoveOutOfScope` modal + the
/// `pending_removal_prompts` queue but differ in copy and in which
/// command "yes" dispatches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemovalReason {
    /// Workspace fell out of filter scope but still has live
    /// terminals. Yes → `Command::Kill` (drop row + kill terminals,
    /// worktree left on disk).
    OutOfScope,
    /// The PR merged. Yes → `Command::RemoveMergedWorkspace` (kill
    /// sessions, delete the worktree, drop the row).
    Merged,
}

/// Concrete target a destructive `ActionConfirm` modal was mounted
/// against — resolved from the sidebar selection when the confirm
/// mounts and stashed in `pending_action_confirm`. Dispatch on
/// "Yes" fires against this stash, never the live selection, so a
/// daemon event that moves the cursor under the modal can't redirect
/// the action onto a different row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionConfirmTarget {
    /// A workspace row (PR / issue / pre-PR scratch workspace).
    Workspace(lazybox_core::SessionKey),
    /// A project header — `Archive` here deletes the whole project.
    Project(lazybox_core::ProjectKey),
}

/// One queued workspace-removal prompt. Surfaced one at a time as a
/// Confirm modal by `maybe_mount_next_removal_prompt`.
#[derive(Debug, Clone)]
pub(crate) struct RemovalPrompt {
    pub(crate) workspace_key: lazybox_core::WorkspaceKey,
    /// Compact `owner/repo#N` identifier for the modal copy.
    pub(crate) label: String,
    /// Primary task title (out-of-scope only) rendered inline so the
    /// user recognizes the work. `None` for the merged path, which
    /// keys off the label alone.
    pub(crate) title: Option<String>,
    /// Live terminals removal would kill — quoted back in the copy.
    pub(crate) terminal_count: usize,
    pub(crate) reason: RemovalReason,
    /// Merged path: any backing worktree has uncommitted/unpushed
    /// work, so the copy warns before the force-delete.
    pub(crate) has_local_work: bool,
}

/// App-level message vocabulary for modals + globals.
#[derive(Debug, PartialEq, Clone)]
pub enum Msg {
    SplashConfirmed,
    /// The feature tour was dismissed or finished — mark it seen so
    /// it doesn't re-launch, and pop the modal.
    TourFinished,
    AppClose,
    Confirmed(bool),
    InputSubmitted(String),
    TextareaSubmitted(String),
    ChoicePicked(Vec<usize>),
    ChoiceRefresh,
    ChoiceBack,
    LoadingResolved(PayloadCarrier),
    PollingError((String, String, String, String)),
    PollingTimeout,
    PollingEmptyInbox(Vec<String>),
    ModalDismissed,
    /// Sidebar / Right / Terminals routes — kept in case a future
    /// pane goes through tuirealm. Today panes drain themselves
    /// directly inside the orchestrator's pane-dispatch path.
    SidebarCmds,
    RightCmds,
    TerminalCmds,
}

/// Wrapper that lets us put a non-`PartialEq` payload inside `Msg`.
#[derive(Clone)]
pub struct PayloadCarrier(
    pub std::sync::Arc<std::sync::Mutex<Option<Box<dyn std::any::Any + Send>>>>,
);

impl PartialEq for PayloadCarrier {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for PayloadCarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PayloadCarrier(<opaque>)")
    }
}

impl PayloadCarrier {
    pub fn take(&self) -> Option<Box<dyn std::any::Any + Send>> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

/// Which pane has focus when no modal is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneFocus {
    Sidebar,
    Right,
    Terminals,
}

impl PaneFocus {
    fn next(self) -> Self {
        match self {
            PaneFocus::Sidebar => PaneFocus::Right,
            PaneFocus::Right => PaneFocus::Terminals,
            PaneFocus::Terminals => PaneFocus::Sidebar,
        }
    }
}

/// Top-level application state.
pub struct Model<T: TerminalAdapter> {
    pub app: Application<Id, Msg, UserEvent>,
    pub terminal: T,
    /// Z-stack of modal ids — top is rendered last + receives input.
    pub modal_stack: Vec<Id>,
    /// Authenticated user logins per provider source. Populated
    /// from `IpcEvent::ViewerIdentities`. Passed into RightPane so
    /// activity bylines authored by the local user render as `@me`.
    pub viewer_logins: std::collections::HashMap<String, String>,
    /// Which pane has focus when no modal is active.
    focus: PaneFocus,
    /// Three pane wrappers held as typed fields so the orchestrator
    /// can call `.drain_cmds()` etc. directly. The wrappers also
    /// track their own `focused: bool` flag, which we keep in sync
    /// via `set_focus_attr()`.
    sidebar: Sidebar,
    right: Right,
    terminals: Terminals,
    /// Project records mirrored from the daemon. Keyed by
    /// `ProjectKey` so lookups during sidebar grouping are O(1).
    /// Populated by `Event::Snapshot` (initial) and
    /// `Event::ProjectUpserted` (new project or first-sight repo);
    /// `Event::ProjectRemoved` drops entries. Stage 2 stores them
    /// here; stages 3+ render headers from this map.
    pub projects: std::collections::BTreeMap<lazybox_core::ProjectKey, lazybox_core::Project>,
    /// Project keys *this client* synthesized from `selected_scopes`
    /// in `refresh_subscribed_projects` — placeholder headers for
    /// repos the user subscribed to that the daemon hasn't surfaced a
    /// workspace for yet. Tracked apart from daemon-authoritative
    /// projects so that when the user *unsubscribes* a repo we can drop
    /// its placeholder (the daemon never had a record for it, so no
    /// `ProjectRemoved` will ever arrive). A daemon `ProjectUpserted` /
    /// `Snapshot` for the same key promotes it to authoritative and
    /// removes it from this set — see `events.rs`.
    synthesized_projects: std::collections::BTreeSet<lazybox_core::ProjectKey>,
    /// IPC client for forwarding pane-emitted commands to the daemon.
    pub client: Client,
    /// Watches the inbound daemon-event channel depth after each
    /// drain. A backlog that climbs tick-over-tick means the TUI is
    /// consuming slower than the daemon produces — the signature of a
    /// runaway producer or a leak. See `helpers::BacklogMonitor`.
    event_backlog: helpers::BacklogMonitor,
    pub redraw: bool,
    pub quit: bool,
    /// Setup wizard / settings palette / editor-open state — see
    /// `SetupCtx`. Lives in one struct so the eight related fields
    /// don't clutter the top-level Model definition.
    setup: SetupCtx,
    /// Sender into the custom `ChannelPort`. Run loop pushes
    /// keyboard events here when a modal is up so Application's
    /// listener thread picks them up + dispatches.
    modal_event_tx: mpsc::Sender<RealmEvent<UserEvent>>,
    /// q-q double-tap quit latch. First `q` outside a terminal arms;
    /// second `q` within `ui_defaults.quit_double_tap_window` quits.
    /// Any other key disarms via `q_latch.disarm()`.
    q_latch: crate::confirm_latch::DoubleTapLatch,
    /// Grouped two-step (leader-key) chord state, issue #126. A group
    /// leader key (`g` for the github group) arms it; the next key
    /// picks an action within the group and fires it through the
    /// unified `dispatch_action`. Drives the which-key popup in
    /// `view`. Operator-pending, not timed — see `LeaderLatch`.
    leader: crate::confirm_latch::LeaderLatch<lazybox_tui_core::action::ActionGroup>,
    /// Last left-click position + timestamp. A second left-click on
    /// the same row within `DOUBLE_CLICK_WINDOW` is treated as a
    /// double-click; the right pane's double-click handler then
    /// toggles expand/collapse on the card. Crossterm doesn't
    /// report double-clicks natively — we synthesize them here.
    last_click: Option<(u16, u16, std::time::Instant)>,
    /// True if the user has typed at least one non-Tab key since
    /// focus entered the terminal pane. While `false`, Tab in the
    /// terminal pane cycles focus like everywhere else; once the
    /// user has typed anything, Tab routes to the PTY (autocomplete).
    /// Reset to `false` on every focus-enter of `Terminals` so each
    /// fresh visit gets the cycle-out behavior.
    terminal_user_typed_since_focus: bool,
    /// `true` between a Shift-R Refresh and the next PollCompleted
    /// arriving from the daemon. Drives a one-shot "✓ sync ok"
    /// footer notice so the user knows the manual refresh actually
    /// landed (silent spinner-clears were being read as "did
    /// anything happen?"). Cleared on the next PollCompleted OR a
    /// ProviderError for the same source.
    pending_refresh_ack: bool,
    /// `Some(source)` while the footer notice is a provider "✗ sync
    /// failed" banner (Permanent severity, so it never auto-fades),
    /// naming the provider whose poll failed. Lets the next successful
    /// `PollCompleted` *from that same provider* clear the stale banner
    /// once its sync recovers — otherwise a transient failure left the
    /// red notice up forever even though syncing was healthy again.
    ///
    /// Tracking the source (not just a bool) matters because lazybox
    /// polls several providers concurrently (GitHub, Linear, Slack): a
    /// successful Linear poll must not erase a still-valid GitHub
    /// failure banner. Any other `flash`/`flash_*` call resets it to
    /// `None`, so we only clear the notice when it's still the
    /// sync-error we set.
    sync_error_source: Option<String>,
    /// Whether lazybox is capturing mouse events. Toggled by F8 /
    /// Alt-s. When `false`, lazybox has issued `DisableMouseCapture`
    /// so the host terminal regains native text selection (which
    /// spans lazybox's whole window including UI chrome — uglier
    /// than lazybox's pane-scoped selection but useful as a fallback).
    /// When `true`, lazybox owns mouse: clicks drive its UI, drags
    /// inside the terminal pane do lazybox-side text selection.
    #[allow(dead_code)] // accessed indirectly via the toggle handler
    mouse_capture_on: bool,
    /// Active lazybox-side text selection in the terminal pane.
    /// `(start_cell, end_cell)` in absolute viewport coords, set on
    /// mouse Down inside the terminal rect (when the inner program
    /// isn't tracking mouse itself) and extended on Drag. On Up the
    /// selected cells are extracted from libghostty's grid and
    /// copied to the host clipboard via OSC 52.
    terminal_selection: Option<((u16, u16), (u16, u16))>,
    /// `]]` escape from the terminal pane: first press of the escape
    /// char arms; a second within the window arms the `]]` *leader*
    /// (see `terminal_leader_at`) instead of forwarding to the PTY.
    escape_latch: crate::confirm_latch::DoubleTapLatch,
    /// `]]` leader armed-at instant. Set when `]]` completes with a
    /// snippet library present: the *next* key selects a binding
    /// (snippet) or cancels; if no key arrives within
    /// `ui_defaults.escape_window` the pane leaves on the idle tick
    /// (`tick_terminal_leader`). `None` when not armed.
    terminal_leader_at: Option<std::time::Instant>,
    /// Pending `--workspace` / `--session` preselect from the CLI.
    /// Applied after the daemon's first Snapshot — by then the
    /// sidebar has the full workspace list and `focus_workspace_key`
    /// can land. Cleared once applied (one-shot).
    preselect: Option<Preselect>,
    /// Width of the sidebar column as a percentage of total width.
    /// Adjustable via `Shift-Left`/`Shift-Right` (and mouse drag);
    /// Splits, last-viewport snapshot, and active drag — see
    /// `LayoutCtx`.
    layout: LayoutCtx,
    /// Workspace key the reply textarea (if mounted) is targeting.
    /// Set by `mount_reply`; consumed by `Msg::TextareaSubmitted` to
    /// build the `Command::PostReply` payload.
    pending_reply: Option<lazybox_core::SessionKey>,
    /// Set by `mount_request_reviewers`; consumed by
    /// `handle_input_submitted` when `Id::RequestReviewers` is the
    /// top modal. Holds the workspace whose PR we'll request
    /// reviewers on.
    pending_review_request: Option<lazybox_core::WorkspaceKey>,
    /// Candidate logins shown in the `RequestReviewers` picker.
    /// Indices from `Msg::ChoicePicked` index back into this Vec.
    /// Cleared after the picker dispatches.
    review_choices: Vec<String>,
    /// Same shape but for the add-assignees flow.
    pending_assignees_request: Option<lazybox_core::WorkspaceKey>,
    /// Candidate logins shown in the `AddAssignees` picker.
    assignees_choices: Vec<String>,
    /// Workspace key the `ManageLabels` picker is targeting. Stashed
    /// at mount time so when `Event::RepoLabels` lands the picker
    /// can re-mount with the repo's labels. Cleared on submit /
    /// dismiss.
    pending_labels_request: Option<lazybox_core::WorkspaceKey>,
    /// Repo-label names rendered in the `ManageLabels` picker. Order
    /// matches the picker's row indices so `Msg::ChoicePicked(indices)`
    /// indexes back into this list. Cleared on submit / dismiss.
    labels_choices: Vec<String>,
    /// Workspace currently waiting on the `SnoozeDuration` picker's
    /// result. `Msg::ChoicePicked` reads this + `snooze_choices` to
    /// turn the picked index into a `Command::Snooze`.
    pending_snooze_workspace: Option<lazybox_core::SessionKey>,
    /// The duration each picker option maps to. Order MUST match
    /// the labels rendered in `mount_snooze_picker`.
    snooze_choices: Vec<std::time::Duration>,
    /// Queued workspace-removal prompts — either out-of-scope
    /// workspaces with running terminals (`WorkspaceOutOfScope`) or
    /// merged PRs (`MergedPrRemovable`). The daemon won't auto-remove
    /// either; each lands here and one at a time gets surfaced as a
    /// Confirm modal so the user decides. See [`RemovalReason`].
    pending_removal_prompts: std::collections::VecDeque<RemovalPrompt>,
    /// Workspace + reason currently being prompted about. Set when the
    /// RemoveOutOfScope modal mounts; consumed by `Msg::Confirmed`,
    /// which uses the reason to pick the command to dispatch.
    active_removal_prompt: Option<(lazybox_core::WorkspaceKey, RemovalReason)>,
    /// Pending issue→PR merge prompts. Daemon stalls a merge when
    /// the issue has live sessions and emits
    /// `WorkspaceMergePending`; we queue here and surface one at a
    /// time as a Confirm modal. Tuple: issue key, PR key, issue
    /// label, PR label, live terminal count.
    pending_merge_prompts: std::collections::VecDeque<(
        lazybox_core::WorkspaceKey,
        lazybox_core::WorkspaceKey,
        String,
        String,
        usize,
    )>,
    /// (issue, PR) pair currently being prompted about. Consumed by
    /// `Msg::Confirmed` when the top modal is `Id::MergeConfirm`.
    active_merge_prompt: Option<(lazybox_core::WorkspaceKey, lazybox_core::WorkspaceKey)>,
    /// Workspace key whose PR is being confirmed for merge by the
    /// `Shift-M` Confirm modal. Set when the modal mounts, taken on
    /// `Msg::Confirmed` / `Msg::ModalDismissed`.
    /// Source workspace key the `Shift-A` adopt picker is gathering
    /// a target for. Set when the picker mounts; consumed when the
    /// user picks (or dismisses).
    pending_adopt_source: Option<lazybox_core::WorkspaceKey>,
    /// Candidate target workspaces for the active adopt picker,
    /// in the same order as the picker's row indices. `Msg::ChoicePicked`
    /// indexes into this to recover the chosen `WorkspaceKey`.
    adopt_choices: Vec<lazybox_core::WorkspaceKey>,
    /// Candidate projects for the active "start agent" (`Shift-W`)
    /// picker, in row order. `Msg::ChoicePicked` indexes into this to
    /// recover the chosen `ProjectKey`, then mounts the name input.
    start_agent_project_choices: Vec<lazybox_core::ProjectKey>,
    /// Transient UI status (polling spinner + footer notice). See
    /// `StatusCtx`.
    status: StatusCtx,
    /// Resolved values for the magic-number knobs that used to be
    /// module-level `const`s — read from `~/.lazybox/config.yaml::ui`,
    /// or `UiDefaults::default()` when unset / not loaded.
    ui_defaults: lazybox_config::UiDefaults,
    /// Workspace keys for which we've already fired
    /// `Command::FetchPrDetails` this session — the lazy-fetch path
    /// that back-fills review-thread activity. Used to dedupe the
    /// trigger so a flicker of focus doesn't spam the daemon.
    /// Cleared when a workspace is removed (`Event::WorkspaceRemoved`)
    /// so a re-added workspace gets a fresh fetch.
    pr_details_fetched: std::collections::HashSet<lazybox_core::WorkspaceKey>,
    /// Last `SessionKey` we sent a `Command::FocusWorkspace` for.
    /// Single source of truth for "did the cursor leave the previous
    /// workspace?". `sync_panes` reads it after every key/mouse
    /// dispatch and emits a fresh `FocusWorkspace` when the selected
    /// workspace key has changed. Centralizing here means every
    /// cursor-mutating path (j/k, mouse click, programmatic
    /// preselect) feeds the daemon's round-robin scheduler without
    /// each call site needing its own emit hook.
    last_focused_session_key: Option<lazybox_core::SessionKey>,
    /// Active sidebar right-click context menu state: the workspace
    /// row the menu was raised over plus the ordered list of catalog
    /// `Action`s the picker is offering. `Msg::ChoicePicked` indexes
    /// back into the Vec and dispatches the same IpcCommand the
    /// matching keyboard shortcut would have. None when no menu is
    /// open.
    pending_sidebar_context: Option<(
        lazybox_core::SessionKey,
        Vec<lazybox_tui_core::action::Action>,
    )>,
    /// User-supplied key overrides for catalog actions. Keys are
    /// snake_case `ActionKind` names (see `ActionKind::name`); values
    /// are key-spec strings. Empty when the user hasn't configured
    /// `ui.action_keys` — catalog defaults apply.
    action_key_overrides: std::collections::BTreeMap<String, String>,
    /// Action queued behind an `ActionConfirm` modal, paired with the
    /// concrete target it was aimed at when the modal mounted. Set by
    /// `mount_action_confirm`, taken (and dispatched if Yes) by
    /// the `Msg::Confirmed` handler. None when no destructive
    /// confirm is currently up.
    ///
    /// The target is resolved at MOUNT time, not at confirm time —
    /// daemon events (a workspace removal, a poll-driven re-sort) can
    /// move the sidebar cursor while the modal is up, and re-reading
    /// the selection on "Yes" would kill / merge a different row than
    /// the prompt named.
    pending_action_confirm: Option<(lazybox_tui_core::action::Action, ActionConfirmTarget)>,
    /// Latest inspector report driving the `InspectList` modal. The
    /// first slot in the Choice modal is the "delete all safe"
    /// shortcut, hence the wrapper enum on indices.
    pending_inspect_rows: Vec<lazybox_ipc::WorktreeInspectionDto>,
    /// Row picked from `InspectList`, waiting on the `InspectConfirm`
    /// confirm modal. Consumed by `Msg::Confirmed(true)`.
    pending_inspect_target: Option<lazybox_ipc::WorktreeInspectionDto>,
    /// Project the next `Id::NewWorkspace` submit should land the
    /// new workspace under. Set by `mount_new_workspace_input(pk)`
    /// from the focused-project resolver, consumed by
    /// `handle_input_submitted`'s `Id::NewWorkspace` arm.
    pending_new_workspace_project: Option<lazybox_core::ProjectKey>,
    /// Name of a project the user just submitted via Shift-N. When
    /// the daemon broadcasts `ProjectUpserted` for a matching name,
    /// we focus its header row + auto-mount the new-workspace input
    /// — without this hand-off, the new project is unreachable via
    /// j/k (RepoHeader rows are skipped by `move_cursor_by`) and the
    /// user has no clear next step.
    pending_focus_project_name: Option<String>,
    /// Issue workspace the user was viewing when it was removed by a
    /// merge. Set in the `WorkspaceRemoved` handler (before the sidebar
    /// moves the cursor off the gone row) and consumed by the matching
    /// `WorkspaceMerged` to follow the moved sessions onto the PR
    /// workspace — otherwise the cursor lands on an arbitrary row and
    /// the merged session looks lost.
    merge_follow_from: Option<lazybox_core::WorkspaceKey>,
    /// Loaded + merged snippet collection (`<lazybox_home>/snippets.yaml`
    /// + `<cwd>/.lazybox/snippets.yaml`). Populated at startup by
    /// `apply_snippets`; the terminal-pane `]` latch reads this to
    /// decide whether to mount the picker. Empty when neither file
    /// exists (the typical first-run state).
    pub(crate) snippets: lazybox_config::Snippets,
    /// Snapshot of the snippet keys the active SnippetPicker is
    /// showing — indexed by `Msg::ChoicePicked` to recover the
    /// underlying entry via `self.snippets.get(...)`. Cleared on
    /// mount/unmount. Storing keys (not full rows) avoids cloning
    /// the snippet body twice on every picker mount.
    pub(crate) snippet_choices: Vec<String>,
    /// Set at startup from `ui.tour_seen` (inverted): `true` means
    /// the feature tour should auto-launch once the panes are
    /// visible. Cleared the moment the tour mounts so it never
    /// double-fires; manual `Shift-T` invocation ignores it. See
    /// `maybe_mount_tour` / `mount_tour`.
    auto_tour_pending: bool,
    /// Progressive feature-discovery tips (#115). `tips_enabled`
    /// mirrors `ui.show_tips` (opt-out); `tips_seen` mirrors
    /// `ui.tour_seen` but per-tip (ids already surfaced, persisted so
    /// a tip never repeats across sessions). `tip_shown_this_session`
    /// caps it to one tip per run so they stay quiet, and
    /// `tips_armed_at` is the idle baseline — a tip only fires once
    /// the footer has sat free of any modal / notice for a beat. See
    /// `tick_tips`.
    tips_enabled: bool,
    tips_seen: Vec<String>,
    tip_shown_this_session: bool,
    tips_armed_at: std::time::Instant,
    /// Inertia damper for trackpad scroll. macOS sends ~20-50 wheel
    /// events per flick (the OS inertia phase); each one moves the
    /// viewport `STEP` rows, so a single gesture scrolls hundreds of
    /// rows past where the user expected. We track the current
    /// burst's direction / count / age so a sustained flick decays
    /// its step and a direction reversal stops the queued inertia
    /// from the prior gesture instead of fighting it. `None` when no
    /// recent scroll.
    pub(crate) scroll_inertia: Option<ScrollInertia>,
    /// Deadline until which the run loop keeps re-rendering after a
    /// modal-bound key/paste was forwarded to the listener channel.
    /// The listener delivers those events asynchronously (~10ms port
    /// cadence) and many modal keys mutate state without emitting a
    /// `Msg` (Confirm arrows, Input typing), so a `Msg`-gated redraw
    /// would miss them. Arming a short window covers the async hop
    /// without blocking the loop. `None` when no modal input is in
    /// flight. See `forward_modal_event`.
    modal_redraw_until: Option<std::time::Instant>,
}

/// State tracked for the trackpad-scroll damper (see
/// `Model::scroll_inertia`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollInertia {
    /// +1 for ScrollDown, -1 for ScrollUp. Sign of the burst.
    pub dir: i8,
    /// Events accepted into this burst so far. Drives the
    /// diminishing-step curve.
    pub count: u32,
    /// Last event time — drives the staleness check.
    pub last_at: std::time::Instant,
}

/// Custom Port that drains events from an `mpsc::Receiver`. Lazybox
/// reads crossterm directly in the run loop (so panes get keys
/// without the listener thread / main thread racing for them) and
/// pushes modal-bound events onto the sender. The listener thread
/// polls this port and delivers them to the Application's mounted
/// modal via the usual subscribe path.
struct ChannelPort {
    rx: mpsc::Receiver<RealmEvent<UserEvent>>,
}

impl Poll<UserEvent> for ChannelPort {
    fn poll(&mut self) -> PortResult<Option<RealmEvent<UserEvent>>> {
        match self.rx.try_recv() {
            Ok(ev) => Ok(Some(ev)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(PortError::PermanentError(
                "event channel disconnected".into(),
            )),
        }
    }
}

/// CLI-driven post-snapshot focus target. Applied once after the
/// first Snapshot so the user lands on a specific workspace +
/// (optionally) session. Used by `--workspace KEY [--session ID]`.
#[derive(Debug, Clone)]
pub struct Preselect {
    /// Workspace key (e.g. `"github:owner/repo#42"`) to land on.
    pub workspace_key: lazybox_core::SessionKey,
    /// Optional session id to focus inside the workspace. Anything
    /// that doesn't parse as a uuid is silently ignored.
    pub session_id_raw: Option<String>,
}

use crate::realm::layout::{LayoutCtx, pane_areas};
use crate::realm::setup_ctx::{SettingsAction, SetupCtx};
use crate::realm::status_ctx::StatusCtx;

/// How long the run loop keeps re-rendering after a modal-bound key is
/// forwarded to the listener channel. Generous multiple of the 10ms
/// port-poll cadence plus a few render frames, so the asynchronously-
/// delivered key is always reflected on screen even when it produces
/// no `Msg`. See `Model::forward_modal_event`.
const MODAL_REDRAW_WINDOW: Duration = Duration::from_millis(120);

/// How long the footer must sit idle (no modal, no notice) after
/// startup before a feature tip (#115) is allowed to surface. Long
/// enough that the first-run tour and the initial-poll spinner clear
/// first, short enough that a settled user sees a tip the same
/// session. See `Model::tick_tips`.
const TIP_IDLE_DELAY: Duration = Duration::from_secs(8);

/// How long the first `q` stays armed waiting for the second tap.
// `Q_DOUBLE_TAP_WINDOW` retired — value lives on `ui_defaults`
// now, sourced from `~/.lazybox/config.yaml::ui.quit_double_tap_window`
// with `lazybox_config::UiDefaults::default()` as the fallback.

/// Escape-char for the terminal-pane breakout sequence. Two
/// consecutive presses (with no intervening non-`]` key) returns
/// focus to the sidebar instead of forwarding to the PTY.
// `TERMINAL_ESCAPE_CHAR` retired — value lives on `ui_defaults`,
// sourced from `~/.lazybox/config.yaml::ui.terminal_escape_char`
// (default `]`).

impl<T: TerminalAdapter> Model<T> {
    /// Backend-independent constructor — both `new` (crossterm) and
    /// `new_for_test` (TestTerminalAdapter) go through this so the
    /// common Application setup + field initializers only live in
    /// one place. Callers are responsible for prepping the terminal
    /// (raw mode, alt screen, mouse capture) before passing it in.
    fn build(terminal: T, client: Client) -> Self {
        // Build the modal-event channel + register a custom Port for
        // it. Crossterm input is read directly in the run loop —
        // there's no `crossterm_input_listener` here, so the listener
        // thread doesn't race the main thread for keystrokes.
        let (modal_event_tx, modal_event_rx) = mpsc::channel();
        let app: Application<Id, Msg, UserEvent> = Application::init(
            EventListenerCfg::default()
                .add_port(
                    Box::new(ChannelPort { rx: modal_event_rx }),
                    Duration::from_millis(10),
                    16,
                )
                .tick_interval(Duration::from_millis(50)),
        );
        Self {
            app,
            terminal,
            modal_stack: Vec::new(),
            viewer_logins: std::collections::HashMap::new(),
            focus: PaneFocus::Sidebar,
            sidebar: Sidebar::new(SIDEBAR_PID),
            right: Right::new(RIGHT_PID),
            terminals: Terminals::new(TERMINALS_PID),
            projects: std::collections::BTreeMap::new(),
            synthesized_projects: std::collections::BTreeSet::new(),
            client,
            event_backlog: helpers::BacklogMonitor::default(),
            redraw: true,
            quit: false,
            setup: SetupCtx::new(),
            modal_event_tx,
            q_latch: crate::confirm_latch::DoubleTapLatch::new(),
            leader: crate::confirm_latch::LeaderLatch::new(),
            escape_latch: crate::confirm_latch::DoubleTapLatch::new(),
            terminal_leader_at: None,
            last_click: None,
            terminal_user_typed_since_focus: false,
            pending_refresh_ack: false,
            sync_error_source: None,
            mouse_capture_on: true,
            terminal_selection: None,
            preselect: None,
            layout: LayoutCtx::new(),
            pending_reply: None,
            pending_review_request: None,
            review_choices: Vec::new(),
            pending_assignees_request: None,
            assignees_choices: Vec::new(),
            pending_labels_request: None,
            labels_choices: Vec::new(),
            pending_snooze_workspace: None,
            snooze_choices: Vec::new(),
            pending_removal_prompts: std::collections::VecDeque::new(),
            active_removal_prompt: None,
            pending_merge_prompts: std::collections::VecDeque::new(),
            active_merge_prompt: None,
            pending_adopt_source: None,
            adopt_choices: Vec::new(),
            start_agent_project_choices: Vec::new(),
            status: StatusCtx::new(),
            ui_defaults: lazybox_config::UiDefaults::default(),
            pr_details_fetched: std::collections::HashSet::new(),
            last_focused_session_key: None,
            pending_sidebar_context: None,
            action_key_overrides: std::collections::BTreeMap::new(),
            pending_action_confirm: None,
            pending_inspect_rows: Vec::new(),
            pending_inspect_target: None,
            pending_new_workspace_project: None,
            pending_focus_project_name: None,
            merge_follow_from: None,
            snippets: lazybox_config::Snippets::default(),
            snippet_choices: Vec::new(),
            auto_tour_pending: false,
            tips_enabled: false,
            tips_seen: Vec::new(),
            tip_shown_this_session: false,
            tips_armed_at: std::time::Instant::now(),
            scroll_inertia: None,
            modal_redraw_until: None,
        }
    }
}

/// Best-effort restore of the host terminal — disable raw mode,
/// leave the alt screen, drop mouse capture + bracketed paste +
/// kitty keyboard flags. Idempotent (each crossterm call no-ops
/// when the state isn't active). Called both on clean shutdown and
/// from the panic hook so a crash doesn't strand the user's
/// terminal in raw mode with mouse-tracking on, where every input
/// becomes escape sequences pasted into the prompt.
/// Provider ids that can enumerate scopes (orgs/repos). Passed to the
/// pure `SetupRunner` so it knows which providers get a scope-picking
/// step without holding the `ScopeSource`s themselves (those stay in
/// `setup.inputs` for the executor).
fn scope_provider_ids(
    sources: &std::sync::Arc<Vec<Box<dyn lazybox_core::ScopeSource>>>,
) -> std::collections::BTreeSet<String> {
    sources
        .iter()
        .map(|s| s.provider_id().to_string())
        .collect()
}

fn restore_terminal() {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags,
    };
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(
        out,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
    );
    let _ = disable_raw_mode();
    // Flush so the host terminal sees the resets before the
    // panic message (or shell prompt) takes over the screen.
    use std::io::Write;
    let _ = out.flush();
}

/// Install a panic hook that restores the terminal before falling
/// through to the default panic printer. Without this, a panic
/// during the TUI run leaves the host stuck in raw mode + the
/// alt screen, with the panic message painted on top of the still-
/// live mouse-tracking escape stream — the screenshot the user
/// just shared. Idempotent across multiple Model::new calls.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            prev(info);
        }));
    });
}

impl Model<CrosstermTerminalAdapter> {
    pub fn new(client: Client) -> anyhow::Result<Self> {
        install_panic_hook();
        let mut terminal = CrosstermTerminalAdapter::new()?;
        terminal.enable_raw_mode()?;
        terminal.enter_alternate_screen()?;
        // Mouse capture: clicks/drags drive splitter resize +
        // click-to-focus + lazybox-side text selection inside the
        // terminal pane (extracted from libghostty's grid, copied
        // via OSC 52). F8 / Alt-s toggles capture off if the user
        // wants the host's native selection (which spans across
        // lazybox's UI chrome and is uglier).
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture,);
        // Bracketed paste: the host terminal wraps Cmd-V'd text in
        // `ESC [ 200 ~ … ESC [ 201 ~` so we can tell "user pasted a
        // chunk" from "user typed N characters very fast." Without
        // it, every paste hits Claude / shell as a stream of
        // keystrokes — autocomplete fires mid-paste, the input
        // jumps around, etc. The `Event::Paste(text)` handler
        // below forwards the wrapped sequence to the PTY so the
        // inner program sees it as a single paste.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste,);
        // Focus reporting (DEC mode 1004): the host terminal emits
        // `FocusGained` / `FocusLost` as the user switches windows.
        // Lazybox tracks it so desktop notifications only fire while
        // lazybox is unfocused — a banner for what you're already
        // looking at is just noise. Terminals that don't support it
        // never send the events, so notifications keep firing there.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableFocusChange,);
        // Ask the host terminal to disambiguate modified Enter /
        // Tab / Backspace etc. via the kitty keyboard protocol.
        // Without this, most terminals collapse Shift-Enter into
        // the same byte sequence as Enter and lazybox can't tell
        // "submit" from "newline in input" — Claude Code's prompt
        // then ignores Shift-Enter the user pressed expecting a
        // newline. Terminals that don't support the protocol
        // silently ignore the request.
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
            ),
        );
        // Splash is mounted lazily by `start_setup_wizard`. Returning
        // users (with a persisted setup) boot straight to the panes.
        let mut model = Self::build(terminal, client);
        // Subscribe up-front for both first-run and returning users.
        // First-run gets an empty snapshot before the wizard finishes
        // (no polling has run yet) so nothing flickers in behind the
        // wizard. Subscribe is idempotent on the daemon side.
        let _ = model.client.send(IpcCommand::Subscribe);
        model.set_focus_attr();
        Ok(model)
    }
}

/// Headless constructor: builds the same orchestrator state without
/// touching raw mode / alternate screen / mouse capture, so tests
/// can drive `handle_pane_key` / `handle_daemon_event` against a
/// fake backend.
impl Model<tuirealm::terminal::TestTerminalAdapter> {
    pub fn new_for_test(
        client: Client,
        size: tuirealm::ratatui::layout::Size,
    ) -> anyhow::Result<Self> {
        let terminal = tuirealm::terminal::TestTerminalAdapter::new(size)
            .map_err(|e| anyhow::anyhow!("test adapter init: {e:?}"))?;
        Ok(Self::build(terminal, client))
    }
}

impl<T: TerminalAdapter> Model<T> {
    /// Pre-load a `--workspace` / `--session` target. Applied once
    /// the first daemon Snapshot lands.
    pub fn with_preselect(mut self, p: Preselect) -> Self {
        self.preselect = Some(p);
        self
    }

    /// Install the on-setup-complete hook before the main loop
    /// starts. `main.rs::run_embedded_realm` uses this to kick off
    /// the polling loop with the user's persisted selections.
    pub fn with_setup_complete_hook(mut self, hook: crate::realm::SetupCompleteHook) -> Self {
        self.setup.on_complete = Some(hook);
        self
    }

    /// Trigger the setup wizard. Called from `run_embedded_realm`
    /// when no persisted setup exists, AND from `reopen_setup` when
    /// the user wants to add a repo / agent / scope mid-session.
    /// Mounts the welcome splash; the runner consumes the next
    /// `Msg::SplashConfirmed` and unrolls into Providers / Agents /
    /// Filters / Scopes / Repos.
    pub fn start_setup_wizard(
        &mut self,
        report: crate::setup::SetupReport,
        sources: std::sync::Arc<Vec<Box<dyn lazybox_core::ScopeSource>>>,
    ) {
        let scope_providers = scope_provider_ids(&sources);
        self.setup.inputs = Some((report.clone(), sources));
        self.setup.runner = Some(crate::setup_flow::SetupRunner::new(report, scope_providers));
        self.mount_modal(Id::Splash, Splash::new());
    }

    /// Pre-populate the cached setup inputs without launching the
    /// wizard. `run_embedded_realm` calls this for returning users
    /// so the in-session `reopen_setup` path works without re-
    /// running detection.
    pub fn cache_setup_inputs(
        &mut self,
        report: crate::setup::SetupReport,
        sources: std::sync::Arc<Vec<Box<dyn lazybox_core::ScopeSource>>>,
    ) {
        self.setup.inputs = Some((report, sources));
    }

    /// Cache the user's existing PersistedSetup so partial flows
    /// from the Settings palette can pre-seed the wizard with
    /// current state instead of starting from defaults.
    pub fn cache_persisted_setup(&mut self, persisted: lazybox_core::PersistedSetup) {
        self.setup.persisted = Some(persisted);
        // Mirror narrowed-repo scopes into the sidebar so headers
        // appear at startup, before the first poll completes.
        self.refresh_subscribed_projects();
    }

    /// Hand in the editors detected at startup. The `E` shortcut
    /// reads from this list; empty list = footer notice on `E`.
    pub fn cache_editors(&mut self, editors: Vec<crate::editors::EditorTemplate>) {
        self.setup.editors = editors;
    }

    /// Apply `~/.lazybox/config.yaml::attention` +
    /// `ui.collapsed_repos` + `agent_shortcuts` to the sidebar at
    /// startup. Must be called before the first daemon Subscribe
    /// so the saved collapse state is in place when the Snapshot
    /// arrives.
    pub fn apply_sidebar_config(
        &mut self,
        attention: lazybox_config::AttentionConfig,
        collapsed_repos: std::collections::BTreeSet<String>,
        agent_shortcuts: std::collections::HashMap<char, String>,
        default_agent: Option<String>,
        display: &lazybox_config::DisplayConfig,
        ui: &lazybox_config::UiDefaults,
    ) {
        // Both panes consume the configured agent: sidebar `f` for
        // CI-fail, right pane `f` for selected comments.
        if let Some(agent) = default_agent.clone().filter(|s| !s.is_empty()) {
            self.right.set_default_agent(agent);
        }
        self.sidebar.apply_inner_config(
            attention,
            collapsed_repos,
            agent_shortcuts,
            default_agent,
            display,
            ui,
        );
        // Stash resolved defaults for model-level knobs (`q-q`
        // window, terminal-escape char, split step) that used to be
        // hardcoded consts.
        self.ui_defaults = ui.clone();
        self.right.apply_ui_defaults(ui);
    }

    /// Install the loaded snippet collection. Called from the
    /// startup path in `main.rs` after `Snippets::load_merged`. The
    /// terminal-pane `]<key>` latch reads from `self.snippets`
    /// directly, so this is the only handoff needed.
    pub fn apply_snippets(&mut self, snippets: lazybox_config::Snippets) {
        self.snippets = snippets;
    }

    /// Arm the auto-launch of the feature tour. `main.rs` passes
    /// `!ui.tour_seen` so a brand-new install (or one upgraded into
    /// the feature) gets the walkthrough once. `maybe_mount_tour`
    /// consumes the flag at the right moment (after setup, or at
    /// startup for returning users).
    pub fn set_auto_tour(&mut self, pending: bool) {
        self.auto_tour_pending = pending;
    }

    /// Launch the tour now if it's armed. Idempotent — mounting
    /// clears the flag, so this is safe to call from multiple boot
    /// paths (returning-user startup and first-run wizard finish).
    pub fn maybe_mount_tour(&mut self) {
        if self.auto_tour_pending {
            self.mount_tour();
        }
    }

    /// Mount the feature-tour overlay. Used by the `Shift-T` shortcut
    /// (always) and by `maybe_mount_tour` (when armed). Clears the
    /// auto-launch flag so it can't re-fire.
    pub(crate) fn mount_tour(&mut self) {
        use crate::realm::components::tour::Tour;
        self.auto_tour_pending = false;
        if matches!(self.modal_stack.last(), Some(Id::Tour)) {
            return;
        }
        self.mount_modal(Id::Tour, Tour::new());
    }

    /// Persist `ui.tour_seen = true` so the tour stops auto-launching.
    /// Best-effort: a write failure just means it may re-prompt next
    /// boot, which is harmless.
    fn mark_tour_seen(&mut self) {
        if let Err(e) = lazybox_config::Config::save_with(|c| c.ui.tour_seen = true) {
            tracing::warn!("save tour_seen failed: {e}");
        }
    }

    /// Seed the feature-tips state from `~/.lazybox/config.yaml` at
    /// startup. `enabled` is `ui.show_tips`; `seen` is `ui.tips_seen`.
    pub fn set_tips(&mut self, enabled: bool, seen: Vec<String>) {
        self.tips_enabled = enabled;
        self.tips_seen = seen;
    }

    /// Surface one progressive feature-discovery tip (#115) when the
    /// moment is quiet. Called once per run-loop iteration alongside
    /// the other tick helpers.
    ///
    /// Deliberately conservative so tips never nag: at most one tip
    /// per session, only while no modal is up and no notice already
    /// occupies the footer, and only after the footer has been idle
    /// for `TIP_IDLE_DELAY` (so a tip never races the first-run tour
    /// or the initial-poll spinner). The tip itself is a dim,
    /// auto-fading `Hint` — it never steals focus.
    ///
    /// The gating decision lives in `Self::pick_tip` (pure, no side
    /// effects); this wrapper records the tip as shown + persists it +
    /// flashes it.
    pub fn tick_tips(&mut self) {
        let Some(tip) = self.pick_tip() else {
            return;
        };
        self.tip_shown_this_session = true;
        self.tips_seen.push(tip.id.clone());
        self.persist_tip_seen(tip.id);
        self.flash_hint(tip.message);
    }

    /// Decide which feature tip (if any) should surface right now —
    /// all the "stay quiet" gating, with no side effects so it's
    /// unit-testable. Returns the resolved tip to show, or `None` when
    /// tips are off, one already showed this session, a modal / notice
    /// holds the footer, the idle delay hasn't elapsed, or no tip
    /// matches the current state.
    fn pick_tip(&self) -> Option<lazybox_tui_core::tips::ResolvedTip> {
        if !self.tips_enabled || self.tip_shown_this_session {
            return None;
        }
        if !self.modal_stack.is_empty() || self.status.notice.is_some() {
            return None;
        }
        if self.tips_armed_at.elapsed() < TIP_IDLE_DELAY {
            return None;
        }
        let ctx = lazybox_tui_core::tips::TipContext {
            agent_waiting: self.sidebar.has_asking_agent(),
            failing_ci: self.sidebar.has_failing_ci(),
            in_terminal: self.focus == PaneFocus::Terminals,
        };
        lazybox_tui_core::tips::next_tip(&ctx, &self.tips_seen, &self.action_key_overrides)
    }

    /// Append `id` to `ui.tips_seen` so the tip never resurfaces.
    /// Best-effort, mirroring `mark_tour_seen`: a write failure just
    /// means the tip may show once more next boot.
    fn persist_tip_seen(&self, id: String) {
        if let Err(e) = lazybox_config::Config::save_with(move |c| {
            if !c.ui.tips_seen.contains(&id) {
                c.ui.tips_seen.push(id.clone());
            }
        }) {
            tracing::warn!("save tips_seen failed: {e}");
        }
    }

    /// Mount the snippet picker with an initial filter (typically
    /// the single char the user typed after `]`). Picker rows are
    /// derived from the model's snippet collection; their keys are
    /// stashed in `self.snippet_choices` so `handle_choice_picked`
    /// can resolve a picked index back to a snippet via
    /// `self.snippets.get(...)` without re-running the filter.
    pub(crate) fn mount_snippet_picker(&mut self, initial_filter: String) {
        use crate::realm::components::snippet_picker::{PickerRow, SnippetPicker};
        if matches!(self.modal_stack.last(), Some(Id::SnippetPicker)) {
            return;
        }
        if self.snippets.is_empty() {
            // The user typed `]<key>` expecting a snippet and there
            // are none. Flash a hint pointing at the snippets file
            // so they know how to configure one.
            self.flash_info("no snippets configured — add some to ~/.lazybox/snippets.yaml");
            return;
        }
        let mut rows = Vec::with_capacity(self.snippets.len());
        let mut keys = Vec::with_capacity(self.snippets.len());
        for (k, v) in self.snippets.all() {
            rows.push(PickerRow::new(k, v));
            keys.push(k.to_string());
        }
        self.snippet_choices = keys;
        let picker = SnippetPicker::new(rows, initial_filter);
        self.mount_modal(Id::SnippetPicker, picker);
    }

    /// Apply catalog-driven action key overrides (`ui.action_keys`).
    /// Map of snake_case `ActionKind` names → key-spec strings;
    /// catalog lookups in `find_action_for_chord` consult this map
    /// first and fall back to the catalog default. See
    /// `lazybox_tui_core::action::ActionKind::name` for the key
    /// vocabulary.
    pub fn apply_action_key_overrides(
        &mut self,
        overrides: std::collections::BTreeMap<String, String>,
    ) {
        self.action_key_overrides = overrides;
    }

    /// Synthesize Project records for every scope the user is
    /// subscribed to (e.g. `github:owner/repo`) and merge them into
    /// the TUI's project table — so a freshly-added repo gets a
    /// sidebar header even before polling finds workspaces under it.
    ///
    /// Called at startup with the persisted state and again on every
    /// wizard Finish. The synthesized records are CACHE entries; if
    /// the daemon later broadcasts a `ProjectUpserted` for the same
    /// key (which happens on first workspace from that repo), the
    /// authoritative daemon-side record overwrites the synthetic one
    /// — identical shape, no visible change.
    fn refresh_subscribed_projects(&mut self) {
        let Some(p) = &self.setup.persisted else {
            return;
        };
        // The repo-level project keys the user is currently
        // subscribed to, mapped to their display name (`owner/repo`).
        let mut desired: std::collections::BTreeMap<lazybox_core::ProjectKey, String> =
            std::collections::BTreeMap::new();
        for set in p.selected_scopes.values() {
            for scope in set {
                // `provider:owner/repo` → ProjectKey::github(owner,
                // repo). Skip org-level entries (`provider:owner`
                // with no `/`) — those mean "whole org" and the per-
                // repo projects materialize as polling finds them.
                let Some((source, rest)) = scope.split_once(':') else {
                    continue;
                };
                if !rest.contains('/') {
                    continue;
                }
                let pk = match source {
                    "github" => {
                        let (owner, name) = rest.split_once('/').expect("contains '/' verified");
                        lazybox_core::ProjectKey::github(owner, name)
                    }
                    "linear" => lazybox_core::ProjectKey::linear(rest),
                    _ => continue,
                };
                desired.insert(pk, rest.to_string());
            }
        }

        let mut changed = false;
        // Add a placeholder header for each freshly-subscribed repo.
        for (pk, name) in &desired {
            if !self.projects.contains_key(pk) {
                self.projects.insert(
                    pk.clone(),
                    lazybox_core::Project::new(pk.clone(), name.clone(), chrono::Utc::now()),
                );
                self.synthesized_projects.insert(pk.clone());
                changed = true;
            }
        }
        // Drop placeholders for repos the user just unsubscribed. Only
        // remove keys WE synthesized — daemon-authoritative projects
        // are owned by `ProjectUpserted` / `ProjectRemoved` and must
        // survive a scope edit (a whole-org subscription surfaces repos
        // we never placed here, and "no scopes" means "all").
        let stale: Vec<lazybox_core::ProjectKey> = self
            .synthesized_projects
            .iter()
            .filter(|k| !desired.contains_key(*k))
            .cloned()
            .collect();
        for pk in stale {
            self.projects.remove(&pk);
            self.synthesized_projects.remove(&pk);
            changed = true;
        }
        if changed {
            self.sidebar.apply_projects(self.projects.clone());
        }
    }

    /// Send a command to the daemon, logging failures. Wraps the raw
    /// `client.send` so a dead channel (daemon restarted, socket
    /// closed) leaves a breadcrumb in `/tmp/lazybox.log` instead of
    /// silently vanishing. Most call sites genuinely don't care if
    /// the send fails (Subscribe is idempotent, terminal-Write loses
    /// keystrokes on a dead channel anyway) — but a silent log helps
    /// debug "I pressed X and nothing happened" after the fact.
    /// Damp trackpad-scroll inertia. macOS trackpad flicks emit
    /// ~20-50 wheel events over ~500ms — the OS inertia phase — each
    /// at full STEP. Without damping, a flick scrolls hundreds of
    /// rows past where the user wanted, and reversing mid-flick
    /// fights the queued events instead of cancelling them.
    ///
    /// A burst is a run of same-direction events arriving within
    /// `MOMENTUM_GAP` of each other — i.e. one uninterrupted OS
    /// momentum stream. Behaviour:
    /// - OS momentum tail (the run keeps coming at the ~16 ms frame
    ///   cadence): STEP decays — 5 → 3 at event 5, → 1 at event 8,
    ///   then events past `STOP_AT` are dropped entirely so the tail
    ///   stops the view instead of trickling for the full 1–2 s.
    /// - Anything that breaks the run — the first scroll, a direction
    ///   reversal, or a deliberate tick spaced wider than
    ///   `MOMENTUM_GAP` — starts a fresh burst at full STEP. This is
    ///   what keeps sustained manual scrolling from decaying into the
    ///   hard stop and getting silently dropped (#86), and what lets a
    ///   reverse-flick cancel queued inertia instead of being
    ///   swallowed.
    ///
    /// The returned isize is always the **magnitude** (positive) of
    /// the scroll step; sign is applied by the caller using
    /// `raw_up`. `0` means "drop this event" and only ever happens to
    /// the tail of a genuine momentum fling.
    pub(crate) fn dampen_scroll_step(&mut self, is_up: bool) -> isize {
        self.dampen_scroll_step_at(is_up, std::time::Instant::now())
    }

    /// Core of [`Self::dampen_scroll_step`] with the clock injected so
    /// tests can drive precise inter-event cadences without sleeping.
    fn dampen_scroll_step_at(&mut self, is_up: bool, now: std::time::Instant) -> isize {
        const STEP_INITIAL: isize = 5;
        const STEP_MID: isize = 3;
        const STEP_TAIL: isize = 1;
        /// Max gap between two events for the second to count as a
        /// continuation of the same OS momentum stream. Inertia events
        /// arrive at the ~16 ms frame cadence; 60 ms clears that with
        /// margin while staying well under the spacing of hand-driven
        /// ticks. A wider gap — a reversal, a deliberate tick, or the
        /// first scroll in a while — starts a fresh burst at full step,
        /// so sustained manual scrolling never accumulates toward the
        /// hard stop and gets dropped (#86).
        const MOMENTUM_GAP: std::time::Duration = std::time::Duration::from_millis(60);
        /// Within a burst, the step drops at these counts.
        const DECAY_AT: u32 = 5;
        const TAIL_AT: u32 = 8;
        /// Hard stop. Past this count the remaining momentum tail is
        /// dropped. At ~16 ms per event that's ~640 ms of inertia —
        /// long enough that a genuine flick travels a useful distance
        /// (the SGR path batches each event's full `scaled` step into
        /// one Write now, so the tail isn't N round trips of one line
        /// anymore), while a runaway OS momentum stream still gets
        /// cut instead of trickling for the full 1–2 s.
        const STOP_AT: u32 = 40;

        let new_dir: i8 = if is_up { -1 } else { 1 };

        // A scroll only continues the live momentum stream if it runs
        // in the same direction AND lands within the frame cadence.
        // `saturating_duration_since` is belts-and-braces against a
        // non-monotonic clock; with `std::time::Instant` it never
        // actually saturates.
        let continues = self.scroll_inertia.filter(|s| {
            s.dir == new_dir && now.saturating_duration_since(s.last_at) <= MOMENTUM_GAP
        });

        match continues {
            Some(mut s) => {
                s.count = s.count.saturating_add(1);
                s.last_at = now;
                let step = if s.count >= STOP_AT {
                    0
                } else if s.count >= TAIL_AT {
                    STEP_TAIL
                } else if s.count >= DECAY_AT {
                    STEP_MID
                } else {
                    STEP_INITIAL
                };
                self.scroll_inertia = Some(s);
                step
            }
            None => {
                self.scroll_inertia = Some(ScrollInertia {
                    dir: new_dir,
                    count: 1,
                    last_at: now,
                });
                STEP_INITIAL
            }
        }
    }

    fn send_cmd(&self, cmd: IpcCommand) {
        if let Err(e) = self.client.send(cmd) {
            tracing::warn!("ipc send failed: {e}");
        }
    }

    /// Set the footer notice + mark the screen dirty. Three
    /// shortcuts for the three severities the codebase uses most
    /// (`info`, `hint`, `error`) plus a generic `flash` for the
    /// rare `Retryable` / `Auth` cases.
    ///
    /// Pulled out because 30+ call sites open-coded
    /// `self.status.notice = Some(Notice::new(...)); self.redraw = true;`,
    /// and forgetting the `redraw = true` left the notice invisible
    /// until the next event triggered a render — a known footgun.
    pub fn flash_info(&mut self, msg: impl Into<String>) {
        self.flash(msg, crate::realm::components::footer::NoticeSeverity::Info);
    }

    pub fn flash_hint(&mut self, msg: impl Into<String>) {
        self.flash(msg, crate::realm::components::footer::NoticeSeverity::Hint);
    }

    /// Footer indicator for an over-budget run-loop iteration. Surfaces
    /// a stall live (warn-colored, auto-fading) so it's obvious without
    /// tailing the perf log. The caller gates this behind
    /// `LAZYBOX_PERF=1`; the watchdog rate-limits it to ≤1/s.
    pub fn flash_perf_stall(&mut self, elapsed: Duration, worst_phase: &str, worst: Duration) {
        self.flash(
            format!(
                "⚠ UI stall {}ms · {worst_phase} {}ms",
                elapsed.as_millis(),
                worst.as_millis()
            ),
            crate::realm::components::footer::NoticeSeverity::Retryable,
        );
    }

    pub fn flash_error(&mut self, msg: impl Into<String>) {
        self.flash(
            msg,
            crate::realm::components::footer::NoticeSeverity::Permanent,
        );
    }

    /// Like `flash_error`, but tags the notice as a provider "sync
    /// failed" banner owned by `source`, so the next successful
    /// `PollCompleted` from that same provider can clear it once sync
    /// recovers. See the `sync_error_source` field and
    /// [`Self::clear_sync_error_if_recovered`].
    pub fn flash_sync_error(&mut self, source: &str, msg: impl Into<String>) {
        self.flash_error(msg);
        // `flash` (called via `flash_error`) just reset the flag to
        // `None`; re-arm it *after* so the banner is attributed to the
        // provider that actually failed.
        self.sync_error_source = Some(source.to_string());
    }

    /// Clear the sticky "✗ sync failed" banner iff it's still on
    /// screen *and* it belongs to `source` — i.e. the provider that
    /// failed is the one that just recovered. A poll from any other
    /// provider leaves the banner untouched. Returns `true` if a
    /// banner was cleared (caller can skip a redundant redraw flag).
    pub fn clear_sync_error_if_recovered(&mut self, source: &str) -> bool {
        if self.sync_error_source.as_deref() == Some(source) {
            self.sync_error_source = None;
            self.status.notice = None;
            self.redraw = true;
            true
        } else {
            false
        }
    }

    pub fn flash(
        &mut self,
        msg: impl Into<String>,
        severity: crate::realm::components::footer::NoticeSeverity,
    ) {
        use crate::realm::components::footer::Notice;
        // Any fresh notice supersedes a sync-error banner, so the
        // "clear on recovery" tag only stays armed while the
        // sync-error notice is the one actually on screen.
        self.sync_error_source = None;
        self.status.notice = Some(Notice::new(msg, severity));
        self.redraw = true;
    }

    /// Mount a modal under `id` with the standard "subscribe to any
    /// event, always" subscription, push it onto the modal stack,
    /// activate it, and mark the screen dirty.
    ///
    /// Bundles the four-step ritual every `mount_*` helper repeats:
    /// `app.mount` → `modal_stack.push` → `app.active` → `redraw`.
    /// Forgetting any step has been a recurring source of "modal up
    /// but won't dismiss" or "modal mounted but invisible until next
    /// keypress" bugs.
    pub fn mount_modal<C>(&mut self, id: Id, component: C)
    where
        C: tuirealm::component::AppComponent<Msg, UserEvent> + 'static,
    {
        self.mount_modal_boxed(id, Box::new(component));
    }

    /// Same as [`Self::mount_modal`] but accepts an already-boxed
    /// component. Use this when the caller has a
    /// `Box<dyn AppComponent>` (e.g. setup-flow runners that
    /// dispatch on a polymorphic boxed step).
    pub fn mount_modal_boxed(
        &mut self,
        id: Id,
        component: Box<dyn tuirealm::component::AppComponent<Msg, UserEvent>>,
    ) {
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        let _ = self.app.mount(
            id.clone(),
            component,
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(id.clone());
        let _ = self.app.active(&id);
        self.redraw = true;
    }

    /// Forward a modal-bound event (key or paste) to the listener's
    /// `ChannelPort` and arm the redraw window. The listener thread
    /// delivers the event to the mounted modal on its own ~10ms
    /// cadence and the next run-loop `tick` picks up any resulting
    /// `Msg` — so this never blocks the dispatcher. The window exists
    /// because modal keys that mutate state without emitting a `Msg`
    /// (Confirm arrows, Input typing) leave nothing for the tick to
    /// observe; rendering across the short window guarantees the
    /// change is shown without a per-keystroke busy-wait.
    pub(crate) fn forward_modal_event(&mut self, ev: RealmEvent<UserEvent>) {
        let _ = self.modal_event_tx.send(ev);
        self.modal_redraw_until = Some(std::time::Instant::now() + MODAL_REDRAW_WINDOW);
    }

    /// Whether a forwarded modal event is still inside its redraw
    /// window. Clears the window once it has elapsed so an idle modal
    /// stops re-rendering. Called once per run-loop iteration.
    pub(crate) fn modal_redraw_pending(&mut self) -> bool {
        match self.modal_redraw_until {
            Some(deadline) if std::time::Instant::now() < deadline => true,
            Some(_) => {
                self.modal_redraw_until = None;
                false
            }
            None => false,
        }
    }

    /// Drain a handler's returned IPC commands into `send_cmd`.
    /// Used at the `update()` call sites so handlers can be
    /// unit-tested in isolation: tests construct a Model, call
    /// `handle_X`, and assert on the returned `Vec<IpcCommand>`
    /// without ever needing a real IPC client.
    fn dispatch_cmds(&self, cmds: Vec<IpcCommand>) {
        for cmd in cmds {
            self.send_cmd(cmd);
        }
    }

    /// Override the initial sidebar / right-top split percentages
    /// from `~/.lazybox/config.yaml::ui`. Each value is clamped to
    /// `[SPLIT_MIN, SPLIT_MAX]`. `None` keeps the default.
    pub fn with_splits(mut self, sidebar_pct: Option<u16>, right_top_pct: Option<u16>) -> Self {
        self.layout.apply_persisted(sidebar_pct, right_top_pct);
        self
    }

    /// Open the focused workspace's worktree in an editor. Bound to
    /// `E` from the sidebar. 1 detected editor → launch directly;
    /// 2+ → mount a Choice picker; 0 → footer notice with hint.
    /// If the workspace has no session yet (no worktree on disk),
    /// spawn a shell first — the daemon creates the worktree as a
    /// side-effect, and the editor launches once `TerminalSpawned`
    /// arrives.
    pub fn open_editor(&mut self) {
        let Some(workspace_key) = self.sidebar.selected_workspace_key().cloned() else {
            return;
        };
        if self.setup.editors.is_empty() {
            let path = lazybox_core::paths::config_yaml();
            self.flash_info(format!(
                "no editor detected — add one under `editors:` in {}",
                path.display(),
            ));
            return;
        }
        let worktree = self
            .sidebar
            .selected_workspace()
            .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()));
        // If there's no worktree yet, queue the editor launch and
        // ask the daemon to provision a session — `handle_daemon_event`
        // fires the editor on the matching `TerminalSpawned`.
        let Some(worktree) = worktree else {
            // Pick the editor up front (or remember the picker is
            // pending). Single editor → queue + spawn immediately.
            // Multiple → show the picker first, queue when picked.
            if self.setup.editors.len() == 1 {
                self.setup.pending_editor_launch =
                    Some((workspace_key.clone(), self.setup.editors[0].clone()));
                self.send_cmd(IpcCommand::Spawn {
                    session_key: workspace_key.clone(),
                    session_id: None,
                    kind: lazybox_ipc::TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                });
                self.flash_info(format!(
                    "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                    self.setup.editors[0].display
                ));
            } else {
                // Multi-editor: defer editor pick + record that the
                // dispatch needs to spawn first.
                self.setup.pending_editor_workspace = Some(workspace_key);
                self.mount_editor_picker();
            }
            return;
        };

        match self.setup.editors.len() {
            1 => {
                let editor = self.setup.editors[0].clone();
                self.launch_editor(&editor, &worktree);
            }
            _ => self.mount_editor_picker(),
        }
    }

    fn mount_editor_picker(&mut self) {
        use crate::realm::components::choice::Choice;
        let labels: Vec<String> = self
            .setup
            .editors
            .iter()
            .map(|e| e.display.clone())
            .collect();
        self.setup.editor_choices = self.setup.editors.clone();
        let modal = Choice::single("Open in which editor?", labels)
            .title("Open editor")
            .label(|s: &String| s.clone());
        self.mount_modal(Id::Editor, modal);
    }

    /// Route a [`crate::components::terminal_stack::ClickTarget`] produced by a right-click in the agent
    /// view to the right opener: URLs and `#N` / `owner/repo#N` issue
    /// references go to the system browser; file paths open in the
    /// configured editor (jumping to `line:col` when present).
    pub fn open_click_target(&mut self, target: crate::components::terminal_stack::ClickTarget) {
        use crate::components::terminal_stack::ClickTarget;
        match target {
            ClickTarget::Url(url) => self.open_external_url(&url),
            ClickTarget::Issue { repo, number } => self.open_issue_ref(repo, number),
            ClickTarget::Path { path, line, col } => self.open_path_in_editor(&path, line, col),
        }
    }

    /// Hand `url` to the platform browser launcher and surface the
    /// outcome in the footer.
    fn open_external_url(&mut self, url: &str) {
        match crate::editors::open_url(url, self.ui_defaults.browser.as_deref()) {
            Ok(()) => {
                tracing::info!(%url, "opened url from terminal");
                self.flash_hint(format!("opened {url}"));
            }
            Err(e) => {
                tracing::warn!(%url, "open_url failed: {e}");
                self.flash_error(format!("open failed: {e}"));
            }
        }
    }

    /// Resolve an issue reference to a GitHub URL and open it. A bare
    /// `#N` borrows the focused workspace's repo; `owner/repo#N`
    /// carries its own. GitHub redirects `/issues/N` to `/pull/N`
    /// when the number is a PR, so this one URL shape covers both.
    fn open_issue_ref(&mut self, repo: Option<String>, number: u64) {
        let repo = repo.or_else(|| self.focused_repo());
        let Some(repo) = repo else {
            self.flash_info(format!("no repo to resolve #{number}"));
            return;
        };
        if !repo.contains('/') {
            self.flash_info(format!("can't resolve #{number} — repo is '{repo}'"));
            return;
        }
        let url = format!("https://github.com/{repo}/issues/{number}");
        self.open_external_url(&url);
    }

    /// The `owner/repo` of the workspace whose terminals are focused,
    /// falling back to the sidebar selection. `None` when neither has
    /// a repo-bearing task (e.g. a from-scratch workspace).
    fn focused_repo(&self) -> Option<String> {
        let from_active = self
            .terminals
            .active_session()
            .and_then(|sk| self.sidebar.workspace_by_key(sk));
        from_active
            .or_else(|| self.sidebar.selected_workspace())
            .and_then(|w| w.primary_task())
            .and_then(|t| t.repo.clone())
    }

    /// The on-disk worktree of the workspace whose terminals are
    /// focused. Used to resolve `./` and bare-relative paths clicked
    /// in the transcript.
    fn focused_worktree(&self) -> Option<std::path::PathBuf> {
        self.terminals
            .active_session()
            .and_then(|sk| self.sidebar.workspace_by_key(sk))
            .or_else(|| self.sidebar.selected_workspace())
            .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()))
    }

    /// Expand a clicked path token to an absolute path: `~` → home,
    /// relative → joined onto the focused session's worktree (or left
    /// as-is when there's no worktree to anchor against).
    fn resolve_clicked_path(&self, raw: &str) -> std::path::PathBuf {
        use std::path::PathBuf;
        if raw == "~" {
            if let Some(home) = home_dir() {
                return home;
            }
        } else if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = home_dir() {
                return home.join(rest);
            }
        }
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            return p;
        }
        match self.focused_worktree() {
            Some(worktree) => worktree.join(raw),
            None => p,
        }
    }

    /// Open a clicked file path in the configured editor. With one
    /// detected editor we launch directly; with several we use the
    /// first (the workspace `E` picker remains the place to choose).
    /// A `line[:col]` suffix is forwarded so the editor jumps there.
    fn open_path_in_editor(&mut self, raw: &str, line: Option<u32>, col: Option<u32>) {
        if self.setup.editors.is_empty() {
            let path = lazybox_core::paths::config_yaml();
            self.flash_info(format!(
                "no editor detected — add one under `editors:` in {}",
                path.display(),
            ));
            return;
        }
        let resolved = self.resolve_clicked_path(raw);
        let editor = self.setup.editors[0].clone();
        match crate::editors::open_file(&editor, &resolved, line, col) {
            Ok(()) => {
                tracing::info!(path = %resolved.display(), editor = %editor.id, "opened file from terminal");
                let where_ = match (line, col) {
                    (Some(l), Some(c)) => format!("{}:{l}:{c}", resolved.display()),
                    (Some(l), None) => format!("{}:{l}", resolved.display()),
                    _ => resolved.display().to_string(),
                };
                self.flash_hint(format!("opened {where_} in {}", editor.display));
            }
            Err(e) => {
                tracing::warn!(path = %resolved.display(), "open_file failed: {e}");
                self.flash_error(format!("failed to open {raw}: {e}"));
            }
        }
    }

    /// Open the global snippets file (`<lazybox_home>/snippets.yaml`)
    /// in the configured editor, seeding a commented template the
    /// first time so a brand-new user lands on a working example
    /// rather than an empty buffer. Snippets are loaded once at
    /// startup, so the footer reminds the user to relaunch.
    fn open_snippets_file(&mut self) {
        if self.setup.editors.is_empty() {
            let path = lazybox_core::paths::config_yaml();
            self.flash_info(format!(
                "no editor detected — add one under `editors:` in {}",
                path.display(),
            ));
            return;
        }
        let path = lazybox_config::Snippets::default_global_path();
        if !path.exists() {
            if let Some(parent) = path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                self.flash_error(format!("couldn't create {}: {e}", parent.display()));
                return;
            }
            if let Err(e) = std::fs::write(&path, lazybox_config::Snippets::starter_template()) {
                self.flash_error(format!("couldn't seed {}: {e}", path.display()));
                return;
            }
        }
        let editor = self.setup.editors[0].clone();
        match crate::editors::open_file(&editor, &path, None, None) {
            Ok(()) => {
                tracing::info!(path = %path.display(), editor = %editor.id, "opened snippets file");
                self.flash_info(format!(
                    "editing {} in {} — relaunch lazybox to load changes",
                    path.display(),
                    editor.display
                ));
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "open snippets failed: {e}");
                self.flash_error(format!("failed to open {}: {e}", path.display()));
            }
        }
    }

    fn launch_editor(
        &mut self,
        editor: &crate::editors::EditorTemplate,
        worktree: &std::path::Path,
    ) {
        match crate::editors::launch(editor, worktree) {
            Ok(()) => {
                tracing::info!(
                    editor = %editor.id,
                    worktree = %worktree.display(),
                    "launched editor"
                );
                self.flash_info(format!(
                    "opened {} in {}",
                    worktree.display(),
                    editor.display
                ));
            }
            Err(e) => {
                tracing::warn!("editor launch failed: {e}");
                self.flash_error(format!("failed to launch {}: {e}", editor.display));
            }
        }
    }

    /// Open the in-session Settings palette. Builds a small picker
    /// with actions like "Add a repo (github)" / "Edit agents" /
    /// etc., scoped to the user's current providers. Falls back to
    /// the full wizard when there's no cached persisted setup yet
    /// (first-run path or `--test` mode).
    pub fn open_settings(&mut self) {
        use crate::realm::components::choice::Choice;

        if self.setup.runner.is_some() || matches!(self.modal_stack.last(), Some(Id::Setup)) {
            return;
        }

        let actions = self.build_settings_actions();
        if actions.is_empty() {
            // No persisted setup → fall back to the full wizard.
            self.reopen_setup();
            return;
        }
        let labels: Vec<String> = actions.iter().map(|a| a.label()).collect();
        self.setup.settings_actions = actions;
        let modal = Choice::single("What do you want to configure?", labels)
            .title("Settings")
            .label(|s: &String| s.clone());
        self.mount_modal(Id::Setup, modal);
    }

    /// Build the visible actions from the user's cached persisted
    /// setup. Per-provider actions only appear if the provider is
    /// enabled. Always includes the "full setup" escape hatch.
    fn build_settings_actions(&self) -> Vec<SettingsAction> {
        let Some(p) = &self.setup.persisted else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        for provider_id in &p.enabled_providers {
            let label = match provider_id.as_str() {
                "github" => "GitHub".to_string(),
                "linear" => "Linear".to_string(),
                other => other.to_string(),
            };
            actions.push(SettingsAction::EditScopes {
                provider_id: provider_id.clone(),
                label: label.clone(),
            });
            actions.push(SettingsAction::EditFilters {
                provider_id: provider_id.clone(),
                label,
            });
        }
        actions.push(SettingsAction::EditProviders);
        actions.push(SettingsAction::EditAgents);
        let skip_permissions = lazybox_config::Config::load()
            .map(|c| c.agent.skip_permissions)
            .unwrap_or(false);
        actions.push(SettingsAction::ToggleSkipPermissions {
            enabled: skip_permissions,
        });
        actions.push(SettingsAction::EditSnippets);
        actions.push(SettingsAction::InspectWorktrees);
        actions.push(SettingsAction::CleanWorktrees);
        actions.push(SettingsAction::FullSetup);
        actions
    }

    /// Dispatch a Settings palette pick. Builds a partial-entry
    /// SetupRunner pre-seeded with current persisted state, then
    /// mounts the first step. The on_setup_complete hook (installed
    /// by main.rs) handles persistence on Finish.
    pub fn dispatch_settings_action(&mut self, action: SettingsAction) {
        use crate::setup_flow::{PartialEntry, SetupRunner};
        // Config toggles write straight to YAML — they don't need the
        // cached detection inputs the wizard flows below depend on.
        if let SettingsAction::ToggleSkipPermissions { enabled } = action {
            let now = !enabled;
            match lazybox_config::Config::save_with(|c| c.agent.skip_permissions = now) {
                Ok(()) => self.flash_info(if now {
                    "skip permission prompts: on — new sessions launch with --dangerously-skip-permissions"
                } else {
                    "skip permission prompts: off — new sessions prompt before each tool use"
                }),
                Err(e) => self.flash_info(format!("couldn't save config: {e}")),
            }
            return;
        }
        // Editing snippets opens a file — no wizard runner, no cached
        // detection inputs needed.
        if matches!(action, SettingsAction::EditSnippets) {
            self.open_snippets_file();
            return;
        }
        let Some((report, sources)) = self.setup.inputs.clone() else {
            tracing::warn!("dispatch_settings_action: no cached inputs");
            return;
        };
        let entry = match action {
            SettingsAction::EditProviders => PartialEntry::EditProviders,
            SettingsAction::EditAgents => PartialEntry::EditAgents,
            SettingsAction::EditFilters { provider_id, .. } => {
                PartialEntry::EditFilter(provider_id)
            }
            SettingsAction::EditScopes { provider_id, .. } => PartialEntry::EditScopes(provider_id),
            SettingsAction::FullSetup => {
                self.start_setup_wizard(report, sources);
                return;
            }
            SettingsAction::CleanWorktrees => {
                self.mount_clean_worktrees_confirm();
                return;
            }
            SettingsAction::InspectWorktrees => {
                self.start_inspect_worktrees();
                return;
            }
            SettingsAction::ToggleSkipPermissions { .. } => return,
            // Handled by the early return above; listed for exhaustiveness.
            SettingsAction::EditSnippets => return,
        };
        // Pre-seed the accumulator from persisted state so partial
        // flows don't drop the user's other-provider config.
        let outcome = match self.setup.persisted.clone() {
            Some(p) => crate::setup_flow::persisted_to_outcome(p, report),
            None => crate::setup_flow::SetupOutcome::default_enabled(report),
        };
        let (runner, step) = SetupRunner::at_partial(outcome, scope_provider_ids(&sources), entry);
        self.setup.runner = Some(runner);
        let owned_runner = self.setup.runner.take().expect("just set");
        self.handle_runner_step(owned_runner, step);
    }

    /// Re-open the full setup wizard mid-session. Uses the cached
    /// `(report, sources)` populated at startup. No-op when the
    /// cache is empty (`--test`, `--connect`).
    pub fn reopen_setup(&mut self) {
        if self.setup.runner.is_some() {
            return;
        }
        let Some((report, sources)) = self.setup.inputs.clone() else {
            tracing::warn!("reopen_setup: no cached setup inputs");
            return;
        };
        self.start_setup_wizard(report, sources);
    }

    /// Mount the first-poll progress modal. Called from the
    /// on-setup-complete hook (and from the returning-user kickoff
    /// path) once polling has been kicked off on the daemon side.
    pub fn show_polling(&mut self, sources: Vec<String>) {
        self.status.show_polling(sources);
        self.redraw = true;
    }

    /// Restore terminal state (idempotent).
    pub fn shutdown(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture,);
        // Drop the bracketed-paste enable we set in `new`. Without
        // this the host terminal keeps wrapping pastes in
        // `ESC[200~…ESC[201~` even after lazybox exits — every
        // subsequent shell paste shows the literal markers.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste,);
        // Drop the focus-reporting request from `new` so the host
        // terminal stops emitting focus events into the user's shell.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableFocusChange,);
        // Drop the kitty keyboard protocol bits we pushed in `new`.
        // Skipping this would leak the request into the user's host
        // shell after lazybox exits — subsequent commands would still
        // receive disambiguated key events they didn't ask for.
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags,
        );
        let _ = self.terminal.leave_alternate_screen();
        let _ = self.terminal.disable_raw_mode();
    }
    /// Render the current frame.
    pub fn view(&mut self) {
        // Pull state out before the closure so the borrow checker is
        // happy — `terminal.draw` takes `&mut self.terminal` while we
        // also need `&mut self.app` etc. inside.
        let sidebar_pct = self.layout.sidebar_pct;
        let right_top_pct = self.layout.right_top_pct;
        let sidebar_user_resized = self.layout.sidebar_user_resized;
        // Pick the polling indicator for the footer:
        // - During the initial blocking modal, surface the rich
        //   first-poll spinner.
        // - Otherwise, surface the lightweight background indicator
        //   that fires on every subsequent cycle (so the user always
        //   knows whether lazybox is currently talking to GitHub).
        // Footer spinner priority: the blocking first-poll modal owns
        // it at startup; otherwise an in-flight spawn (the user's
        // just-pressed `w`/`c`/`s`) beats the ambient background-poll
        // indicator, since it's direct feedback for an action they're
        // waiting on. Background poll is the steady-state fallback.
        let polling_status: Option<(&'static str, String)> =
            if let Some(p) = self.status.polling.as_ref() {
                Some((p.spinner_glyph(), p.status_label()))
            } else if let Some(sp) = self.status.spawning.as_ref() {
                Some((sp.spinner_glyph(), sp.label()))
            } else {
                self.status
                    .bg_poll
                    .as_ref()
                    .map(|bg| (bg.spinner_glyph(), bg.label()))
            };
        // Resolve the focused pane's CONTEXTUAL bindings for the
        // footer hint bar. Contextual = state-aware short list
        // ("Shift-M merge" when the row is READY, "w fix CI" when
        // CI is failing, etc.) so the user always sees what's
        // actionable right now, not a generic alphabet. The full
        // keymap stays in `?` help.
        let keymap: Vec<crate::pane::Binding> = match self.focus {
            PaneFocus::Sidebar => self.sidebar.contextual_bindings(&self.action_key_overrides),
            PaneFocus::Right => self.right.contextual_bindings(&self.action_key_overrides),
            PaneFocus::Terminals => self
                .terminals
                .contextual_bindings(&self.action_key_overrides),
        };
        // Universal hints appended to every pane's footer (issue #100):
        // the orientation + escape shortcuts a lost first-time user
        // always needs in view. `quit` last so it's the rightmost,
        // most-findable hint.
        let globals: Vec<crate::pane::Binding> = {
            use lazybox_tui_core::action::{ActionDef, ActionKind};
            [ActionKind::OpenHelp, ActionKind::OpenTour, ActionKind::Quit]
                .into_iter()
                .map(|k| {
                    let def = ActionDef::for_kind(k);
                    crate::pane::Binding {
                        keys: def.effective_keys_display(&self.action_key_overrides),
                        label: std::borrow::Cow::Borrowed(def.label),
                    }
                })
                .collect()
        };
        let notice = self.status.notice.clone();
        // Snippet rows for the `]]` leader popup — built only while the
        // leader is armed so the steady-state render pays nothing.
        let snippet_leader_rows: Vec<(String, String)> = if self.terminal_leader_at.is_some() {
            self.snippets
                .all()
                .map(|(k, s)| (k.to_string(), s.description.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let mut captured_area = Rect::default();
        let _ = self.terminal.draw(|f| {
            let area = f.area();
            captured_area = area;
            let (pane_area, footer_area) = split_for_footer(area);
            let (left, right_top, right_bottom) =
                pane_areas(pane_area, sidebar_pct, right_top_pct, sidebar_user_resized);
            self.sidebar.view_in(left, f);
            self.right.view_in(right_top, f);
            self.terminals.view_in(right_bottom, f);

            // Selection highlight overlay. Painted AFTER the terminal
            // widget so the reverse-video pass lands on the just-
            // rendered cells. Bounded to `right_bottom` so a drag
            // that strayed into the sidebar / activity panes doesn't
            // leak the highlight across lazybox's pane chrome —
            // matches what the user expects from a per-pane
            // selection (compare to the host terminal's native
            // selection, which crosses panes).
            if let Some((start, end)) = self.terminal_selection {
                paint_selection(f.buffer_mut(), right_bottom, start, end);
            }

            // Footer: keymap + globals + polling status + notice.
            crate::realm::components::footer::render(
                f,
                footer_area,
                &keymap,
                &globals,
                polling_status.as_ref().map(|(s, l)| (*s, l.as_str())),
                notice.as_ref(),
            );

            // Which-key popup for an armed leader chord (#126). Drawn
            // above the footer but below any modal — in practice the
            // two never co-occur (arming doesn't mount a modal, and
            // the leader is consumed before a modal-mounting action
            // fires), so z-order is moot.
            if let Some(group) = self.leader.pending().copied() {
                crate::realm::components::which_key::render(f, area, group);
            }
            // Which-key popup for the armed terminal `]]` leader
            // (#205): lists the snippet keys reachable as `]]<key>`.
            if self.terminal_leader_at.is_some() {
                crate::realm::components::which_key::render_terminal_leader(
                    f,
                    area,
                    self.ui_defaults.terminal_escape_char,
                    &snippet_leader_rows,
                );
            }
            // After the first press of the `q q` quit chord, surface a
            // which-key style nudge so the chord is self-explanatory
            // rather than silently swallowing the keystroke (#100).
            if self.q_latch.is_armed() {
                use lazybox_tui_core::action::{ActionDef, ActionKind};
                let keys = ActionDef::for_kind(ActionKind::Quit)
                    .effective_keys_display(&self.action_key_overrides);
                crate::realm::components::which_key::render_quit_hint(f, area, &keys);
            }

            // Modal stack last (highest z-order).
            if let Some(top) = self.modal_stack.last() {
                self.app.view(top, f, area);
            }
        });
        self.layout.last_area = captured_area;
        // Resize commands are queued by the terminal stack's render
        // path each time a slot's rect changes. Drain + ship them so
        // libghostty's PTY learns the new size — without this,
        // typing into a freshly-shown terminal produces output that
        // falls off the bottom of the live grid.
        for cmd in self.terminals.drain_cmds() {
            self.send_cmd(cmd);
        }
    }

    /// Apply one `Msg`.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::SplashConfirmed => {
                // Splash is only mounted during the setup wizard now,
                // so this always advances into Providers. The
                // returning-user "subscribe + focus" path runs from
                // `Model::new` directly.
                if let Some(mut runner) = self.setup.runner.take() {
                    let step = runner.step_splash_confirmed();
                    self.handle_runner_step(runner, step);
                } else {
                    // Defensive: if Splash somehow ended up mounted
                    // without a runner, just pop it.
                    self.pop_modal();
                }
            }
            Msg::TourFinished => {
                self.mark_tour_seen();
                self.pop_modal();
            }
            Msg::AppClose => {
                self.quit = true;
            }
            Msg::SidebarCmds => {
                for cmd in self.sidebar.drain_cmds() {
                    self.send_cmd(cmd);
                }
            }
            Msg::RightCmds => {
                for cmd in self.right.drain_cmds() {
                    self.send_cmd(cmd);
                }
            }
            Msg::TerminalCmds => {
                for cmd in self.terminals.drain_cmds() {
                    self.send_cmd(cmd);
                }
            }
            Msg::ChoicePicked(picks) => {
                let cmds = self.handle_choice_picked(picks);
                self.dispatch_cmds(cmds);
            }
            Msg::ChoiceRefresh => {
                if let Some(mut runner) = self.setup.runner.take() {
                    let step = runner.step_choice_refresh();
                    self.handle_runner_step(runner, step);
                }
            }
            Msg::ChoiceBack => {
                if let Some(mut runner) = self.setup.runner.take() {
                    let step = runner.step_choice_back();
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::LoadingResolved(carrier) => {
                if let Some(mut runner) = self.setup.runner.take() {
                    // Recover the typed LoadResult once, here at the
                    // boundary — the runner stays free of `Box<dyn Any>`.
                    let step = match carrier
                        .take()
                        .and_then(crate::realm::setup_screen::downcast_load_result)
                    {
                        Some(result) => runner.step_loading_resolved(result),
                        None => {
                            tracing::warn!("LoadingResolved: payload was not a LoadResult");
                            crate::setup_flow::RunnerStep::Cancel
                        }
                    };
                    self.handle_runner_step(runner, step);
                } else {
                    self.pop_modal();
                }
            }
            Msg::ModalDismissed => {
                let cmds = self.handle_modal_dismissed();
                self.dispatch_cmds(cmds);
            }
            Msg::Confirmed(yes) => {
                let cmds = self.handle_confirmed(yes);
                self.dispatch_cmds(cmds);
            }
            Msg::TextareaSubmitted(body) => {
                let cmds = self.handle_textarea_submitted(body);
                self.dispatch_cmds(cmds);
            }
            Msg::InputSubmitted(text) => {
                let cmds = self.handle_input_submitted(text);
                self.dispatch_cmds(cmds);
            }
            // Polling outcomes — surface as footer notices, never
            // as full-screen modals. Permanent + auth errors are
            // sticky; retryable ones (which shouldn't reach here)
            // auto-fade in render.
            Msg::PollingError((source, kind, detail, message)) => {
                use crate::realm::components::footer::NoticeSeverity;
                tracing::warn!("polling error from {source} ({kind}): {message} — {detail}");
                let severity = match kind.as_str() {
                    "auth" => NoticeSeverity::Auth,
                    "retryable" => NoticeSeverity::Retryable,
                    _ => NoticeSeverity::Permanent,
                };
                self.flash(format!("{source}: {message}"), severity);
            }
            Msg::PollingTimeout => {
                tracing::info!("polling first-cycle timeout — modal dismissed");
            }
            Msg::PollingEmptyInbox(queries) => {
                tracing::info!("polling completed with empty inbox; queries seen: {queries:?}");
            }
        }
    }
}

/// The user's home directory, for expanding `~`-prefixed paths
/// clicked in the terminal. Honors `$HOME` (and `%USERPROFILE%` on
/// Windows); returns `None` when neither is set so callers fall back
/// to leaving the path unexpanded.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}
