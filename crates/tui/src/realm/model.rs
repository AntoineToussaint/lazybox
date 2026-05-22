//! `Model` — the realm-side replacement for pilot's `App` struct.
//!
//! ## Architecture
//!
//! Panes (Sidebar / Right / Terminals) are **not** mounted into the
//! tuirealm `Application`. They live as typed fields on `Model` and
//! we drive their `view`/`on_event`/`handle_key` directly. tuirealm's
//! `Application` only owns **modals** — that's where its mount/unmount
//! + Z-stack semantics actually pay off.
//!
//! Why: pilot's panes are persistently visible, mutate often, and the
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

use crate::PaneId;
use crate::realm::UserEvent;
use crate::realm::components::right::Right;
use crate::realm::components::sidebar::Sidebar;
use crate::realm::components::splash::Splash;
use crate::realm::components::terminals::Terminals;
use crate::realm::keymap::realm_key_to_crossterm;
use pilot_ipc::{Client, Command as IpcCommand, Event as IpcEvent};
use std::sync::mpsc;
use std::time::Duration;
use tuirealm::application::{Application, PollStrategy};
use tuirealm::event::{Event as RealmEvent, Key, KeyEvent as RealmKey, KeyModifiers};
use tuirealm::listener::{EventListenerCfg, Poll, PortError, PortResult};
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, Borders};
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
    /// Confirm dialog asking the user to remove a workspace that fell
    /// out of scope while having running terminals. The pending
    /// workspace_key lives in `pending_removal_prompt` so the
    /// `Msg::Confirmed(true)` handler knows what to delete.
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
    /// Unified confirm modal for any destructive catalog action.
    /// `Model::dispatch_action` routes here when
    /// `ActionDef::is_destructive()` is true; the pending `Action`
    /// lives in `pending_action_confirm` and fires on
    /// `Msg::Confirmed(true)`. Replaces the per-action confirm
    /// modals (MergePrConfirm, the kill latch, …) — one modal id,
    /// one Yes-handler, one place to remember.
    ActionConfirm,
}

/// App-level message vocabulary for modals + globals.
#[derive(Debug, PartialEq, Clone)]
pub enum Msg {
    SplashConfirmed,
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
    pub projects: std::collections::BTreeMap<pilot_core::ProjectKey, pilot_core::Project>,
    /// IPC client for forwarding pane-emitted commands to the daemon.
    pub client: Client,
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
    /// Whether pilot is capturing mouse events. Toggled by F8 /
    /// Alt-s. When `false`, pilot has issued `DisableMouseCapture`
    /// so the host terminal regains native text selection (which
    /// spans pilot's whole window including UI chrome — uglier
    /// than pilot's pane-scoped selection but useful as a fallback).
    /// When `true`, pilot owns mouse: clicks drive its UI, drags
    /// inside the terminal pane do pilot-side text selection.
    #[allow(dead_code)] // accessed indirectly via the toggle handler
    mouse_capture_on: bool,
    /// Active pilot-side text selection in the terminal pane.
    /// `(start_cell, end_cell)` in absolute viewport coords, set on
    /// mouse Down inside the terminal rect (when the inner program
    /// isn't tracking mouse itself) and extended on Drag. On Up the
    /// selected cells are extracted from libghostty's grid and
    /// copied to the host clipboard via OSC 52.
    terminal_selection: Option<((u16, u16), (u16, u16))>,
    /// `]]` escape from the terminal pane: first press of the escape
    /// char arms; a second within the window kicks focus back to
    /// the sidebar instead of forwarding to the PTY.
    escape_latch: crate::confirm_latch::DoubleTapLatch,
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
    pending_reply: Option<pilot_core::SessionKey>,
    /// Set by `mount_request_reviewers`; consumed by
    /// `handle_input_submitted` when `Id::RequestReviewers` is the
    /// top modal. Holds the workspace whose PR we'll request
    /// reviewers on.
    pending_review_request: Option<pilot_core::WorkspaceKey>,
    /// Candidate logins shown in the `RequestReviewers` picker.
    /// Indices from `Msg::ChoicePicked` index back into this Vec.
    /// Cleared after the picker dispatches.
    review_choices: Vec<String>,
    /// Same shape but for the add-assignees flow.
    pending_assignees_request: Option<pilot_core::WorkspaceKey>,
    /// Candidate logins shown in the `AddAssignees` picker.
    assignees_choices: Vec<String>,
    /// Workspaces that fell out of scope (filter / scope change) but
    /// have running terminals — the daemon won't auto-remove those.
    /// Each `WorkspaceOutOfScope` event lands here; one at a time
    /// gets surfaced as a Confirm modal so the user decides whether
    /// to kill the running sessions.
    pending_removal_prompts:
        std::collections::VecDeque<(pilot_core::WorkspaceKey, String, Option<String>, usize)>,
    /// Workspace currently being prompted about. Set when the
    /// RemoveOutOfScope modal mounts; consumed by `Msg::Confirmed`.
    active_removal_prompt: Option<pilot_core::WorkspaceKey>,
    /// Pending issue→PR merge prompts. Daemon stalls a merge when
    /// the issue has live sessions and emits
    /// `WorkspaceMergePending`; we queue here and surface one at a
    /// time as a Confirm modal. Tuple: issue key, PR key, issue
    /// label, PR label, live terminal count.
    pending_merge_prompts: std::collections::VecDeque<(
        pilot_core::WorkspaceKey,
        pilot_core::WorkspaceKey,
        String,
        String,
        usize,
    )>,
    /// (issue, PR) pair currently being prompted about. Consumed by
    /// `Msg::Confirmed` when the top modal is `Id::MergeConfirm`.
    active_merge_prompt: Option<(pilot_core::WorkspaceKey, pilot_core::WorkspaceKey)>,
    /// Workspace key whose PR is being confirmed for merge by the
    /// `Shift-M` Confirm modal. Set when the modal mounts, taken on
    /// `Msg::Confirmed` / `Msg::ModalDismissed`.
    /// Source workspace key the `Shift-A` adopt picker is gathering
    /// a target for. Set when the picker mounts; consumed when the
    /// user picks (or dismisses).
    pending_adopt_source: Option<pilot_core::WorkspaceKey>,
    /// Candidate target workspaces for the active adopt picker,
    /// in the same order as the picker's row indices. `Msg::ChoicePicked`
    /// indexes into this to recover the chosen `WorkspaceKey`.
    adopt_choices: Vec<pilot_core::WorkspaceKey>,
    /// Transient UI status (polling spinner + footer notice). See
    /// `StatusCtx`.
    status: StatusCtx,
    /// Resolved values for the magic-number knobs that used to be
    /// module-level `const`s — read from `~/.pilot/config.yaml::ui`,
    /// or `UiDefaults::default()` when unset / not loaded.
    ui_defaults: pilot_config::UiDefaults,
    /// Workspace keys for which we've already fired
    /// `Command::FetchPrDetails` this session — the lazy-fetch path
    /// that back-fills review-thread activity. Used to dedupe the
    /// trigger so a flicker of focus doesn't spam the daemon.
    /// Cleared when a workspace is removed (`Event::WorkspaceRemoved`)
    /// so a re-added workspace gets a fresh fetch.
    pr_details_fetched: std::collections::HashSet<pilot_core::WorkspaceKey>,
    /// Active sidebar right-click context menu state: the workspace
    /// row the menu was raised over plus the ordered list of catalog
    /// `Action`s the picker is offering. `Msg::ChoicePicked` indexes
    /// back into the Vec and dispatches the same IpcCommand the
    /// matching keyboard shortcut would have. None when no menu is
    /// open.
    pending_sidebar_context: Option<(pilot_core::SessionKey, Vec<pilot_tui_core::action::Action>)>,
    /// User-supplied key overrides for catalog actions. Keys are
    /// snake_case `ActionKind` names (see `ActionKind::name`); values
    /// are key-spec strings. Empty when the user hasn't configured
    /// `ui.action_keys` — catalog defaults apply.
    action_key_overrides: std::collections::BTreeMap<String, String>,
    /// Action queued behind an `ActionConfirm` modal. Set by
    /// `mount_action_confirm`, taken (and dispatched if Yes) by
    /// the `Msg::Confirmed` handler. None when no destructive
    /// confirm is currently up.
    pending_action_confirm: Option<pilot_tui_core::action::Action>,
    /// Project the next `Id::NewWorkspace` submit should land the
    /// new workspace under. Set by `mount_new_workspace_input(pk)`
    /// from the focused-project resolver, consumed by
    /// `handle_input_submitted`'s `Id::NewWorkspace` arm.
    pending_new_workspace_project: Option<pilot_core::ProjectKey>,
}

/// Custom Port that drains events from an `mpsc::Receiver`. Pilot
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
/// (optionally) session. Used by `--workspace KEY [--session ID]`
/// and the detach flow that re-spawns pilot with these flags.
#[derive(Debug, Clone)]
pub struct Preselect {
    /// Workspace key (e.g. `"github:owner/repo#42"`) to land on.
    pub workspace_key: pilot_core::SessionKey,
    /// Optional session id to focus inside the workspace. Anything
    /// that doesn't parse as a uuid is silently ignored.
    pub session_id_raw: Option<String>,
}

use crate::realm::layout::{LayoutCtx, pane_areas};
use crate::realm::setup_ctx::{SettingsAction, SetupCtx};
use crate::realm::status_ctx::StatusCtx;

/// How long the first `q` stays armed waiting for the second tap.
// `Q_DOUBLE_TAP_WINDOW` retired — value lives on `ui_defaults`
// now, sourced from `~/.pilot/config.yaml::ui.quit_double_tap_window`
// with `pilot_config::UiDefaults::default()` as the fallback.

/// Escape-char for the terminal-pane breakout sequence. Two
/// consecutive presses (with no intervening non-`]` key) returns
/// focus to the sidebar instead of forwarding to the PTY.
// `TERMINAL_ESCAPE_CHAR` retired — value lives on `ui_defaults`,
// sourced from `~/.pilot/config.yaml::ui.terminal_escape_char`
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
            client,
            redraw: true,
            quit: false,
            setup: SetupCtx::new(),
            modal_event_tx,
            q_latch: crate::confirm_latch::DoubleTapLatch::new(),
            escape_latch: crate::confirm_latch::DoubleTapLatch::new(),
            last_click: None,
            terminal_user_typed_since_focus: false,
            mouse_capture_on: true,
            terminal_selection: None,
            preselect: None,
            layout: LayoutCtx::new(),
            pending_reply: None,
            pending_review_request: None,
            review_choices: Vec::new(),
            pending_assignees_request: None,
            assignees_choices: Vec::new(),
            pending_removal_prompts: std::collections::VecDeque::new(),
            active_removal_prompt: None,
            pending_merge_prompts: std::collections::VecDeque::new(),
            active_merge_prompt: None,
            pending_adopt_source: None,
            adopt_choices: Vec::new(),
            status: StatusCtx::new(),
            ui_defaults: pilot_config::UiDefaults::default(),
            pr_details_fetched: std::collections::HashSet::new(),
            pending_sidebar_context: None,
            action_key_overrides: std::collections::BTreeMap::new(),
            pending_action_confirm: None,
            pending_new_workspace_project: None,
        }
    }
}

impl Model<CrosstermTerminalAdapter> {
    pub fn new(client: Client) -> anyhow::Result<Self> {
        let mut terminal = CrosstermTerminalAdapter::new()?;
        terminal.enable_raw_mode()?;
        terminal.enter_alternate_screen()?;
        // Mouse capture: clicks/drags drive splitter resize +
        // click-to-focus + pilot-side text selection inside the
        // terminal pane (extracted from libghostty's grid, copied
        // via OSC 52). F8 / Alt-s toggles capture off if the user
        // wants the host's native selection (which spans across
        // pilot's UI chrome and is uglier).
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
        // Ask the host terminal to disambiguate modified Enter /
        // Tab / Backspace etc. via the kitty keyboard protocol.
        // Without this, most terminals collapse Shift-Enter into
        // the same byte sequence as Enter and pilot can't tell
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
    pub fn with_setup_complete_hook(
        mut self,
        hook: std::sync::Arc<dyn Fn(crate::setup_flow::SetupOutcome) + Send + Sync>,
    ) -> Self {
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
        sources: std::sync::Arc<Vec<Box<dyn pilot_core::ScopeSource>>>,
    ) {
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        self.setup.inputs = Some((report.clone(), sources.clone()));
        self.setup.runner = Some(crate::setup_flow::SetupRunner::new(report, sources));
        let _ = self.app.mount(
            Id::Splash,
            Box::new(Splash::new()),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        let _ = self.app.active(&Id::Splash);
        self.modal_stack.push(Id::Splash);
        self.redraw = true;
    }

    /// Pre-populate the cached setup inputs without launching the
    /// wizard. `run_embedded_realm` calls this for returning users
    /// so the in-session `reopen_setup` path works without re-
    /// running detection.
    pub fn cache_setup_inputs(
        &mut self,
        report: crate::setup::SetupReport,
        sources: std::sync::Arc<Vec<Box<dyn pilot_core::ScopeSource>>>,
    ) {
        self.setup.inputs = Some((report, sources));
    }

    /// Cache the user's existing PersistedSetup so partial flows
    /// from the Settings palette can pre-seed the wizard with
    /// current state instead of starting from defaults.
    pub fn cache_persisted_setup(&mut self, persisted: pilot_core::PersistedSetup) {
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

    /// Apply `~/.pilot/config.yaml::attention` +
    /// `ui.collapsed_repos` + `agent_shortcuts` to the sidebar at
    /// startup. Must be called before the first daemon Subscribe
    /// so the saved collapse state is in place when the Snapshot
    /// arrives.
    pub fn apply_sidebar_config(
        &mut self,
        attention: pilot_config::AttentionConfig,
        collapsed_repos: std::collections::BTreeSet<String>,
        agent_shortcuts: std::collections::HashMap<char, String>,
        default_agent: Option<String>,
        display: &pilot_config::DisplayConfig,
        ui: &pilot_config::UiDefaults,
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


    /// Apply catalog-driven action key overrides (`ui.action_keys`).
    /// Map of snake_case `ActionKind` names → key-spec strings;
    /// catalog lookups in `find_action_for_chord` consult this map
    /// first and fall back to the catalog default. See
    /// `pilot_tui_core::action::ActionKind::name` for the key
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
        let Some(p) = &self.setup.persisted else { return };
        let mut changed = false;
        for set in p.selected_scopes.values() {
            for scope in set {
                // `provider:owner/repo` → ProjectKey::github(owner,
                // repo). Skip org-level entries (`provider:owner`
                // with no `/`) — those mean "whole org" and the per-
                // repo projects materialize as polling finds them.
                let Some((source, rest)) = scope.split_once(':') else { continue };
                if !rest.contains('/') {
                    continue;
                }
                let pk = match source {
                    "github" => {
                        let (owner, name) = rest.split_once('/').expect("contains '/' verified");
                        pilot_core::ProjectKey::github(owner, name)
                    }
                    "linear" => pilot_core::ProjectKey::linear(rest),
                    _ => continue,
                };
                if !self.projects.contains_key(&pk) {
                    self.projects.insert(
                        pk.clone(),
                        pilot_core::Project::new(pk, rest, chrono::Utc::now()),
                    );
                    changed = true;
                }
            }
        }
        if changed {
            self.sidebar.apply_projects(self.projects.clone());
        }
    }

    /// Send a command to the daemon, logging failures. Wraps the raw
    /// `client.send` so a dead channel (daemon restarted, socket
    /// closed) leaves a breadcrumb in `/tmp/pilot.log` instead of
    /// silently vanishing. Most call sites genuinely don't care if
    /// the send fails (Subscribe is idempotent, terminal-Write loses
    /// keystrokes on a dead channel anyway) — but a silent log helps
    /// debug "I pressed X and nothing happened" after the fact.
    fn send_cmd(&self, cmd: IpcCommand) {
        if let Err(e) = self.client.send(cmd) {
            tracing::warn!("ipc send failed: {e}");
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
    /// from `~/.pilot/config.yaml::ui`. Each value is clamped to
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
        use crate::realm::components::footer::{Notice, NoticeSeverity};

        let Some(workspace_key) = self.sidebar.selected_workspace_key().cloned() else {
            return;
        };
        if self.setup.editors.is_empty() {
            let path = pilot_core::paths::config_yaml();
            self.status.notice = Some(Notice::new(
                format!(
                    "no editor detected — add one under `editors:` in {}",
                    path.display(),
                ),
                NoticeSeverity::Info,
            ));
            self.redraw = true;
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
                    kind: pilot_ipc::TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                });
                self.status.notice = Some(Notice::new(
                    format!(
                        "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                        self.setup.editors[0].display
                    ),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
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
        use tuirealm::subscription::{EventClause, Sub, SubClause};
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
        let _ = self.app.mount(
            Id::Editor,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Editor);
        let _ = self.app.active(&Id::Editor);
        self.redraw = true;
    }

    fn launch_editor(
        &mut self,
        editor: &crate::editors::EditorTemplate,
        worktree: &std::path::Path,
    ) {
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        match crate::editors::launch(editor, worktree) {
            Ok(()) => {
                tracing::info!(
                    editor = %editor.id,
                    worktree = %worktree.display(),
                    "launched editor"
                );
                self.status.notice = Some(Notice::new(
                    format!("opened {} in {}", worktree.display(), editor.display),
                    NoticeSeverity::Info,
                ));
            }
            Err(e) => {
                tracing::warn!("editor launch failed: {e}");
                self.status.notice = Some(Notice::new(
                    format!("failed to launch {}: {e}", editor.display),
                    NoticeSeverity::Permanent,
                ));
            }
        }
        self.redraw = true;
    }

    /// Open the in-session Settings palette. Builds a small picker
    /// with actions like "Add a repo (github)" / "Edit agents" /
    /// etc., scoped to the user's current providers. Falls back to
    /// the full wizard when there's no cached persisted setup yet
    /// (first-run path or `--test` mode).
    pub fn open_settings(&mut self) {
        use crate::realm::components::choice::Choice;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

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
        let _ = self.app.mount(
            Id::Setup,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Setup);
        let _ = self.app.active(&Id::Setup);
        self.redraw = true;
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
        };
        // Pre-seed the accumulator from persisted state so partial
        // flows don't drop the user's other-provider config.
        let outcome = match self.setup.persisted.clone() {
            Some(p) => crate::setup_flow::persisted_to_outcome(p, report),
            None => crate::setup_flow::SetupOutcome::default_enabled(report),
        };
        let (runner, step) = SetupRunner::at_partial(outcome, sources, entry);
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
        // `ESC[200~…ESC[201~` even after pilot exits — every
        // subsequent shell paste shows the literal markers.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste,);
        // Drop the kitty keyboard protocol bits we pushed in `new`.
        // Skipping this would leak the request into the user's host
        // shell after pilot exits — subsequent commands would still
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
        //   knows whether pilot is currently talking to GitHub).
        let polling_status: Option<(&'static str, String)> = if let Some(p) =
            self.status.polling.as_ref()
        {
            Some((p.spinner_glyph(), p.status_label()))
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
            PaneFocus::Sidebar => self.sidebar.contextual_bindings(),
            PaneFocus::Right => self.right.contextual_bindings(),
            PaneFocus::Terminals => self.terminals.contextual_bindings(),
        };
        let notice = self.status.notice.clone();
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
            // leak the highlight across pilot's pane chrome —
            // matches what the user expects from a per-pane
            // selection (compare to the host terminal's native
            // selection, which crosses panes).
            if let Some((start, end)) = self.terminal_selection {
                paint_selection(f.buffer_mut(), right_bottom, start, end);
            }

            // Footer: keymap + polling status + notice.
            crate::realm::components::footer::render(
                f,
                footer_area,
                &keymap,
                polling_status.as_ref().map(|(s, l)| (*s, l.as_str())),
                notice.as_ref(),
            );

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
                    let payload = carrier.take().unwrap_or_else(|| Box::new(()));
                    let step = runner.step_loading_resolved(payload);
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
                tracing::warn!("polling error from {source} ({kind}): {message} — {detail}");
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let severity = match kind.as_str() {
                    "auth" => NoticeSeverity::Auth,
                    "retryable" => NoticeSeverity::Retryable,
                    _ => NoticeSeverity::Permanent,
                };
                self.status.notice = Some(Notice::new(format!("{source}: {message}"), severity));
                self.redraw = true;
            }
            Msg::PollingTimeout => {
                tracing::info!("polling first-cycle timeout — modal dismissed");
            }
            Msg::PollingEmptyInbox(queries) => {
                tracing::info!("polling completed with empty inbox; queries seen: {queries:?}");
            }
        }
    }

    /// Reply textarea submit. Build a `PostReply` for the
    /// workspace that mounted the textarea. Empty bodies dismiss
    /// without posting; the footer "submitted — fetching" notice +
    /// an immediate `Refresh` keep the user from waiting on the
    /// 60s poll loop.
    ///
    /// **Effects**: returns IPC commands as a `Vec` (not sent
    /// inline) so unit tests can drive this handler with fixture
    /// state and assert on the returned commands without a real
    /// IPC client. Notice + modal-stack stay as direct mutations
    /// (tests inspect `Model` state after the call).
    pub fn handle_textarea_submitted(&mut self, body: String) -> Vec<IpcCommand> {
        self.pop_modal();
        let mut cmds = Vec::new();
        let target = self.pending_reply.take();
        if let Some(session_key) = target
            && !body.trim().is_empty()
        {
            cmds.push(IpcCommand::PostReply { session_key, body });
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.status.notice = Some(Notice::new(
                "Reply submitted — fetching…",
                NoticeSeverity::Info,
            ));
            cmds.push(IpcCommand::Refresh);
        }
        cmds
    }

    /// Input modal submit (single-line text). Dispatch by which
    /// Input modal is currently on top. Handles `NewWorkspace`
    /// (→ `CreateWorkspace`), `RequestReviewers`, `AddAssignees`.
    ///
    /// Reviewer / assignee inputs accept comma- or whitespace-
    /// separated logins. The `@` prefix is optional and stripped.
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    pub fn handle_input_submitted(&mut self, text: String) -> Vec<IpcCommand> {
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        let mut cmds = Vec::new();
        match top {
            Some(Id::NewWorkspace) => {
                let name = text.trim().to_string();
                let project_key = self.pending_new_workspace_project.take();
                match (name.is_empty(), project_key) {
                    (false, Some(project_key)) => {
                        tracing::info!(
                            workspace_name = %name,
                            project_key = %project_key,
                            "creating new pre-PR workspace under project",
                        );
                        cmds.push(IpcCommand::CreateWorkspace { name, project_key });
                    }
                    (false, None) => {
                        tracing::warn!(
                            workspace_name = %name,
                            "new-workspace submit without a stashed project_key — dropped",
                        );
                    }
                    _ => {}
                }
            }
            Some(Id::NewProject) => {
                let name = text.trim().to_string();
                if !name.is_empty() {
                    tracing::info!(project_name = %name, "creating new local project");
                    cmds.push(IpcCommand::CreateProject { name });
                }
            }
            // RequestReviewers / AddAssignees used to go through an
            // Input modal but were migrated to a `Choice::multi`
            // picker — see `mount_request_reviewers` /
            // `handle_choice_picked`. The corresponding Input arms
            // were removed; an Input modal under those Ids never
            // mounts anymore, so a stray submit would just fall
            // through to the default arm below.
            _ => {
                // Unknown input source — silently drop. The pop
                // above already cleared the modal.
            }
        }
        cmds
    }

    /// Route a Choice modal pick to the right handler. Five
    /// distinct flows share the same `Msg::ChoicePicked` envelope
    /// (Adopt target, Editor picker, Settings palette, runner-
    /// driven flows, plain pop-on-pick) — this fn fans out by
    /// inspecting the top modal id + setup state.
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    /// The Editor / Settings / runner branches may still emit
    /// commands internally via helper methods (`launch_editor`,
    /// `dispatch_settings_action`, `handle_runner_step`); only
    /// the directly-visible IPC commands land in the Vec.
    pub fn handle_choice_picked(&mut self, picks: Vec<usize>) -> Vec<IpcCommand> {
        let mut cmds = Vec::new();
        // Sidebar right-click context menu. Pick → dispatch the
        // same IpcCommand the matching keyboard shortcut would.
        // Empty pick (Esc) clears the stash silently.
        if matches!(self.modal_stack.last(), Some(Id::SidebarContext)) {
            use pilot_tui_core::action::Action;
            let stash = self.pending_sidebar_context.take();
            self.pop_modal();
            if let (Some((session_key, actions)), Some(&idx)) =
                (stash.as_ref(), picks.first())
                && let Some(action) = actions.get(idx)
            {
                let workspace_key = pilot_core::WorkspaceKey::new(session_key.as_str().to_string());
                match action {
                    Action::SpawnAgent(agent_id) => {
                        cmds.push(IpcCommand::Spawn {
                            session_key: session_key.clone(),
                            session_id: None,
                            kind: pilot_ipc::TerminalKind::Agent(agent_id.clone()),
                            cwd: None,
                            initial_prompt: None,
                        });
                    }
                    Action::SpawnShell => {
                        cmds.push(IpcCommand::Spawn {
                            session_key: session_key.clone(),
                            session_id: None,
                            kind: pilot_ipc::TerminalKind::Shell,
                            cwd: None,
                            initial_prompt: None,
                        });
                    }
                    Action::OpenEditor => {
                        // Same path as the `e` keyboard shortcut.
                        // Selection already moved on to this row
                        // via the right-click hit-test; `open_editor`
                        // operates on whatever's selected.
                        self.open_editor();
                    }
                    Action::MarkAllRead => {
                        cmds.push(IpcCommand::MarkRead {
                            session_key: session_key.clone(),
                        });
                    }
                    Action::MergePr => {
                        cmds.push(IpcCommand::MergePr { workspace_key });
                    }
                    Action::Archive => {
                        cmds.push(IpcCommand::Kill {
                            session_key: session_key.clone(),
                        });
                    }
                    // The menu only offers the six variants above
                    // (see `mount_sidebar_context_menu`'s candidate
                    // list). Anything else is a bug — fail loud so
                    // it surfaces in tests rather than silently
                    // doing nothing.
                    other => tracing::warn!(
                        "sidebar context menu: unhandled action {other:?}",
                    ),
                }
                self.redraw = true;
            }
            return cmds;
        }
        // Adopt picker (Id::AdoptTarget) — pick → send the
        // `Command::AdoptSessions` mapping source→target. Empty
        // pick (Esc → no Msg, but cover the defensive case) drops
        // the stash without firing.
        if matches!(self.modal_stack.last(), Some(Id::AdoptTarget)) {
            let target = picks
                .first()
                .and_then(|i| self.adopt_choices.get(*i).cloned());
            self.adopt_choices.clear();
            self.pop_modal();
            let source = self.pending_adopt_source.take();
            if let (Some(source_key), Some(target_key)) = (source, target) {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                cmds.push(IpcCommand::AdoptSessions {
                    source_workspace_key: source_key.clone(),
                    target_workspace_key: target_key.clone(),
                });
                self.status.notice = Some(Notice::new(
                    format!("adopted sessions: {source_key} → {target_key}"),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
            }
            return cmds;
        }
        // Reviewer picker (Id::RequestReviewers) — picks index
        // into `review_choices`. Empty pick drops the slot.
        if matches!(self.modal_stack.last(), Some(Id::RequestReviewers)) {
            let logins: Vec<String> = picks
                .iter()
                .filter_map(|i| self.review_choices.get(*i).cloned())
                .collect();
            self.review_choices.clear();
            self.pop_modal();
            let workspace_key = self.pending_review_request.take();
            if let (Some(workspace_key), false) = (workspace_key, logins.is_empty()) {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let count = logins.len();
                cmds.push(IpcCommand::RequestReviewers {
                    workspace_key,
                    logins,
                });
                self.status.notice = Some(Notice::new(
                    format!("requested {count} reviewer(s)"),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
            }
            return cmds;
        }
        // Assignees picker (Id::AddAssignees) — same shape.
        if matches!(self.modal_stack.last(), Some(Id::AddAssignees)) {
            let logins: Vec<String> = picks
                .iter()
                .filter_map(|i| self.assignees_choices.get(*i).cloned())
                .collect();
            self.assignees_choices.clear();
            self.pop_modal();
            let workspace_key = self.pending_assignees_request.take();
            if let (Some(workspace_key), false) = (workspace_key, logins.is_empty()) {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let count = logins.len();
                cmds.push(IpcCommand::AddAssignees {
                    workspace_key,
                    logins,
                });
                self.status.notice = Some(Notice::new(
                    format!("added {count} assignee(s)"),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
            }
            return cmds;
        }
        // Editor picker (Id::Editor) — pick → launch (or defer
        // behind a session-spawn when the workspace has no
        // worktree yet).
        if matches!(self.modal_stack.last(), Some(Id::Editor)) {
            let editor = picks
                .first()
                .and_then(|i| self.setup.editor_choices.get(*i).cloned());
            self.setup.editor_choices.clear();
            self.pop_modal();
            let Some(editor) = editor else { return cmds };
            if let Some(workspace_key) = self.setup.pending_editor_workspace.take() {
                self.setup.pending_editor_launch = Some((workspace_key.clone(), editor.clone()));
                cmds.push(IpcCommand::Spawn {
                    session_key: workspace_key.clone(),
                    session_id: None,
                    kind: pilot_ipc::TerminalKind::Shell,
                    cwd: None,
                    initial_prompt: None,
                });
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                self.status.notice = Some(Notice::new(
                    format!(
                        "Provisioning worktree for {workspace_key} — opening in {} when ready…",
                        editor.display
                    ),
                    NoticeSeverity::Info,
                ));
                self.redraw = true;
                return cmds;
            }
            // Worktree already on disk — launch directly.
            let worktree = self
                .sidebar
                .selected_workspace()
                .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()));
            if let Some(worktree) = worktree {
                self.launch_editor(&editor, &worktree);
            }
            return cmds;
        }
        // Settings palette is a non-runner Choice modal — if the
        // user just picked an action, route into a partial wizard
        // flow before falling through.
        if !self.setup.settings_actions.is_empty()
            && matches!(self.modal_stack.last(), Some(Id::Setup))
            && self.setup.runner.is_none()
        {
            let action = picks
                .first()
                .and_then(|i| self.setup.settings_actions.get(*i).cloned());
            self.setup.settings_actions.clear();
            self.pop_modal();
            if let Some(action) = action {
                self.dispatch_settings_action(action);
            }
            return cmds;
        }
        if let Some(mut runner) = self.setup.runner.take() {
            let step = runner.step_choice_picked(picks);
            self.handle_runner_step(runner, step);
        } else {
            self.pop_modal();
        }
        cmds
    }

    /// `Esc` / mount-stack pop. Setup wizard takes priority; the
    /// non-runner case routes by which prompt was on top so the
    /// daemon learns the "no" decision (merge stalls would otherwise
    /// re-prompt on the next poll).
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    /// Note: the setup-runner branch may still send commands
    /// internally via `handle_runner_step`; tests that drive the
    /// wizard path need to mock at a different layer.
    pub fn handle_modal_dismissed(&mut self) -> Vec<IpcCommand> {
        if let Some(mut runner) = self.setup.runner.take() {
            let step = runner.step_dismissed();
            self.handle_runner_step(runner, step);
            return Vec::new();
        }
        // Dispatch by which modal was on top BEFORE the pop so we
        // route the "no" decision correctly.
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        let mut cmds = Vec::new();
        match top {
            Some(Id::RemoveOutOfScope) => {
                self.active_removal_prompt = None;
            }
            Some(Id::MergeConfirm) => {
                // Esc on the merge modal = "no, keep them
                // separate." Tell the daemon to drop the stall so
                // future polls don't re-prompt.
                if let Some((issue_key, pr_key)) = self.active_merge_prompt.take() {
                    cmds.push(IpcCommand::ConfirmMerge {
                        issue_workspace_key: issue_key,
                        pr_workspace_key: pr_key,
                        accept: false,
                    });
                }
            }
            Some(Id::ActionConfirm) => {
                // Esc = cancel destructive action; drop the
                // queued Action without firing.
                self.pending_action_confirm = None;
            }
            _ => {}
        }
        // Always try to surface a queued prompt after a modal
        // dismisses — not just when the dismissed modal itself was
        // a prompt. Otherwise a user who has Help / Settings open
        // when the daemon emits a prompt would have it stuck in
        // the queue.
        self.maybe_mount_next_removal_prompt();
        self.maybe_mount_next_merge_prompt();
        cmds
    }

    /// `y` / `n` answer on a ConfirmModal. Routes by which modal
    /// id was on top; each branch maps `yes` to a side-effect
    /// (kill workspace, post merge-confirm to daemon, etc.).
    ///
    /// **Effects**: returns commands as a `Vec` for testability.
    pub fn handle_confirmed(&mut self, yes: bool) -> Vec<IpcCommand> {
        let top = self.modal_stack.last().cloned();
        self.pop_modal();
        let mut cmds = Vec::new();
        match top {
            Some(Id::RemoveOutOfScope) => {
                let target = self.active_removal_prompt.take();
                if yes && let Some(workspace_key) = target {
                    // Kill terminals + delete workspace.
                    let session_key: pilot_core::SessionKey = (&workspace_key).into();
                    cmds.push(IpcCommand::Kill { session_key });
                }
            }
            Some(Id::MergeConfirm) => {
                if let Some((issue_key, pr_key)) = self.active_merge_prompt.take() {
                    cmds.push(IpcCommand::ConfirmMerge {
                        issue_workspace_key: issue_key,
                        pr_workspace_key: pr_key,
                        accept: yes,
                    });
                }
            }
            Some(Id::ActionConfirm) => {
                // Unified destructive-action confirm. Yes →
                // dispatch the queued action via the unchecked
                // path (the gate already fired). No / Esc → drop
                // the stash silently.
                let pending = self.pending_action_confirm.take();
                if yes && let Some(action) = pending {
                    cmds.extend(self.dispatch_action_unchecked(&action));
                    self.redraw = true;
                }
            }
            Some(Id::CleanWorktreesConfirm) => {
                if yes {
                    use crate::realm::components::footer::{Notice, NoticeSeverity};
                    cmds.push(IpcCommand::CleanWorktrees);
                    // The work happens asynchronously on the daemon
                    // (filesystem walk + git worktree remove per
                    // session) — surface a placeholder notice so the
                    // user knows the click registered. The final
                    // count comes back via
                    // `Event::CleanWorktreesCompleted`.
                    self.status.notice = Some(Notice::new(
                        "cleaning worktrees…",
                        NoticeSeverity::Info,
                    ));
                    self.redraw = true;
                }
            }
            _ => {}
        }
        self.maybe_mount_next_removal_prompt();
        self.maybe_mount_next_merge_prompt();
        cmds
    }

    /// Apply a [`crate::setup_flow::RunnerStep`] returned by the
    /// runner — mount the next modal, fire the on-complete hook, or
    /// drop the wizard. The `runner` argument lets us conditionally
    /// hold on to the runner across step transitions: `Next` puts it
    /// back; `Finish` / `Cancel` drop it.
    fn handle_runner_step(
        &mut self,
        runner: crate::setup_flow::SetupRunner,
        step: crate::setup_flow::RunnerStep,
    ) {
        use crate::setup_flow::RunnerStep;
        match step {
            RunnerStep::Next(component) => {
                self.setup.runner = Some(runner);
                self.mount_setup_modal(component);
            }
            RunnerStep::Finish(outcome) => {
                let sources: Vec<String> = outcome.enabled_providers.iter().cloned().collect();
                // Cache the new persisted state so subsequent partial
                // flows (Settings → Add a repo) see the latest scopes.
                self.setup.persisted = Some(crate::setup_flow::outcome_to_persisted(&outcome));
                // Push the new repo subscriptions into the sidebar so
                // the user sees a header for the freshly-added repo
                // even before polling finds workspaces under it.
                self.refresh_subscribed_projects();
                if let Some(hook) = self.setup.on_complete.as_ref() {
                    hook(outcome);
                }
                self.unmount_setup_modal();
                self.send_cmd(IpcCommand::Subscribe);
                // Kick off an immediate poll so a freshly added repo
                // surfaces its open PRs/issues within seconds instead
                // of waiting for the long-lived 60s loop tick.
                self.send_cmd(IpcCommand::Refresh);
                self.set_focus_attr();
                if !sources.is_empty() {
                    self.show_polling(sources);
                }
            }
            RunnerStep::Cancel => {
                self.unmount_setup_modal();
                self.send_cmd(IpcCommand::Subscribe);
                self.set_focus_attr();
            }
            RunnerStep::Stay => {
                self.setup.runner = Some(runner);
            }
        }
    }

    /// Unmount whatever's at `Id::Setup` (or `Id::Splash` if the
    /// wizard hasn't moved off splash yet) and mount `component`
    /// there. The setup id is reused — only one wizard step is ever
    /// live at a time.
    fn mount_setup_modal(
        &mut self,
        component: Box<dyn tuirealm::component::AppComponent<Msg, UserEvent>>,
    ) {
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        // Unmount whatever's on top.
        if let Some(top) = self.modal_stack.last().cloned() {
            let _ = self.app.umount(&top);
            self.modal_stack.pop();
        }
        let _ = self.app.mount(
            Id::Setup,
            component,
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Setup);
        let _ = self.app.active(&Id::Setup);
        self.redraw = true;
    }

    /// Drop whatever setup-related modal is on top of the stack.
    /// Called on Finish / Cancel.
    fn unmount_setup_modal(&mut self) {
        if let Some(top) = self.modal_stack.last().cloned() {
            let _ = self.app.umount(&top);
            self.modal_stack.pop();
        }
        if let Some(top) = self.modal_stack.last() {
            let _ = self.app.active(top);
        }
        self.redraw = true;
    }

    /// Single fan-out from a catalog `Action` to its effect (IPC
    /// command, modal mount, focus shift, …). Surfaces (keyboard,
    /// right-click menu, future remap UI) all call this so behavior
    /// stays consistent across them.
    ///
    /// **Returns** the IPC commands the action produces, if any.
    /// UI-only effects (modal mounts, focus moves) happen via
    /// `&mut self` and aren't reflected in the return.
    ///
    /// **Coverage**: today this handles the *simple* workspace
    /// actions whose effect is one IpcCommand against the focused
    /// row. The complex ones (Work / Snooze / AdoptSessions / Reply
    /// / etc.) still live in `handle_pane_key`'s match arms — they
    /// either need extra resolver / modal logic or already route
    /// cleanly through the existing `Intent` resolvers. As panes'
    /// key handlers migrate to use this dispatcher, more cases
    /// move in.
    pub fn dispatch_action(
        &mut self,
        action: &pilot_tui_core::action::Action,
    ) -> Vec<IpcCommand> {
        use pilot_tui_core::action::ActionDef;
        // Destructive gate, type-system enforced via the catalog.
        // Every destructive action is routed through the unified
        // Confirm modal first; the pending action lives in
        // `pending_action_confirm` and fires on `Msg::Confirmed(true)`.
        // This is the *only* path through `dispatch_action` for
        // destructive variants — there's no way to fire one
        // without the user confirming.
        if ActionDef::for_action(action).is_destructive() {
            self.mount_action_confirm(action.clone());
            return Vec::new();
        }
        self.dispatch_action_unchecked(action)
    }

    /// Internal: actually carry out an action without checking the
    /// destructive flag. Public `dispatch_action` gates on
    /// `is_destructive` and routes through the Confirm modal for
    /// the destructive ones — this method is what the modal's
    /// `Msg::Confirmed(true)` handler calls AFTER the user
    /// approved.
    ///
    /// Callers OTHER than `dispatch_action` and the
    /// `ActionConfirm` Yes-handler must not exist. Keeping it
    /// `pub(crate)` so the type system makes that hard to break.
    pub(crate) fn dispatch_action_unchecked(
        &mut self,
        action: &pilot_tui_core::action::Action,
    ) -> Vec<IpcCommand> {
        use pilot_tui_core::action::Action;
        let mut cmds = Vec::new();
        // Workspace-scoped actions need a target — grab the
        // sidebar's selection. Mismatch (no selection) silently
        // drops the action; the catalog's `availability` gates the
        // surface from offering it in that state.
        let session_key = self.sidebar.selected_workspace_key().cloned();
        // `session_id` is non-None when the cursor sits on a
        // session sub-row of a workspace; passing it makes the
        // daemon target that specific session instead of picking
        // / creating one. Matches the sidebar's existing spawn
        // handlers — without this, `c` / `s` on a focused session
        // would silently spawn into the wrong session.
        let session_id = self.sidebar.selected_session_id();
        match action {
            Action::SpawnShell => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        session_key: sk,
                        session_id,
                        kind: pilot_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                    });
                }
            }
            Action::SpawnAgent(agent_id) => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        session_key: sk,
                        session_id,
                        kind: pilot_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                    });
                }
            }
            Action::Work => {
                // Polymorphic spawn driven by `classify_work`:
                // PR-with-failing-CI gets "fix CI", issue gets
                // "implement issue", PR with open review threads
                // gets "address review", … Resolver returns
                // SpawnAgent with the right prompt, the dispatcher
                // just translates to IpcCommand.
                let default_agent = self.sidebar.default_agent().to_string();
                let workspace = self.sidebar.selected_workspace().cloned();
                let intent = crate::intent::resolve_work(workspace.as_ref(), &[], &default_agent);
                if let crate::intent::Intent::SpawnAgent {
                    workspace_key,
                    agent_id,
                    prompt,
                } = intent
                {
                    cmds.push(IpcCommand::Spawn {
                        session_key: (&workspace_key).into(),
                        session_id,
                        kind: pilot_ipc::TerminalKind::Agent(agent_id),
                        cwd: None,
                        initial_prompt: prompt,
                    });
                }
            }
            Action::OpenEditor => {
                // `open_editor` is the orchestrator's existing
                // helper — it picks the right template (single
                // editor → launch directly; multiple → mount
                // picker; none → footer notice).
                self.open_editor();
            }
            Action::NewWorkspace => {
                let focused = self.sidebar.focused_project_key();
                match crate::intent::resolve_new_workspace(focused) {
                    crate::intent::Intent::MountNewWorkspaceInput { project_key } => {
                        self.mount_new_workspace_input(project_key);
                    }
                    crate::intent::Intent::Notice(msg) => {
                        use crate::realm::components::footer::{Notice, NoticeSeverity};
                        self.status.notice =
                            Some(Notice::new(msg, NoticeSeverity::Info));
                        self.redraw = true;
                    }
                    _ => {}
                }
            }
            Action::NewProject => {
                self.mount_new_project_input();
            }
            Action::MarkAllRead => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::MarkRead { session_key: sk });
                }
            }
            Action::Archive => {
                // Destructive — only reachable from
                // `dispatch_action_unchecked` after the user said
                // Yes on the unified ActionConfirm modal. Just
                // fire the Kill.
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Kill { session_key: sk });
                }
            }
            Action::AdoptSessions => {
                // Resolver decides "has sessions to adopt?": yes
                // → mount the target picker; no → footer notice.
                // Same shape as the inline handler had.
                let workspace = self.sidebar.selected_workspace().cloned();
                match crate::intent::resolve_adopt(workspace.as_ref()) {
                    crate::intent::Intent::MountAdoptPicker { source_key } => {
                        self.mount_adopt_picker(source_key);
                    }
                    crate::intent::Intent::Notice(msg) => {
                        use crate::realm::components::footer::{Notice, NoticeSeverity};
                        self.status.notice = Some(Notice::new(msg, NoticeSeverity::Info));
                        self.redraw = true;
                    }
                    _ => {}
                }
            }
            Action::ToggleSnooze => {
                // Resolver decides Snooze (when not snoozed) vs
                // Unsnooze (when snoozed) based on the workspace
                // state. The catalog dispatch reads
                // `ui_defaults.short_snooze` so the user's YAML
                // override (`ui.short_snooze`) drives the duration.
                let now = chrono::Utc::now();
                let workspace = self.sidebar.selected_workspace().cloned();
                let intent = crate::intent::resolve_short_snooze(
                    workspace.as_ref(),
                    now,
                    self.ui_defaults.short_snooze,
                );
                match intent {
                    crate::intent::Intent::Snooze {
                        session_key,
                        duration,
                    } => {
                        let until = now
                            + chrono::Duration::from_std(duration)
                                .unwrap_or(chrono::Duration::hours(4));
                        cmds.push(IpcCommand::Snooze { session_key, until });
                    }
                    crate::intent::Intent::Unsnooze { session_key } => {
                        cmds.push(IpcCommand::Unsnooze { session_key });
                    }
                    _ => {}
                }
            }
            Action::MergePr => {
                // Destructive — only reachable from
                // `dispatch_action_unchecked` after the user said
                // Yes on the unified ActionConfirm. Re-check the
                // merge preconditions defensively, then fire the
                // IPC. (Catalog availability gates the surface
                // from offering the action when CI / review /
                // conflict state isn't ready, so this re-check
                // mostly catches the rare race where state
                // changed while the modal was open.)
                let workspace = self.sidebar.selected_workspace().cloned();
                if let crate::intent::Intent::MergePr { workspace_key } =
                    crate::intent::resolve_merge(workspace.as_ref())
                {
                    cmds.push(IpcCommand::MergePr { workspace_key });
                }
            }
            Action::Refresh => {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                cmds.push(IpcCommand::Refresh);
                // Pre-arm the bg_poll indicator so the user gets
                // feedback on the keystroke — same as the
                // `Shift+R` handler did inline before.
                self.status.note_poll_progress("github", "manual refresh requested");
                self.status.notice = Some(Notice::new(
                    "refreshing…".to_string(),
                    NoticeSeverity::Hint,
                ));
                self.redraw = true;
            }
            Action::OpenHelp => {
                self.mount_help();
            }
            Action::OpenSettings => {
                self.open_settings();
            }
            Action::JumpToAsking => {
                if self.sidebar.focus_next_asking_workspace() {
                    self.focus = PaneFocus::Sidebar;
                    self.set_focus_attr();
                    self.redraw = true;
                }
            }
            Action::Reply => {
                // Reply targets the focused workspace. Resolver
                // returns `Intent::MountReply` when a workspace is
                // selected; we mount the textarea modal. Fires from
                // both Sidebar and Right (catalog Section::Workspace
                // covers both focuses).
                let intent = crate::intent::resolve_reply(self.sidebar.selected_workspace());
                if let crate::intent::Intent::MountReply { workspace_key } = intent {
                    let session_key: pilot_core::SessionKey = (&workspace_key).into();
                    self.mount_reply(session_key);
                }
            }
            Action::RequestReviewers => {
                if let Some(ws) = self.sidebar.selected_workspace()
                    && ws.pr.is_some()
                {
                    let ws_key = ws.key.clone();
                    self.mount_request_reviewers(ws_key);
                }
            }
            Action::AddAssignees => {
                if let Some(ws) = self.sidebar.selected_workspace() {
                    // Assignment requires a GraphQL Assignable id —
                    // PR or gh issue with a node_id. Empty pre-PR
                    // workspaces don't qualify.
                    let has_target = ws.pr.as_ref().map(|p| p.node_id.is_some()).unwrap_or(false)
                        || ws
                            .gh_issues
                            .first()
                            .map(|i| i.node_id.is_some())
                            .unwrap_or(false);
                    if has_target {
                        let ws_key = ws.key.clone();
                        self.mount_add_assignees(ws_key);
                    }
                }
            }
            // Actions not yet handled here stay in the existing
            // handlers. As we migrate, the per-key match arms in
            // `handle_pane_key` and the pane wrappers get deleted
            // and the case lands here.
            other => {
                tracing::debug!(
                    "dispatch_action: {other:?} not yet migrated; falling back to legacy handler",
                );
            }
        }
        cmds
    }

    /// Top-level key handler when no modal is active. Routes Tab,
    /// global escapes, and forwards everything else to the focused
    /// pane wrapper.
    fn handle_pane_key(&mut self, key: RealmKey) {
        match key.code {
            // Tab cycles panes — but ONLY when the active pane has
            // no PTY swallowing keys. Inside a terminal with a live
            // PTY, Tab belongs to the shell / agent; the `]]`
            // escape sequence is the only way out (tmux-style
            // prefix model). With no terminals running, Tab cycles
            // normally — there's no inner program to forward it to.
            Key::Tab
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && (self.focus != PaneFocus::Terminals
                        || self.terminals.is_empty()
                        || !self.terminal_user_typed_since_focus) =>
            {
                // Empty terminal pane OR fresh-entry-no-typing-yet →
                // cycle focus instead of forwarding Tab to the PTY.
                // After the user has typed even one character in this
                // focus session the flag flips and Tab goes to the
                // shell for autocomplete.
                self.q_latch.disarm();
                self.focus = self.focus.next();
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            _ if self.focus != PaneFocus::Terminals && self.matches_quit_chord(&key) => {
                // Quit chord (catalog `ActionKind::Quit`, default `q q`,
                // overridable via `ui.action_keys.quit`). `Double(inner)`
                // is the two-press latch; `Single` fires on first press.
                let chord = self.resolve_quit_chord();
                use pilot_tui_core::action::KeyChord;
                if matches!(chord, Some(KeyChord::Single { .. })) {
                    self.quit = true;
                    return;
                }
                if self.q_latch.tap(self.ui_defaults.quit_double_tap_window) {
                    self.quit = true;
                    return;
                }
                self.redraw = true;
                return;
            }
            // `?` Help, `!` JumpToAsking — both go through the
            // catalog dispatch above (Section::Global).
            // `Enter` on the sidebar = "open this row" → focus the
            // Activity pane so the user can read comments / reply.
            // Used to be a dead binding before this migration; right
            // pane keeps its own Enter meaning (toggle section);
            // terminals forward Enter as `\r` to the PTY.
            _ if self.focus == PaneFocus::Sidebar
                && key.code == Key::Enter
                && key.modifiers.is_empty() =>
            {
                self.q_latch.disarm();
                self.focus = PaneFocus::Right;
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            // Shift-arrows: resize splitters. Disabled inside a
            // terminal so the shell can still bind them.
            Key::Left | Key::Right | Key::Up | Key::Down
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    && self.focus != PaneFocus::Terminals =>
            {
                self.q_latch.disarm();
                let (dx, dy) = match key.code {
                    Key::Left => (-self.ui_defaults.split_step_percent, 0),
                    Key::Right => (self.ui_defaults.split_step_percent, 0),
                    Key::Up => (0, -self.ui_defaults.split_step_percent),
                    Key::Down => (0, self.ui_defaults.split_step_percent),
                    _ => (0, 0),
                };
                if self.layout.nudge_splits(dx, dy) {
                    self.redraw = true;
                }
                return;
            }
            // Ctrl-Shift-D: detach the focused pane into a new pilot
            // process. Many terminals report Ctrl-Shift-letter as the
            // capital letter with CONTROL set; some include SHIFT too.
            // Match either form.
            Key::Char('D')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.focus != PaneFocus::Terminals =>
            {
                self.q_latch.disarm();
                if let Some(spec) = self.focused_detach_spec() {
                    spawn_detached_pilot(&spec);
                }
                return;
            }
            // Toggle pilot's mouse capture so the host terminal
            // (Ghostty / iTerm2) regains native text selection. When
            // OFF the user can trackpad-select inside claude / shell
            // scrollback and Cmd-C normally; toggle back on for
            // splitter drag etc. Bound to multiple chords because
            // terminals report Ctrl-Shift-S inconsistently and
            // Ctrl-S itself is XOFF flow control:
            //   - F8         — function key, never conflicts with TTY
            //   - Alt-s      — Option-s on Mac (Alt-s elsewhere)
            //   - Ctrl-Alt-s — extra fallback for non-mac users
            // Available from any pane (including Terminals) so users
            // in claude can escape to a copy gesture without breaking
            // flow.
            Key::Function(8) => {
                self.q_latch.disarm();
                self.toggle_mouse_capture();
                return;
            }
            Key::Char('s')
                if key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.q_latch.disarm();
                self.toggle_mouse_capture();
                return;
            }
            Key::Char('s' | 'S')
                if key.modifiers.contains(KeyModifiers::ALT)
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.q_latch.disarm();
                self.toggle_mouse_capture();
                return;
            }
            // Reply (`r`), RequestReviewers (`Shift-V`), AddAssignees
            // (`Shift-G`) all dispatch via the catalog (Section::
            // Workspace, accepted from both Sidebar and Right focus).
            // See `find_action_for_chord` + the whitelist in
            // `dispatch_focused_key`.
            // `e` OpenEditor, `n` NewWorkspace, `Shift-N` NewProject,
            // `,` Settings — all go through catalog dispatch above.
            // Shift-R refresh is handled by the catalog dispatch
            // path — `Action::Refresh`'s `dispatch_action` arm
            // pushes `IpcCommand::Refresh` and pre-arms the
            // footer's bg-poll spinner so the user gets keystroke
            // feedback before the first PollProgress lands.
            // Shift-A from the sidebar: open the "adopt sessions"
            // Shift+A AdoptSessions is handled by the catalog
            // dispatch (`Action::AdoptSessions`) — same resolver,
            // same modal mount, same Notice fallback when no
            // sessions to adopt.
            _ => {
                // Any other key disarms.
                self.q_latch.disarm();
            }
        }

        // Terminal-pane escape sequence (`]]` by default). Two
        // consecutive presses of the escape char inside a terminal
        // return focus to the sidebar instead of forwarding to the
        // PTY. The first `]` is held back; if a non-`]` key arrives
        // before the second `]`, the held char is flushed to the PTY
        // first so the user's `]` isn't silently swallowed.
        if self.focus == PaneFocus::Terminals
            && key.modifiers.is_empty()
            && matches!(key.code, Key::Char(c) if c == self.ui_defaults.terminal_escape_char)
        // (escape-char dispatch reads from `ui_defaults.terminal_escape_char`)
        {
            // The escape sequence is the SAME key twice in a row, so
            // a fixed "long enough" window is fine — any other key
            // arriving between the two `]`s falls through to the
            // flush-held branch below. Use a generous 1s window so a
            // hesitant user still gets out.
            const ESCAPE_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);
            if self.escape_latch.tap(ESCAPE_WINDOW) {
                self.focus = PaneFocus::Sidebar;
                self.set_focus_attr();
                self.redraw = true;
                return;
            }
            return;
        }
        if self.focus == PaneFocus::Terminals && self.escape_latch.is_armed() {
            self.escape_latch.disarm();
            // Non-`]` key arrived after a held `]` — flush the held
            // char to the PTY before the new key, so typing patterns
            // like `]a` aren't lost.
            let mut held_cmds: Vec<IpcCommand> = Vec::new();
            let held = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(self.ui_defaults.terminal_escape_char),
                crossterm::event::KeyModifiers::NONE,
            );
            self.terminals.handle_key_direct(held, &mut held_cmds);
            for cmd in held_cmds {
                self.send_cmd(cmd);
            }
        }

        // We have a typed key already; skip the synthetic Event
        // round-trip and call the pane wrappers' direct entry points.
        let ct = realm_key_to_crossterm(&key);
        let mut cmds: Vec<IpcCommand> = Vec::new();

        // Catalog lookup first. If the keystroke matches a catalog
        // `Action` AND `dispatch_action` knows how to handle it
        // (returns a non-empty Vec or mutates state), the pane's
        // direct handler is skipped. Per-key match arms in the
        // panes still cover what `dispatch_action` doesn't yet —
        // see that function's coverage comment.
        if self.focus != PaneFocus::Terminals
            && let Some(chord) = key_event_to_chord(ct)
            && let Some(def) = find_action_for_chord(&chord, self.focus, &self.action_key_overrides)
        {
            use pilot_tui_core::action::Action;
            // Reconstruct a runtime Action from the static ActionDef.
            // `SpawnAgent` is the only variant with runtime data
            // (the agent id) — we don't yet have per-agent catalog
            // entries (`c` → claude, `x` → codex, …), so we let
            // those keys fall through to the pane handler. Once
            // the catalog grows per-agent entries (driven by the
            // user's enabled agents list), this map widens.
            // Whitelist of catalog actions whose `dispatch_action`
            // path is fully equivalent to the existing per-pane
            // handler. Excluded: `MergePr` (sidebar mounts a
            // Confirm modal before firing) + `Archive` (sidebar
            // uses a two-press latch). Adding those here would
            // bypass the safety affordance — they migrate when
            // dispatch_action grows the confirm / latch wrappers.
            //
            // `OpenEditor`, `NewWorkspace`, `NewProject`,
            // `OpenHelp`, `OpenSettings`, `Refresh` already have
            // global match arms in `handle_pane_key` that fire
            // before this point — the whitelist entries here cover
            // the case where focus is Sidebar and the user remaps
            // those keys via `ui.action_keys`. The legacy arms
            // still win when the user hasn't overridden, so
            // behavior is unchanged.
            let action: Option<Action> = match def.kind {
                pilot_tui_core::action::ActionKind::SpawnShell => Some(Action::SpawnShell),
                pilot_tui_core::action::ActionKind::MarkAllRead => Some(Action::MarkAllRead),
                pilot_tui_core::action::ActionKind::Work => Some(Action::Work),
                pilot_tui_core::action::ActionKind::OpenEditor => Some(Action::OpenEditor),
                pilot_tui_core::action::ActionKind::NewWorkspace => Some(Action::NewWorkspace),
                pilot_tui_core::action::ActionKind::NewProject => Some(Action::NewProject),
                pilot_tui_core::action::ActionKind::MergePr => Some(Action::MergePr),
                pilot_tui_core::action::ActionKind::Archive => Some(Action::Archive),
                pilot_tui_core::action::ActionKind::ToggleSnooze => Some(Action::ToggleSnooze),
                pilot_tui_core::action::ActionKind::Refresh => Some(Action::Refresh),
                pilot_tui_core::action::ActionKind::AdoptSessions => Some(Action::AdoptSessions),
                pilot_tui_core::action::ActionKind::Reply => Some(Action::Reply),
                pilot_tui_core::action::ActionKind::RequestReviewers => {
                    Some(Action::RequestReviewers)
                }
                pilot_tui_core::action::ActionKind::AddAssignees => Some(Action::AddAssignees),
                pilot_tui_core::action::ActionKind::OpenHelp => Some(Action::OpenHelp),
                pilot_tui_core::action::ActionKind::OpenSettings => Some(Action::OpenSettings),
                pilot_tui_core::action::ActionKind::JumpToAsking => Some(Action::JumpToAsking),
                _ => None,
            };
            if let Some(action) = action {
                // Any catalog dispatch counts as "non-quit key" so
                // the q q chord resets. Matches every legacy match
                // arm in handle_pane_key — every one of those
                // disarms before doing work.
                self.q_latch.disarm();
                cmds.extend(self.dispatch_action(&action));
                // Drain queued cmds + early return — the catalog
                // handled the key, the pane shouldn't see it.
                self.sync_panes();
                for cmd in cmds {
                    self.send_cmd(cmd);
                }
                self.redraw = true;
                return;
            }
        }

        match self.focus {
            PaneFocus::Sidebar => self.sidebar.handle_key_direct(ct, &mut cmds),
            PaneFocus::Right => self.right.handle_key_direct(ct, &mut cmds),
            // Terminals pane with NO active terminal can't route to a
            // PTY. The empty-state hint says "press s for shell, c
            // for claude" — those bindings live on Sidebar, so we
            // forward there instead. PTY-routing resumes once the
            // first TerminalSpawned arrives.
            PaneFocus::Terminals if self.terminals.is_empty() => {
                self.sidebar.handle_key_direct(ct, &mut cmds);
            }
            PaneFocus::Terminals => {
                // Anything routed to the PTY counts as "user typed":
                // Tab gates above won't see this key as a cycle
                // trigger anymore.
                self.terminal_user_typed_since_focus = true;
                self.terminals.handle_key_direct(ct, &mut cmds);
            }
        }
        // Surface spawn intent in the footer so the user sees that
        // worktree creation / process startup is happening (can take
        // 1-3s on first session). The notice clears when the matching
        // `TerminalSpawned` arrives in `handle_daemon_event`.
        for cmd in &cmds {
            if let IpcCommand::Spawn { kind, .. } = cmd {
                use crate::realm::components::footer::{Notice, NoticeSeverity};
                let label = match kind {
                    pilot_ipc::TerminalKind::Shell => "shell".to_string(),
                    pilot_ipc::TerminalKind::Agent(a) => a.to_string(),
                    other => format!("{other:?}").to_lowercase(),
                };
                self.status.notice = Some(Notice::new(
                    format!("Spawning {label}…"),
                    NoticeSeverity::Info,
                ));
            }
        }
        // Rewrite Spawn-with-initial_prompt → InjectPrompt when an
        // agent terminal already exists for the workspace. The user
        // pressing `w` on a PR that already has a running claude tab
        // expects the new prompt to land in that claude (continue the
        // conversation), not a second claude tab. Same shape works
        // for codex / cursor / generic agents.
        for cmd in cmds {
            let rewritten = match cmd {
                IpcCommand::Spawn {
                    session_key,
                    session_id,
                    kind: pilot_ipc::TerminalKind::Agent(agent_id),
                    cwd,
                    initial_prompt: Some(prompt),
                } => {
                    if let Some(terminal_id) =
                        self.sidebar.find_agent_terminal(&session_key, &agent_id)
                    {
                        use crate::realm::components::footer::{Notice, NoticeSeverity};
                        self.status.notice = Some(Notice::new(
                            format!("→ injecting into existing {agent_id}"),
                            NoticeSeverity::Hint,
                        ));
                        IpcCommand::InjectPrompt {
                            terminal_id,
                            prompt,
                        }
                    } else {
                        IpcCommand::Spawn {
                            session_key,
                            session_id,
                            kind: pilot_ipc::TerminalKind::Agent(agent_id),
                            cwd,
                            initial_prompt: Some(prompt),
                        }
                    }
                }
                other => other,
            };
            self.send_cmd(rewritten);
        }
        // (Shift+M "Merge PR #N?" used to queue a pending request
        // here that the orchestrator drained; that's gone — the
        // catalog path in `dispatch_action` mounts the confirm
        // directly when the key fires.)
        // Sidebar j/k changes selection — propagate to right + terminals.
        self.sync_panes();
        self.redraw = true;
    }

    /// Returns true when the q-q latch is armed (used by the bottom
    /// hint bar to show "press q again" briefly).
    pub fn q_arm_pending(&self) -> bool {
        self.q_latch.is_armed()
    }

    /// Read-only accessor — which pane currently has focus. Used by
    /// tests + (in future) the bottom hint bar.
    pub fn focus(&self) -> PaneFocus {
        self.focus
    }

    /// Sidebar / right / activity split percentages — exposed so tests
    /// can verify Shift-arrow + drag updates apply correctly.
    pub fn split_pcts(&self) -> (u16, u16) {
        (self.layout.sidebar_pct, self.layout.right_top_pct)
    }

    /// Top of the modal stack (or None if no modal is mounted). Used
    /// by tests to verify that `?` mounts the help modal, etc.
    pub fn top_modal(&self) -> Option<&Id> {
        self.modal_stack.last()
    }

    /// Test entry point: drive a key through `handle_pane_key`. Lets
    /// integration tests bypass the run-loop's crossterm polling.
    pub fn dispatch_key(&mut self, key: RealmKey) {
        self.handle_pane_key(key);
    }

    /// Test entry point: drive a key through the *modal* pipeline —
    /// send into `modal_event_tx`, poll `app.tick` until the modal
    /// produces a Msg (or a short deadline elapses), then `update`
    /// each Msg. Mirrors the runloop's modal branch (lines ~2049-2106
    /// in `run_loop`). Exists because `dispatch_key` calls
    /// `handle_pane_key`, which is gated on an empty modal stack and
    /// therefore can't exercise key handling for a mounted Confirm,
    /// Input, etc.
    pub fn dispatch_modal_key(&mut self, key: RealmKey) {
        let _ = self.modal_event_tx.send(RealmEvent::Keyboard(key));
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            match self.app.tick(PollStrategy::Once(Duration::ZERO)) {
                Ok(messages) if !messages.is_empty() => {
                    for msg in messages {
                        self.update(msg);
                    }
                    return;
                }
                Ok(_) => {}
                Err(_) => return,
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Test entry point: drive a mouse event through `handle_mouse`
    /// after manually setting `last_area` (since `view()` would
    /// otherwise be needed to populate it).
    pub fn dispatch_mouse_in(&mut self, m: crossterm::event::MouseEvent, area: Rect) {
        self.layout.last_area = area;
        self.handle_mouse(m);
    }

    /// Test accessor — read-only handle to the sidebar wrapper.
    pub fn sidebar(&self) -> &crate::realm::components::sidebar::Sidebar {
        &self.sidebar
    }

    /// Look up the Quit chord — catalog default OR
    /// `ui.action_keys.quit` override. Returns the parsed `KeyChord`
    /// (`Double` for `q q`, `Single` for a single-letter remap).
    /// Cached at call sites; cheap to re-parse.
    fn resolve_quit_chord(&self) -> Option<pilot_tui_core::action::KeyChord> {
        use pilot_tui_core::action::{ActionDef, ActionKind, KeyChord};
        let def = ActionDef::for_kind(ActionKind::Quit);
        def.effective_chord(&self.action_key_overrides)
            .or_else(|| KeyChord::parse(def.default_keys))
    }

    /// Matches the FIRST key of the Quit chord (the entry-point for
    /// the latch). For `Double` chords this is the inner single
    /// chord's first press; for `Single` chords this is the chord
    /// itself.
    fn matches_quit_chord(&self, key: &RealmKey) -> bool {
        use pilot_tui_core::action::KeyChord;
        let Some(chord) = self.resolve_quit_chord() else {
            return false;
        };
        let first = match &chord {
            KeyChord::Single { .. } => chord,
            KeyChord::Double(inner) => (**inner).clone(),
        };
        let Some(input) = key_event_to_chord(realm_key_to_crossterm(key)) else {
            return false;
        };
        input == first
    }

    /// DetachSpec for the focused pane, or None if it can't detach
    /// (e.g. cursor on a repo header in the sidebar).
    fn focused_detach_spec(&self) -> Option<crate::pane::DetachSpec> {
        match self.focus {
            PaneFocus::Sidebar => self.sidebar.detachable(),
            PaneFocus::Right => self.right.detachable(),
            PaneFocus::Terminals => self.terminals.detachable(),
        }
    }

    /// Mouse routing:
    /// Handle a bracketed-paste event from the host terminal. The
    /// host wraps the pasted text in `ESC[200~ … ESC[201~` and
    /// crossterm hands us the inner string. We forward the same
    /// wrapped sequence to the focused terminal's PTY so the
    /// inner program (Claude, shell, vim) sees a single paste
    /// instead of a stream of keystrokes — important because
    /// shells trigger autocomplete on individual keys, Claude
    /// treats fast keystrokes as paste anyway (different bracket
    /// markers), etc.
    ///
    /// Only fires when the terminal pane is focused. Other panes
    /// don't have a useful paste-target today (reply textarea has
    /// its own keyboard path through tuirealm).
    pub fn handle_paste(&mut self, text: &str) {
        if self.focus != PaneFocus::Terminals {
            return;
        }
        let Some(terminal_id) = self.terminals.active_terminal_id() else {
            return;
        };
        // ESC[200~ <text> ESC[201~ — the standard bracketed-paste
        // wire format. Inner programs that opted into bracketed
        // paste mode (Claude does, most modern shells do) see this
        // as one atomic chunk and skip their per-keystroke
        // autocomplete / autoindent reactions.
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        self.send_cmd(IpcCommand::Write { terminal_id, bytes });
        self.redraw = true;
    }

    /// - Down on a splitter line → start drag (resize panes on
    ///   subsequent Drag events until Up).
    /// - Down anywhere else → focus the pane the click landed in.
    /// - Up → end the active drag.
    /// - ScrollUp/Down over the terminal pane → forward to the
    ///   terminal's scrollback (libghostty handles the actual move).
    pub fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        if self.layout.last_area.width == 0 || self.layout.last_area.height == 0 {
            return;
        }
        let (sidebar_rect, right_top_rect, right_bottom_rect) = pane_areas(
            self.layout.last_area,
            self.layout.sidebar_pct,
            self.layout.right_top_pct,
            self.layout.sidebar_user_resized,
        );

        match m.kind {
            MouseEventKind::Down(button) => {
                self.q_latch.disarm();
                // Tab-strip click on the terminal pane top row →
                // switch active tab. Checked BEFORE the
                // "forward to inner program" path because the tab
                // strip belongs to pilot, not to Claude/shell.
                if matches!(button, crossterm::event::MouseButton::Left)
                    && let Some(idx) = self.terminals.tab_at(m.column, m.row)
                {
                    self.terminals.set_active_tab(idx);
                    self.focus = PaneFocus::Terminals;
                    self.set_focus_attr();
                    self.redraw = true;
                    return;
                }
                // Right-click in the sidebar → open the workspace
                // context menu. Move the cursor to the clicked row
                // first (same as left-click) so the menu acts on
                // the visible selection. The menu is mounted as a
                // standard `Choice` modal; pick → dispatch.
                if matches!(button, crossterm::event::MouseButton::Right)
                    && rect_contains(sidebar_rect, m.column, m.row)
                {
                    self.focus = PaneFocus::Sidebar;
                    self.set_focus_attr();
                    if self.sidebar.click_to_select(sidebar_rect, m.row) {
                        self.sync_panes();
                    }
                    if let Some(ws) = self.sidebar.selected_workspace() {
                        let session_key: pilot_core::SessionKey = (&ws.key).into();
                        self.mount_sidebar_context_menu(session_key);
                    }
                    return;
                }
                // A left-click in the terminal pane ALWAYS starts a
                // potential pilot selection — we commit to that
                // even when the inner program is mouse-tracking
                // (claude / vim / less). Why: macOS's "Option /
                // Shift to bypass app mouse" convention sends the
                // bypassed drag straight to the host terminal,
                // whose native selection happily extends across
                // pilot's sidebar + activity panes (the screenshot
                // the user kept seeing). The host can't draw a
                // pane-bounded highlight; only pilot can. Trade-
                // off: drag-as-mouse-tracking-input to claude is
                // disabled, but plain Down/Up (click) still
                // forwards on release if start == end — so single
                // clicks the inner app cares about still work.
                let claim_for_selection = rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && matches!(button, crossterm::event::MouseButton::Left)
                    && self
                        .layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                        .is_none();

                // Forward CLICK-down to mouse-tracking inner programs
                // only when we're NOT claiming for selection — i.e.,
                // non-left buttons. Left clicks are deferred: we set
                // up a pilot selection on Down and decide on Up
                // whether to forward (start == end → click) or copy
                // (start != end → drag-selection).
                if !claim_for_selection
                    && rect_contains(right_bottom_rect, m.column, m.row)
                    && self.focus == PaneFocus::Terminals
                    && self.terminals.focused_terminal_tracks_mouse()
                    && self
                        .layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                        .is_none()
                {
                    let cell_col = m.column.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = m.row.saturating_sub(right_bottom_rect.y) as u32;
                    let vt_button = match button {
                        crossterm::event::MouseButton::Left => libghostty_vt::mouse::Button::Left,
                        crossterm::event::MouseButton::Middle => {
                            libghostty_vt::mouse::Button::Middle
                        }
                        crossterm::event::MouseButton::Right => libghostty_vt::mouse::Button::Right,
                    };
                    if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                        libghostty_vt::mouse::Action::Press,
                        Some(vt_button),
                        cell_col,
                        cell_row,
                    ) {
                        self.send_cmd(IpcCommand::Write { terminal_id, bytes });
                        self.redraw = true;
                        return;
                    }
                }
                if let Some(target) =
                    self.layout
                        .hit_test_splitter(m.column, m.row, sidebar_rect, right_top_rect)
                {
                    self.layout.active_drag = Some(target);
                    return;
                }
                let target = if rect_contains(sidebar_rect, m.column, m.row) {
                    Some(PaneFocus::Sidebar)
                } else if rect_contains(right_top_rect, m.column, m.row) {
                    Some(PaneFocus::Right)
                } else if rect_contains(right_bottom_rect, m.column, m.row) {
                    Some(PaneFocus::Terminals)
                } else {
                    None
                };
                if let Some(focus) = target {
                    if self.focus != focus {
                        self.focus = focus;
                        self.set_focus_attr();
                        self.redraw = true;
                    }
                    // Clicking inside the sidebar should also move the
                    // cursor to whatever row was clicked (workspace
                    // selection).
                    if focus == PaneFocus::Sidebar
                        && self.sidebar.click_to_select(sidebar_rect, m.row)
                    {
                        self.sync_panes();
                        self.redraw = true;
                    }
                    // Right (Activity) pane clicks. Single click =
                    // toggle multi-select on the card / toggle section
                    // header. Double click on a card = toggle
                    // expand/collapse on it. Crossterm doesn't ship
                    // double-click events so synthesize from timing:
                    // a second left-click on the same cell within
                    // 400ms = double.
                    // Pilot-side selection start: any left-click that
                    // landed in the terminal pane. Recording start ==
                    // end means a click-without-drag is treated as a
                    // click in the Up handler — it'll then forward
                    // press+release to the inner program if it's
                    // mouse-tracking.
                    if focus == PaneFocus::Terminals
                        && matches!(button, crossterm::event::MouseButton::Left)
                        && claim_for_selection
                    {
                        self.terminal_selection = Some(((m.column, m.row), (m.column, m.row)));
                    } else {
                        let _ = button;
                    }
                    if focus == PaneFocus::Right {
                        const DOUBLE_CLICK_WINDOW: std::time::Duration =
                            std::time::Duration::from_millis(400);
                        let is_double = matches!(button, crossterm::event::MouseButton::Left)
                            && self
                                .last_click
                                .map(|(c, r, t)| {
                                    c == m.column
                                        && r == m.row
                                        && t.elapsed() <= DOUBLE_CLICK_WINDOW
                                })
                                .unwrap_or(false);
                        let handled = if is_double {
                            self.last_click = None; // consume the pair
                            self.right.handle_mouse_double_click(m.column, m.row)
                        } else {
                            self.last_click = Some((m.column, m.row, std::time::Instant::now()));
                            self.right.handle_mouse_click(m.column, m.row)
                        };
                        if handled {
                            self.redraw = true;
                        }
                        // Surface the selection toggle in the footer
                        // — the ✓ glyph alone was too subtle for
                        // users coming from mouse-heavy IDEs.
                        if let Some(msg) = self.right.drain_selection_notice() {
                            use crate::realm::components::footer::{Notice, NoticeSeverity};
                            self.status.notice = Some(Notice::new(msg, NoticeSeverity::Hint));
                        }
                    }
                }
            }
            MouseEventKind::Drag(_) => {
                if let Some(target) = self.layout.active_drag {
                    if self.layout.update_drag(target, m.column, m.row) {
                        self.redraw = true;
                    }
                    return;
                }
                // Extend pilot-side terminal selection. Updating
                // the end cell triggers a redraw so the highlighted
                // range visibly follows the cursor.
                if let Some((start, _)) = self.terminal_selection {
                    self.terminal_selection = Some((start, (m.column, m.row)));
                    self.redraw = true;
                }
            }
            MouseEventKind::Up(button) => {
                let was_drag = self.layout.active_drag.take().is_some();
                if was_drag {
                    // Persist the final split percentages — drag
                    // events fire dozens per second, so we deferred
                    // the write until release.
                    self.layout.persist();
                }
                // Pilot-side selection release: classify Up as a
                // drag-copy (different start vs end → reverse-video
                // range matters → OSC 52 the extracted text) or as
                // a plain click that we deferred from Down (start ==
                // end → forward press+release to the inner program
                // so its click handlers fire).
                let mut click_no_drag_at: Option<(u16, u16)> = None;
                if let Some((start, end)) = self.terminal_selection.take() {
                    let was_drag = start != end;
                    if was_drag {
                        let text = self.terminals.extract_text(right_bottom_rect, start, end);
                        if !text.trim().is_empty() {
                            emit_clipboard_copy(&text);
                            use crate::realm::components::footer::{Notice, NoticeSeverity};
                            let lines = text.lines().count();
                            self.status.notice = Some(Notice::new(
                                format!(
                                    "copied {} line{} to clipboard",
                                    lines,
                                    if lines == 1 { "" } else { "s" }
                                ),
                                NoticeSeverity::Hint,
                            ));
                        }
                    } else {
                        // Up at the same cell as Down → this was a
                        // click, not a drag. Replay press+release to
                        // the inner program if it's mouse-tracking.
                        click_no_drag_at = Some(start);
                    }
                    self.redraw = true;
                }
                if let Some((col, row)) = click_no_drag_at
                    && rect_contains(right_bottom_rect, col, row)
                    && self.focus == PaneFocus::Terminals
                    && self.terminals.focused_terminal_tracks_mouse()
                {
                    // Forward the press FIRST, then the release —
                    // claude/vim/etc. need both to register a click.
                    // The Down handler skipped the press because it
                    // was claiming the input for selection; here we
                    // know the user didn't drag, so we replay it.
                    let cell_col = col.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = row.saturating_sub(right_bottom_rect.y) as u32;
                    let vt_button = match button {
                        crossterm::event::MouseButton::Left => libghostty_vt::mouse::Button::Left,
                        crossterm::event::MouseButton::Middle => {
                            libghostty_vt::mouse::Button::Middle
                        }
                        crossterm::event::MouseButton::Right => libghostty_vt::mouse::Button::Right,
                    };
                    for action in [
                        libghostty_vt::mouse::Action::Press,
                        libghostty_vt::mouse::Action::Release,
                    ] {
                        if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                            action,
                            Some(vt_button),
                            cell_col,
                            cell_row,
                        ) {
                            self.send_cmd(IpcCommand::Write { terminal_id, bytes });
                        }
                    }
                    self.redraw = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // Wheel inside the activity pane → scroll the
                // activity list. Three rows per notch matches the
                // sidebar-list scroll feel; the inner pane clamps
                // to total length.
                if rect_contains(right_top_rect, m.column, m.row) {
                    // 8 rows/notch — trackpad scrolling on macOS
                    // emits many events per gesture, and at 3
                    // rows/notch a "swipe" only moves a third of the
                    // visible window which felt molasses-slow vs
                    // native terminals.
                    const STEP: isize = 8;
                    let delta = if matches!(m.kind, MouseEventKind::ScrollUp) {
                        -STEP
                    } else {
                        STEP
                    };
                    if self.right.scroll_activity(delta) {
                        self.redraw = true;
                    }
                    return;
                }
                // Bail silently when the cursor isn't over the
                // terminal pane — sidebar / footer ignore scroll,
                // no need to surface a notice.
                if !rect_contains(right_bottom_rect, m.column, m.row) {
                    return;
                }
                // Pilot sessions are wrapped in `tmux attach`, and the
                // tmux client always runs on the alternate screen —
                // libghostty's own Delta scroll is a guaranteed no-op
                // there. With `mouse on` in the tmux config, encoding
                // the wheel as SGR mouse and writing it to the PTY
                // lets tmux drive its own scrollback (or forward the
                // event to an inner program that's tracking mouse,
                // like claude/vim/less). That's why scroll "used to
                // work" — encode + forward is the only way to scroll
                // anything pilot wraps.
                // SGR mouse wheel is the only protocol every inner
                // program agrees on for "scroll the viewport by ONE
                // line":
                // - shell (tmux mouse on → tmux's copy-mode line
                //   scroll): one wheel = one line of scrollback
                // - vim / less: one wheel = one line
                // - claude code: one wheel = one message-list line
                //
                // The xterm `alternateScroll` pattern (synthesize
                // arrow keys or PgUp/PgDn) saves work in the inner
                // program but it ALSO changes the semantic unit
                // (page instead of line, or worse, arrow = prompt-
                // history navigation in claude). One-line-per-wheel
                // is what gives the smooth native feel; any
                // multiplier or page-jump breaks that.
                //
                // The earlier "horribly slow" complaint with SGR
                // was actually about render batching, not the
                // protocol — when many wheels arrived in a burst
                // we drained them all then rendered once, so the
                // user saw 1-2 frames per gesture. Per-event
                // rendering (now restored) gives one frame per
                // claude response, which is the progressive feel
                // the user asked for.
                if self.terminals.focused_terminal_tracks_mouse() {
                    let cell_col = m.column.saturating_sub(right_bottom_rect.x) as u32;
                    let cell_row = m.row.saturating_sub(right_bottom_rect.y) as u32;
                    let button = if matches!(m.kind, MouseEventKind::ScrollUp) {
                        libghostty_vt::mouse::Button::Four
                    } else {
                        libghostty_vt::mouse::Button::Five
                    };
                    if let Some((terminal_id, bytes)) = self.terminals.encode_mouse(
                        libghostty_vt::mouse::Action::Press,
                        Some(button),
                        cell_col,
                        cell_row,
                    ) {
                        self.send_cmd(IpcCommand::Write { terminal_id, bytes });
                        // Eager redraw — paints whatever claude /
                        // tmux has flushed so far. Without this,
                        // pilot's `redraw` only flips when a daemon
                        // `TerminalOutput` event lands, which means
                        // a quick burst of wheels with no inner
                        // re-render in between (rare but observable
                        // in claude's "preserve last frame" mode)
                        // produces zero pilot frames. The actual
                        // visible change comes from claude's
                        // response, but flagging redraw here means
                        // the next loop iteration will pick up any
                        // partial chunk the inner program just sent
                        // and paint it.
                        self.redraw = true;
                        return;
                    }
                }
                // Fallback: terminal isn't tracking mouse — drive
                // libghostty's own viewport (raw PTY backend path).
                const STEP: isize = 5;
                let delta = if matches!(m.kind, MouseEventKind::ScrollUp) {
                    -STEP
                } else {
                    STEP
                };
                let _ = self.terminals.scroll_active(delta);
                self.redraw = true;
            }
            _ => {}
        }
    }

    /// Mount the reply textarea targeted at `workspace_key`. Submit
    /// → `Msg::TextareaSubmitted(body)` → orchestrator builds a
    /// `Command::PostReply { session_key, body }`.
    fn mount_reply(&mut self, workspace_key: pilot_core::SessionKey) {
        use crate::realm::components::textarea::Textarea;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if matches!(self.modal_stack.last(), Some(Id::Reply)) {
            return;
        }

        let label = workspace_key.to_string();
        let modal = Textarea::new("Reply").with_header(format!("on {label}"));
        let _ = self.app.mount(
            Id::Reply,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Reply);
        let _ = self.app.active(&Id::Reply);
        self.pending_reply = Some(workspace_key);
        self.redraw = true;
    }

    /// Mount the "New workspace" name prompt under a specific
    /// Project. Submit → `Msg::InputSubmitted(name)` while
    /// `Id::NewWorkspace` is on top → `Command::CreateWorkspace
    /// { name, project_key }`. The project_key is stashed on self
    /// here and consumed by `handle_input_submitted`.
    fn mount_new_workspace_input(&mut self, project_key: pilot_core::ProjectKey) {
        use crate::realm::components::input::Input;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if matches!(self.modal_stack.last(), Some(Id::NewWorkspace)) {
            return;
        }
        self.pending_new_workspace_project = Some(project_key);

        let modal = Input::new("Name this workspace")
            .title("New workspace")
            .placeholder("e.g. spike-rate-limit, refactor-auth, …")
            .with_validator(|s: &str| !s.trim().is_empty());
        let _ = self.app.mount(
            Id::NewWorkspace,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::NewWorkspace);
        let _ = self.app.active(&Id::NewWorkspace);
        self.redraw = true;
    }

    /// Mount the "New project" name prompt. Submit →
    /// `Msg::InputSubmitted(name)` while `Id::NewProject` is on top
    /// → `Command::CreateProject { name }`. Daemon creates a local
    /// project keyed `local-<slug>` (idempotent on collision).
    fn mount_new_project_input(&mut self) {
        use crate::realm::components::input::Input;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if matches!(self.modal_stack.last(), Some(Id::NewProject)) {
            return;
        }

        let modal = Input::new("Name this project")
            .title("New project")
            .placeholder("e.g. my-experiments, side-quests, scratch, …")
            .with_validator(|s: &str| !s.trim().is_empty());
        let _ = self.app.mount(
            Id::NewProject,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::NewProject);
        let _ = self.app.active(&Id::NewProject);
        self.redraw = true;
    }

    /// Mount the "request reviewers" multi-select picker for the
    /// given workspace's PR. Candidates are gathered from the
    /// workspace's known people; Space toggles, Enter submits →
    /// `Msg::ChoicePicked(indices)` → `handle_choice_picked` looks
    /// up the chosen logins in `review_choices` and dispatches
    /// `Command::RequestReviewers`.
    pub(crate) fn mount_request_reviewers(&mut self, workspace_key: pilot_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        if matches!(self.modal_stack.last(), Some(Id::RequestReviewers)) {
            return;
        }
        let candidates = self.gather_candidate_logins(&workspace_key, true);
        if candidates.is_empty() {
            self.status.notice = Some(Notice::new(
                "no candidate reviewers yet — interact with the PR first",
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        }
        let labels: Vec<String> = candidates.iter().map(|l| format!("@{l}")).collect();
        self.review_choices = candidates;
        self.pending_review_request = Some(workspace_key);
        let modal = Choice::multi("Request review from", labels)
            .title("Add reviewers")
            .label(|s: &String| s.clone());
        let _ = self.app.mount(
            Id::RequestReviewers,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::RequestReviewers);
        let _ = self.app.active(&Id::RequestReviewers);
        self.redraw = true;
    }

    /// Mount the "add assignees" multi-select picker for the
    /// workspace's PR or issue. Symmetric with
    /// `mount_request_reviewers`.
    pub(crate) fn mount_add_assignees(&mut self, workspace_key: pilot_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        if matches!(self.modal_stack.last(), Some(Id::AddAssignees)) {
            return;
        }
        let candidates = self.gather_candidate_logins(&workspace_key, false);
        if candidates.is_empty() {
            self.status.notice = Some(Notice::new(
                "no candidate assignees yet — interact with the task first",
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        }
        let labels: Vec<String> = candidates.iter().map(|l| format!("@{l}")).collect();
        self.assignees_choices = candidates;
        self.pending_assignees_request = Some(workspace_key);
        let modal = Choice::multi("Assign to", labels)
            .title("Add assignees")
            .label(|s: &String| s.clone());
        let _ = self.app.mount(
            Id::AddAssignees,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::AddAssignees);
        let _ = self.app.active(&Id::AddAssignees);
        self.redraw = true;
    }

    /// Build the candidate-logins list for the picker. Source set
    /// is the workspace's known people: existing reviewers,
    /// assignees, activity authors. Excludes the local user
    /// (no self-review) and either the existing reviewers (when
    /// building for the reviewer picker — they're already on the
    /// PR) OR the existing assignees (for the assignees picker).
    /// Dedupes; first-seen order preserved so the most relevant
    /// faces are at the top.
    fn gather_candidate_logins(
        &self,
        workspace_key: &pilot_core::WorkspaceKey,
        exclude_existing_reviewers: bool,
    ) -> Vec<String> {
        let Some(ws) = self
            .sidebar
            .workspace_iter()
            .find(|(k, _)| k.as_str() == workspace_key.as_str())
            .map(|(_, w)| w)
        else {
            return Vec::new();
        };
        let mut excluded: std::collections::HashSet<String> = self
            .viewer_logins
            .values()
            .cloned()
            .collect();
        if exclude_existing_reviewers {
            if let Some(pr) = &ws.pr {
                for r in &pr.reviewers {
                    excluded.insert(r.clone());
                }
            }
        } else if let Some(pr) = &ws.pr {
            for a in &pr.assignees {
                excluded.insert(a.clone());
            }
        }
        let mut out: Vec<String> = Vec::new();
        let push = |login: &str, out: &mut Vec<String>| {
            if !login.is_empty()
                && !excluded.contains(login)
                && !out.iter().any(|l| l == login)
            {
                out.push(login.to_string());
            }
        };
        if let Some(pr) = &ws.pr {
            for a in &pr.assignees {
                push(a, &mut out);
            }
            for r in &pr.reviewers {
                push(r, &mut out);
            }
        }
        for issue in &ws.gh_issues {
            for a in &issue.assignees {
                push(a, &mut out);
            }
        }
        for act in &ws.activity {
            push(&act.author, &mut out);
        }
        out
    }

    /// Build + mount a Help modal listing the focused pane's keymap
    /// plus the global section. Idempotent: re-pressing `?` while
    /// help is up is a no-op (the existing modal stays).
    fn mount_help(&mut self) {
        use crate::realm::components::help::Help;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if self.modal_stack.last() == Some(&Id::Help) {
            return;
        }
        // Help reads from `ActionDef::all()` — the single source of
        // truth. Every action surfaces, grouped by section. Previously
        // each pane's `keymap()` was stitched in here with a separate
        // hand-curated GLOBAL block, which is how `g` (sidebar refresh)
        // shipped without ever appearing in the help. Now adding an
        // entry to the catalog automatically surfaces it.
        let _ = self.app.mount(
            Id::Help,
            Box::new(Help::from_catalog()),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::Help);
        let _ = self.app.active(&Id::Help);
        self.redraw = true;
    }

    /// If there's a queued "out-of-scope workspace has active
    /// sessions" prompt and no modal is currently up, mount it. The
    /// user's answer (Y → kill, N/Esc → keep) is handled in the
    /// `Msg::Confirmed` / `Msg::ModalDismissed` arms.
    fn maybe_mount_next_removal_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some((workspace_key, label, title, count)) = self.pending_removal_prompts.pop_front()
        else {
            return;
        };
        let terminals_phrase = if count == 1 {
            "1 running terminal".to_string()
        } else {
            format!("{count} running terminals")
        };
        // Trim the title so a verbose PR description doesn't make the
        // modal three lines tall. 80 chars + an ellipsis fits within
        // the dynamic-height Confirm modal cleanly.
        let runner_label = match title.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => {
                let title_short = if t.chars().count() > 80 {
                    let truncated: String = t.chars().take(79).collect();
                    format!("{truncated}…")
                } else {
                    t.to_string()
                };
                format!(
                    "{label} \"{title_short}\" is no longer in your filter scope but has {terminals_phrase} — kill and remove?"
                )
            }
            None => format!(
                "{label} is no longer in your filter scope but has {terminals_phrase} — kill and remove?"
            ),
        };
        let modal = Confirm::new(runner_label).default_no();
        self.active_removal_prompt = Some(workspace_key);
        let _ = self.app.mount(
            Id::RemoveOutOfScope,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::RemoveOutOfScope);
        let _ = self.app.active(&Id::RemoveOutOfScope);
        self.redraw = true;
    }

    /// Mount the `Shift-A` adopt-target picker. Lists every other
    /// workspace the user could move sessions into. No-op when there
    /// are no other workspaces — show a hint instead since there's
    /// nothing to pick.
    /// Unified Confirm-modal mount for any destructive catalog
    /// action. Stashes the action in `pending_action_confirm`;
    /// `Msg::Confirmed(true)` reads it back, dispatches via
    /// `dispatch_action_unchecked`, and drains IPC commands.
    /// `Msg::Confirmed(false)` (or Esc) drops the stash silently.
    ///
    /// The body text comes from the catalog
    /// (`ActionDef::confirm_prompt`) — keeps prompt copy in the
    /// same place as the destructive flag, so adding a new
    /// destructive action is one catalog entry, not "remember to
    /// add a prompt".
    fn mount_action_confirm(&mut self, action: pilot_tui_core::action::Action) {
        use crate::realm::components::confirm::Confirm;
        use pilot_tui_core::action::ActionDef;
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        let prompt = ActionDef::for_action(&action)
            .confirm_prompt()
            .unwrap_or("Confirm action?");
        self.pending_action_confirm = Some(action);
        let modal = Confirm::new(prompt).default_no();
        let _ = self.app.mount(
            Id::ActionConfirm,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::ActionConfirm);
        let _ = self.app.active(&Id::ActionConfirm);
        self.redraw = true;
    }

    /// Confirm prompt before dispatching `Command::CleanWorktrees`.
    /// The destructive bit is on disk — sessions + their worktrees
    /// are gone after this. PR/issue rows stay because we only
    /// touch session records. `Msg::Confirmed(true)` fires the IPC;
    /// `(false)` / dismiss drops the prompt silently.
    fn mount_clean_worktrees_confirm(&mut self) {
        use crate::realm::components::confirm::Confirm;
        use tuirealm::subscription::{EventClause, Sub, SubClause};
        let modal = Confirm::new(
            "Wipe every worktree whose session has no live terminal? \
             PR / issue rows stay; active sessions are skipped.",
        )
        .default_no();
        let _ = self.app.mount(
            Id::CleanWorktreesConfirm,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::CleanWorktreesConfirm);
        let _ = self.app.active(&Id::CleanWorktreesConfirm);
        self.redraw = true;
    }

    /// Build the action list for a right-click on a sidebar
    /// workspace row, then mount a Choice modal to pick one. The
    /// menu only offers actions that *make sense* for this row —
    /// e.g. `MergePr` only when the PR is in a merge-ready state —
    /// so the user never sees a no-op entry.
    fn mount_sidebar_context_menu(&mut self, session_key: pilot_core::SessionKey) {
        use crate::realm::components::choice::Choice;
        use pilot_tui_core::action::{Action, ActionDef, ActionKind, availability};
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        // Snapshot the workspace state — the catalog's `availability`
        // resolver takes `Option<&Workspace>` and decides whether each
        // action makes sense for this row. No bespoke gating logic
        // duplicated here — `availability` defers to the same
        // `intent::*` resolvers the keyboard path uses.
        let workspace = self.sidebar.workspace_by_key(&session_key).cloned();

        // Catalog-driven menu: list every workspace-scoped action,
        // then filter by `availability`. Adding a new action to
        // the catalog automatically surfaces it here (gated by its
        // resolver) — no more "remembered to wire menu but forgot
        // help."
        let candidates: Vec<Action> = vec![
            Action::SpawnAgent("claude".into()),
            Action::SpawnShell,
            Action::OpenEditor,
            Action::MarkAllRead,
            Action::MergePr,
            Action::Archive,
        ];
        let actions: Vec<Action> = candidates
            .into_iter()
            .filter(|a| {
                if !availability(a.kind(), workspace.as_ref()) {
                    return false;
                }
                // Surface-specific extra gate: don't offer "open
                // editor" when no editor was detected at startup.
                // The catalog can't know about pilot's setup state,
                // so the menu enforces this one.
                if a.kind() == ActionKind::OpenEditor && self.setup.editors.is_empty() {
                    return false;
                }
                true
            })
            .collect();

        // Labels format: `<verb>  (<key>)` — same as before, just
        // sourced from the catalog so a default_keys remap flows
        // through automatically. `SpawnAgent("claude")` overrides
        // the static "spawn agent" label with the actual agent id.
        let labels: Vec<String> = actions
            .iter()
            .map(|a| {
                let def = ActionDef::for_action(a);
                let verb = match a {
                    Action::SpawnAgent(id) => format!("spawn {id}"),
                    _ => def.label.to_string(),
                };
                format!("{verb}  ({})", def.default_keys)
            })
            .collect();

        self.pending_sidebar_context = Some((session_key, actions));
        let modal = Choice::single("Actions", labels)
            .title("Workspace actions")
            .label(|s: &String| s.clone());
        let _ = self.app.mount(
            Id::SidebarContext,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::SidebarContext);
        let _ = self.app.active(&Id::SidebarContext);
        self.redraw = true;
    }

    fn mount_adopt_picker(&mut self, source_key: pilot_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        // Build (target_key, label) pairs from every workspace EXCEPT
        // the source. Labels prefer the primary task's `owner/repo#N`
        // form so the picker reads like the inbox rows.
        let mut items: Vec<(pilot_core::WorkspaceKey, String)> = Vec::new();
        for (key, ws) in self.sidebar.workspace_iter() {
            if key.as_str() == source_key.as_str() {
                continue;
            }
            let label = ws
                .primary_task()
                .map(|t| t.id.key.clone())
                .unwrap_or_else(|| ws.name.clone());
            items.push((pilot_core::WorkspaceKey::new(key.as_str()), label));
        }
        if items.is_empty() {
            self.status.notice = Some(Notice::new(
                "no other workspace to adopt sessions into",
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        }
        let labels: Vec<String> = items.iter().map(|(_, l)| l.clone()).collect();
        self.adopt_choices = items.into_iter().map(|(k, _)| k).collect();
        self.pending_adopt_source = Some(source_key);

        let modal = Choice::single("Move sessions to which workspace?", labels)
            .title("Adopt sessions")
            .label(|s: &String| s.clone());
        let _ = self.app.mount(
            Id::AdoptTarget,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::AdoptTarget);
        let _ = self.app.active(&Id::AdoptTarget);
        self.redraw = true;
    }

    /// Surface the next queued issue→PR merge prompt when no modal
    /// is currently up. The user's answer drives `Msg::Confirmed` /
    /// `Msg::ModalDismissed`, which dispatch a `Command::ConfirmMerge`
    /// back to the daemon. Default-no: silently absorbing a session
    /// the user is in the middle of using would be the surprising
    /// outcome, so Enter biases toward "leave them separate".
    fn maybe_mount_next_merge_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;
        use tuirealm::subscription::{EventClause, Sub, SubClause};

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some((issue_key, pr_key, issue_label, pr_label, count)) =
            self.pending_merge_prompts.pop_front()
        else {
            return;
        };
        let terminals_phrase = if count == 1 {
            "1 running terminal".to_string()
        } else {
            format!("{count} running terminals")
        };
        let question = format!(
            "{pr_label} closes {issue_label}, which has {terminals_phrase}. \
             Merge the issue's sessions into the PR workspace?",
        );
        let modal = Confirm::new(question).default_no();
        self.active_merge_prompt = Some((issue_key, pr_key));
        let _ = self.app.mount(
            Id::MergeConfirm,
            Box::new(modal),
            vec![Sub::new(EventClause::Any, SubClause::Always)],
        );
        self.modal_stack.push(Id::MergeConfirm);
        let _ = self.app.active(&Id::MergeConfirm);
        self.redraw = true;
    }

    /// Push a modal.
    pub fn push_modal(&mut self, id: Id) {
        self.modal_stack.push(id.clone());
        let _ = self.app.active(&id);
        self.redraw = true;
    }

    fn pop_modal(&mut self) {
        if let Some(top) = self.modal_stack.pop() {
            // Always unmount — every modal id is now transient
            // (mounted on demand by start_setup_wizard / mount_help /
            // mount_reply / etc.).
            let _ = self.app.umount(&top);
        }
        if let Some(top) = self.modal_stack.last() {
            let _ = self.app.active(top);
        }
        self.redraw = true;
    }

    /// Flip pilot's mouse capture on/off. Issues
    /// `EnableMouseCapture` / `DisableMouseCapture` to stdout so the
    /// host terminal switches between "send mouse to pilot" and
    /// "handle mouse natively (selection works)". Footer notice
    /// confirms which mode is now active.
    fn toggle_mouse_capture(&mut self) {
        use crate::realm::components::footer::{Notice, NoticeSeverity};
        self.mouse_capture_on = !self.mouse_capture_on;
        let (msg, _) = if self.mouse_capture_on {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture,);
            ("mouse: pilot (clicks → splitter/focus, wheel → scroll)", ())
        } else {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture,);
            (
                "mouse: host (native selection ON — Ctrl-Shift-S to flip back)",
                (),
            )
        };
        self.status.notice = Some(Notice::new(msg, NoticeSeverity::Hint));
        self.redraw = true;
    }

    fn set_focus_attr(&mut self) {
        self.sidebar.set_focused(self.focus == PaneFocus::Sidebar);
        self.right.set_focused(self.focus == PaneFocus::Right);
        self.terminals
            .set_focused(self.focus == PaneFocus::Terminals);
        // Reset the typed-since-focus flag every time focus changes.
        // A fresh visit to the terminal pane starts with `false` so
        // a single Tab cycles back out (no input → no autocomplete
        // target). After the first non-Tab key the flag flips and
        // Tab routes to the PTY normally.
        self.terminal_user_typed_since_focus = false;
    }

    /// Forward an inbound daemon event into all three panes. Each
    /// pane decides whether the event is relevant. After the very
    /// first Snapshot, apply any pending CLI preselect. Also feeds
    /// the polling modal so it can detect "first task arrived".
    pub fn handle_daemon_event(&mut self, event: IpcEvent) {
        // Viewer identities — fold into the local map and forward
        // to RightPane so activity bylines can render `@me`. This
        // arrives once per daemon connection (just after Snapshot)
        // and re-emits whenever the gh client's authenticated user
        // changes (token rotation).
        if let IpcEvent::ViewerIdentities { logins } = &event {
            for (source, login) in logins {
                self.viewer_logins.insert(source.clone(), login.clone());
            }
            self.right.set_viewer_logins(self.viewer_logins.clone());
            self.redraw = true;
            return;
        }
        // Project lifecycle events. Mirror into `self.projects` so
        // the sidebar can render headers from it, then push the
        // updated map to the sidebar component.
        if let IpcEvent::ProjectUpserted(p) = &event {
            self.projects.insert(p.key.clone(), (**p).clone());
            self.sidebar.apply_projects(self.projects.clone());
            self.redraw = true;
            return;
        }
        if let IpcEvent::ProjectRemoved(key) = &event {
            self.projects.remove(key);
            self.sidebar.apply_projects(self.projects.clone());
            self.redraw = true;
            return;
        }
        // Snapshot's project list seeds the same map on reconnect.
        // Push to the sidebar AFTER the snapshot's WorkspaceUpserted-
        // equivalent rows are processed below, so the first render
        // already has both layers.
        if let IpcEvent::Snapshot { projects, .. } = &event {
            for p in projects {
                self.projects.insert(p.key.clone(), p.clone());
            }
            self.sidebar.apply_projects(self.projects.clone());
        }

        let is_snapshot = matches!(&event, IpcEvent::Snapshot { .. });
        let is_spawn = matches!(
            &event,
            IpcEvent::TerminalSpawned { .. } | IpcEvent::TerminalFocusRequested { .. }
        );

        // Out-of-scope workspaces with running terminals — queue a
        // Confirm prompt before killing anything. Don't forward the
        // event to panes; they'd just ignore it anyway and a queued
        // prompt is the only reasonable response.
        if let IpcEvent::WorkspaceOutOfScope {
            workspace_key,
            label,
            title,
            active_terminal_count,
        } = &event
        {
            // Dedupe: ignore re-emits for the workspace currently
            // being prompted about OR already queued. The daemon
            // dedupes per-process, but a daemon restart would reset
            // its state and could spam the same prompt. Belt and
            // braces.
            let already_active = self
                .active_removal_prompt
                .as_ref()
                .map(|k| k == workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_removal_prompts
                .iter()
                .any(|(k, _, _, _)| k == workspace_key);
            if !already_active && !already_queued {
                self.pending_removal_prompts.push_back((
                    workspace_key.clone(),
                    label.clone(),
                    title.clone(),
                    *active_terminal_count,
                ));
                self.maybe_mount_next_removal_prompt();
                self.redraw = true;
            }
            return;
        }
        // Same pattern for issue→PR merge prompts: queue + surface
        // one at a time so the modal stack doesn't pile up.
        if let IpcEvent::WorkspaceMergePending {
            issue_workspace_key,
            pr_workspace_key,
            issue_label,
            pr_label,
            active_terminal_count,
        } = &event
        {
            let already_active = self
                .active_merge_prompt
                .as_ref()
                .map(|(i, _)| i == issue_workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_merge_prompts
                .iter()
                .any(|(i, _, _, _, _)| i == issue_workspace_key);
            if !already_active && !already_queued {
                self.pending_merge_prompts.push_back((
                    issue_workspace_key.clone(),
                    pr_workspace_key.clone(),
                    issue_label.clone(),
                    pr_label.clone(),
                    *active_terminal_count,
                ));
                self.maybe_mount_next_merge_prompt();
                self.redraw = true;
            }
            return;
        }
        // Silent-merge notice: the daemon collapsed an issue row into
        // its PR without prompting (no live sessions to worry about).
        // Flash a footer line so the row disappearance has context.
        if let IpcEvent::WorkspaceMerged {
            issue_label,
            pr_label,
            ..
        } = &event
        {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.status.notice = Some(Notice::new(
                format!("merged {issue_label} into {pr_label}"),
                NoticeSeverity::Info,
            ));
            self.redraw = true;
            return;
        }
        // Shift-M completed: GitHub accepted the merge. Optimistically
        // flip the local task state to Merged so the badge pill
        // changes IMMEDIATELY — without this the user has to wait up
        // to the next poll cycle (~30s) for the visual to catch up,
        // which felt broken. Refresh still goes out so the next
        // poll backfills everything else.
        if let IpcEvent::PrMerged {
            pr_label,
            workspace_key,
            ..
        } = &event
        {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.sidebar.mark_workspace_merged(workspace_key);
            self.status.notice = Some(Notice::new(
                format!("merged {pr_label}"),
                NoticeSeverity::Info,
            ));
            // Queue a "remove merged workspace?" prompt. Reuses the
            // existing RemoveOutOfScope confirm flow (Kill on Yes,
            // keep on No) — same UX, just triggered after a merge
            // instead of an out-of-scope detection. Active-terminal
            // count from sidebar lookup so the message reads truthfully.
            let already_active = self
                .active_removal_prompt
                .as_ref()
                .map(|k| k == workspace_key)
                .unwrap_or(false);
            let already_queued = self
                .pending_removal_prompts
                .iter()
                .any(|(k, _, _, _)| k == workspace_key);
            if !already_active && !already_queued {
                self.pending_removal_prompts.push_back((
                    workspace_key.clone(),
                    pr_label.clone(),
                    Some(format!("PR {pr_label} merged — remove workspace?")),
                    0,
                ));
                self.maybe_mount_next_removal_prompt();
            }
            self.send_cmd(IpcCommand::Refresh);
            self.redraw = true;
            return;
        }
        // Clear the lazy-fetch dedupe entry when a workspace is
        // removed, so a re-added workspace (e.g. user re-checks a
        // filter) gets a fresh details fetch on next focus.
        if let IpcEvent::WorkspaceRemoved(key) = &event {
            self.pr_details_fetched.remove(key);
        }
        self.sidebar.on_daemon_event(&event);
        // Surface Active→Asking transitions in the footer with a
        // brief Hint-severity notice. The sidebar already pushed an
        // OS notification + flipped its `?` glyph; this is the
        // in-pilot equivalent for users running with notifications
        // muted. Last one wins if multiple workspaces transition
        // in the same tick — they'll see them in sequence anyway as
        // the 3s Hint fade clears each.
        if let Some(msg) = self.sidebar.drain_pending_asking_notices().pop() {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            self.status.notice = Some(Notice::new(msg, NoticeSeverity::Hint));
        }
        self.right.on_daemon_event(&event);
        self.terminals.on_daemon_event(&event);
        if let Some(p) = self.status.polling.as_mut() {
            p.feed_daemon_event(&event);
        }
        // Background-poll indicator. Lights up whenever the daemon
        // emits PollProgress (any cycle, initial or not); clears on
        // PollCompleted. Visible only after the initial Polling modal
        // is gone — the modal already shows its own (richer) spinner
        // and we don't want two indicators flashing at once.
        if self.status.polling.is_none() {
            match &event {
                IpcEvent::PollProgress { source, message } => {
                    self.status.note_poll_progress(source, message);
                    self.redraw = true;
                }
                IpcEvent::PollCompleted { source, .. } => {
                    self.status.note_poll_completed(source);
                    self.redraw = true;
                }
                _ => {}
            }
        }
        // CleanWorktrees finished — replace the "cleaning…" notice
        // with the final count so the user sees how much was done.
        if let IpcEvent::CleanWorktreesCompleted { removed, skipped } = &event {
            use crate::realm::components::footer::{Notice, NoticeSeverity};
            let msg = if *skipped == 0 {
                format!("cleaned {removed} worktree(s)")
            } else {
                format!("cleaned {removed} worktree(s) · kept {skipped} (active)")
            };
            self.status.notice = Some(Notice::new(msg, NoticeSeverity::Hint));
            self.redraw = true;
        }
        if is_snapshot && self.preselect.is_some() {
            self.apply_preselect();
        }
        if is_spawn {
            // A terminal just appeared — auto-focus the Terminals
            // pane so the user can start typing immediately, and
            // clear any "Spawning…" footer notice that was set when
            // the matching Spawn command was sent.
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
            self.status.clear_spawning_notice();
            self.sync_panes();
            // Editor-deferred-by-spawn: the user pressed `e` on a
            // workspace with no worktree; we asked the daemon to
            // spawn a shell so a worktree got provisioned. Look
            // up the queued target's worktree from the sidebar's
            // workspace map (NOT `selected_workspace()`) so the
            // launch fires even if the user has since navigated
            // to a different workspace.
            if let Some((target_key, editor)) = self.setup.pending_editor_launch.clone()
                && let Some(worktree) = self
                    .sidebar
                    .workspace_by_key(&target_key)
                    .and_then(|w| w.sessions.first().map(|s| s.worktree_path.clone()))
            {
                self.setup.pending_editor_launch = None;
                self.launch_editor(&editor, &worktree);
            }
        } else {
            self.sync_panes();
        }
        self.redraw = true;
    }

    /// Auto-fade transient notices. Called once per iteration in
    /// the run loop. Severity decides the timeout:
    /// - Retryable: 5s. Hiccups self-heal, no need to linger.
    /// - Info: 15s. Spawn-progress and similar — long enough that a
    ///   slow worktree creation doesn't fade mid-flight; short
    ///   enough that a stuck notice (e.g. spawn never landed)
    ///   doesn't follow the user around forever.
    /// - Permanent / Auth: stay until dismissed (`e`).
    pub fn tick_notice(&mut self) {
        if self.status.tick_notice() {
            self.redraw = true;
        }
    }

    /// Drive the right-pane auto-mark-read timer. Called once per
    /// iteration. When the timer fires on an unread row under the
    /// cursor, the inner pane mutates its workspace state AND we
    /// ship `Command::MarkActivityRead` so the daemon persists.
    /// Without this hook the auto-mark never fires — the timer
    /// counted forever and unread badges never dropped.
    pub fn tick_right(&mut self) {
        if let Some((session_key, index)) = self.right.tick() {
            tracing::info!(
                %session_key,
                index,
                "auto-mark-read fired → Command::MarkActivityRead",
            );
            self.send_cmd(IpcCommand::MarkActivityRead { session_key, index });
            self.redraw = true;
        }
    }

    /// Drive the polling spinner + termination check from the run
    /// loop. Cheap; called every iteration. Returns Some(msg) when
    /// the polling modal wants to be torn down.
    pub fn polling_tick(&mut self) -> Option<Msg> {
        let msg = self.status.polling_tick();
        if msg.is_some() {
            self.redraw = true;
        }
        msg
    }

    /// Tear down the polling modal. Called when its tick / feed
    /// returns Some(msg) (saw workspace, timed out, etc.).
    fn dismiss_polling(&mut self) {
        if self.status.dismiss_polling() {
            self.redraw = true;
        }
    }

    /// Project sidebar selection onto the right pane + terminal stack.
    /// Cheap to call; the inner setters bail when nothing changed.
    /// Called after every key dispatch and every daemon event.
    fn sync_panes(&mut self) {
        let workspace = self.sidebar.selected_workspace().cloned();
        let session_key = self.sidebar.selected_workspace_key().cloned();
        // Lazy-fetch trigger: when the focused workspace has a PR
        // and we haven't pulled its review-thread activity this
        // session, kick off the back-fill. The dedupe set prevents
        // re-firing on every key press / poll event for the same
        // workspace; `WorkspaceRemoved` clears the entry so a
        // re-added workspace gets a fresh fetch.
        if let Some(w) = workspace.as_ref()
            && w.pr.is_some()
            && !self.pr_details_fetched.contains(&w.key)
        {
            self.pr_details_fetched.insert(w.key.clone());
            tracing::info!(
                workspace_key = %w.key.as_str(),
                "lazy-fetch: requesting PR details",
            );
            self.send_cmd(IpcCommand::FetchPrDetails {
                workspace_key: w.key.clone(),
            });
        }
        // Also forward the workspace's persisted SessionLayout to
        // the terminal stack so the user's tile arrangement
        // follows them across workspace switches. Each workspace's
        // default session carries its own Tabs/Splits state; the
        // stack used to keep whatever layout the LAST workspace
        // had, so jumping from a split workspace to a tabs one
        // would render the new one with the old split's tree.
        let layout = workspace
            .as_ref()
            .and_then(|w| w.default_session())
            .map(|s| s.layout.clone())
            .unwrap_or_default();
        self.right.set_workspace(workspace);
        self.terminals.set_active_session(session_key);
        self.terminals.set_layout(layout);
    }

    /// Apply the pending `--workspace [--session]` selection. One-shot
    /// — clears `self.preselect` so subsequent snapshots don't
    /// override the user's manual cursor moves.
    fn apply_preselect(&mut self) {
        let Some(p) = self.preselect.take() else {
            return;
        };
        let landed = self.sidebar.focus_workspace_key(&p.workspace_key);
        if !landed {
            tracing::info!(
                "preselect: workspace key {:?} not found in first snapshot",
                p.workspace_key
            );
            return;
        }
        if let Some(raw) = p.session_id_raw
            && let Ok(uuid) = uuid::Uuid::parse_str(&raw)
        {
            let _ = self.sidebar.focus_session_id(pilot_core::SessionId(uuid));
            // Move focus to terminals so the user can type immediately.
            self.focus = PaneFocus::Terminals;
            self.set_focus_attr();
        }
    }
}

/// True if `(col, row)` lies within `rect`'s half-open bounds.
fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Paint a drag-selection range as reverse-video over `rect`. `start`
/// and `end` are screen coordinates from the mouse events; we
/// normalize so the lower-row end is the start and the higher-row end
/// is the end, then highlight cells in the visual range:
///
/// - Single-row selection: cells from `min_col` to `max_col`.
/// - Multi-row selection: from `start_col` to end-of-row on the start
///   row, full rows between, and start-of-row to `end_col` on the
///   final row.
///
/// All writes are clipped to `rect` so a drag that strayed outside
/// the terminal pane can't recolor pilot's sidebar or activity feed.
fn paint_selection(buf: &mut ratatui::buffer::Buffer, rect: Rect, start: (u16, u16), end: (u16, u16)) {
    use ratatui::style::Modifier;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let max_x = rect.x.saturating_add(rect.width.saturating_sub(1));
    let max_y = rect.y.saturating_add(rect.height.saturating_sub(1));
    // Normalize so `a` is row-earlier or equal to `b`.
    let (a, b) = if (start.1, start.0) <= (end.1, end.0) {
        (start, end)
    } else {
        (end, start)
    };
    // Clamp endpoints to the terminal rect.
    let clamp = |p: (u16, u16)| {
        (
            p.0.clamp(rect.x, max_x),
            p.1.clamp(rect.y, max_y),
        )
    };
    let a = clamp(a);
    let b = clamp(b);
    // No-op for a degenerate "click without drag" — Up handler
    // already skips the copy in that case; the highlight pass would
    // just reverse-video one cell, which is more confusing than
    // helpful.
    if a == b {
        return;
    }
    let mut y = a.1;
    while y <= b.1 {
        let row_start = if y == a.1 { a.0 } else { rect.x };
        let row_end = if y == b.1 { b.0 } else { max_x };
        let (lo, hi) = if row_start <= row_end {
            (row_start, row_end)
        } else {
            (row_end, row_start)
        };
        let mut x = lo;
        while x <= hi {
            // `buf[(x, y)]` is bounds-checked but our clamp already
            // guarantees in-range; this just sets the modifier
            // without touching the underlying char.
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            x = x.saturating_add(1);
        }
        if y == max_y {
            break;
        }
        y = y.saturating_add(1);
    }
}

/// Convert a crossterm `KeyEvent` to a typed `KeyChord` for catalog
/// lookup. Uppercase letters auto-shift so `KeyEvent { Char('M'),
/// no_mods }` produces the same chord as `KeyEvent { Char('m'),
/// SHIFT }` — matches the catalog's parser convention. Returns
/// `None` for codes the catalog doesn't model (function keys,
/// release events).
fn key_event_to_chord(
    key: crossterm::event::KeyEvent,
) -> Option<pilot_tui_core::action::KeyChord> {
    use crossterm::event::{KeyCode, KeyModifiers};
    use pilot_tui_core::action::{ChordCode, KeyChord, NamedKey};

    let mut ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let _ = &mut ctrl;
    let mut shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let code = match key.code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                shift = true;
            }
            ChordCode::Char(c.to_ascii_lowercase())
        }
        KeyCode::Tab => ChordCode::Named(NamedKey::Tab),
        KeyCode::Enter => ChordCode::Named(NamedKey::Enter),
        KeyCode::Esc => ChordCode::Named(NamedKey::Esc),
        KeyCode::Backspace => ChordCode::Named(NamedKey::Backspace),
        KeyCode::Up => ChordCode::Named(NamedKey::Up),
        KeyCode::Down => ChordCode::Named(NamedKey::Down),
        KeyCode::Left => ChordCode::Named(NamedKey::Left),
        KeyCode::Right => ChordCode::Named(NamedKey::Right),
        KeyCode::Home => ChordCode::Named(NamedKey::Home),
        KeyCode::End => ChordCode::Named(NamedKey::End),
        KeyCode::PageUp => ChordCode::Named(NamedKey::PageUp),
        KeyCode::PageDown => ChordCode::Named(NamedKey::PageDown),
        KeyCode::Delete => ChordCode::Named(NamedKey::Delete),
        KeyCode::Insert => ChordCode::Named(NamedKey::Insert),
        // Space is reported as Char(' ') by crossterm — covered by
        // the Char arm above. Function keys / unknown variants fall
        // through to None.
        _ => return None,
    };
    let _ = ctrl;
    Some(KeyChord::Single {
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        shift,
        alt,
        code,
    })
}

/// Look up the catalog `Action` matching `chord` in the sections
/// the focused pane should resolve. Globals always match; pane-
/// scoped sections only match when their pane is focused.
///
/// Honors user keybinding overrides from `~/.pilot/config.yaml::ui
/// .action_keys`: each catalog entry's effective chord falls back
/// to its default only when the user hasn't set an override for
/// that `ActionKind::name()`.
///
/// Returns `None` when no catalog entry has a matching chord —
/// the caller falls back to the legacy match arms (used today for
/// navigation keys, latches, and any action whose `default_keys`
/// is a presentation form like `g/G`).
fn find_action_for_chord(
    chord: &pilot_tui_core::action::KeyChord,
    focus: PaneFocus,
    overrides: &std::collections::BTreeMap<String, String>,
) -> Option<&'static pilot_tui_core::action::ActionDef> {
    use pilot_tui_core::action::{ActionDef, Section};
    let allowed = |s: Section| -> bool {
        match (s, focus) {
            (Section::Global, _) => true,
            // Workspace = "operates on the focused workspace". The
            // workspace cursor lives in the sidebar, but it's still
            // the active reference frame when the user is reading
            // the right pane — so accept both. Reply / Shift-V /
            // Shift-G all dual-fire today, and this widening lets
            // their inline match arms retire.
            (Section::Workspace, PaneFocus::Sidebar | PaneFocus::Right) => true,
            // Activity = "operates on the focused activity row" —
            // the row cursor only exists on the right pane.
            (Section::Activity, PaneFocus::Right) => true,
            // Terminal section binds to actual PTY keys; we don't
            // route them through the catalog yet — the terminal
            // pane forwards `all keys` to the PTY and the escape
            // sequence (`]]`) has its own latch logic.
            _ => false,
        }
    };
    ActionDef::all()
        .find(|d| allowed(d.section) && d.effective_chord(overrides).as_ref() == Some(chord))
}

/// Spawn a new `pilot` process pinned to the focused pane's
/// detachable scope. Detached: the new process gets its own session
/// so closing the parent doesn't kill it. Errors are logged, not
/// surfaced — detach is best-effort UX.
fn spawn_detached_pilot(spec: &crate::pane::DetachSpec) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("detach: current_exe unavailable: {e}");
            return;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&spec.args);
    // Decouple from the parent so closing this pilot doesn't take
    // the detached one with it. Implementation lives in
    // `crate::platform` — setsid() on unix, DETACHED_PROCESS on
    // Windows (TODO).
    crate::platform::detach_child_process(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    if let Err(e) = cmd.spawn() {
        tracing::warn!("detach: spawn failed: {e}");
    }
}

/// Carve the bottom row off for the footer. Returns
/// (pane_area, footer_area) — `pane_area` is what the three panes
/// fill; `footer_area` is the 1-row hint/status line at the bottom.
fn split_for_footer(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (area, Rect::default());
    }
    let pane = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    let footer = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    (pane, footer)
}

#[allow(dead_code)]
fn placeholder(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" pilot · realm migration scaffold ")
        .borders(Borders::ALL);
    f.render_widget(block, area);
}

/// Run the realm-based pilot loop with a pre-built IPC client.
/// `main.rs::run_embedded_realm` constructs the client + daemon pair
/// before calling this so the daemon is already serving when the UI
/// boots.
pub fn run_with_client(client: Client) -> anyhow::Result<()> {
    let mut model = Model::new(client)?;
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

/// Test-only: run with an unconnected client. Useful for manual
/// smoke tests without spinning up the full daemon stack.
pub fn run() -> anyhow::Result<()> {
    let (client, _server) = pilot_ipc::channel::pair();
    run_with_client(client)
}

/// Run the loop on a pre-configured model. Used by
/// `main::run_embedded_realm` so it can install the on-setup-complete
/// hook + start the wizard before entering the loop.
pub fn run_loop_with_model<T: TerminalAdapter>(mut model: Model<T>) -> anyhow::Result<()> {
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

fn run_loop<T: TerminalAdapter>(model: &mut Model<T>) -> anyhow::Result<()> {
    while !model.quit {
        // 1. Drain inbound daemon events (cheap try_recv).
        while let Ok(evt) = model.client.rx.try_recv() {
            model.handle_daemon_event(evt);
        }

        // 2. Polling-modal spinner heartbeat + retryable notice fade.
        if let Some(msg) = model.polling_tick() {
            model.dismiss_polling();
            model.update(msg);
        }
        model.tick_notice();
        model.tick_right();

        // 3. Process tuirealm-side messages (timer ticks for Loading,
        // injected modal keys). Non-blocking — listener thread already
        // queued any work it had.
        if let Ok(messages) = model.app.tick(PollStrategy::Once(Duration::ZERO)) {
            if !messages.is_empty() {
                model.redraw = true;
                for msg in messages {
                    model.update(msg);
                }
            }
        }

        // 4. Render if dirty — before the blocking input read so the
        // user sees their last action immediately.
        if model.redraw {
            // Per-frame timing log behind the `pilot=debug` filter.
            // Lets us see in `/tmp/pilot.log` whether a slow scroll
            // is the render itself (would show large `frame_ms`)
            // versus daemon round-trips between renders. Cheap —
            // `Instant::now` is ~10ns and `tracing::debug!` is a
            // no-op when the level isn't enabled.
            let t = std::time::Instant::now();
            model.view();
            let elapsed_ms = t.elapsed().as_micros() as f32 / 1000.0;
            tracing::debug!(frame_ms = elapsed_ms, "render");
            model.redraw = false;
        }

        // 5. Block briefly for input. One event per iteration,
        // render between events — the "drain all then render once"
        // pattern looked good on paper (fewer renders per second)
        // but broke scroll fluidity: a 30-event trackpad gesture
        // collapsed into a single jump-cut render, so the user saw
        // the screen teleport from start to end with no
        // intermediate frames ("not progressive, I don't even see
        // which direction I'm going"). The render cost is 1-2ms
        // (verified via the `render frame_ms` debug log) so per-
        // event rendering at 50-100Hz easily keeps up.
        //
        // The 16ms poll is the IDLE-WAIT bound: when no events are
        // queued, we block here up to one display refresh worth.
        // With events queued, `poll` returns immediately — we don't
        // pay the 16ms; the loop body runs again. So during an
        // active scroll burst, this loop runs as fast as the
        // render + daemon-roundtrip allows, which is what gives
        // the progressive-scroll feel.
        const POLL_IDLE: Duration = Duration::from_millis(16);
        if let Ok(true) = crossterm::event::poll(POLL_IDLE)
            && let Ok(event) = crossterm::event::read()
        {
            dispatch_event(model, event);
        }
    }
    Ok(())
}

/// Route one crossterm event to the right handler. Extracted from
/// the run-loop body so the loop can `dispatch_event` once per
/// poll, then poll(0) to drain the rest before rendering — the
/// batching is what turns 20 scroll-wheel events into 1 frame.
fn dispatch_event<T: TerminalAdapter>(model: &mut Model<T>, event: crossterm::event::Event) {
    match event {
        crossterm::event::Event::Key(key) => {
            // With KeyboardEnhancementFlags::REPORT_EVENT_TYPES pushed
            // at startup, the host terminal distinguishes Press /
            // Repeat / Release. We skip Release only — Repeat must
            // be honored so held keys autorepeat (arrow keys in
            // Claude code, holding j to scroll, etc.). The previous
            // filter skipped Repeat too, which made every "held key"
            // feel broken even though Backspace worked (Backspace
            // events arrive as Press from the terminal's auto-repeat
            // emulation when extended keyboards aren't on).
            if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
                return;
            }
            let realm_key = crossterm_to_realm(key);
            if model.modal_stack.is_empty() {
                model.handle_pane_key(realm_key);
            } else {
                let _ = model.modal_event_tx.send(RealmEvent::Keyboard(realm_key));
                // ChannelPort is polled by the listener thread every
                // 10ms, so a tight 15ms window often expires before
                // the listener delivers the event we just pushed —
                // the keypress sits in the channel and isn't acted on
                // until the user presses another key. The Confirm
                // modal showed this loudly: "Y not responsive; Esc
                // worked after a few tries".
                //
                // Poll in a short loop with a 150ms deadline so we
                // keep checking until messages arrive or the user
                // perceives latency. 150ms is well under the human-
                // noticeable threshold for key feedback but long
                // enough to absorb the 10ms listener cadence + jitter.
                let deadline = std::time::Instant::now() + Duration::from_millis(150);
                let mut handled = false;
                loop {
                    match model.app.tick(PollStrategy::Once(Duration::ZERO)) {
                        Ok(messages) if !messages.is_empty() => {
                            for msg in messages {
                                model.update(msg);
                            }
                            handled = true;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // After the first tick lands, drain anything else the
                // modal pushed in the same window — a single tuirealm
                // `Cmd` can fan out into multiple `Msg`s and we don't
                // want them to straggle into the next keypress.
                if handled
                    && let Ok(messages) = model.app.tick(PollStrategy::Once(Duration::ZERO))
                {
                    for msg in messages {
                        model.update(msg);
                    }
                }
                // Modals can mutate internal state without producing a
                // `Msg`, so force a redraw too.
                model.redraw = true;
            }
        }
        crossterm::event::Event::Mouse(m) => {
            if model.modal_stack.is_empty() {
                model.handle_mouse(m);
            }
        }
        crossterm::event::Event::Paste(text) => {
            // Bracketed paste arrived. Two destinations depending on
            // where focus is — both go through `handle_paste` which
            // inspects pane state.
            if model.modal_stack.is_empty() {
                model.handle_paste(&text);
            } else {
                // Modal owns input — forward as raw text via the
                // modal event channel. The textarea modal will see
                // this as a multi-char paste and insert at cursor.
                let _ = model.modal_event_tx.send(RealmEvent::Paste(text));
            }
        }
        _ => {}
    }
}

fn crossterm_to_realm(key: crossterm::event::KeyEvent) -> RealmKey {
    use crossterm::event::{KeyCode as CKC, KeyModifiers as CKM};
    let code = match key.code {
        CKC::Char(c) => Key::Char(c),
        CKC::Enter => Key::Enter,
        CKC::Esc => Key::Esc,
        CKC::Backspace => Key::Backspace,
        CKC::Left => Key::Left,
        CKC::Right => Key::Right,
        CKC::Up => Key::Up,
        CKC::Down => Key::Down,
        CKC::Home => Key::Home,
        CKC::End => Key::End,
        CKC::PageUp => Key::PageUp,
        CKC::PageDown => Key::PageDown,
        CKC::Tab => Key::Tab,
        CKC::BackTab => Key::BackTab,
        CKC::Delete => Key::Delete,
        CKC::Insert => Key::Insert,
        CKC::F(n) => Key::Function(n),
        _ => Key::Null,
    };
    let mut mods = KeyModifiers::empty();
    if key.modifiers.contains(CKM::SHIFT) {
        mods |= KeyModifiers::SHIFT;
    }
    if key.modifiers.contains(CKM::CONTROL) {
        mods |= KeyModifiers::CONTROL;
    }
    if key.modifiers.contains(CKM::ALT) {
        mods |= KeyModifiers::ALT;
    }
    RealmKey::new(code, mods)
}

/// Write OSC 52 clipboard-set to the host terminal's stdout. The host
/// (Ghostty / iTerm2 / Kitty / WezTerm) lands the text on the system
/// clipboard. Format: `ESC ] 52 ; c ; <base64> ESC \`. Wraps the
/// pilot-side "copy from terminal selection" gesture — without OSC 52
/// the extracted text would just live in memory.
fn emit_clipboard_copy(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x1b\\");
    use std::io::Write;
    let _ = std::io::stdout().write_all(sequence.as_bytes());
    let _ = std::io::stdout().flush();
}

/// Tiny RFC 4648 base64 encoder. Pilot doesn't have a `base64` dep
/// and pulling one in for one OSC 52 call is overkill. ~25 lines,
/// allocation-free aside from the output `String`.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod effects_tests {
    //! Handler effect-contract tests.
    //!
    //! These exercise the `handle_X(&mut self, ...) -> Vec<IpcCommand>`
    //! contract on the orchestrator's biggest message handlers
    //! (textarea submit, input submit, confirm y/n, modal dismiss,
    //! choice pick). Each test:
    //!
    //!   1. constructs a `Model` with `new_for_test`;
    //!   2. seeds the internal state the handler expects to read
    //!      (`pending_reply`, `active_merge_prompt`, modal stack, …);
    //!   3. calls `handle_X(...)`;
    //!   4. asserts on the returned `Vec<IpcCommand>` directly —
    //!      no need to drive a real IPC client.
    //!
    //! Inline `mod tests` (not `tests/`) so the test can poke
    //! private fields. Effect contracts that drift would be a
    //! silent regression otherwise — these tests freeze them.
    use super::*;
    use pilot_core::{SessionKey, WorkspaceKey};
    use pilot_ipc::channel;
    use tuirealm::ratatui::layout::Size;

    fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
        let (client, _server) = channel::pair();
        Model::new_for_test(client, Size::new(120, 40)).expect("model init")
    }

    /// Reply submission with a non-empty body + a pending reply
    /// target produces `PostReply` followed by `Refresh` (in that
    /// order — the Refresh kicks an immediate poll instead of
    /// waiting on the 60s loop).
    #[test]
    fn textarea_submitted_with_pending_reply_returns_postreply_then_refresh() {
        let mut m = build_model();
        let key = SessionKey::from("github:o/r#1");
        m.pending_reply = Some(key.clone());
        let cmds = m.handle_textarea_submitted("hello".into());
        assert_eq!(cmds.len(), 2);
        match &cmds[0] {
            IpcCommand::PostReply { session_key, body } => {
                assert_eq!(session_key, &key);
                assert_eq!(body, "hello");
            }
            other => panic!("expected PostReply, got {other:?}"),
        }
        assert!(matches!(cmds[1], IpcCommand::Refresh));
    }

    /// Empty body short-circuits — no command produced, the
    /// modal is still popped (internal state), and the pending
    /// reply target is cleared. The whitespace case is handled
    /// the same way.
    #[test]
    fn textarea_submitted_with_empty_body_returns_no_commands() {
        let mut m = build_model();
        m.pending_reply = Some(SessionKey::from("github:o/r#1"));
        let cmds = m.handle_textarea_submitted("   ".into());
        assert!(cmds.is_empty());
        assert!(m.pending_reply.is_none());
    }

    /// No pending reply target → no command, even with a body.
    /// Defensive case (shouldn't reach this handler without a
    /// pending reply, but the contract handles it).
    #[test]
    fn textarea_submitted_with_no_target_returns_no_commands() {
        let mut m = build_model();
        let cmds = m.handle_textarea_submitted("hello".into());
        assert!(cmds.is_empty());
    }

    /// NewWorkspace input with a non-empty trimmed name AND a
    /// pre-stashed project_key produces `CreateWorkspace { name,
    /// project_key }`. Without a stashed project_key the submit
    /// drops (see `mount_new_workspace_input` — the catalog `n`
    /// flow only mounts when a project is focused).
    #[test]
    fn input_submitted_for_new_workspace_returns_create_workspace() {
        let mut m = build_model();
        let pk = pilot_core::ProjectKey::local("my-project");
        m.modal_stack.push(Id::NewWorkspace);
        m.pending_new_workspace_project = Some(pk.clone());
        let cmds = m.handle_input_submitted("  my-feature  ".into());
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::CreateWorkspace { name, project_key } => {
                assert_eq!(name, "my-feature");
                assert_eq!(project_key, &pk);
            }
            other => panic!("expected CreateWorkspace, got {other:?}"),
        }
    }

    /// Empty / whitespace-only input is dropped silently.
    #[test]
    fn input_submitted_with_empty_text_returns_no_commands() {
        let mut m = build_model();
        m.modal_stack.push(Id::NewWorkspace);
        let cmds = m.handle_input_submitted("   ".into());
        assert!(cmds.is_empty());
    }

    /// `y` on a RemoveOutOfScope confirm produces a `Kill` for
    /// the workspace + clears the prompt slot.
    #[test]
    fn confirmed_yes_on_remove_out_of_scope_returns_kill() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        m.active_removal_prompt = Some(ws_key.clone());
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_confirmed(true);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::Kill { session_key } => {
                assert_eq!(session_key, &SessionKey::from(&ws_key));
            }
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    /// `n` on RemoveOutOfScope clears the slot without producing
    /// a Kill — user said no, daemon doesn't need to hear about it.
    #[test]
    fn confirmed_no_on_remove_out_of_scope_returns_no_commands() {
        let mut m = build_model();
        m.active_removal_prompt = Some(WorkspaceKey::new("github:o/r#1"));
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_confirmed(false);
        assert!(cmds.is_empty());
    }

    /// `y` on MergeConfirm → `ConfirmMerge { accept: true }`.
    /// `n` on the same → `ConfirmMerge { accept: false }`. Both
    /// produce a command (the daemon needs to know either way so
    /// it stops re-prompting).
    #[test]
    fn confirmed_routes_merge_confirm_yes_and_no_to_daemon() {
        for (input, expected_accept) in [(true, true), (false, false)] {
            let mut m = build_model();
            let issue = WorkspaceKey::new("github:o/r#1");
            let pr = WorkspaceKey::new("github:o/r#2");
            m.active_merge_prompt = Some((issue.clone(), pr.clone()));
            m.modal_stack.push(Id::MergeConfirm);
            let cmds = m.handle_confirmed(input);
            assert_eq!(cmds.len(), 1, "input={input}");
            match &cmds[0] {
                IpcCommand::ConfirmMerge {
                    issue_workspace_key,
                    pr_workspace_key,
                    accept,
                } => {
                    assert_eq!(issue_workspace_key, &issue);
                    assert_eq!(pr_workspace_key, &pr);
                    assert_eq!(*accept, expected_accept, "input={input}");
                }
                other => panic!("expected ConfirmMerge, got {other:?}"),
            }
        }
    }

    /// Esc on a MergeConfirm modal acts the same as `n` — the
    /// daemon needs the explicit "no" so it drops the stall and
    /// doesn't re-prompt on the next poll.
    #[test]
    fn modal_dismissed_on_merge_confirm_sends_accept_false() {
        let mut m = build_model();
        m.active_merge_prompt = Some((
            WorkspaceKey::new("github:o/r#1"),
            WorkspaceKey::new("github:o/r#2"),
        ));
        m.modal_stack.push(Id::MergeConfirm);
        let cmds = m.handle_modal_dismissed();
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::ConfirmMerge { accept, .. } => assert!(!*accept),
            other => panic!("expected ConfirmMerge, got {other:?}"),
        }
    }

    /// Esc on a RemoveOutOfScope modal clears the slot but
    /// produces no command — there's nothing to tell the daemon;
    /// the workspace stays out of scope on its end too.
    #[test]
    fn modal_dismissed_on_remove_out_of_scope_clears_slot_silently() {
        let mut m = build_model();
        m.active_removal_prompt = Some(WorkspaceKey::new("github:o/r#1"));
        m.modal_stack.push(Id::RemoveOutOfScope);
        let cmds = m.handle_modal_dismissed();
        assert!(cmds.is_empty());
        assert!(m.active_removal_prompt.is_none());
    }

    /// Adopt picker: source + target workspace keys flow into an
    /// `AdoptSessions` command. The picks index resolves into the
    /// `adopt_choices` slot we set up.
    #[test]
    fn choice_picked_for_adopt_target_returns_adopt_sessions() {
        let mut m = build_model();
        let source = WorkspaceKey::new("github:o/r#1");
        let target = WorkspaceKey::new("github:o/r#2");
        m.pending_adopt_source = Some(source.clone());
        m.adopt_choices = vec![target.clone()];
        m.modal_stack.push(Id::AdoptTarget);
        let cmds = m.handle_choice_picked(vec![0]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::AdoptSessions {
                source_workspace_key,
                target_workspace_key,
            } => {
                assert_eq!(source_workspace_key, &source);
                assert_eq!(target_workspace_key, &target);
            }
            other => panic!("expected AdoptSessions, got {other:?}"),
        }
        // Side state: the adoption slot + choice list both clear.
        assert!(m.pending_adopt_source.is_none());
        assert!(m.adopt_choices.is_empty());
    }

    /// `Id::RequestReviewers` picker: selecting two indices into
    /// `review_choices` produces `Command::RequestReviewers` with
    /// those logins resolved + the workspace key from
    /// `pending_review_request`. (Migrated from the older Input
    /// modal — see `mount_request_reviewers`.)
    #[test]
    fn choice_picked_on_request_reviewers_modal_returns_request_reviewers_cmd() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#1");
        m.pending_review_request = Some(ws_key.clone());
        m.review_choices = vec!["alice".into(), "bob".into(), "carol".into()];
        m.modal_stack.push(Id::RequestReviewers);
        let cmds = m.handle_choice_picked(vec![0, 2]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::RequestReviewers {
                workspace_key,
                logins,
            } => {
                assert_eq!(workspace_key, &ws_key);
                assert_eq!(logins, &vec!["alice".to_string(), "carol".to_string()]);
            }
            other => panic!("expected RequestReviewers, got {other:?}"),
        }
        assert!(m.pending_review_request.is_none());
        assert!(m.review_choices.is_empty());
    }

    /// `Id::AddAssignees` picker symmetry.
    #[test]
    fn choice_picked_on_add_assignees_modal_returns_add_assignees_cmd() {
        let mut m = build_model();
        let ws_key = WorkspaceKey::new("github:o/r#5");
        m.pending_assignees_request = Some(ws_key.clone());
        m.assignees_choices = vec!["alice".into(), "bob".into()];
        m.modal_stack.push(Id::AddAssignees);
        let cmds = m.handle_choice_picked(vec![1]);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            IpcCommand::AddAssignees {
                workspace_key,
                logins,
            } => {
                assert_eq!(workspace_key, &ws_key);
                assert_eq!(logins, &vec!["bob".to_string()]);
            }
            other => panic!("expected AddAssignees, got {other:?}"),
        }
    }

    /// Empty pick (Esc — defensive) drops the slot without firing.
    #[test]
    fn choice_picked_on_request_reviewers_with_empty_picks_returns_no_commands() {
        let mut m = build_model();
        m.pending_review_request = Some(WorkspaceKey::new("github:o/r#1"));
        m.review_choices = vec!["alice".into()];
        m.modal_stack.push(Id::RequestReviewers);
        let cmds = m.handle_choice_picked(vec![]);
        assert!(cmds.is_empty());
    }
}

#[cfg(test)]
mod base64_tests {
    use super::base64_encode;

    #[test]
    fn rfc4648_test_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
