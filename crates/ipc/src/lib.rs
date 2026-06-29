//! Lazybox IPC — protocol between the TUI and the daemon.
//!
//! The daemon is the single source of truth for all state (sessions,
//! worktrees, PTYs, provider polling, persistence). The TUI issues
//! `Command`s and receives `Event`s.
//!
//! **Communication is abstracted behind `Client` / `Connection` traits.**
//! The common case — TUI and daemon living in one process — uses the
//! `channel` transport: a pair of tokio mpsc channels, zero
//! serialization, zero sockets. The remote case — TUI running on a
//! laptop connecting to a daemon on a workstation over SSH — uses the
//! `socket` transport: length-prefixed bincode over a Unix socket
//! (which SSH's `-L` forwards). Client code never branches on which.
//!
//! # Wire framing (socket transport only)
//!
//! Each message on the wire is `[u32 BE length][bincode payload]`.
//! Max frame size is `MAX_FRAME_BYTES` (64 MiB).

use lazybox_core::SessionKey;
use serde::{Deserialize, Serialize};
use std::fmt;

pub mod channel;
pub mod proxy;
pub mod socket;
pub mod transport;

pub use proxy::{ApiProvider, ProxyRecord, ToolCall};

pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Magic prefix of the 8-byte connection preamble each side sends
/// before any frames (`PROTOCOL_MAGIC ++ PROTOCOL_VERSION as u32 LE`).
/// Lets a peer distinguish "wrong-version lazybox" from "not lazybox
/// at all" before bincode ever touches the stream.
pub const PROTOCOL_MAGIC: [u8; 4] = *b"LZBX";

/// Wire protocol version, negotiated by the connection handshake
/// (`socket::client_handshake` / `socket::server_handshake`).
///
/// MUST be bumped on ANY change to the `Command` / `Event` encodings —
/// bincode identifies enum variants by ordinal and structs by field
/// order, so adding, removing, or reordering a variant or field makes
/// an old peer silently misread every subsequent frame. The handshake
/// turns that garbage into a clear "restart the daemon" error.
pub const PROTOCOL_VERSION: u32 = 5;

/// This binary's build identity: the workspace version plus the git
/// short SHA captured at compile time (`build.rs`). Two binaries built
/// from the same commit share this string; a stale daemon and a fresh
/// client differ.
///
/// `PROTOCOL_VERSION` only changes when the wire format does, so two
/// builds dozens of commits apart can both be protocol v5 and connect
/// cleanly while behaving differently. The handshake exchanges this
/// string so the client can surface a "restart the daemon" banner on a
/// build skew the protocol version can't see.
pub const BUILD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("LAZYBOX_BUILD_SHA"));

/// The build commit, suffix-free (no `-dirty`), or `"unknown"` when
/// built outside a git checkout. Distinct from the SHA baked into
/// [`BUILD_VERSION`] (which carries the dirty marker) because the
/// staleness guard feeds it to `git rev-list` as a revision, where the
/// suffix would make it unresolvable.
pub const BUILD_GIT_SHA: &str = env!("LAZYBOX_BUILD_GIT_SHA");

/// Absolute path of the git checkout this binary was built from, or
/// empty when built outside one (a release tarball). The staleness
/// guard runs `git -C <this> rev-list --count <BUILD_GIT_SHA>..origin/main`
/// to count how far behind `main` the running build is; an empty value
/// disables the check rather than guessing.
pub const BUILD_SOURCE_DIR: &str = env!("LAZYBOX_BUILD_SOURCE_DIR");

/// Stable id for a spawned terminal. Distinct from SessionKey because a
/// single session may hold multiple terminals (agent + shell + logs).
///
/// `Default = TerminalId(0)` backs `#[serde(default)]` on optional
/// terminal-id fields — `0` never corresponds to a real allocation (the
/// daemon's id allocator starts at 1), so it's a safe sentinel for "the
/// producer omitted the field." Note the default only ever applies on
/// self-describing transports (the JSON gateway): bincode is not
/// self-describing, so on the socket transport an absent field is a
/// decode error, never a default — mixed-version peers are rejected by
/// the protocol-version handshake instead, and adding even a trailing
/// field requires a `PROTOCOL_VERSION` bump.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalId(pub u64);

/// Stable id for a structured agent runtime. This is intentionally
/// separate from `TerminalId`: a run may be stream-json only, terminal
/// only, or mirrored into both surfaces by higher layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentRunId(pub u64);

/// Runtime surface requested for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRuntimeMode {
    /// Traditional PTY/terminal byte stream.
    Terminal,
    /// Structured stream-json, independent of PTY bytes.
    StreamJson,
}

/// What to launch inside a terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminalKind {
    /// A known agent by id (e.g. "claude", "codex"). The daemon looks
    /// up the `Agent` impl and computes argv.
    Agent(String),
    /// Plain shell — `config.shell.command`.
    Shell,
    /// Tail a file inside the worktree.
    LogTail { path: String },
}

/// `RunnerKind` is the user-facing vocabulary: every PTY child of a
/// session is a "runner", whether it runs an agent or a plain shell.
/// `TerminalKind` is the wire-historic name kept for back-compat.
/// They're the same type — pick whichever reads better at the call
/// site. New code should prefer `RunnerKind`.
pub type RunnerKind = TerminalKind;

/// `RunnerId` mirrors `TerminalId` for the same reason. Daemon-
/// allocated u64 — a session-local handle, not a global UUID.
pub type RunnerId = TerminalId;

impl TerminalKind {
    /// Whether at most one runner of this kind may exist in a single
    /// session. Singleton kinds (Agent variants — Claude, Codex,
    /// Cursor) toggle-or-focus on duplicate spawn requests; multi
    /// kinds (Shell) always spawn a new instance.
    pub fn is_singleton(&self) -> bool {
        matches!(self, TerminalKind::Agent(_))
    }

    /// Equality of "uniqueness identity". Two singleton kinds collide
    /// iff their agent ids match. Two shells never collide. LogTail
    /// collides on path.
    pub fn singleton_key(&self) -> Option<String> {
        match self {
            TerminalKind::Agent(id) => Some(format!("agent:{id}")),
            TerminalKind::LogTail { path } => Some(format!("logtail:{path}")),
            TerminalKind::Shell => None,
        }
    }
}

/// What the agent's PTY is doing right now. Drives the side-panel
/// state slot (working spinner / "needs input" pill / done / idle) and
/// the TerminalStack tab badge.
///
/// The four states are mutually exclusive and share a single UI
/// slot per session. They're produced per-agent-kind by
/// [`Agent::detect_state`](../lazybox_agents/trait.Agent.html) and the
/// agent's lifecycle hooks — each agent decides how to recognise
/// "working" / "input needed" from its own PTY output. An agent with
/// no opinion returns `None`, which consumers treat as `Idle` (so an
/// unknown agent never falsely reports `Working`).
///
/// `InputNeeded` and `Done` are the two states where the user must
/// act, so they raise an alert (desktop notification + footer notice);
/// `Working` and `Idle` are silent.
///
/// Variants are appended, never reordered: the socket transport
/// encodes this enum by bincode ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// Actively producing output / running a tool right now — the
    /// agent's status line shows a streaming spinner / pulser.
    Working,
    /// Paused waiting on the user at a structural prompt: a permission
    /// gate, chooser, or Y/N prompt (see issues #26 / #122). Freeform
    /// conversational asks are deliberately not flagged. → alert.
    InputNeeded,
    /// Idle with no active work: freshly launched, or sitting at a
    /// ready composer having never run a task. The safe default for
    /// any agent that can't tell. Silent — nothing to act on.
    Idle,
    /// Finished its turn — the agent ran work and has now come to rest
    /// (Claude's `Stop` hook). Distinct from `Idle`, which never
    /// worked. → alert. Sticky: a subsequent idle reading keeps `Done`
    /// until the agent works again or asks for input.
    ///
    /// Hook-exclusive: only the lifecycle-hook path ever emits `Done`.
    /// The PTY screen-scrape detector has no "finished" anchor to read
    /// and tops out at `Idle`, so a hookless agent never reaches `Done`.
    Done,
}

/// A normalized lifecycle hook fired by an agent, decoupled from the
/// agent's wire JSON. Claude Code emits these via configured hooks
/// (`Stop`, `Notification`, `PreToolUse`, …); lazybox injects a hook
/// command at spawn so the daemon receives deterministic state signals
/// instead of screen-scraping the PTY. The wire JSON → `HookEvent`
/// translation lives in `lazybox_agents::hook`; mapping `HookEvent` →
/// [`AgentState`] lives there too. This type is the IPC-stable shape
/// carried by [`Command::IngestHook`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEvent {
    pub kind: HookEventKind,
    /// The agent's own session id (Claude's `session_id`). Informational
    /// — lazybox correlates by the backend key it baked into the hook
    /// command, not this — but captured for the structured-stream path.
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// Tool being invoked, for `PreToolUse` / `PostToolUse`.
    pub tool_name: Option<String>,
    /// Notification descriptor (`notification_type` or `message`), used
    /// to distinguish a permission/elicitation prompt from an idle one.
    pub notification: Option<String>,
}

/// The lifecycle point a [`HookEvent`] fired at. `Other` is the
/// catch-all for hook names lazybox doesn't map to a state transition.
///
/// `PermissionRequest`, `SubagentStart`, and `PostCompact` are events
/// Claude Code does not actually fire — nothing produces them anymore,
/// but the variants stay (in place) because the socket transport
/// encodes this enum by bincode ordinal. `UserPromptSubmit` is appended
/// at the end for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEventKind {
    SessionStart,
    SessionEnd,
    PreToolUse,
    PostToolUse,
    Notification,
    PermissionRequest,
    Stop,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Other,
    UserPromptSubmit,
}

/// User input sent to a structured agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInputMessage {
    /// Human-readable user text.
    pub text: Option<String>,
    /// Raw JSON payload for runtimes that accept structured input.
    pub json: Option<String>,
}

/// Decision for a tool/permission request emitted by an agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentApprovalDecision {
    Approve,
    Deny { reason: Option<String> },
}

/// Answer to a structured question from an agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentQuestionAnswer {
    pub answer: String,
}

/// Token/cost usage reported by a structured agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    /// Cost in millionths of a USD. Integer wire value avoids float
    /// compatibility issues across languages.
    pub cost_usd_micros: Option<u64>,
}

/// Stable identity for the human or service account connected to a
/// Lazybox daemon. The current local daemon uses `local`; remote/multi-user
/// clients should authenticate into distinct principal ids.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrincipalId(String);

impl PrincipalId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn local() -> Self {
        Self("local".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for PrincipalId {
    fn default() -> Self {
        Self::local()
    }
}

impl From<&str> for PrincipalId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for PrincipalId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PrincipalId").field(&self.0).finish()
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Public, non-secret credential metadata that clients may receive in
/// snapshots or events. Secret material is deliberately not represented
/// here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredentialMetadata {
    pub principal_id: PrincipalId,
    pub provider_id: String,
    pub source: String,
    pub scopes: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Secret-bearing credential bootstrap payload. Custom `Debug` keeps
/// daemon command tracing from printing provider tokens.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderCredentialInput {
    pub provider_id: String,
    pub token: String,
    pub source: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl fmt::Debug for ProviderCredentialInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredentialInput")
            .field("provider_id", &self.provider_id)
            .field("token", &"[REDACTED]")
            .field("source", &self.source)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Spawn parameters carried alongside `Command::InjectPrompt` so the
/// daemon can fall back to creating a fresh terminal when the cached
/// `terminal_id` no longer exists (agent died between the user's `w`
/// press and the command arriving). Mirrors the `Spawn` variant's
/// fields exactly so the rewrite is a straight field-for-field copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnFallback {
    pub session_key: SessionKey,
    #[serde(default)]
    pub session_id: Option<lazybox_core::SessionId>,
    pub kind: TerminalKind,
    pub cwd: Option<String>,
}

/// One row of `Event::WorktreesInspected`. Mirrors
/// `lazybox_git_ops::WorktreeInspection` as a wire-friendly value type
/// (no `SystemTime`, no library-specific enum). `reasons` carries the
/// short tags from `OrphanReason::tag()` so clients can render
/// without needing to depend on `lazybox-git-ops`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInspectionDto {
    pub path: std::path::PathBuf,
    pub bare_path: Option<std::path::PathBuf>,
    pub branch: Option<String>,
    pub session_id: Option<String>,
    pub reasons: Vec<String>,
    pub size_bytes: u64,
    /// Most-recent mtime in the worktree, as seconds since the Unix
    /// epoch. `None` when the directory is gone (prunable entries).
    pub last_modified_unix: Option<u64>,
    pub has_uncommitted_changes: bool,
    pub has_unpushed_commits: bool,
    pub is_safe_to_delete: bool,
}

/// TUI → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Start streaming events. Connection replies with `Event::Snapshot`
    /// then a live stream.
    Subscribe,
    /// Create a fresh `Session` (== fresh worktree folder) inside the
    /// workspace identified by `session_key` (this name on the wire
    /// holds the workspace key — see the SessionKey docs). The
    /// daemon allocates a new `SessionId`, sets up the worktree on
    /// disk, and emits `Event::SessionCreated`. The TUI uses this
    /// when the user explicitly wants a separate folder from any
    /// existing sessions.
    CreateSession {
        session_key: SessionKey,
        kind: TerminalKind,
        /// Optional friendly label. Defaults to the kind's name.
        label: Option<String>,
    },
    /// Spawn a terminal inside a session. `session_id == Some(id)`
    /// targets that specific session; `None` lets the daemon pick the
    /// workspace's default session (creating one on the fly when the
    /// workspace has no sessions yet). The session supplies the cwd
    /// (its worktree path). `cwd` may override that for ad-hoc spawns.
    Spawn {
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<lazybox_core::SessionId>,
        kind: TerminalKind,
        cwd: Option<String>,
        /// Optional initial prompt the daemon should inject after the
        /// agent reaches its ready state. Drives the `f`-for-fix flow:
        /// sidebar/activity panes pre-build the agent instruction so
        /// the user doesn't have to retype it. Ignored for `Shell` —
        /// shells don't define `Agent::inject_prompt`.
        #[serde(default)]
        initial_prompt: Option<String>,
    },
    Write {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
    },
    /// Persist the latest prompt the user submitted to an agent
    /// terminal so the pinned "you ▸ …" recap survives a restart. The
    /// recap is composed client-side from the *outgoing* keystroke
    /// stream (see `TerminalSlot::record_pty_bytes`), which the daemon
    /// never reconstructs, so the client reports each committed message
    /// here for the daemon to store against the terminal's backend key.
    /// Replayed back to clients via `TerminalSnapshot::last_user_message`.
    RecordUserMessage {
        terminal_id: TerminalId,
        message: String,
    },
    /// Inject a prompt into an EXISTING agent terminal — same flow
    /// the daemon uses for Spawn's `initial_prompt`, but targeting
    /// a live terminal so `w fix CI` / `w address comments` reuses
    /// the user's running claude tab instead of spawning a second
    /// one. The daemon looks up the agent for `terminal_id` and
    /// runs `agent.inject_prompt(prompt)` + `agent.inject_submit()`,
    /// the same paste/submit split used at spawn time.
    ///
    /// `fallback_spawn` covers the race where the TUI cached the
    /// agent's terminal id, the agent died, `TerminalExited` is in
    /// flight but the user already pressed `w`. Without the fallback
    /// the daemon used to silently no-op and the prompt was lost.
    /// When set, an unknown / dead `terminal_id` is rewritten back
    /// into a Spawn with the carried parameters + `prompt` as the
    /// initial prompt.
    InjectPrompt {
        terminal_id: TerminalId,
        prompt: String,
        #[serde(default)]
        fallback_spawn: Option<SpawnFallback>,
    },
    Resize {
        terminal_id: TerminalId,
        cols: u16,
        rows: u16,
    },
    Close {
        terminal_id: TerminalId,
    },
    /// A lifecycle hook fired by an agent (Claude Code), forwarded by
    /// the `lazybox hook-ingest` helper the daemon injects at spawn. The
    /// daemon maps it to an [`AgentState`] transition — deterministic
    /// state, no PTY screen-scraping.
    ///
    /// Correlation is by `backend_key` — the stable backend session
    /// key (tmux session name) lazybox baked into the hook command at
    /// spawn. Backend keys survive daemon restarts, unlike
    /// `TerminalId`s, which restart at the process boundary while a
    /// tmux-backed agent keeps running with the old id in its settings
    /// file. `terminal_id` is the legacy correlation field from
    /// pre-backend-key settings files; the daemon drops hooks that
    /// carry only it (such a session just falls back to PTY
    /// detection). The field stays in this position — and
    /// `backend_key` is appended last — because the socket transport
    /// encodes commands with bincode, which is field-order sensitive.
    /// The `#[serde(default)]` below only takes effect on the JSON
    /// gateway; bincode is not self-describing and never applies
    /// defaults, so appending this field was a wire-format change
    /// protected by the protocol-version handshake (a
    /// `PROTOCOL_VERSION` bump), not by the attribute.
    IngestHook {
        terminal_id: TerminalId,
        hook: HookEvent,
        #[serde(default)]
        backend_key: Option<String>,
    },
    Kill {
        session_key: SessionKey,
    },
    /// Answer to a `MergedPrRemovable` event (the user confirmed the
    /// "this PR merged — remove its workspace and worktree?" modal).
    /// Kills the workspace's sessions, force-deletes its backing
    /// worktree directories, then drops the row — the worktree
    /// deletion `Kill` deliberately skips. `force` is implied by the
    /// confirm modal having warned about any uncommitted/unpushed
    /// work, so the daemon always reaps the dirs here.
    RemoveMergedWorkspace {
        session_key: SessionKey,
    },
    /// Delete a Project: kill every workspace under it (which kills
    /// every backing terminal) then drop the Project record. The
    /// daemon broadcasts `WorkspaceRemoved` for each workspace then
    /// `ProjectRemoved` for the project so the sidebar can drop the
    /// rows in one batch. Destructive — gated by the unified
    /// ActionConfirm modal on the TUI side.
    DeleteProject {
        project_key: lazybox_core::ProjectKey,
    },
    /// Manually collapse an issue workspace into the PR workspace
    /// that closes it. Same end-state as the auto-detect path
    /// (`merge_closing_issue_workspaces`) but invoked by the user —
    /// bypasses the `rejected_merge` / `prompted_merge` dedupe
    /// state so a previously-dismissed prompt becomes actionable
    /// again. The daemon picks the target PR by scanning workspaces
    /// for one whose `closes_issues` includes this issue.
    CollapseIntoPr {
        issue_workspace_key: SessionKey,
    },
    MarkRead {
        session_key: SessionKey,
    },
    /// Hint to the daemon: the user is now looking at this
    /// workspace. The polling layer uses it to bump the workspace's
    /// repo to the front of the round-robin sync cursor so a
    /// comment landing on the visible PR shows up next cycle
    /// instead of waiting the rest of the rotation. No store
    /// mutation, no broadcast — pure scheduling hint.
    ///
    /// Sent on sidebar cursor moves alongside the existing
    /// `MarkRead`. The daemon silently ignores it if the workspace
    /// has no upstream repo (e.g. a locally-created pre-PR
    /// sandbox).
    FocusWorkspace {
        session_key: SessionKey,
    },
    /// Mark exactly one activity row as read. The auto-mark-on-hover
    /// flow uses this so a brief glance at one comment doesn't flip
    /// the whole workspace's unread badge to zero. `index` is the
    /// activity slot in `Workspace.activity` after the daemon's
    /// `sort_activity` pass — the same view the TUI sees.
    MarkActivityRead {
        session_key: SessionKey,
        index: usize,
    },
    /// Reverse a previous `MarkActivityRead`. Bound to the `z` undo
    /// affordance.
    UnmarkActivityRead {
        session_key: SessionKey,
        index: usize,
    },
    /// Create a brand-new pre-PR workspace with a user-chosen name
    /// inside a specific Project. The daemon allocates a fresh
    /// `WorkspaceKey` (slug-based, with a numeric suffix on
    /// collision), stamps the workspace with `project_key`, persists,
    /// and broadcasts. Used by the sidebar's `n` key — which requires
    /// the cursor to be on a Project header (or a workspace under one)
    /// so `project_key` is always resolvable.
    CreateWorkspace {
        name: String,
        /// The Project this workspace lives under. The TUI resolves
        /// it from the sidebar cursor before sending; the daemon
        /// trusts the value (no project-exists check today — the
        /// `n` flow can't fire without a focused project, and a
        /// stale key just produces an orphan workspace).
        project_key: lazybox_core::ProjectKey,
        /// When `Some(agent_id)`, the daemon immediately spawns that
        /// agent (claude / codex / cursor / …) into the freshly
        /// created workspace, so the user lands in a live session
        /// instead of an empty row. `None` leaves the workspace bare.
        /// The TUI sets this to the configured default agent for both
        /// the `n` key and the global "start agent" shortcut.
        spawn_agent: Option<String>,
    },
    /// Create a brand-new local Project — a top-level container the
    /// sidebar groups workspaces under, like a github repo but with
    /// no upstream provider. Slugified to `local-<slug>`; idempotent
    /// on collision (re-opens the existing project, same shape as
    /// `polling::ensure_project_for_workspace` for provider
    /// projects). Bound to `Shift-N` in the default keymap.
    CreateProject {
        name: String,
    },
    /// Update the per-session tile/tab layout (`SessionLayout`).
    /// Persisted so the user's split arrangement survives restart.
    /// `layout_json` carries the serialized `lazybox_core::SessionLayout`
    /// — a string here keeps the wire type free of a core dep without
    /// forcing the IPC crate into the workspace types.
    SetSessionLayout {
        session_key: SessionKey,
        session_id_raw: String,
        layout_json: String,
    },
    Snooze {
        session_key: SessionKey,
        until: chrono::DateTime<chrono::Utc>,
    },
    Unsnooze {
        session_key: SessionKey,
    },
    /// Post a top-level reply to the workspace's primary task. Today
    /// this maps to "create an issue/PR comment" on GitHub; future
    /// providers (Linear, etc.) wire their own send path. The daemon
    /// posts via the workspace's owning provider, then `Refresh`-es so
    /// the new comment lands in the activity feed on the next poll.
    PostReply {
        session_key: SessionKey,
        body: String,
    },
    Refresh,
    Shutdown,
    /// Answer to a `WorkspaceMergePending` event. When the daemon
    /// detects a PR that `closes` an issue whose workspace has live
    /// sessions, it stalls the merge and asks via the TUI. The TUI
    /// replies here: `accept=true` runs the merge (sessions move to
    /// the PR workspace, issue row disappears); `accept=false`
    /// leaves both rows visible and the stall is dropped — the
    /// merge won't be re-prompted for this issue until lazybox
    /// restarts.
    ConfirmMerge {
        issue_workspace_key: lazybox_core::WorkspaceKey,
        pr_workspace_key: lazybox_core::WorkspaceKey,
        accept: bool,
    },
    /// Manual "adopt": move all sessions from `source_workspace_key`
    /// into `target_workspace_key`. Driven by the sidebar's `Shift-A`
    /// picker — useful when you started work on the wrong row and
    /// want to migrate the running agent without losing it. Unlike
    /// the issue→PR merge, the source workspace is NOT deleted; it
    /// just becomes a session-less tracking row the user can ignore
    /// or remove via `Shift-X`.
    AdoptSessions {
        source_workspace_key: lazybox_core::WorkspaceKey,
        target_workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Merge the workspace's PR via the provider. Fires from the
    /// sidebar's `Shift-M` shortcut on a READY (approved + green
    /// CI) row. The daemon looks up the PR's `node_id` and calls
    /// the GraphQL `mergePullRequest` mutation. Method defaults
    /// to the repo's setting; future per-repo config can override.
    MergePr {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Request reviews on the workspace's PR from the given GitHub
    /// logins. Adds to the existing reviewer set (no replacement).
    /// Only meaningful when the focused workspace's primary task is
    /// a PR — daemon resolves the PR's node ID from its stored task
    /// and calls the `requestReviews` GraphQL mutation. Logins
    /// flow through unchanged; the daemon resolves each to a node
    /// ID before issuing the mutation.
    RequestReviewers {
        workspace_key: lazybox_core::WorkspaceKey,
        logins: Vec<String>,
    },
    /// Add assignees to the workspace's PR or issue. Works on any
    /// `Assignable` (both PRs and issues). Same flow as
    /// `RequestReviewers` — daemon resolves logins → node IDs and
    /// calls the `addAssigneesToAssignable` mutation.
    AddAssignees {
        workspace_key: lazybox_core::WorkspaceKey,
        logins: Vec<String>,
    },
    /// Replace the assignee set on the workspace's PR / issue.
    /// Daemon diffs against the currently-persisted assignees and
    /// fires `addAssigneesToAssignable` + `removeAssigneesFromAssignable`
    /// as needed so this is a single atomic-feeling "set to exactly
    /// this list" operation from the TUI's POV. Empty list clears
    /// every assignee.
    SetAssignees {
        workspace_key: lazybox_core::WorkspaceKey,
        logins: Vec<String>,
    },
    /// Replace the label set on the workspace's PR / issue. Daemon
    /// diffs against the currently-persisted labels and fires
    /// `addLabelsToLabelable` + `removeLabelsFromLabelable` as
    /// needed. Empty list clears every label. Names are matched
    /// against the repo's full label set; names that don't exist on
    /// the repo are silently dropped.
    SetLabels {
        workspace_key: lazybox_core::WorkspaceKey,
        names: Vec<String>,
    },
    /// Ask the daemon to fetch the repository's full label set for
    /// the workspace's PR / issue and broadcast it back via
    /// `Event::RepoLabels`. Used by the label picker on mount so
    /// the user can pick from every label the repo defines (not
    /// just the ones currently applied).
    FetchRepoLabels {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Admin command: walk every persisted workspace, drop sessions
    /// whose terminals aren't currently live, and remove the
    /// corresponding worktrees from disk. Used to reclaim disk
    /// space without losing the inbox (the PR / issue rows stay —
    /// only the working-tree directories + session records are
    /// torn down). Live terminals are skipped so a running claude
    /// agent isn't pulled out from under itself.
    ///
    /// Daemon broadcasts a `WorkspaceUpserted` for every workspace
    /// it touched so the sidebar shows the now-empty session list.
    /// A final `CleanWorktreesCompleted` lets the TUI surface
    /// "cleaned N worktrees, freed N GB" in the footer.
    CleanWorktrees,
    /// Walk `<state_root>/worktrees/` + every bare clone, classify
    /// each entry (orphan reasons, size, last modified, uncommitted /
    /// unpushed work), and reply with `Event::WorktreesInspected`.
    /// Read-only — no deletes happen until the TUI follows up with
    /// per-row `DeleteOrphanedWorktree` calls.
    InspectWorktrees,
    /// Delete a single worktree the inspector flagged.
    ///
    /// `force = false` makes the daemon re-check safety on its side
    /// (uncommitted / unpushed / locked) and refuse if anything looks
    /// risky. `force = true` overrides that gate — only sent after an
    /// explicit user confirm for a row with local work. Reply arrives
    /// as `Event::OrphanedWorktreeDeleted`.
    DeleteOrphanedWorktree {
        path: std::path::PathBuf,
        #[serde(default)]
        force: bool,
    },
    /// Lazy-fetch the workspace's PR-detail activity (review-thread
    /// comments). The inbox-scan query intentionally skips
    /// `reviewThreads` for cost reasons; this command back-fills
    /// them when the user opens the workspace's activity pane. The
    /// daemon merges the result into the workspace's activity list
    /// and broadcasts an updated `WorkspaceUpserted`. No-op when
    /// the workspace has no PR (issues don't have review threads).
    FetchPrDetails {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Start an agent runtime using a structured protocol surface. This
    /// does not replace `Spawn`; terminal clients can keep using PTY
    /// bytes while structured clients subscribe to run events.
    StartAgentRun {
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<lazybox_core::SessionId>,
        agent: String,
        mode: AgentRuntimeMode,
        cwd: Option<String>,
        initial_input: Option<AgentInputMessage>,
    },
    SendAgentInput {
        run_id: AgentRunId,
        message: AgentInputMessage,
    },
    InterruptAgentRun {
        run_id: AgentRunId,
    },
    DecideAgentApproval {
        run_id: AgentRunId,
        request_id: String,
        decision: AgentApprovalDecision,
    },
    AnswerAgentQuestion {
        run_id: AgentRunId,
        question_id: String,
        answer: AgentQuestionAnswer,
    },
    /// Store/update a provider credential for one Lazybox principal.
    /// This is the bootstrap path for local desktop clients; future
    /// API connection auth can make `principal_id` implicit.
    UpsertProviderCredential {
        principal_id: PrincipalId,
        credential: ProviderCredentialInput,
    },
    RemoveProviderCredential {
        principal_id: PrincipalId,
        provider_id: String,
    },
    ListProviderCredentials {
        principal_id: PrincipalId,
    },
}

/// Connection → TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    // ── Hierarchy reminder ────────────────────────────────────────
    //
    // Repo (string `"owner/name"` from the task's provider)
    //  └── Workspace (one unit-of-work; `Workspace`)
    //       └── Session (= one folder worktree; runtime state)
    //            └── Terminal (= one PTY rooted in that folder)
    //
    // Snapshot carries Workspace rows; Sessions and Terminals are
    // recovered separately so a client reconnecting mid-flight can
    // re-bind to its running agents. `WorkspaceUpserted` /
    // `WorkspaceRemoved` are the fan-out events; `TerminalSpawned` /
    // `TerminalOutput` / `TerminalExited` track the bottom layer.
    /// Initial snapshot reply to `Subscribe`. Sent once before the
    /// live stream starts so the client has a baseline. The row model
    /// is `Workspace` — one per worktree, holding the linked PR +
    /// issues; every component reads from the workspace directly and
    /// projects to a primary task via `workspace.primary_task()`.
    Snapshot {
        workspaces: Vec<lazybox_core::Workspace>,
        terminals: Vec<TerminalSnapshot>,
        /// Top-level Projects the daemon knows about. Sidebar
        /// headers render from here so empty projects (no workspaces
        /// yet) still appear. The `#[serde(default)]` only matters on
        /// self-describing transports (the JSON gateway); it does NOT
        /// make older daemons wire-compatible over the socket —
        /// bincode never applies defaults, so adding this field was a
        /// wire-format change guarded by the protocol-version
        /// handshake (`PROTOCOL_VERSION` bump). Any future trailing
        /// field needs the same bump.
        #[serde(default)]
        projects: Vec<lazybox_core::Project>,
    },
    /// Authenticated user's login per provider source ("github" →
    /// "AntoineToussaint", etc.). Emitted once after the daemon's
    /// gh/linear client(s) initialize; the TUI uses these logins
    /// to render activity authored by the current user as `@me`
    /// instead of the bare login. Empty vec is valid — providers
    /// that don't have an identified user just omit themselves.
    ViewerIdentities {
        logins: Vec<(String, String)>,
    },
    /// A workspace was created or updated.
    /// Boxed because Workspace is several KB once activity is
    /// populated; keeping the `Event` enum slim avoids worst-case
    /// async-channel overhead.
    WorkspaceUpserted(Box<lazybox_core::Workspace>),
    WorkspaceRemoved(lazybox_core::WorkspaceKey),
    /// A project (top-level container — github repo, linear team,
    /// or local) was registered or updated. Sidebar headers render
    /// from these. Emitted both on initial snapshot replay AND when
    /// polling discovers a new repo/team.
    ProjectUpserted(Box<lazybox_core::Project>),
    /// A project was removed (user deleted a local project, or all
    /// scope/filter rules that pointed at it were dropped).
    ProjectRemoved(lazybox_core::ProjectKey),
    /// Daemon detected that this workspace fell out of scope (the
    /// user removed its repo, narrowed the filter so its task no
    /// longer matches, etc.) AND it has running terminals. The
    /// daemon does NOT auto-delete; instead it sends this event so
    /// the TUI can ask the user whether to kill the running sessions
    /// or keep the workspace anyway. The TUI responds with either
    /// `Command::Kill { session_key }` (drop everything) or by doing
    /// nothing (workspace stays out-of-scope but visible).
    WorkspaceOutOfScope {
        workspace_key: lazybox_core::WorkspaceKey,
        /// Compact identifier for the confirm modal — `owner/repo#N`
        /// for PRs/issues, the workspace key otherwise.
        label: String,
        /// Primary task title (PR/issue subject) when known. The TUI
        /// renders it inline so the user can recognize which work is
        /// about to be killed without memorizing PR numbers.
        #[serde(default)]
        title: Option<String>,
        /// Number of live terminals (Claude/codex/shell/…) the user
        /// would lose if they confirm removal.
        active_terminal_count: usize,
    },
    /// Daemon detected a PR that closes an issue and wants to merge
    /// the issue's workspace into the PR's — BUT the issue has live
    /// sessions. The TUI asks the user; reply via
    /// `Command::ConfirmMerge`. Without live sessions the daemon
    /// merges silently and emits `WorkspaceMerged` instead.
    WorkspaceMergePending {
        issue_workspace_key: lazybox_core::WorkspaceKey,
        pr_workspace_key: lazybox_core::WorkspaceKey,
        /// Compact `owner/repo#N` form for both sides — the TUI renders
        /// them in the confirm modal so the user can recognize what's
        /// about to fold without memorizing keys.
        issue_label: String,
        pr_label: String,
        /// Number of live terminals on the issue's side. The reason
        /// we're prompting in the first place; the modal text quotes
        /// it back so the user knows what they'd be moving.
        active_terminal_count: usize,
    },
    /// Daemon collapsed an issue workspace into its closing PR. Sent
    /// for both the silent (no live sessions) and the
    /// post-confirm paths. The TUI flashes a footer notice so the
    /// inbox-row disappearance isn't surprising.
    WorkspaceMerged {
        issue_workspace_key: lazybox_core::WorkspaceKey,
        pr_workspace_key: lazybox_core::WorkspaceKey,
        issue_label: String,
        pr_label: String,
    },
    /// The PR for `workspace_key` was just merged via `Command::MergePr`.
    /// The local Task still reads `Open` until the next poll catches
    /// up, so the TUI flashes a footer notice so the keypress doesn't
    /// look like a no-op.
    PrMerged {
        workspace_key: lazybox_core::WorkspaceKey,
        pr_label: String,
    },
    /// A PR just transitioned open→merged and its workspace is a
    /// candidate for removal. Emitted once per merge (the upsert path
    /// only acts on the open→merged transition, so a re-poll of an
    /// already-merged PR doesn't re-fire). The daemon has inspected
    /// the backing worktree(s); the TUI prompts the user — reusing
    /// the removal-confirm modal — and, on yes, replies with
    /// `Command::RemoveMergedWorkspace`. On no it does nothing and the
    /// merge won't re-prompt (the transition is already persisted).
    ///
    /// Suppressed when `worktree.auto_cleanup_merged` is enabled — that
    /// opt-in path reaps safe worktrees silently instead.
    MergedPrRemovable {
        workspace_key: lazybox_core::WorkspaceKey,
        /// Compact `owner/repo#N` form for the confirm modal copy.
        label: String,
        /// Live terminals that removal would kill. Quoted back so the
        /// user knows what they'd lose.
        active_terminal_count: usize,
        /// Any backing worktree has uncommitted or unpushed work. The
        /// modal warns when set; removal force-deletes regardless.
        has_local_work: bool,
    },
    /// Response to `Command::FetchRepoLabels`. Carries the
    /// repository's full label set (name + color) so the TUI's
    /// label picker can populate without round-tripping a
    /// request/response correlation id — the picker is keyed by
    /// `workspace_key` so the receiver knows which mount to fill.
    RepoLabels {
        workspace_key: lazybox_core::WorkspaceKey,
        labels: Vec<lazybox_core::Label>,
    },
    /// A new session (= folder worktree) was provisioned inside its
    /// workspace. Sent in response to `Command::CreateSession` and
    /// also when the daemon auto-creates a session for a workspace-
    /// scoped Spawn. Sidebar uses this to expand the workspace row
    /// into session sub-rows once the count crosses 1.
    SessionCreated(Box<lazybox_core::WorkspaceSession>),
    /// Progress signal during first-time worktree provisioning (cold
    /// clone / fetch / `git worktree add` / mounts / scripts). Emitted
    /// only on the provisioning path — an instant resume of an existing
    /// worktree sends none, so the TUI's progress modal never flashes
    /// for the fast path. `session_key` ties the events to the spawn
    /// the user just triggered; the matching `TerminalSpawned` *queues*
    /// the dismiss rather than tearing the modal down on the spot, so a
    /// fast provision still walks every step before the modal closes.
    WorktreeProgress {
        session_key: SessionKey,
        step: WorktreeStep,
        status: WorktreeStepStatus,
    },
    /// A session ended (process exited and the worktree was reaped,
    /// OR the user explicitly killed it). Includes the workspace
    /// key so consumers can look up which row to update without
    /// parsing the session id.
    SessionEnded {
        workspace_key: lazybox_core::WorkspaceKey,
        session_id: lazybox_core::SessionId,
    },
    TerminalSpawned {
        terminal_id: TerminalId,
        session_key: SessionKey,
        kind: TerminalKind,
        /// Launched in no-permission / bypass mode (autonomous
        /// session running unattended). Drives the session UI's
        /// "no-perms" indicator. Always `false` for interactive and
        /// shell terminals.
        #[serde(default)]
        no_permission: bool,
    },
    TerminalOutput {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
        /// Monotonic per-terminal sequence for gap detection.
        seq: u64,
    },
    /// Re-establish a terminal's grid from the daemon-side replay ring
    /// after the bounded event channel dropped one or more
    /// `TerminalOutput` chunks for it (see [`channel`] / the server's
    /// per-connection forwarder). Dropping raw bytes mid-stream
    /// corrupts the libghostty-vt parser, so the forwarder never lets a
    /// partial stream through: it drops the coalescable output, then
    /// emits exactly one `TerminalResync` carrying the full ring. The
    /// consumer resets its parser and re-feeds `replay`, which
    /// reconstructs the correct screen without the dropped bytes. `seq`
    /// is the ring's last sequence — the consumer adopts it so the
    /// resumed live stream (all `seq` strictly greater) applies exactly
    /// once.
    TerminalResync {
        terminal_id: TerminalId,
        replay: Vec<u8>,
        seq: u64,
    },
    TerminalExited {
        terminal_id: TerminalId,
        exit_code: Option<i32>,
    },
    /// Daemon-driven "focus this existing terminal instead of
    /// spawning a duplicate". Fired by the singleton guard in
    /// `handle_spawn` when a `Spawn { Agent(id) }` lands and a
    /// matching agent is already running. The TUI moves the active
    /// tab + focuses the terminal stack.
    TerminalFocusRequested {
        terminal_id: TerminalId,
    },
    /// Every terminal keyed to `from` now belongs to `to`. Broadcast
    /// when the daemon rebadges terminals during an issue→PR collapse
    /// (or a manual adopt) so live TUI clients re-point their terminal
    /// slots BEFORE the `WorkspaceRemoved` for `from` arrives — without
    /// this the terminal stack drops the moved terminals on removal and
    /// the session vanishes from view until the next daemon restart.
    TerminalsRebadged {
        from: SessionKey,
        to: SessionKey,
    },
    AgentState {
        /// Workspace the asking agent lives in. Kept for backwards
        /// compatibility with TUI consumers that index by workspace
        /// (sidebar "needs input" badge); the per-terminal id below is
        /// what new consumers (e.g. the chat dispatcher's
        /// channel-per-(session, agent) routing) should key off.
        session_key: SessionKey,
        /// Which terminal flipped state. A workspace with two agents
        /// running (Claude + Codex) emits two distinct `AgentState`
        /// events; previously the wire carried only `session_key` and
        /// consumers couldn't tell them apart. The `#[serde(default)]`
        /// (→ `TerminalId(0)`) only applies on the JSON gateway, where
        /// an older producer can omit the field; over the socket,
        /// bincode never applies defaults — adding this field was a
        /// wire-format change guarded by the protocol-version
        /// handshake (`PROTOCOL_VERSION` bump), and mixed-version
        /// peers are rejected at connect, not papered over.
        #[serde(default)]
        terminal_id: TerminalId,
        state: AgentState,
    },
    ProviderError {
        source: String,
        /// Terse one-line summary for the status bar.
        message: String,
        /// Full diagnostic for the error modal / dev tools. Empty
        /// if the producer didn't classify the error (legacy path).
        #[serde(default)]
        detail: String,
        /// Severity. See [`ProviderErrorKind`] — the daemon writes
        /// `kind.as_str()` here. Empty (uncategorized) is treated
        /// as `Permanent` for safety.
        #[serde(default)]
        kind: String,
    },
    /// Emitted at the end of every successful poll cycle, even when
    /// no tasks matched. The TUI uses this to distinguish "polling
    /// hasn't run yet" from "polling ran and found nothing matching
    /// the user's filter." Drives the polling-modal's empty-state
    /// dismiss path.
    PollCompleted {
        source: String,
        /// Number of tasks the source's filter kept post-fetch.
        count: usize,
    },
    /// Granular progress signal during a poll cycle. Drives the
    /// polling modal's "what is lazybox doing right now" indicator
    /// (e.g. "Querying PRs in acme/widget…", "Got 5 PRs,
    /// fetching reviews…"). Also great for debugging — every
    /// progress step shows up in the log.
    PollProgress {
        source: String,
        /// Short, user-facing description of the current step.
        message: String,
    },
    Notification {
        title: String,
        body: String,
    },
    /// `Command::CleanWorktrees` finished. `removed` is the number of
    /// worktrees actually torn down (skipping ones with live
    /// terminals); `skipped` is the count of sessions that were left
    /// alone because their terminal was still attached. The TUI uses
    /// this to surface a `cleaned N worktrees · M kept (active)`
    /// footer notice.
    CleanWorktreesCompleted {
        removed: usize,
        skipped: usize,
    },
    /// `Command::InspectWorktrees` finished. `inspections` is the
    /// full report — both flagged orphans and healthy entries — in
    /// path-sorted order. Drives the in-app inspector modal.
    WorktreesInspected {
        inspections: Vec<WorktreeInspectionDto>,
    },
    /// `Command::DeleteOrphanedWorktree` finished. `ok = false` means
    /// the daemon refused (safety gate) or the underlying `git
    /// worktree remove` failed; `error` carries the human-readable
    /// reason in that case.
    OrphanedWorktreeDeleted {
        path: std::path::PathBuf,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    /// Structured telemetry from the LLM proxy: one record per
    /// request/response the agent made through the daemon-injected
    /// HTTP proxy. Clients use this to populate the Cost/Tokens tile
    /// and the tool-call activity timeline.
    ProxyRecord(ProxyRecord),
    AgentRunStarted {
        run_id: AgentRunId,
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<lazybox_core::SessionId>,
        agent: String,
        mode: AgentRuntimeMode,
    },
    /// Lossless raw stream-json line or object text from the runtime.
    AgentRawJson {
        run_id: AgentRunId,
        json: String,
    },
    AgentDebug {
        run_id: AgentRunId,
        message: String,
    },
    AgentAssistantTextDelta {
        run_id: AgentRunId,
        delta: String,
    },
    AgentToolCallStarted {
        run_id: AgentRunId,
        call_id: String,
        name: String,
        input_json: Option<String>,
    },
    AgentToolCallDelta {
        run_id: AgentRunId,
        call_id: String,
        delta_json: String,
    },
    AgentToolCallFinished {
        run_id: AgentRunId,
        call_id: String,
        output_json: Option<String>,
        error: Option<String>,
    },
    AgentPermissionRequest {
        run_id: AgentRunId,
        request_id: String,
        tool_name: String,
        input_json: Option<String>,
        reason: Option<String>,
    },
    AgentUserQuestion {
        run_id: AgentRunId,
        question_id: String,
        prompt: String,
        choices: Vec<String>,
        allow_freeform: bool,
    },
    AgentUsage {
        run_id: AgentRunId,
        usage: AgentUsage,
    },
    AgentTurnFinished {
        run_id: AgentRunId,
        result: Option<String>,
        session_id: Option<String>,
        error: Option<String>,
    },
    AgentRunFinished {
        run_id: AgentRunId,
        exit_code: Option<i32>,
        error: Option<String>,
    },
    ProviderCredentialUpdated {
        principal_id: PrincipalId,
        provider_id: String,
        metadata: ProviderCredentialMetadata,
    },
    ProviderCredentialRemoved {
        principal_id: PrincipalId,
        provider_id: String,
    },
    ProviderCredentialsListed {
        principal_id: PrincipalId,
        credentials: Vec<ProviderCredentialMetadata>,
    },
}

/// Severity classification for `Event::ProviderError`. The TUI uses
/// this to decide whether to auto-mount an error modal or just flash
/// a footer notice. Stored on the wire as the plain string returned
/// by `as_str` so existing TUI matches keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// Network blip, rate limit, transient API failure. The TUI
    /// shows a footer notice and we keep polling.
    Retryable,
    /// Credentials failed to resolve / unauthorized. The TUI walks
    /// the user through re-auth.
    Auth,
    /// Genuine misconfiguration or upstream invariant violation.
    /// Surfaces an error modal.
    Permanent,
}

impl ProviderErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::Auth => "auth",
            Self::Permanent => "permanent",
        }
    }
}

/// A single phase of first-time worktree provisioning, reported by
/// `Event::WorktreeProgress`. Ordered as the daemon runs them so the
/// TUI's progress checklist can render them top-to-bottom without
/// carrying ordering on the wire.
///
/// `Clone`/`Fetch`/`WorktreeAdd` are the sub-phases of what used to be a
/// single opaque `Checkout` step — split so the long cold-clone phase
/// animates with advancing sub-progress instead of one spinner that
/// jumps straight to done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeStep {
    /// `git clone --bare` — the slow part on a brand-new repo. Skipped
    /// (instant) when a healthy bare clone is already cached.
    Clone,
    /// Refreshing the remote-tracking ref before branching off it.
    Fetch,
    /// `git worktree add` materializing the checkout on disk.
    WorktreeAdd,
    /// Applying configured mounts + setup scripts to the fresh tree.
    Setup,
}

/// State transition for a [`WorktreeStep`]. `Started`/`Done` advance
/// the checklist; `Failed` carries the error so the modal can surface
/// it instead of dismissing silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeStepStatus {
    Started,
    Done,
    Failed(String),
}

impl Event {
    /// Build a `ProviderError` event with the given source / message
    /// / kind. Replaces ~28 hand-rolled struct literals that all
    /// carried the same `detail: String::new()` shape. Use the
    /// kind-specific shortcuts below when classification is clear at
    /// the call site.
    pub fn provider_error(
        source: &str,
        message: impl Into<String>,
        kind: ProviderErrorKind,
    ) -> Self {
        Self::ProviderError {
            source: source.to_string(),
            message: message.into(),
            detail: String::new(),
            kind: kind.as_str().to_string(),
        }
    }

    /// Shorthand for a retryable provider error. Use for the
    /// transient cases — network, rate limit, "GitHub said please
    /// try again." The TUI doesn't escalate these to a modal.
    pub fn provider_error_retryable(source: &str, message: impl Into<String>) -> Self {
        Self::provider_error(source, message, ProviderErrorKind::Retryable)
    }

    /// Shorthand for a permanent provider error. Use for the genuine
    /// "this won't fix itself by waiting" cases — bad config, missing
    /// repo, malformed task. The TUI auto-mounts a modal.
    pub fn provider_error_permanent(source: &str, message: impl Into<String>) -> Self {
        Self::provider_error(source, message, ProviderErrorKind::Permanent)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub terminal_id: TerminalId,
    pub session_key: SessionKey,
    pub kind: TerminalKind,
    /// Recent PTY output (daemon-side ring buffer). The client feeds
    /// this into its libghostty-vt to reconstruct the screen.
    pub replay: Vec<u8>,
    pub last_seq: u64,
    /// Launched in no-permission / bypass mode. Lets a reconnecting
    /// client re-render the "no-perms" indicator without waiting for
    /// a fresh `TerminalSpawned`.
    #[serde(default)]
    pub no_permission: bool,
    /// Last prompt the user submitted to this terminal (Agent-only;
    /// `None` for shells and for agents that haven't received a prompt
    /// yet). Persisted daemon-side from `Command::RecordUserMessage` so
    /// a reconnecting client can restore the pinned "you ▸ …" recap row
    /// — the ring-buffer `replay` only carries PTY *output*, never the
    /// input we composed, so the recap can't be reconstructed from it.
    #[serde(default)]
    pub last_user_message: Option<String>,
}

// ── Transport abstraction ──────────────────────────────────────────────

use tokio::sync::mpsc;

/// Capacity of the bounded daemon→client event channel. This is the
/// hard memory ceiling on inbound events: the channel never holds more
/// than this many `Event`s no matter how fast the daemon produces
/// `TerminalOutput`. When it fills, the per-connection forwarder drops
/// the coalescable output for the affected terminal and re-syncs its
/// grid from the daemon ring (see [`Event::TerminalResync`]).
///
/// Sized well above the run loop's per-tick drain cap
/// (`MAX_EVENTS_PER_TICK` = 256) so ordinary single-frame bursts ride
/// through without ever tripping the drop path — overflow only kicks in
/// when the consumer is genuinely, sustainedly behind.
pub const EVENT_CHANNEL_CAPACITY: usize = 2048;

/// The plumbing a per-connection event forwarder owns: it drains
/// `raw_rx` (everything the serve loop emits for this client) and
/// writes into the bounded `client_tx`, applying drop-and-resync to
/// `TerminalOutput` so the bounded channel can't grow without bound.
/// Constructed by the transports ([`channel::pair`], [`socket`]) and
/// handed to the server's `serve`, which spawns the forwarder with the
/// daemon config it needs to fetch ring replays.
pub struct EventForward {
    /// Raw, unbounded stream the serve loop writes to (`Connection::tx`).
    /// Drained promptly by the forwarder so it never accumulates.
    pub raw_rx: mpsc::UnboundedReceiver<Event>,
    /// Bounded sink the client reads from (`Client::rx`, possibly via a
    /// socket). The forwarder's `try_send`/`reserve` against this is
    /// what enforces the memory ceiling.
    pub client_tx: mpsc::Sender<Event>,
}

/// A live connection to the daemon. Owned by the TUI.
///
/// Local (in-process) daemons hand back a `Client` whose `send`/`recv`
/// are tokio channel operations — no serialization at all. Remote
/// daemons hand back a `Client` whose internals read and write frames
/// over a socket. The TUI doesn't see the difference.
pub struct Client {
    tx: mpsc::UnboundedSender<Command>,
    /// Inbound daemon events. Bounded — see [`EVENT_CHANNEL_CAPACITY`].
    /// Pub so the realm orchestrator can `try_recv` non-blocking from a
    /// sync main loop. (Old async loop uses `Client::recv` instead.)
    pub rx: mpsc::Receiver<Event>,
}

impl Client {
    /// Build a `Client` from a pair of pre-wired channels. Used by both
    /// transports — `channel::pair` for in-process, `socket::connect`
    /// for remote.
    pub fn from_channels(tx: mpsc::UnboundedSender<Command>, rx: mpsc::Receiver<Event>) -> Self {
        Self { tx, rx }
    }

    // The Err variant carries a full Command (144 bytes); boxing
    // just to placate clippy would burden every successful send for
    // a path that only fires when the daemon has shut down.
    #[allow(clippy::result_large_err)]
    pub fn send(&self, cmd: Command) -> Result<(), mpsc::error::SendError<Command>> {
        self.tx.send(cmd)
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

/// The server-side of a connection. One per connected client.
///
/// A daemon's main loop holds many `Connection`s. Events the daemon
/// wants to send go on `tx` (an **unbounded** raw stream); `rx`
/// receives commands from that specific client. The serve loop never
/// blocks on `tx`.
///
/// When `forward` is `Some`, the raw `tx` stream does not reach the
/// client directly — a forwarder (spawned by the server) drains it into
/// a bounded client channel, applying drop-and-resync to high-volume
/// `TerminalOutput`. Transports that want the memory ceiling
/// ([`channel::pair`], [`socket`]) set it; the JSON API gateway leaves
/// it `None` and reads the raw stream itself.
pub struct Connection {
    pub tx: mpsc::UnboundedSender<Event>,
    pub rx: mpsc::UnboundedReceiver<Command>,
    forward: Option<EventForward>,
}

impl Connection {
    /// Build a `Connection` with no forwarder: the raw `tx` stream is
    /// the client stream. Used by the JSON API gateway, whose consumer
    /// reads the unbounded receiver directly.
    pub fn from_channels(
        tx: mpsc::UnboundedSender<Event>,
        rx: mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        Self {
            tx,
            rx,
            forward: None,
        }
    }

    /// Build a `Connection` whose raw `tx` stream is bridged to the
    /// client through `forward`. The server spawns the forwarder from
    /// `take_forward`.
    pub fn with_forward(
        tx: mpsc::UnboundedSender<Event>,
        rx: mpsc::UnboundedReceiver<Command>,
        forward: EventForward,
    ) -> Self {
        Self {
            tx,
            rx,
            forward: Some(forward),
        }
    }

    /// Take the forwarder plumbing, if any. The server calls this once
    /// at the start of `serve` and spawns the drop-and-resync task.
    pub fn take_forward(&mut self) -> Option<EventForward> {
        self.forward.take()
    }
}
