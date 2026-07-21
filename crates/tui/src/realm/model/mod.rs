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
mod host_terminal;
mod inputs;
mod keys;
mod modals;
mod terminal_leader;
#[cfg(test)]
mod tests;

pub use helpers::{run_loop_with_model, run_with_client};
use host_terminal::HostTerminalGuard;
pub use host_terminal::restore_host_terminal;

/// Runtime-backed rows for the generated public keybinding reference.
/// Kept doc-hidden because this is build/test plumbing rather than a
/// stable library API.
#[doc(hidden)]
pub fn terminal_leader_reference_rows() -> Vec<(String, String)> {
    terminal_leader::LeaderCmd::reference_rows()
}

// Re-export helper free functions so sibling submodules
// (`keys.rs`, etc.) can keep their `super::foo` import shape after
// the helpers moved out of mod.rs.
pub(crate) use helpers::{
    emit_clipboard_copy, find_action_for_seq, find_action_for_stroke, key_event_to_stroke,
    paint_selection, rect_contains, seq_continuations, split_for_footer,
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
    /// "Ask Lazybox" help modal (#302), opened by pressing `?` on the
    /// `?` help panel. Typing fuzzy-searches the runtime catalog;
    /// Enter sends the text as a question to the headless help-agent
    /// run. Conversation state lives in `Model::help_convo` (shared
    /// `Arc`), so daemon-event handlers stream the answer in without
    /// remounting.
    HelpAsk,
    Error,
    /// Startup notice for a newer source commit or published release.
    Update,
    Polling,
    Reply,
    /// Single-line input prompt for naming a brand-new pre-PR
    /// workspace. Submit → `Command::CreateWorkspace { name }`.
    NewWorkspace,
    /// Single-line input prompt for naming a brand-new local
    /// Project. Submit → `Command::CreateProject { name }`.
    NewProject,
    /// Repo picker for the `x p` new-workspace flow. Lists the
    /// already-tracked repos plus a "create a new local project"
    /// escape hatch. Choices live in `new_workspace_repo_choices`;
    /// `Msg::ChoicePicked` either funnels into the new-workspace name
    /// input under the chosen repo, or mounts `NewProject`.
    NewWorkspaceRepo,
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
    /// Picker for the `x a` ("adopt") flow — pick the target
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
    /// Multi-select picker mounted on `g l` (`ManageLabels`).
    /// Lists the repository's full label set with the currently-
    /// applied labels pre-checked; submit → `Command::SetLabels`.
    /// Works on issues too — both PRs and issues implement GraphQL's
    /// `Labelable` interface.
    ManageLabels,
    /// Automation-policies menu mounted on `g p` (`ManagePolicies`,
    /// issue #363). Single-pick `Choice` listing every policy on the
    /// focused PR/issue with its on/off state; picking a row toggles
    /// that policy and re-opens the menu. Rows + the workspace key live
    /// in `policy_choices` / `pending_policy_workspace`;
    /// `Msg::ChoicePicked` resolves the index to a toggle command.
    PolicyPicker,
    /// Duration picker mounted on `z` (ToggleSnooze) when the
    /// workspace is NOT currently snoozed. Single-pick choice
    /// modal with several common snooze durations (1h, today,
    /// tomorrow, next week, 1 month, forever). The pending
    /// workspace key lives in `pending_snooze_workspace`;
    /// `Msg::ChoicePicked` reads it + the picked Duration and
    /// dispatches `Command::Snooze`.
    SnoozeDuration,
    /// Single-line URL input for the "Configure LLM gateway" settings
    /// action. Submit → write the global `agent.llm_gateway_url` to YAML
    /// (empty input clears it).
    LlmGatewayUrl,
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
    /// Snippet picker mounted from the terminal pane on `]]s<key>`.
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
    /// Notices log window (default `Shift-M`, #309). Scrollable view of
    /// recent footer notices built from `self.status.messages`; `c`
    /// clears the log, any other non-scroll key dismisses.
    Messages,
    /// Spinner + step checklist shown while a first spawn on a fresh
    /// workspace provisions its worktree. Mounted on the first
    /// `WorktreeProgress` daemon event (so an instant resume never
    /// flashes it), re-mounted from `worktree_progress` as steps land
    /// and the display walks them at a minimum dwell, dismissed once the
    /// matching `TerminalSpawned` has queued it AND every step has been
    /// shown (or Esc / a failed step the user acknowledges).
    WorktreeProgress,
    /// Fuzzy switcher over every workspace (`JumpToWorkspace`, default
    /// `` ` ``; from a terminal, `]]` then `` ` ``). The parallel
    /// session keys live in `jump_choices`; `Msg::ChoicePicked`
    /// resolves the picked index and lands the cursor via
    /// `focus_workspace_key`. See `realm::components::jump_picker`.
    JumpPicker,
    /// Theme picker (`t`, or the `,` Settings palette). Single-pick
    /// `Choice` over `theme::list()` with live preview on highlight:
    /// arrowing applies a palette at once, Enter keeps it and writes
    /// `ui.theme`, Esc restores the theme stashed in
    /// `theme_picker_prev`. Names live in `theme_choices`.
    ThemePicker,
    /// Read-only snippets browser (`]`, or the `,` Settings palette).
    /// Scrollable list of the merged snippet library — key, origin,
    /// description, body — so snippets are discoverable outside the
    /// `]]s<key>` terminal snippet leader (#237). `e` opens the YAML for editing
    /// (`Msg::OpenSnippetsFile`); any other non-scroll key dismisses.
    /// See `realm::components::snippet_browser`.
    SnippetBrowser,
    /// Snippet-pick step of the broadcast flow (`Shift-B` on a sidebar
    /// multi-select). Same `SnippetPicker` component as
    /// `Id::SnippetPicker`, but the pick doesn't send — it funnels into
    /// the `BroadcastText` compose step (`Ctrl-F` skips the snippet).
    /// Targets live in `pending_broadcast`.
    BroadcastSnippet,
    /// Compose step of the broadcast flow: a Textarea pre-filled with
    /// the picked snippet's body (custom text appends after it).
    /// Submit → one delivery per target in `pending_broadcast`.
    BroadcastText,
    /// Single-pick `Choice` over the enabled agents (`,` Settings →
    /// "Change default agent"), opened on the current default. Pick →
    /// persist `setup.default_agent` and update the panes live. Ids
    /// live in `default_agent_choices`.
    DefaultAgentPicker,
    /// Single-pick `Choice` over the just-picked default agent's model
    /// tiers (chained after `DefaultAgentPicker` when the agent
    /// declares any), opened on its current default tier. Pick →
    /// persist `agents.<id>.models.default` so bare `w` / `Shift-W` /
    /// auto-work spawns use it; per-spawn tier chords (`w S`) still
    /// override. Esc keeps the current tier. Aliases live in
    /// `default_model_choices`, the target agent in
    /// `default_model_agent`.
    DefaultModelPicker,
    /// Confirm-with-preview for an action the Ask Lazybox help agent
    /// proposed (#353) — `add_snippet` or `edit_config`. The pending
    /// intent lives in `pending_help_action`; `Msg::Confirmed(true)`
    /// applies it natively (write + hot-reload / persist + live-apply).
    /// Esc / No drops the stash and changes nothing.
    HelpActionConfirm,
    /// Single-pick `Choice` mounted when `w` ("work on this") lands on
    /// a workspace with SEVERAL distinct running agents (#418) —
    /// injecting must not silently guess between them. The listed
    /// agent ids + the spawn params to replay live in
    /// `pending_work_picker`; `Msg::ChoicePicked` resolves the index
    /// and fires the same work spawn `w` would have, targeted at the
    /// chosen agent.
    WorkAgentPicker,
}

impl Id {
    /// Whether a click *outside* this modal should close it (same as
    /// Esc) and let the click fall through to its normal action.
    ///
    /// True for the read-only / progress overlays that shouldn't trap
    /// the user — the worktree-provisioning checklist, sync status,
    /// help, and the snippet picker/browser. Deliberately false for the
    /// destructive confirms (archive, merged-workspace removal, orphan
    /// deletes, …): a stray click must never dismiss or trigger data
    /// loss, so those keep owning input until the user answers.
    pub(crate) fn dismissable_by_outside_click(&self) -> bool {
        matches!(
            self,
            Id::WorktreeProgress
                | Id::SyncStatus
                | Id::Messages
                | Id::Help
                | Id::HelpAsk
                | Id::SnippetPicker
                | Id::SnippetBrowser
        )
    }
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
    /// The issue closed. Same cleanup command as `Merged` (kill
    /// sessions, delete the worktree, drop the row) — a separate
    /// variant only so the confirm copy reads "closed" not "merged".
    Closed,
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

/// A validated `edit_config` edit (#353), derived by
/// `Model::validate_config_edit` from an allowlisted help-agent intent.
/// Carries everything the confirm preview and the apply step need, so
/// neither re-derives the mapping from key string to typed field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigEdit {
    /// Canonical dotted key, one of the allowlisted `&'static` paths —
    /// the apply step matches on it to pick the typed `save_with` field.
    key: &'static str,
    /// The validated value to persist (canonicalized, e.g. a theme
    /// name normalized to its exact registered spelling).
    value: String,
    /// Human summary for the confirm preview and the post-apply notice,
    /// e.g. `theme → Lazybox Light`.
    summary: String,
    /// True when the change only takes effect after a restart (the
    /// keymap preset is read once at startup).
    needs_restart: bool,
}

/// In-flight broadcast (`Shift-B`): the targets resolved from the
/// sidebar multi-select when the flow mounted — stashed, not re-read
/// at send time, so a daemon event that reshuffles the sidebar under
/// the modals can't change who gets the message — plus the snippet
/// picked in step one (`None` = free text only). Consumed on submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BroadcastDraft {
    pub(crate) targets: Vec<lazybox_core::SessionKey>,
    pub(crate) snippet_key: Option<String>,
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
    /// `?` pressed on the Shortcuts panel — return to Ask Lazybox.
    HelpAskOpen,
    /// `?` pressed at Ask Lazybox's empty prompt — swap to the compact
    /// all-shortcuts index.
    HelpIndexOpen,
    /// Question submitted from the `HelpAsk` modal. The modal stays
    /// mounted; the answer streams back into `Model::help_convo`.
    HelpAsked(String),
    /// `e` pressed in the snippets browser — close it and open the
    /// global snippets YAML in the user's editor (#237).
    OpenSnippetsFile,
    LoadingResolved(PayloadCarrier),
    /// Spinner heartbeat from the `WorktreeProgress` modal. Carries no
    /// data — its only job is to be a non-empty message so the run loop
    /// repaints the advancing spinner during the silent checkout.
    WorktreeProgressTick,
    PollingError((String, String, String, String)),
    PollingTimeout,
    PollingEmptyInbox(Vec<String>),
    ModalDismissed,
    /// `c` pressed in the messages window (#309) — wipe the notice
    /// history and re-render the (now empty) window.
    MessagesCleared,
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
    /// Focus mode (issue #156): when `true`, the sidebar and activity
    /// pane are hidden and the focused workspace's terminal expands to
    /// near-fullscreen behind a slim event header. Focus is pinned to
    /// `Terminals` while this is on; leaving the terminal via `]]`
    /// clears it. Toggled by `.` (sidebar) or `]]f` (terminal);
    /// `]]<digit>` jumps straight to a specific agent.
    focus_mode: bool,
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
    /// Leader-chord state (issues #126, #102). The first keystroke of a
    /// `Chord::Seq` (e.g. `g` for the github actions) arms it with that
    /// keystroke; the next key completes the sequence and fires its
    /// action through the unified `dispatch_action`. Which catalog
    /// entries continue a given prefix is a pure function of the
    /// catalog (`seq_continuations`) — no hardcoded group table.
    /// Drives the which-key popup in `view`. Operator-pending, not
    /// timed — see `LeaderLatch`.
    leader: crate::confirm_latch::LeaderLatch<lazybox_tui_core::action::KeyStroke>,
    /// Highlighted row in the armed catalog leader's which-key popup, or
    /// `None` when nothing is highlighted yet (the direct-letter default).
    /// Arrow / `j` / `k` set and move it; `Enter` fires the highlighted
    /// continuation. Reset whenever the leader (re)arms or disarms so the
    /// highlight never outlives its popup (#343).
    leader_highlight: Option<usize>,
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
    /// Set by [`Self::send_cmd`] when a command failed to reach the
    /// daemon (dead channel — the daemon exited or the socket closed).
    /// A `Cell` because `send_cmd` is `&self` (it's called from deep
    /// inside borrow-heavy paths); the run loop's per-iteration
    /// `tick_daemon_health` drains it into the one-shot disconnect
    /// notice. Without this, a dead daemon meant every keypress
    /// "succeeded" silently while nothing happened (#zombie-UI).
    cmd_send_failed: std::cell::Cell<bool>,
    /// A bounded remote-command queue refused one command because the socket
    /// writer is behind. Unlike a closed channel this is retryable and must
    /// not falsely brand the daemon disconnected.
    cmd_send_overloaded: std::cell::Cell<bool>,
    /// One-shot latch for the "daemon disconnected" Permanent notice —
    /// the disconnect is detected repeatedly (every failed send, every
    /// wake on the closed event channel), but the banner should be
    /// raised once, not re-flashed per keypress.
    daemon_disconnect_notified: bool,
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
    /// (see `terminal_leader_armed`) instead of forwarding to the PTY.
    escape_latch: crate::confirm_latch::DoubleTapLatch,
    /// Whether the `]]` leader is armed. Set when `]]` completes; the
    /// *next* key selects a binding — a snippet key opens the picker, a
    /// digit / `f` / `` ` `` jumps, a third escape char (`]]]`) leaves to
    /// the sidebar, and Esc cancels back to the terminal. Deliberately
    /// NOT timed (#252): a timed leave raced the user typing a snippet
    /// key, so browsing snippets could silently drop them to the sidebar.
    /// Cleared by the completing key, or on an abandonment signal (a
    /// mouse click, via `cancel_leader_chords`).
    terminal_leader_armed: bool,
    /// Highlighted row in the armed `]]` leader popup, or `None` for the
    /// direct-key default. `j` / `k` move it (arrows stay bound to tile /
    /// tab movement, #286); `Enter` fires the highlighted command. Reset
    /// on every (dis)arm of the leader (#343).
    terminal_leader_highlight: Option<usize>,
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
    /// Body of the most recently submitted reply, kept until the next
    /// reply is composed. If the daemon later reports the post failed
    /// (`ProviderError { source: "reply" }`), the composed text would
    /// otherwise be gone — the textarea was consumed on submit — so
    /// the failure handler records it into the messages log (Shift-M)
    /// where the user can recover it.
    last_reply_body: Option<String>,
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
    /// Stash for the `w` multi-agent chooser (`Id::WorkAgentPicker`,
    /// #418): the running agent ids listed (row order) plus the spawn
    /// params the pick replays through `push_work_spawn`. Cleared on
    /// submit / dismiss.
    pending_work_picker: Option<crate::realm::model::modals::PendingWorkPicker>,
    /// Workspace the `PolicyPicker` (`g p`, issue #363) is targeting.
    /// `Msg::ChoicePicked` reads it + `policy_choices` to turn the
    /// picked index into a toggle command. Cleared on dismiss.
    pending_policy_workspace: Option<lazybox_core::WorkspaceKey>,
    /// The toggle each policy-menu row maps to, in row order. Order
    /// MUST match the labels rendered in `mount_policy_picker`.
    policy_choices: Vec<crate::realm::model::modals::PolicyToggle>,
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
    /// Accumulated state behind the `Id::WorktreeProgress` modal, keyed
    /// to the spawn whose worktree is being provisioned. `Some` only
    /// while the checklist is up — created on the first
    /// `WorktreeProgress` event, cleared when the modal dismisses.
    worktree_progress: Option<crate::realm::components::worktree_progress::WorktreeProgressState>,
    /// Session whose worktree-provisioning checklist the user dismissed
    /// with Esc while the operation was still running. Later
    /// `WorktreeProgress` events for THIS session are absorbed silently
    /// instead of resurrecting the modal on top of whatever the user is
    /// typing; the marker clears when the op completes
    /// (`TerminalSpawned` / spawn failure) or a different session
    /// starts provisioning. A failed step still surfaces as a footer
    /// error so a dismissed checklist can't hide a broken provision.
    worktree_progress_dismissed: Option<lazybox_core::SessionKey>,
    /// Workspace key whose PR is being confirmed for merge by the
    /// `g m` Confirm modal. Set when the modal mounts, taken on
    /// `Msg::Confirmed` / `Msg::ModalDismissed`.
    /// Source workspace key the `x a` adopt picker is gathering
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
    /// Candidate repos for the active `x p` new-workspace picker,
    /// in row order. The picker shows one extra trailing row (the
    /// "create a new local project" escape hatch), so a pick index
    /// equal to this vec's length means "make a new project" rather
    /// than indexing it.
    new_workspace_repo_choices: Vec<lazybox_core::ProjectKey>,
    /// Transient UI status (polling spinner + footer notice). See
    /// `StatusCtx`.
    status: StatusCtx,
    /// Resolved values for the magic-number knobs that used to be
    /// module-level `const`s — read from `~/.lazybox/config.yaml::ui`,
    /// or `UiDefaults::default()` when unset / not loaded.
    ui_defaults: lazybox_config::UiDefaults,
    /// Auto-fix opt-out label names (`auto_fix.opt_out_labels`), used by
    /// the policies menu (issue #363) to reflect which labels currently
    /// opt a PR out of auto-fix. Display-only — the daemon enforces the
    /// authoritative set. Defaults to the standard set until config is
    /// applied.
    auto_fix_opt_out_labels: Vec<String>,
    /// Workspace keys for which we've already fired
    /// `Command::FetchPrDetails` this session — the lazy-fetch path
    /// that back-fills review-thread activity. Used to dedupe the
    /// trigger so a flicker of focus doesn't spam the daemon.
    /// Cleared when a workspace is removed (`Event::WorkspaceRemoved`)
    /// so a re-added workspace gets a fresh fetch.
    pr_details_fetched: std::collections::HashSet<lazybox_core::WorkspaceKey>,
    /// Workspace keys we've already auto-fired `Command::MergePr` for
    /// this arming. The persisted `auto_merge_on_green` flag is the
    /// arm; this transient set is the one-shot latch that stops us
    /// re-dispatching every poll while the merge is in flight. Cleared
    /// when the PR leaves the merge-ready state (so a fresh green
    /// re-fires after a failed race) and on `Event::WorkspaceRemoved`.
    auto_merge_fired: std::collections::HashSet<lazybox_core::WorkspaceKey>,
    /// Workspace keys whose PR GitHub confirmed as merged (via
    /// `Command::MergePr` → `Event::PrMerged`) but whose next poll hasn't
    /// caught up yet. Held Model-side — not in a single pane — because the
    /// MERGED state must show identically in the sidebar row AND the
    /// right-pane header, and `maybe_auto_merge` must not re-fire on a PR
    /// we already know is merged. Every incoming `WorkspaceUpserted` /
    /// `Snapshot` is patched through [`Self::apply_merge_latch`] before
    /// fan-out, so an interim poll still reporting `Open` can't flicker
    /// the row back. Cleared once a poll confirms the terminal state
    /// (Merged/Closed) or on `Event::WorkspaceRemoved`. The merge already
    /// succeeded, so holding MERGED is authoritative, never optimistic.
    merge_confirmed: std::collections::HashSet<lazybox_core::WorkspaceKey>,
    /// Last `SessionKey` we sent a `Command::FocusWorkspace` for.
    /// Single source of truth for "did the cursor leave the previous
    /// workspace?". `sync_panes` reads it after every key/mouse
    /// dispatch and emits a fresh `FocusWorkspace` when the selected
    /// workspace key has changed. Centralizing here means every
    /// cursor-mutating path (j/k, mouse click, programmatic
    /// preselect) feeds the daemon's round-robin scheduler without
    /// each call site needing its own emit hook.
    last_focused_session_key: Option<lazybox_core::SessionKey>,
    /// Set by a daemon-event handler that needs the panes re-projected
    /// from the (possibly moved) sidebar selection, flushed to a single
    /// `sync_panes` once the whole drain batch is handled. A merge burst
    /// (`TerminalsRebadged` → `WorkspaceRemoved` → `WorkspaceMerged`) or
    /// a multi-row poll arrives as several events in one drain; running
    /// `sync_panes` per event would clone the selected `Workspace` and
    /// re-emit `FocusWorkspace` for every intermediate cursor position,
    /// when only the batch's final selection matters. Coalescing collapses
    /// that to one projection per batch.
    needs_pane_sync: bool,
    /// Per-workspace memory of which pane the user last rested in,
    /// keyed by workspace. Re-selecting a workspace (sidebar click)
    /// restores this focus so typing lands where the user left off —
    /// without it a click always dropped focus on the sidebar, silently
    /// swallowing keystrokes meant for an agent terminal (#182). Snapshotted
    /// at input-event entry (the steady focus *before* the event mutates
    /// anything) so the click that moves focus to the sidebar never
    /// overwrites the terminal focus of the workspace being left.
    /// Session-scoped — not persisted across launches.
    workspace_focus: std::collections::HashMap<lazybox_core::SessionKey, PaneFocus>,
    /// Per-workspace manual override of the Activity pane's
    /// visibility, keyed by workspace. The pane auto-hides when a
    /// workspace has no activity worth showing (`Right::
    /// has_visible_content`); `ToggleActivityPane` (Shift-P) flips
    /// that and records the user's choice here so navigating away and
    /// back keeps it. Session-scoped — not persisted across launches.
    /// An entry's `bool` is the desired visibility (`true` = shown).
    activity_pane_overrides: std::collections::HashMap<lazybox_core::WorkspaceKey, bool>,
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
    /// snake_case `ActionKind` names (see `ActionKind::name`), or
    /// `spawn_agent.<id>` for a per-agent row; values are key-spec
    /// strings. Empty when the user hasn't configured `ui.action_keys`
    /// — catalog defaults apply.
    action_key_overrides: std::collections::BTreeMap<String, String>,
    /// Enabled agent ids, in catalog-display order. Drives the
    /// generated per-agent `SpawnAgent` rows in [`Self::catalog`].
    /// Defaults to the built-in `claude` / `codex` / `cursor`.
    agents: Vec<String>,
    /// Per-agent model-tier menus (`agents.<id>.models`, with built-in
    /// fallback), keyed by agent id. Drives the `w S` / `a S` tier
    /// chords in [`Self::catalog`] for the default work agent.
    agent_models: std::collections::BTreeMap<String, lazybox_core::AgentModels>,
    /// Runtime action catalog: the static rows plus one generated
    /// `SpawnAgent` row per enabled agent, with `ui.action_keys`
    /// overrides baked into each entry's chords. Rebuilt whenever the
    /// agents list or overrides change; consulted by keyboard
    /// dispatch, the which-key popup, and the help panel.
    catalog: Vec<lazybox_tui_core::action::CatalogEntry>,
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
    /// Action proposed by the Ask Lazybox help agent (#353), queued
    /// behind a `HelpActionConfirm` modal. Set when the agent's answer
    /// parses to an allowlisted intent, taken (and applied if Yes) by
    /// the `Msg::Confirmed` handler. None when no action confirm is up.
    pending_help_action: Option<lazybox_tui_core::help::HelpActionIntent>,
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
    /// Name of a project the user just submitted via x p. When
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
    /// Workspace a `w` ("work on this") spawn was issued on. Set by the
    /// dispatcher when it fires the `Command::Spawn` and consumed by the
    /// matching `TerminalSpawned` / `TerminalFocusRequested` to pin focus
    /// onto that workspace's new agent terminal — even if a slow
    /// first-time worktree provision let the user navigate away before
    /// the terminal landed.
    spawn_follow_to: Option<lazybox_core::SessionKey>,
    /// Terminal the next `sync_panes` should promote to the active tab.
    /// Set alongside [`Self::spawn_follow_to`] so `w` lands on the
    /// freshly-spawned agent rather than whatever tab the followed
    /// workspace last had focused.
    pending_focus_terminal: Option<lazybox_ipc::TerminalId>,
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
    /// Snippet keys sent, most-recent first (capped at
    /// `RECENT_SNIPPETS_MAX`). Passed to each picker as its "Recent"
    /// group so a repeated snippet is one `]]s` + `Enter` away (#252).
    /// Persisted to the state DB via `recent_snippets_store` so the
    /// group survives a restart (#311).
    pub(crate) recent_snippets: Vec<String>,
    /// State-DB handle used to persist `recent_snippets` across
    /// restarts (#311). Set by `seed_recent_snippets` on the embedded
    /// boot path; `None` on the `--connect` path (which loads no
    /// snippets and owns no local store), where MRU stays session-only.
    recent_snippets_store: Option<std::sync::Arc<dyn lazybox_store::Store>>,
    /// Persistence retained only while the startup update modal is open.
    update_store: Option<std::sync::Arc<dyn lazybox_store::Store>>,
    pending_update_target: Option<String>,
    update_dismissal_tx: mpsc::Sender<Result<(), String>>,
    update_dismissal_rx: mpsc::Receiver<Result<(), String>>,
    update_dismissals_pending: usize,
    /// Active broadcast flow (`Shift-B`), if any. Set when the flow
    /// mounts, threaded through the snippet-pick step, consumed by the
    /// compose submit (or dropped on Esc). See [`BroadcastDraft`].
    pub(crate) pending_broadcast: Option<BroadcastDraft>,
    /// Session keys backing the active `JumpPicker`, in the same order
    /// as its rows — `Msg::ChoicePicked(idx)` resolves to a key here.
    /// Cleared on mount/unmount.
    pub(crate) jump_choices: Vec<lazybox_core::SessionKey>,
    /// Theme names backing the active `ThemePicker`, in the same order
    /// as `theme::list()` — `Msg::ChoicePicked(idx)` resolves the name
    /// here to persist. Cleared on mount/unmount.
    pub(crate) theme_choices: Vec<String>,
    /// Theme name active when the picker opened. Live preview mutates
    /// the global theme as the cursor moves; Esc restores this so a
    /// cancelled picker leaves the palette untouched. `None` while no
    /// picker is open.
    pub(crate) theme_picker_prev: Option<String>,
    /// Help-assistant conversation (#302), shared with a mounted
    /// `HelpAsk` modal via `Arc` so the daemon-event handlers can
    /// stream an answer into it without remounting (which would drop
    /// the user's in-flight typing). Persists across modal open/close
    /// — the help run stays alive for the app's lifetime so follow-up
    /// questions reuse the prompt-cached context.
    pub(crate) help_convo: crate::realm::components::help_ask::SharedHelpConvo,
    /// Run id of the live help-agent run, captured from the
    /// `AgentRunStarted` carrying the help sentinel session key.
    /// `None` before the first question (the run starts lazily) and
    /// again after `AgentRunFinished` — the next question then starts
    /// a fresh run (with fresh context).
    help_run: Option<lazybox_ipc::AgentRunId>,
    /// True between dispatching `StartAgentRun` and its
    /// `AgentRunStarted` landing. Questions submitted in that window
    /// queue in `help_pending_questions` rather than double-starting
    /// the run.
    help_run_starting: bool,
    help_pending_questions: Vec<String>,
    /// Agent ids backing the active `DefaultAgentPicker`, in row order —
    /// `Msg::ChoicePicked(idx)` resolves the id here to persist. Cleared
    /// on mount/unmount.
    pub(crate) default_agent_choices: Vec<String>,
    /// Tier aliases backing the active `DefaultModelPicker`, in row
    /// order — `None` is the "agent default" row (no pinned tier).
    /// Cleared on mount/unmount.
    pub(crate) default_model_choices: Vec<Option<String>>,
    /// Agent id the active `DefaultModelPicker` persists against —
    /// stashed at mount so a pick can't land on a drifted default.
    pub(crate) default_model_agent: Option<String>,
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
    /// RAII owner of the host-terminal modes (#211). `Some` only for the
    /// real crossterm `Model::new`; dropping it restores raw mode / alt
    /// screen / mouse / paste / focus / Kitty keyboard on every exit
    /// path. `None` for headless test models, which never touch the
    /// host terminal. See `host_terminal`.
    term_guard: Option<HostTerminalGuard>,
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

use crate::realm::layout::{LayoutCtx, apply_activity_visibility, focus_mode_areas, pane_areas};
use crate::realm::setup_ctx::{SettingsAction, SetupCtx};
use crate::realm::status_ctx::StatusCtx;

/// How long the run loop keeps re-rendering after a modal-bound key is
/// forwarded to the listener channel. Generous multiple of the 10ms
/// port-poll cadence plus a few render frames, so the asynchronously-
/// delivered key is always reflected on screen even when it produces
/// no `Msg`. See `Model::forward_modal_event`.
const MODAL_REDRAW_WINDOW: Duration = Duration::from_millis(120);

/// How many recently-used snippets the picker's "Recent" group holds
/// (#252). Small enough to stay a shortcut list, not a second library —
/// the group is a fast lane for the handful of snippets in active use.
const RECENT_SNIPPETS_MAX: usize = 5;

/// State-DB key under which the recent-snippets MRU is persisted
/// (#311). A plain kv entry — runtime state, like read/unread and
/// snooze — not user-authored `snippets.yaml` content.
const RECENT_SNIPPETS_KV_KEY: &str = "recent_snippets";

const DISMISSED_UPDATE_KV_PREFIX: &str = "dismissed_update_target:";

fn dismissed_update_key(target: &str) -> String {
    format!("{DISMISSED_UPDATE_KV_PREFIX}{target}")
}

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
// sourced from `~/.lazybox/config.yaml::terminal.escape_char`
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
        let (update_dismissal_tx, update_dismissal_rx) = mpsc::channel();
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
            focus_mode: false,
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
            leader_highlight: None,
            escape_latch: crate::confirm_latch::DoubleTapLatch::new(),
            terminal_leader_armed: false,
            terminal_leader_highlight: None,
            last_click: None,
            terminal_user_typed_since_focus: false,
            pending_refresh_ack: false,
            sync_error_source: None,
            cmd_send_failed: std::cell::Cell::new(false),
            cmd_send_overloaded: std::cell::Cell::new(false),
            daemon_disconnect_notified: false,
            mouse_capture_on: true,
            terminal_selection: None,
            preselect: None,
            layout: LayoutCtx::new(),
            pending_reply: None,
            last_reply_body: None,
            pending_review_request: None,
            review_choices: Vec::new(),
            pending_assignees_request: None,
            assignees_choices: Vec::new(),
            pending_labels_request: None,
            labels_choices: Vec::new(),
            pending_snooze_workspace: None,
            snooze_choices: Vec::new(),
            pending_work_picker: None,
            pending_policy_workspace: None,
            policy_choices: Vec::new(),
            pending_removal_prompts: std::collections::VecDeque::new(),
            active_removal_prompt: None,
            pending_merge_prompts: std::collections::VecDeque::new(),
            active_merge_prompt: None,
            worktree_progress: None,
            worktree_progress_dismissed: None,
            pending_adopt_source: None,
            adopt_choices: Vec::new(),
            start_agent_project_choices: Vec::new(),
            new_workspace_repo_choices: Vec::new(),
            status: StatusCtx::new(),
            ui_defaults: lazybox_config::UiDefaults::default(),
            auto_fix_opt_out_labels: lazybox_core::AutoFixSettings::default().opt_out_labels,
            pr_details_fetched: std::collections::HashSet::new(),
            auto_merge_fired: std::collections::HashSet::new(),
            merge_confirmed: std::collections::HashSet::new(),
            last_focused_session_key: None,
            needs_pane_sync: false,
            workspace_focus: std::collections::HashMap::new(),
            activity_pane_overrides: std::collections::HashMap::new(),
            pending_sidebar_context: None,
            action_key_overrides: std::collections::BTreeMap::new(),
            agent_models: std::collections::BTreeMap::new(),
            // Built-in agents + their `a c` / `a x` / `a u` convention.
            // The host overrides this from `setup.agents` via `set_agents`.
            agents: ["claude", "codex", "cursor"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            catalog: lazybox_tui_core::action::ActionDef::catalog(
                &[
                    "claude".to_string(),
                    "codex".to_string(),
                    "cursor".to_string(),
                ],
                &std::collections::BTreeMap::new(),
            ),
            pending_action_confirm: None,
            pending_help_action: None,
            pending_inspect_rows: Vec::new(),
            pending_inspect_target: None,
            pending_new_workspace_project: None,
            pending_focus_project_name: None,
            merge_follow_from: None,
            spawn_follow_to: None,
            pending_focus_terminal: None,
            snippets: lazybox_config::Snippets::default(),
            snippet_choices: Vec::new(),
            recent_snippets: Vec::new(),
            recent_snippets_store: None,
            update_store: None,
            pending_update_target: None,
            update_dismissal_tx,
            update_dismissal_rx,
            update_dismissals_pending: 0,
            pending_broadcast: None,
            jump_choices: Vec::new(),
            theme_choices: Vec::new(),
            theme_picker_prev: None,
            help_convo: Default::default(),
            help_run: None,
            help_run_starting: false,
            help_pending_questions: Vec::new(),
            default_agent_choices: Vec::new(),
            default_model_choices: Vec::new(),
            default_model_agent: None,
            auto_tour_pending: false,
            tips_enabled: false,
            tips_seen: Vec::new(),
            tip_shown_this_session: false,
            tips_armed_at: std::time::Instant::now(),
            scroll_inertia: None,
            modal_redraw_until: None,
            term_guard: None,
        }
    }
}

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

/// Install a panic hook that restores the terminal before falling
/// through to the default panic printer. Without this, a panic
/// during the TUI run leaves the host stuck in raw mode + the
/// alt screen, with the panic message painted on top of the still-
/// live mouse-tracking escape stream — the screenshot the user
/// just shared. The `HostTerminalGuard`'s `Drop` also restores on
/// unwind, but the hook runs *before* unwinding so the reset reaches
/// the host ahead of the panic message; `restore_host_terminal` is
/// once-only, so the later guard drop is a no-op. Idempotent across
/// multiple Model::new calls.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_host_terminal();
            prev(info);
        }));
    });
}

impl Model<CrosstermTerminalAdapter> {
    pub fn new(client: Client) -> anyhow::Result<Self> {
        install_panic_hook();
        let terminal = CrosstermTerminalAdapter::new()?;
        // Enable raw mode, the alt screen, mouse capture, bracketed
        // paste, focus reporting and the Kitty keyboard protocol. The
        // guard owns the whole set: dropping it (clean exit, error, or
        // panic) runs the one symmetric teardown, so the host shell is
        // never stranded in Kitty keyboard mode (#211). See
        // `host_terminal`.
        let term_guard = HostTerminalGuard::new();
        // Splash is mounted lazily by `start_setup_wizard`. Returning
        // users (with a persisted setup) boot straight to the panes.
        let mut model = Self::build(terminal, client);
        model.term_guard = Some(term_guard);
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
    /// `ui.collapsed_repos` to the sidebar at startup. Must be called
    /// before the first daemon Subscribe so the saved collapse state
    /// is in place when the Snapshot arrives. Per-agent spawn keys are
    /// no longer wired here — they're catalog rows; see `set_agents`.
    pub fn apply_sidebar_config(
        &mut self,
        attention: lazybox_config::AttentionConfig,
        collapsed_repos: std::collections::BTreeSet<String>,
        default_agent: Option<String>,
        display: &lazybox_config::DisplayConfig,
        ui: &lazybox_config::UiDefaults,
    ) {
        // Both panes consume the configured agent: sidebar `f` for
        // CI-fail, right pane `f` for selected comments.
        if let Some(agent) = default_agent.clone().filter(|s| !s.is_empty()) {
            self.right.set_default_agent(agent);
        }
        self.sidebar
            .apply_inner_config(attention, collapsed_repos, default_agent, display);
        self.sidebar.set_keep_awake(ui.keep_awake);
        // Stash resolved defaults for model-level knobs (`q-q`
        // window, terminal-escape char, split step) that used to be
        // hardcoded consts.
        self.ui_defaults = ui.clone();
        self.right.apply_ui_defaults(ui);
        self.terminals.apply_ui_defaults(ui);
    }

    /// Install the loaded snippet collection. Called from the
    /// startup path in `main.rs` after `Snippets::load_merged`. The
    /// terminal-pane `]]s<key>` flow reads from `self.snippets`
    /// directly, so this is the only handoff needed.
    pub fn apply_snippets(&mut self, snippets: lazybox_config::Snippets) {
        self.snippets = snippets;
    }

    /// Install the configured auto-fix opt-out label set so the policies
    /// menu (`g p`, issue #363) reflects which labels opt a PR out.
    /// Display-only — the daemon enforces the authoritative set.
    pub fn apply_auto_fix_opt_out_labels(&mut self, labels: Vec<String>) {
        self.auto_fix_opt_out_labels = labels;
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
    /// the snippet key prefix typed after `]]s`). Picker rows are
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
            // The user typed `]]s<key>` expecting a snippet and there
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
        let picker =
            SnippetPicker::new(rows, initial_filter).with_recent(self.recent_snippets.clone());
        self.mount_modal(Id::SnippetPicker, picker);
    }

    /// Kick off the broadcast flow (`Shift-B`): resolve the sidebar's
    /// multi-selected workspaces into a target list and mount the
    /// snippet-pick step (skipped straight to compose when the snippet
    /// library is empty). No selection → a footer nudge instead.
    pub(crate) fn mount_broadcast_picker(&mut self) {
        use crate::realm::components::snippet_picker::{PickerRow, SnippetPicker};
        let targets = self.sidebar.selected_broadcast_keys();
        if targets.is_empty() {
            self.flash_info("nothing selected — mark workspaces with v first");
            return;
        }
        self.pending_broadcast = Some(BroadcastDraft {
            targets,
            snippet_key: None,
        });
        if self.snippets.is_empty() {
            self.mount_broadcast_textarea(None);
            return;
        }
        let mut rows = Vec::with_capacity(self.snippets.len());
        let mut keys = Vec::with_capacity(self.snippets.len());
        for (k, v) in self.snippets.all() {
            rows.push(PickerRow::new(k, v));
            keys.push(k.to_string());
        }
        self.snippet_choices = keys;
        let picker = SnippetPicker::new(rows, String::new())
            .with_recent(self.recent_snippets.clone())
            .with_title(self.broadcast_header())
            .with_free_text_option();
        self.mount_modal(Id::BroadcastSnippet, picker);
    }

    /// Mount the broadcast compose step: a Textarea whose header names
    /// every target ("you selected: …") and whose buffer starts as the
    /// picked snippet's body (custom text appends after it) — or empty
    /// for a free-text-only send. Submit fans out one delivery per
    /// target (`dispatch_broadcast`).
    pub(crate) fn mount_broadcast_textarea(&mut self, snippet_body: Option<String>) {
        use crate::realm::components::textarea::Textarea;
        let Some(draft) = &self.pending_broadcast else {
            return;
        };
        let n = draft.targets.len();
        let title = format!(
            "Broadcast to {n} workspace{}",
            if n == 1 { "" } else { "s" }
        );
        let mut modal = Textarea::new(title).with_header(self.broadcast_header());
        if let Some(body) = snippet_body {
            // Trailing blank line so appended custom text starts on its
            // own line; trimmed back off at send time if unused.
            modal = modal.with_body(format!("{}\n\n", body.trim_end()));
        }
        self.mount_modal(Id::BroadcastText, modal);
    }

    /// "Broadcast to N: a, b, c" — the target recap shown on both
    /// broadcast modals so what's about to be hit is always visible.
    fn broadcast_header(&self) -> String {
        let Some(draft) = &self.pending_broadcast else {
            return String::new();
        };
        let names: Vec<String> = draft
            .targets
            .iter()
            .map(|k| {
                self.sidebar
                    .workspace_by_key(k)
                    .map(|w| w.name.clone())
                    .unwrap_or_else(|| k.to_string())
            })
            .collect();
        format!("Broadcast to {}: {}", names.len(), names.join(", "))
    }

    /// Record a snippet key as just-used: move it to the front of the
    /// MRU list (`recent_snippets`), de-duplicating and capping the
    /// list, then persist it. Drives the picker's "Recent" group (#252)
    /// and keeps it across restarts (#311).
    pub(crate) fn record_recent_snippet(&mut self, key: String) {
        self.recent_snippets.retain(|k| k != &key);
        self.recent_snippets.insert(0, key);
        self.recent_snippets.truncate(RECENT_SNIPPETS_MAX);
        self.persist_recent_snippets();
    }

    /// Best-effort write of `recent_snippets` to the state DB. A write
    /// failure just means the Recent group won't survive this quit —
    /// non-fatal, like `persist_tip_seen`.
    fn persist_recent_snippets(&self) {
        let Some(store) = &self.recent_snippets_store else {
            return;
        };
        let json = match serde_json::to_string(&self.recent_snippets) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("serialize recent_snippets failed: {e}");
                return;
            }
        };
        if let Err(e) = store.set_kv(RECENT_SNIPPETS_KV_KEY, &json) {
            tracing::warn!("persist recent_snippets failed: {e}");
        }
    }

    /// Install the snippet catalog and then restore the persisted
    /// "Recent" MRU against it, in that order (#311). This is the boot
    /// entry point `main.rs` uses: bundling the two steps makes the
    /// ordering un-splittable, so the prune in `seed_recent_snippets`
    /// can never run before `self.snippets` is populated (which would
    /// silently wipe a valid MRU). Keep the two methods it calls out of
    /// the startup path — call this instead.
    pub fn apply_snippets_and_seed_recent(
        &mut self,
        snippets: lazybox_config::Snippets,
        store: std::sync::Arc<dyn lazybox_store::Store>,
    ) {
        self.apply_snippets(snippets);
        self.seed_recent_snippets(store);
    }

    /// Seed `recent_snippets` from the state DB and retain the store
    /// handle for future writes (#311). MUST run *after* `apply_snippets`
    /// — it prunes keys no longer in the loaded catalog (a renamed /
    /// deleted snippet) so they don't sit in the MRU consuming a slot
    /// they can never render into; with an unpopulated catalog it would
    /// instead wipe every key. Without the prune a small rotating set of
    /// live keys never evicts the dead ones, permanently shrinking the
    /// visible Recent group. The startup path enforces the ordering via
    /// `apply_snippets_and_seed_recent`; call that, not this, outside of
    /// tests. A missing / empty / unparseable value yields an empty list
    /// — non-fatal, MRU just starts fresh. The `RECENT_SNIPPETS_MAX` cap
    /// is re-applied here in case the stored list predates a smaller cap.
    pub fn seed_recent_snippets(&mut self, store: std::sync::Arc<dyn lazybox_store::Store>) {
        // Only true when we loaded a value AND the prune/cap actually
        // shortened it — gates the flush-back below so a read failure,
        // an unparseable value, or an already-clean list never triggers
        // a write (which would clobber recoverable bytes with `[]`).
        let mut shortened = false;
        match store.get_kv(RECENT_SNIPPETS_KV_KEY) {
            Ok(Some(json)) => match serde_json::from_str::<Vec<String>>(&json) {
                Ok(mut recent) => {
                    let before = recent.len();
                    recent.retain(|k| self.snippets.get(k).is_some());
                    recent.truncate(RECENT_SNIPPETS_MAX);
                    shortened = recent.len() != before;
                    self.recent_snippets = recent;
                }
                Err(e) => tracing::warn!("parse recent_snippets failed: {e}"),
            },
            Ok(None) => {}
            Err(e) => tracing::warn!("read recent_snippets failed: {e}"),
        }
        self.recent_snippets_store = Some(store);
        // Flush the pruned list back so dead keys don't linger in the DB
        // across future seeds — but only when the load succeeded and
        // dropped something, so we never overwrite a valid or corrupt
        // stored value with an empty list. Best-effort, like every MRU
        // write.
        if shortened {
            self.persist_recent_snippets();
        }
    }

    /// Mount the read-only snippets browser (`]`, or the Settings
    /// palette). Lists the whole merged library — key, origin,
    /// description, body — so a user can discover what's available
    /// without already knowing a `]]s<key>` shortcut (#237). Built-ins
    /// are normally present so it's rarely empty; the browser renders a
    /// placeholder if it's opened before snippets finish loading.
    pub(crate) fn mount_snippet_browser(&mut self) {
        use crate::realm::components::snippet_browser::{BrowserRow, SnippetBrowser};
        if matches!(self.modal_stack.last(), Some(Id::SnippetBrowser)) {
            return;
        }
        let rows: Vec<BrowserRow> = self
            .snippets
            .all()
            .map(|(k, v)| BrowserRow::new(k, v))
            .collect();
        self.mount_modal(
            Id::SnippetBrowser,
            SnippetBrowser::new(rows, self.ui_defaults.terminal_escape_char),
        );
    }

    /// Mount the fuzzy workspace switcher (`JumpToWorkspace`). Rows are
    /// every tracked workspace across repos, attention-needing ones
    /// first; the parallel session keys are stashed in `jump_choices`
    /// so `handle_choice_picked` can land the cursor on the chosen one.
    /// No-op (with a footer hint) when there's nothing to jump to.
    pub(crate) fn mount_jump_picker(&mut self) {
        use crate::realm::components::jump_picker::JumpPicker;
        if matches!(self.modal_stack.last(), Some(Id::JumpPicker)) {
            return;
        }
        let targets = self.sidebar.jump_targets();
        if targets.is_empty() {
            self.flash_info("no workspaces to jump to yet");
            return;
        }
        let (keys, labels): (Vec<_>, Vec<_>) = targets.into_iter().unzip();
        self.jump_choices = keys;
        self.mount_modal(Id::JumpPicker, JumpPicker::new(labels));
    }

    /// Mount the theme picker — a single-pick `Choice` over every
    /// registered palette with live preview: highlighting a row applies
    /// it immediately (`theme::set_by_name`), so the user sees the whole
    /// UI recolor as they arrow. The theme active at open is stashed in
    /// `theme_picker_prev` so Esc restores it; Enter keeps the highlight
    /// and persists it to `ui.theme`. Opens pre-positioned on the
    /// current theme.
    pub(crate) fn mount_theme_picker(&mut self) {
        use crate::realm::components::choice::Choice;
        if matches!(self.modal_stack.last(), Some(Id::ThemePicker)) {
            return;
        }
        let names: Vec<String> = crate::theme::list()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let current = crate::theme::current().name;
        let start = names.iter().position(|n| n == current).unwrap_or(0);
        self.theme_picker_prev = Some(current.to_string());
        self.theme_choices = names.clone();
        let modal = Choice::single("Preview as you move · Enter keeps it", names)
            .title("Theme")
            .label(|s: &String| s.clone())
            .select_index(start)
            .on_highlight(|name: &String| {
                crate::theme::set_by_name(name);
            });
        self.mount_modal(Id::ThemePicker, modal);
    }

    /// Mount the default-agent picker — a single-pick `Choice` over the
    /// enabled agents (`self.agents`), opened on the current default.
    /// Pick → `handle_choice_picked` persists `setup.default_agent` and
    /// updates the panes live. Agent ids are stashed in
    /// `default_agent_choices` so the picked index resolves back.
    pub(crate) fn mount_default_agent_picker(&mut self) {
        use crate::realm::components::choice::Choice;
        if matches!(self.modal_stack.last(), Some(Id::DefaultAgentPicker)) {
            return;
        }
        let registry = lazybox_agents::registry();
        let ids: Vec<String> = self.agents.clone();
        let labels: Vec<String> = ids
            .iter()
            .map(|id| match registry.get(id) {
                Some(agent) => format!("{}  ·  {id}", agent.display_name()),
                None => id.clone(),
            })
            .collect();
        let current = self.sidebar.default_agent();
        let start = ids.iter().position(|id| id == current).unwrap_or(0);
        self.default_agent_choices = ids;
        let modal = Choice::single("Used by `w` work-on-this + new-workspace spawns", labels)
            .title("Default agent")
            .label(|s: &String| s.clone())
            .select_index(start);
        self.mount_modal(Id::DefaultAgentPicker, modal);
    }

    /// Mount the default-model picker — the second step of the
    /// default-agent flow, offering `agent_id`'s declared tiers plus an
    /// "agent default" row, opened on the current default tier. Pick →
    /// `handle_choice_picked` persists `agents.<id>.models.default` so
    /// bare spawns use it (per-spawn tier chords still override); Esc
    /// keeps the current tier. No-op for an agent with no tier menu.
    pub(crate) fn mount_default_model_picker(&mut self, agent_id: &str) {
        use crate::realm::components::choice::Choice;
        if matches!(self.modal_stack.last(), Some(Id::DefaultModelPicker)) {
            return;
        }
        let Some(models) = self.agent_models.get(agent_id) else {
            return;
        };
        if models.tiers.is_empty() {
            return;
        }
        // Row 0 unpins the YAML override. With a built-in default in
        // play that lands back on the built-in tier, not on the
        // agent's ambient model — say so in the label.
        let builtin_label = lazybox_core::AgentModels::builtin(agent_id)
            .and_then(|b| b.default)
            .and_then(|a| models.tier(&a))
            .map(|t| t.label.clone());
        let mut aliases: Vec<Option<String>> = vec![None];
        let mut labels: Vec<String> = vec![match &builtin_label {
            Some(label) => format!("Built-in default  ·  {label}"),
            None => "Agent default  ·  no pinned model".into(),
        }];
        // Fable-class tiers stay spawnable via an explicit chord but
        // are never offered as a default.
        for tier in models.tiers.iter().filter(|t| !t.excluded_from_default()) {
            aliases.push(Some(tier.alias.clone()));
            labels.push(format!("{}  ·  {}", tier.label, tier.alias));
        }
        let start = models
            .default
            .as_ref()
            .and_then(|d| aliases.iter().position(|a| a.as_ref() == Some(d)))
            .unwrap_or(0);
        self.default_model_agent = Some(agent_id.to_string());
        self.default_model_choices = aliases;
        let modal = Choice::single("Used by bare spawns · `w S/M/L` still overrides", labels)
            .title(format!("Default model · {agent_id}"))
            .label(|s: &String| s.clone())
            .select_index(start);
        self.mount_modal(Id::DefaultModelPicker, modal);
    }

    /// Update the default agent both panes resolve `w` against, live —
    /// no restart. Mirrors the startup wiring in `apply_sidebar_config`.
    /// Also rebuilds the action catalog: the `w S` / `a S` tier chords
    /// key off the default agent's menu, so they must re-key to the new
    /// agent's tiers (or disappear when it declares none).
    pub(crate) fn set_default_agent(&mut self, agent: &str) {
        self.sidebar.set_default_agent(agent);
        self.right.set_default_agent(agent);
        self.rebuild_catalog();
    }

    /// Land the cursor on `key` and follow it with the panes: show its
    /// terminal when it has a live one (so a jump-to-agent keeps you
    /// driving), otherwise fall back to the sidebar. Exits focus mode
    /// when the target has no terminal — focus mode needs one to fill
    /// the screen. Backs both the `` ` `` picker and the `]]` `` ` ``
    /// terminal jump.
    pub(crate) fn jump_to_workspace_key(&mut self, key: &lazybox_core::SessionKey) {
        if !self.sidebar.focus_workspace_key(key) {
            self.flash_info("workspace is gone — nothing to jump to");
            return;
        }
        self.sync_panes();
        if self.terminals.active_terminal_id().is_some() {
            self.focus = PaneFocus::Terminals;
            self.terminal_user_typed_since_focus = false;
        } else {
            self.focus_mode = false;
            self.focus = PaneFocus::Sidebar;
        }
        self.set_focus_attr();
        self.redraw = true;
    }

    /// Clear the host terminal and schedule a repaint of every cell —
    /// the recovery for a resize, fullscreen toggle, or display
    /// sleep/wake that left the real screen out of sync with ratatui's
    /// idea of it.
    ///
    /// Deliberately NOT `Terminal::clear()` / `clear_screen()`: those
    /// snapshot the cursor with a `CSI 6 n` query, and the reply is
    /// consumed by the dedicated input-reader thread parked inside
    /// `crossterm::event::read()` (which also holds crossterm's
    /// internal reader lock), so the query stalls the UI thread for
    /// its full 2 s timeout. The backend `clear` is a plain escape
    /// write; the double `swap_buffers` resets both diff buffers so
    /// the next draw rewrites every cell instead of diffing against a
    /// frame the host is no longer showing.
    pub fn force_full_redraw(&mut self) {
        use tuirealm::ratatui::backend::Backend as _;
        let raw = self.terminal.raw_mut();
        let _ = raw.backend_mut().clear();
        raw.swap_buffers();
        raw.swap_buffers();
        self.redraw = true;
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
        self.rebuild_catalog();
    }

    /// Set the enabled agents (catalog-display order) and rebuild the
    /// catalog so each gets its own `SpawnAgent` row. Called at startup
    /// from `setup.agents`.
    pub fn set_agents(&mut self, agents: Vec<String>) {
        self.agents = agents;
        self.rebuild_catalog();
    }

    /// Recompute the runtime catalog from the current agents +
    /// overrides. Cheap (a few dozen rows); called whenever either
    /// input changes.
    /// Set the per-agent model-tier menus (from `agents.<id>.models` +
    /// built-in fallback) and rebuild the catalog so the default work
    /// agent's tiers get `w S` / `a S` chords.
    pub fn set_agent_models(
        &mut self,
        models: std::collections::BTreeMap<String, lazybox_core::AgentModels>,
    ) {
        self.agent_models = models;
        self.rebuild_catalog();
    }

    fn rebuild_catalog(&mut self) {
        // Tier chords track the default work agent's menu — the alias is
        // agent-agnostic at spawn, so one menu of chords serves whatever
        // agent `w` ends up targeting.
        let tiers = self
            .agent_models
            .get(self.sidebar.default_agent())
            .map(|m| m.tiers.as_slice())
            .unwrap_or(&[]);
        self.catalog = lazybox_tui_core::action::ActionDef::catalog_with_tiers(
            &self.agents,
            &self.action_key_overrides,
            tiers,
        );
    }

    /// Read-only handle to the runtime catalog — used by tests and the
    /// generated Keys screen.
    pub fn catalog(&self) -> &[lazybox_tui_core::action::CatalogEntry] {
        &self.catalog
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
            // Don't pretend the keypress worked: flag the failure so
            // the next `tick_daemon_health` raises the disconnect
            // banner. A `Cell` because this method is `&self` and is
            // called from borrow-heavy paths that can't take `&mut`.
            match e {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    self.cmd_send_overloaded.set(true);
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    self.cmd_send_failed.set(true);
                }
            }
        }
    }

    /// Raise the one-shot "daemon disconnected" banner. Permanent
    /// severity: it never auto-fades, and the severity-aware `flash`
    /// keeps routine Info/Hint flashes from displacing it — the UI is
    /// a zombie until the user restarts/reconnects, so the banner must
    /// outlive everything else. Esc still dismisses it (severity only
    /// drives auto-fade + displacement, never dismissability).
    pub(crate) fn note_daemon_disconnected(&mut self) {
        if self.daemon_disconnect_notified {
            return;
        }
        self.daemon_disconnect_notified = true;
        self.flash_error(
            "✗ daemon disconnected — commands are no longer delivered; \
             restart lazybox (or reconnect with `lazybox --connect …`) to resume",
        );
    }

    /// Per-iteration daemon-health check: drain the `send_cmd` failure
    /// flag into the disconnect banner. Called from the run loop's
    /// tick section (and unit tests) so a dead channel surfaces within
    /// one frame of the first failed send instead of never.
    pub(crate) fn tick_daemon_health(&mut self) {
        if self.cmd_send_overloaded.take() {
            self.flash(
                "⚠ command was not accepted — daemon connection is congested; wait and retry",
                crate::realm::components::footer::NoticeSeverity::Retryable,
            );
        }
        if self.cmd_send_failed.take() {
            self.note_daemon_disconnected();
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
        let msg = msg.into();
        // The footer width-caps its notice segment, so a long error
        // (merge rejection reasons, spawn failures) can render
        // truncated. Record the full text in the sync log so the
        // sync-status window (`Shift-D`) can always show it.
        self.status.sync.note_error("ui", "", &msg, "");
        self.flash(
            msg,
            crate::realm::components::footer::NoticeSeverity::Permanent,
        );
    }

    /// Surface a sticky banner when the daemon we connected to was built
    /// from a different commit than this client. The protocol handshake
    /// only catches wire-format skew (`PROTOCOL_FINGERPRINT`); a stale
    /// daemon with the same fingerprint connects silently and
    /// re-introduces already-fixed bugs, so the operator needs to see
    /// the mismatch.
    /// A matching build is the common case and stays quiet.
    pub fn note_daemon_build(&mut self, daemon_build: &str) {
        if daemon_build != lazybox_ipc::BUILD_VERSION {
            self.flash_error(format!(
                "build mismatch: daemon {daemon_build}, client {} — restart the daemon \
                 (`lazybox server stop`) to pick up this build",
                lazybox_ipc::BUILD_VERSION
            ));
        }
    }

    /// Show a startup update notice unless this exact target was dismissed.
    pub fn show_update_if_new(
        &mut self,
        update: crate::build_guard::AvailableUpdate,
        store: Option<std::sync::Arc<dyn lazybox_store::Store>>,
    ) {
        let target = update.target();
        let dismissal_key = dismissed_update_key(&target);
        let dismissed = store
            .as_ref()
            .is_some_and(|store| match store.get_kv(&dismissal_key) {
                Ok(value) => value.is_some(),
                Err(error) => {
                    tracing::warn!("read dismissed update target failed: {error}");
                    false
                }
            });
        if dismissed || self.modal_stack.iter().any(|id| id == &Id::Update) {
            return;
        }

        use crate::realm::components::error::{Accent, ErrorModal};
        let modal = ErrorModal::new(
            "Update available",
            Accent::info("UPDATE"),
            update.modal_body(),
        )
        .dismiss_on_confirm();
        self.pending_update_target = Some(target);
        self.update_store = store;
        self.mount_modal(Id::Update, modal);
    }

    pub(super) fn tick_update_dismissal(&mut self) {
        while let Ok(result) = self.update_dismissal_rx.try_recv() {
            self.handle_update_dismissal_result(result);
        }
    }

    pub(super) fn finish_update_dismissal(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while self.update_dismissals_pending > 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("timed out persisting update dismissal during shutdown");
                break;
            }
            match self.update_dismissal_rx.recv_timeout(remaining) {
                Ok(result) => self.handle_update_dismissal_result(result),
                Err(error) => {
                    tracing::warn!("update dismissal worker ended before persistence: {error}");
                    break;
                }
            }
        }
    }

    fn handle_update_dismissal_result(&mut self, result: Result<(), String>) {
        self.update_dismissals_pending = self.update_dismissals_pending.saturating_sub(1);
        if let Err(error) = result {
            self.flash_error(format!(
                "could not remember update dismissal; it may reappear next launch: {error}"
            ));
        }
    }

    /// Validate the applied keymap config at startup and surface any
    /// problems (issue: silent keymap misconfiguration). `extra`
    /// carries warnings the caller computed before the catalog existed
    /// (today: an unknown `ui.keymap_preset` name). The per-issue
    /// details land in the durable messages log (Shift-M) and the
    /// footer gets one summarizing notice — mirroring how the
    /// build-freshness guard surfaces its banner. Never rejects the
    /// config: the catalog already fell back to parseable defaults.
    pub fn surface_keymap_warnings(&mut self, mut warnings: Vec<String>) {
        warnings.extend(helpers::keymap_config_warnings(
            &self.action_key_overrides,
            &self.catalog,
        ));
        if warnings.is_empty() {
            return;
        }
        for w in &warnings {
            tracing::warn!("keymap config: {w}");
            self.status.messages.record(
                &format!("keymap config: {w}"),
                crate::realm::components::footer::NoticeSeverity::Retryable,
            );
        }
        let n = warnings.len();
        self.flash(
            format!(
                "⚠ keymap config: {n} issue{} — Shift-M for details",
                if n == 1 { "" } else { "s" }
            ),
            crate::realm::components::footer::NoticeSeverity::Retryable,
        );
    }

    /// Like `flash_error`, but tags the notice as a provider "sync
    /// failed" banner owned by `source`, so the next successful
    /// `PollCompleted` from that same provider can clear it once sync
    /// recovers. See the `sync_error_source` field and
    /// [`Self::clear_sync_error_if_recovered`].
    pub fn flash_sync_error(&mut self, source: &str, msg: impl Into<String>) {
        // Not `flash_error`: the provider failure is already in the
        // sync log (recorded for every `ProviderError` event), so
        // logging the banner text would double-count it under "ui".
        self.flash(
            msg,
            crate::realm::components::footer::NoticeSeverity::Permanent,
        );
        // `flash` just reset the flag to `None`; re-arm it *after* so
        // the banner is attributed to the provider that actually
        // failed.
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
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        // Sticky severities own the footer slot: they never auto-fade
        // and demand an acknowledgment (Esc), so they must not be
        // displaced by a routine flash.
        let sticky =
            |s: NoticeSeverity| matches!(s, NoticeSeverity::Permanent | NoticeSeverity::Auth);
        let msg = msg.into();
        // Severity-aware replacement: a lower-severity flash must not
        // displace a live sticky error — pre-fix, a Permanent
        // "✗ merge failed" could be wiped within a second by an Info
        // "✓ sync ok" or a Hint, leaving no trace of the failure. The
        // suppressed notice is routed to the durable messages log
        // (Shift-M) instead — Hints included here, precisely because
        // suppression is the one case where a hint would otherwise
        // vanish without ever being visible.
        if let Some(existing) = &self.status.notice
            && sticky(existing.severity)
            && !sticky(severity)
        {
            self.status.messages.record(&msg, severity);
            self.redraw = true;
            return;
        }
        // Any fresh notice supersedes a sync-error banner, so the
        // "clear on recovery" tag only stays armed while the
        // sync-error notice is the one actually on screen. (Reset only
        // when actually replacing — a suppressed flash above must
        // leave the banner attribution intact.)
        self.sync_error_source = None;
        // Every notice flashes in the footer AND accumulates in the
        // durable messages log (#309) — except one-shot Hints, which
        // are ephemeral UI nudges (`scroll: alt-screen`) that would
        // only clutter the readable history. Record before the string
        // is moved into the Notice.
        if severity != NoticeSeverity::Hint {
            self.status.messages.record(&msg, severity);
        }
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
        // `remount`, not `mount`: a modal re-mounted under an id that is
        // still live in the view (e.g. the `WorktreeProgress` checklist
        // re-mounting itself on every step advance) must *replace* the
        // stale component. `mount` errors with `ComponentAlreadyMounted`
        // and we swallow the result, which left the first-step component
        // frozen on screen while `modal_stack` tracked it as current.
        let _ = self.app.remount(
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

    /// Lock the shared help conversation (#302). Recovers from a
    /// poisoned lock — a panicked render elsewhere must not brick the
    /// help modal for the rest of the session.
    pub(crate) fn help_convo_mut(
        &self,
    ) -> std::sync::MutexGuard<'_, crate::realm::components::help_ask::HelpConvo> {
        match self.help_convo.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
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
                    model_alias: None,
                    session_key: workspace_key.clone(),
                    session_id: None,
                    kind: lazybox_ipc::TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                    on_main: false,
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
                // open_url is fire-and-forget — phrase as in-progress.
                tracing::info!(%url, "opening url from terminal");
                self.flash_hint(format!("opening {url}…"));
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

    /// Open the in-session Settings window: actions grouped under
    /// Providers / Agents / Appearance / Maintenance tabs (see
    /// `SettingsSection`), scoped to the user's current providers.
    /// Falls back to the full wizard when there's no cached persisted
    /// setup yet (first-run path or `--test` mode).
    ///
    /// The rows are stashed FLAT (in tab order) in
    /// `setup.settings_actions`; the tabbed component carries each
    /// row's flat index and emits `Msg::ChoicePicked([flat_idx])` on
    /// Enter, so the pick routing in `handle_choice_picked` is the
    /// same as the old flat palette's.
    pub fn open_settings(&mut self) {
        use crate::realm::components::settings::{Settings, SettingsTab};
        use crate::realm::setup_ctx::SettingsSection;

        if self.setup.runner.is_some() || matches!(self.modal_stack.last(), Some(Id::Setup)) {
            return;
        }

        let actions = self.build_settings_actions();
        if actions.is_empty() {
            // No persisted setup → fall back to the full wizard.
            self.reopen_setup();
            return;
        }
        // Group into tabs, rebuilding the flat list in tab order so
        // the component's flat indices resolve against exactly what
        // `handle_choice_picked` will read back.
        let mut flat: Vec<SettingsAction> = Vec::with_capacity(actions.len());
        let mut tabs: Vec<SettingsTab> = Vec::with_capacity(SettingsSection::ALL.len());
        for section in SettingsSection::ALL {
            let mut rows = Vec::new();
            for action in actions.iter().filter(|a| a.section() == section) {
                rows.push((action.label(), flat.len()));
                flat.push(action.clone());
            }
            tabs.push(SettingsTab { section, rows });
        }
        self.setup.settings_actions = flat;
        self.mount_modal(Id::Setup, Settings::new(tabs));
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
        // One fresh load feeds every config-backed row below, so even a
        // hand-edited YAML shows its current values without a restart.
        let cfg = lazybox_config::Config::load().unwrap_or_default();
        let default_agent = self.sidebar.default_agent().to_string();
        let models = cfg.agent_models(&default_agent);
        let default_tier = models
            .default
            .as_deref()
            .and_then(|a| models.tier(a))
            .map(|t| t.label.clone());
        actions.push(SettingsAction::EditDefaultAgent {
            current: default_agent,
            tier: default_tier,
        });
        // One direct default-model row per enabled agent with a tier
        // menu — picking a default model must not require making that
        // agent the default first.
        for agent_id in &self.agents {
            let models = cfg.agent_models(agent_id);
            if models.tiers.is_empty() {
                continue;
            }
            let tier = models
                .default
                .as_deref()
                .and_then(|a| models.tier(a))
                .map(|t| t.label.clone());
            actions.push(SettingsAction::EditDefaultModel {
                agent_id: agent_id.clone(),
                tier,
            });
        }
        actions.push(SettingsAction::ToggleSkipPermissions {
            enabled: cfg.agent.skip_permissions,
        });
        actions.push(SettingsAction::EditSnippets);
        actions.push(SettingsAction::EditTheme {
            current: crate::theme::current().name.to_string(),
        });
        actions.push(SettingsAction::EditLlmGateway {
            set: cfg.agent.gateway_url().is_some(),
        });
        actions.push(SettingsAction::CheckAgentUpdates);
        actions.push(SettingsAction::UpdateAgentClis);
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
        // Snippets open the read-only browser — discoverable, and `e`
        // there opens the YAML. No wizard runner, no cached inputs.
        if matches!(action, SettingsAction::EditSnippets) {
            self.mount_snippet_browser();
            return;
        }
        // Theme picker is its own live-preview modal — not a wizard step.
        if matches!(action, SettingsAction::EditTheme { .. }) {
            self.mount_theme_picker();
            return;
        }
        // LLM gateway editor is a single URL input that writes straight
        // to YAML — no wizard runner, no cached detection inputs.
        if matches!(action, SettingsAction::EditLlmGateway { .. }) {
            self.mount_gateway_url_input();
            return;
        }
        // Default-agent picker is a single Choice that writes straight
        // to YAML and updates the panes live — no wizard runner.
        if matches!(action, SettingsAction::EditDefaultAgent { .. }) {
            self.mount_default_agent_picker();
            return;
        }
        // Per-agent default-model picker — same modal as the second
        // step of the default-agent flow, minus the agent switch.
        if let SettingsAction::EditDefaultModel { agent_id, .. } = &action {
            let agent_id = agent_id.clone();
            self.mount_default_model_picker(&agent_id);
            return;
        }
        // Agent-CLI update actions are fire-and-forget daemon commands;
        // results come back as AgentCliUpdatesChecked / -UpdateFinished
        // footer notices.
        if matches!(action, SettingsAction::CheckAgentUpdates) {
            self.send_cmd(lazybox_ipc::Command::CheckAgentCliUpdates);
            self.flash_hint("checking agent CLI versions…");
            return;
        }
        if matches!(action, SettingsAction::UpdateAgentClis) {
            self.send_cmd(lazybox_ipc::Command::UpdateAgentClis);
            self.flash_hint("updating agent CLIs in the background…");
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
            // Handled by the early returns above; listed for exhaustiveness.
            SettingsAction::CheckAgentUpdates | SettingsAction::UpdateAgentClis => return,
            SettingsAction::EditSnippets => return,
            SettingsAction::EditTheme { .. } => return,
            SettingsAction::EditLlmGateway { .. } => return,
            SettingsAction::EditDefaultAgent { .. } => return,
            SettingsAction::EditDefaultModel { .. } => return,
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

    /// Restore terminal state by dropping the RAII guard (idempotent).
    /// The run loop calls this on a clean exit; the guard's `Drop` is
    /// the backstop for error / panic / signal paths so the host shell
    /// is never stranded in Kitty keyboard mode (#211).
    pub fn shutdown(&mut self) {
        self.term_guard.take();
    }
    /// Whether the Activity (right) pane should render for the
    /// currently-selected workspace. A per-workspace manual override
    /// (set by `ToggleActivityPane`) wins; otherwise the pane shows
    /// only when the workspace has content worth showing. The
    /// auto-hide rule is about a *selected* workspace with no
    /// activity — with nothing selected (empty inbox) the pane keeps
    /// its prior always-on behavior.
    pub(super) fn activity_pane_visible(&self) -> bool {
        let Some(ws) = self.sidebar.selected_workspace() else {
            return true;
        };
        if let Some(&shown) = self.activity_pane_overrides.get(&ws.key) {
            return shown;
        }
        self.right.has_visible_content()
    }

    /// The three pane rects, accounting for a hidden Activity pane.
    /// When the pane is hidden its row folds into the terminal stack:
    /// the returned `right_top` is zero-height (so hit-tests and the
    /// render skip it) and `right_bottom` spans the full right column.
    pub(super) fn effective_pane_rects(&self, area: Rect) -> (Rect, Rect, Rect) {
        let rects = pane_areas(
            area,
            self.layout.sidebar_pct,
            self.layout.right_top_pct,
            self.layout.sidebar_user_resized,
        );
        apply_activity_visibility(rects, self.activity_pane_visible())
    }

    /// Keep focus off the Activity pane while it's hidden — Tab,
    /// click, and programmatic moves all funnel through here so a
    /// hidden pane never silently swallows keystrokes. Falls through
    /// to the terminal stack (which forwards to the sidebar when no
    /// terminal is live).
    pub(super) fn enforce_pane_focus(&mut self) {
        if self.focus == PaneFocus::Right && !self.activity_pane_visible() {
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
        }
    }

    /// Render the current frame.
    pub fn view(&mut self) {
        // Pull state out before the closure so the borrow checker is
        // happy — `terminal.draw` takes `&mut self.terminal` while we
        // also need `&mut self.app` etc. inside.
        let sidebar_pct = self.layout.sidebar_pct;
        let right_top_pct = self.layout.right_top_pct;
        let sidebar_user_resized = self.layout.sidebar_user_resized;
        // Computed outside the draw closure: calling a `&self` method
        // inside it would capture all of `self` and clash with the
        // disjoint `&mut self.sidebar` / `self.right` borrows below.
        let activity_visible = self.activity_pane_visible();
        // Pick the polling indicator for the footer:
        // - During the initial blocking modal, surface the rich
        //   first-poll spinner.
        // - Otherwise, surface the lightweight background indicator
        //   that fires on every subsequent cycle (so the user always
        //   knows whether lazybox is currently talking to GitHub).
        // Footer spinner priority: the blocking first-poll modal owns
        // it at startup; otherwise an in-flight spawn (the user's
        // just-pressed `w`/`a c`/`s`) beats the ambient background-poll
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
        // ("g m merge" when the row is READY, "w fix CI" when
        // CI is failing, etc.) so the user always sees what's
        // actionable right now, not a generic alphabet. The full
        // keymap stays in `?` help.
        let keymap: Vec<crate::pane::Binding> = match self.focus {
            PaneFocus::Sidebar => self.sidebar.contextual_bindings(&self.catalog),
            PaneFocus::Right => self.right.contextual_bindings(&self.action_key_overrides),
            PaneFocus::Terminals => self
                .terminals
                .contextual_bindings(self.ui_defaults.terminal_escape_char),
        };
        // Universal hints appended to every pane's footer (issue #100):
        // the orientation + escape shortcuts a lost first-time user
        // always needs in view. `quit` last so it's the rightmost,
        // most-findable hint.
        //
        // But in a focused terminal the PTY eats every key, so those
        // globals don't fire — advertising `q q` / `?` there is a lie
        // (issue #114). The catalog's `available_in_terminal` is the
        // single source of truth for this; when nothing universal
        // survives terminal focus we advertise the `]]` gateway that
        // unlocks them instead, so the footer never claims a shortcut
        // the focused pane won't dispatch.
        let globals: Vec<crate::pane::Binding> = {
            use lazybox_tui_core::action::{ActionDef, ActionKind};
            // Footer's curated short tail of `universal_shortcuts()` —
            // kept to three so a narrow line never truncates `quit`
            // off the right edge.
            let tail = [ActionKind::OpenHelp, ActionKind::OpenTour, ActionKind::Quit]
                .map(ActionDef::for_kind);
            if self.focus == PaneFocus::Terminals
                && tail.iter().all(|def| !def.available_in_terminal())
            {
                let help = ActionDef::for_kind(ActionKind::OpenHelp);
                let quit = ActionDef::for_kind(ActionKind::Quit);
                // The way back out is `terminal.escape_char` doubled,
                // owned by the escape-char latch — not a remappable
                // catalog chord (#188). Render it from the configured
                // char so the hint matches what the dispatcher matches.
                let esc = self.ui_defaults.terminal_escape_char;
                vec![crate::pane::Binding {
                    keys: std::borrow::Cow::Owned(format!("{esc}{esc}")),
                    label: std::borrow::Cow::Owned(format!(
                        "exit for {} · {}",
                        help.effective_keys_display(&self.action_key_overrides),
                        quit.effective_keys_display(&self.action_key_overrides),
                    )),
                }]
            } else {
                tail.iter()
                    .map(|def| crate::pane::Binding {
                        keys: def.effective_keys_display(&self.action_key_overrides),
                        label: std::borrow::Cow::Borrowed(def.label),
                    })
                    .collect()
            }
        };
        let notice = self.status.notice.clone();
        // Which-key rows for an armed catalog leader — the
        // `(next-key, label)` continuations of the armed prefix, a pure
        // function of the catalog. Built only while a leader is armed.
        // The group label (`github`, `agent`, …) titles the popup when
        // the continuations belong to a named leader group (#304).
        // Continuations resolve against the same focus the key
        // dispatcher used to arm the leader — an empty terminal pane
        // resolves as the sidebar (see `resolve_focus_for_keys`).
        let (leader_rows, leader_group): (Vec<(String, String)>, Option<&'static str>) =
            if let Some(prefix) = self.leader.pending() {
                let rfocus = self.resolve_focus_for_keys().unwrap_or(self.focus);
                let conts = seq_continuations(prefix, rfocus, &self.catalog);
                let group = conts
                    .iter()
                    .find_map(|(_, e)| lazybox_tui_core::action::leader_group_label(e.kind));
                (
                    conts
                        .into_iter()
                        .map(|(stroke, entry)| (stroke.display(), entry.label.to_string()))
                        .collect(),
                    group,
                )
            } else {
                (Vec::new(), None)
            };
        // Command menu for the `]]` leader popup (#252): the fixed
        // commands (one table with the key dispatch —
        // `terminal_leader::LeaderCmd`, #286) FIRST so they're always
        // visible, then the agent-jump roster (`1..9` → agent workspace
        // name, sidebar order). Ordering matters — the popup caps its
        // rows (`LEADER_MAX_ROWS`) and truncates the tail into "+N
        // more", so a user with many agent workspaces must still see
        // the exit / snippet commands rather than have them pushed off
        // the bottom. A small mnemonic menu rather than the whole
        // snippet library — snippets live one level down, behind `]]s`.
        // Built only while the leader is armed so the steady-state
        // render pays nothing.
        let leader_menu_rows: Vec<(String, String)> = if self.terminal_leader_armed {
            self.terminal_leader_menu_rows()
        } else {
            Vec::new()
        };
        // Focus mode (#156): hide the sidebar + activity pane and give
        // the terminal the whole window behind a slim event header.
        // Resolve the header's contents out here so the draw closure
        // doesn't need to borrow `self` immutably while it also holds
        // the mutable terminal borrow.
        let focus_mode = self.focus_mode;
        let (focus_title, focus_summary, focus_hint) = if focus_mode {
            let active = self.terminals.active_session();
            let name = active
                .and_then(|k| self.sidebar.workspace_by_key(k))
                .or_else(|| self.sidebar.selected_workspace())
                .map(|w| w.name.clone())
                .unwrap_or_else(|| "no workspace".to_string());
            // Prefix the title with this agent's jump number (its
            // 1-based slot in the sidebar-order agent roster) so the
            // user knows which `]]<digit>` lands back here.
            let agents = self.sidebar.agent_workspace_keys();
            let number = active.and_then(|k| agents.iter().position(|a| a == k));
            let title = match number {
                Some(i) => format!("{} · {name}", i + 1),
                None => name,
            };
            // Inside focus mode the PTY owns the keyboard, so the
            // reachable controls are all `]]` leader chords: `]]<digit>`
            // jumps to another agent, `]]q` exits back to the sidebar.
            let esc = self.ui_defaults.terminal_escape_char;
            let hint = format!("{esc}{esc}<n> jump · {esc}{esc}q exit");
            (title, self.sidebar.attention_summary(), hint)
        } else {
            (String::new(), Default::default(), String::new())
        };
        let mut captured_area = Rect::default();
        let _ = self.terminal.draw(|f| {
            let area = f.area();
            captured_area = area;
            let (pane_area, footer_area) = split_for_footer(area);
            let right_bottom = if focus_mode {
                let (header, body) = focus_mode_areas(pane_area);
                crate::realm::components::focus_header::render(
                    f,
                    header,
                    &focus_title,
                    focus_summary,
                    &focus_hint,
                );
                self.terminals.view_in(body, f);
                body
            } else {
                let (left, right_top, right_bottom) = apply_activity_visibility(
                    pane_areas(pane_area, sidebar_pct, right_top_pct, sidebar_user_resized),
                    activity_visible,
                );
                self.sidebar.view_in(left, f);
                // A zero-height `right_top` means the Activity pane is
                // hidden for this workspace — its space went to the
                // terminal stack below.
                if right_top.height > 0 {
                    self.right.view_in(right_top, f);
                }
                self.terminals.view_in(right_bottom, f);
                right_bottom
            };

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

            // Which-key popup for an armed leader chord (#126, #102).
            // Drawn above the footer but below any modal — in practice
            // the two never co-occur (arming doesn't mount a modal, and
            // the leader is consumed before a modal-mounting action
            // fires), so z-order is moot. The rows are a pure function
            // of the armed prefix + the catalog (`seq_continuations`),
            // not a hardcoded group table.
            if let Some(prefix) = self.leader.pending().copied() {
                crate::realm::components::which_key::render(
                    f,
                    area,
                    prefix,
                    leader_group,
                    &leader_rows,
                    self.leader_highlight,
                );
            }
            // Which-key popup for the armed terminal `]]` leader (#205,
            // #252): the agent-jump roster (`]]<digit>`) on top of the
            // fixed command menu (`]]s` snippets, `]]f` focus, `]]q`
            // exit, `` ]]` `` jump, tile management #286).
            if self.terminal_leader_armed {
                crate::realm::components::which_key::render_terminal_leader(
                    f,
                    area,
                    self.ui_defaults.terminal_escape_char,
                    &leader_menu_rows,
                    self.terminal_leader_highlight,
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
                // Flush (not raw-dispatch) so a pick that resolves to a
                // work spawn gets the same spawn-spinner + spawn→inject
                // rewrite the keyboard path gets — the `w` multi-agent
                // chooser (#418) and the sidebar context menu both emit
                // Spawns that must fold into a running agent.
                self.flush_dispatched_cmds(cmds);
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
            Msg::MessagesCleared => {
                // Wipe the durable history and re-render the window
                // against the now-empty log (it stays open showing the
                // placeholder). `mount_messages` short-circuits if the
                // window is already up, so drop it first.
                self.status.messages.clear();
                if self.modal_stack.last() == Some(&Id::Messages) {
                    self.pop_modal();
                }
                self.mount_messages();
            }
            Msg::OpenSnippetsFile => {
                // `e` in the browser: drop the modal, then open the YAML
                // so the editor takes over a clean screen.
                if matches!(self.modal_stack.last(), Some(Id::SnippetBrowser)) {
                    self.pop_modal();
                }
                self.open_snippets_file();
            }
            // The component's `on(Tick)` already advanced the spinner;
            // here we walk the displayed checklist toward the daemon's
            // truth (gated by the min-dwell) and tear the modal down once
            // a queued dismiss has been fully shown. Being a non-empty
            // message is also what flips `redraw` in the run loop.
            Msg::WorktreeProgressTick => self.advance_worktree_progress(),
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
            Msg::HelpAskOpen => {
                // `?` on Shortcuts: return to the primary Ask surface.
                if matches!(self.modal_stack.last(), Some(Id::Help)) {
                    self.pop_modal();
                }
                self.mount_help_ask();
            }
            Msg::HelpIndexOpen => {
                // `?` at Ask's empty prompt: swap to the compact index.
                if matches!(self.modal_stack.last(), Some(Id::HelpAsk)) {
                    self.pop_modal();
                }
                self.mount_help();
            }
            Msg::HelpAsked(question) => {
                // The HelpAsk modal stays mounted — the answer streams
                // back into `help_convo`.
                let cmds = self.handle_help_asked(question);
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
