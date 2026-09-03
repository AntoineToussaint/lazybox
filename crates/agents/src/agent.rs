//! The `Agent` trait and built-in implementations.

use crate::pty::{CreditRecoveryProtocol, EncodedPrompt, PromptIntent, PtyProtocol};
use lazybox_ipc::{AgentRunAccess, AgentState};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Context passed to `Agent::spawn` / `resume`.
#[derive(Debug, Clone, Default)]
pub struct SpawnCtx {
    pub session_key: String,
    pub worktree: PathBuf,
    pub repo: Option<String>,
    pub pr_number: Option<String>,
    pub env: HashMap<String, String>,
    /// Launch the agent with tool-use permission prompts disabled
    /// ("no-permission" / bypass mode). Set for lazybox-spawned autonomous
    /// sessions, and for interactive sessions when the user opts in via
    /// the `agent.skip_permissions` toggle. Honored by agents that
    /// support a bypass flag (Claude → `--dangerously-skip-permissions`,
    /// Codex → `--dangerously-bypass-approvals-and-sandbox`).
    /// Agents without one ignore it.
    pub skip_permissions: bool,
    /// Host access policy for this launch. Read-only is enforced by the
    /// built-in agents' CLI flags and takes precedence over
    /// `skip_permissions`.
    pub access: AgentRunAccess,
    /// Path to a lazybox-generated settings file the agent should launch
    /// with, when the daemon has wired up structured lifecycle hooks
    /// for this spawn. Claude appends `--settings <path>`; agents
    /// without a settings flag ignore it. `None` when hooks aren't
    /// configured (non-Claude agent, or generation failed) — those fall
    /// back to PTY-based state detection.
    pub hook_settings_path: Option<PathBuf>,
    /// Pass `--strict-mcp-config` on unattended launches, cutting the
    /// agent off from the user's ambient MCP servers (#1183). Off by
    /// default (`agent.strict_mcp`); read-only launches are strict
    /// regardless.
    pub strict_mcp: bool,
}

/// The upstream LLM API an agent speaks to. Used to pick the base-URL
/// env var when the user configures a global LLM gateway
/// (`agent.llm_gateway_url`) — Anthropic agents get `ANTHROPIC_BASE_URL`,
/// OpenAI agents get `OPENAI_BASE_URL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    Anthropic,
    OpenAI,
}

impl LlmProvider {
    /// The base-URL environment variable each agent CLI reads to point
    /// itself at a non-default endpoint.
    pub fn base_url_env(&self) -> &'static str {
        match self {
            LlmProvider::Anthropic => "ANTHROPIC_BASE_URL",
            LlmProvider::OpenAI => "OPENAI_BASE_URL",
        }
    }
}

/// Provider-specific wire protocol available for a headless structured
/// run. Interactive terminal support alone does not imply this
/// capability: the daemon only advertises agents whose machine-readable
/// protocol it can normalize into lazybox `Agent*` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredAgentProtocol {
    /// Claude Code's persistent, bidirectional `stream-json` print mode.
    ClaudeStreamJson,
    /// Codex's one-process-per-turn `exec --json` JSONL mode.
    CodexExecJson,
}

impl StructuredAgentProtocol {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeStreamJson => "Claude stream-json",
            Self::CodexExecJson => "Codex exec-json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAuthCommands {
    pub status: Vec<String>,
    pub logout: Vec<String>,
    pub login: Vec<String>,
    /// The provider's explicit "not logged in" token as it appears in
    /// `status` output — the one agent-specific signal a caller may scan for
    /// to confirm a login actually took (a re-auth on a shared login skips
    /// logout, so `login` can exit 0 without truly authenticating). Matched
    /// whitespace-insensitively and case-folded, so store it in a natural
    /// form (Claude's `--json` prints `"loggedIn": false`). `None` means the
    /// caller trusts the exit code alone.
    pub signed_out_marker: Option<&'static str>,
}

/// How to give an agent a per-session credential home so an expired token
/// or an account switch on one session never disturbs the machine-wide
/// login shared by every other session.
///
/// A CLI that reads `home_env` to relocate its credential/config directory
/// can be pointed at a private directory per lazybox workspace. lazybox
/// seeds `seed_files` from the machine-wide home (`$home_env`, else
/// `$HOME/<default_home>`) once, so the session starts already
/// authenticated but re-logs in isolation — a `logout`/`login` on one
/// session rewrites only that session's copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialIsolation {
    /// Env var the CLI reads to relocate its credential/config home
    /// (Codex → `CODEX_HOME`).
    pub home_env: &'static str,
    /// Machine-wide home relative to `$HOME`, used as the seed source when
    /// `home_env` is unset (Codex → `.codex`).
    pub default_home: &'static str,
    /// Files under the home carrying login (and user config) state that
    /// must be copied into a fresh per-session home so it starts
    /// authenticated.
    pub seed_files: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthFailure {
    pub reason: &'static str,
}

pub use crate::pty::PromptShape;

/// A one-time startup chooser that an unattended launch may answer because
/// its configured access policy already made that exact decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnattendedPromptKind {
    WorkspaceTrust,
    PermissionBypassConsent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnattendedPromptNudge {
    pub kind: UnattendedPromptKind,
    /// One fixed chooser keystroke. Adapters cannot reflect dynamic terminal
    /// output back into the PTY.
    pub response: &'static [u8],
}

/// One semantic observation produced by an agent's PTY detector.
///
/// The state and prompt shape travel together so the daemon never has to
/// infer agent-UI semantics from a bare [`AgentState::InputNeeded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentObservation {
    state: AgentState,
    prompt_shape: Option<PromptShape>,
}

impl AgentObservation {
    /// Wrap a detected state. A legacy `InputNeeded` reading is treated as a
    /// chooser because PTY detectors historically only surfaced structural
    /// permission/selection prompts; adapters with free-text prompts should
    /// use [`AgentObservation::input_needed`].
    pub const fn from_state(state: AgentState) -> Self {
        Self {
            state,
            prompt_shape: if matches!(state, AgentState::InputNeeded) {
                Some(PromptShape::Chooser)
            } else {
                None
            },
        }
    }

    /// Build an input-needed observation with its exact interaction shape.
    pub const fn input_needed(prompt_shape: PromptShape) -> Self {
        Self {
            state: AgentState::InputNeeded,
            prompt_shape: Some(prompt_shape),
        }
    }

    /// Detected lifecycle state.
    pub const fn state(self) -> AgentState {
        self.state
    }

    /// Shape of the blocking prompt, present only for `InputNeeded`.
    pub const fn prompt_shape(self) -> Option<PromptShape> {
        self.prompt_shape
    }
}

/// The generic badge rule for an agent id: its first char, uppercased
/// (`'A'` for the degenerate empty id). The single source of the
/// fallback — [`Agent::badge`]'s default and [`Registry::badge_for`]'s
/// unknown-id path both defer here so the rule never drifts.
pub fn default_badge(id: &str) -> char {
    id.chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('A')
}

pub trait Agent: Send + Sync {
    /// Stable id used in config and IPC (`"claude"`, `"codex"`, etc.).
    fn id(&self) -> &str;

    /// Human-readable display name.
    fn display_name(&self) -> &str;

    /// Single-letter badge for the sidebar runner column and any other
    /// compact "which agent is live here" indicator. Declared by the
    /// agent so identity lives in one place: the sidebar never
    /// special-cases a kind, and a new agent can pick a letter that
    /// doesn't collide instead of silently sharing the first char of a
    /// name that's already taken (Codex vs Claude, both `C`). The
    /// default is [`default_badge`] over [`Agent::id`] — fine for a
    /// unique leading letter, overridden when it would collide.
    fn badge(&self) -> char {
        default_badge(self.id())
    }

    /// Which upstream LLM API this agent speaks. Drives base-URL env
    /// injection when an LLM gateway is configured. The default `None`
    /// covers agents whose upstream lazybox can't infer (a `GenericCli`
    /// pointed at an arbitrary command) — they get no gateway injection.
    fn llm_provider(&self) -> Option<LlmProvider> {
        None
    }

    /// Whether the metering proxy can actually meter this agent's traffic.
    ///
    /// Metering works by pointing the provider's base-URL env
    /// (`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`) at the local proxy. That
    /// only captures usage if the agent (a) speaks a known provider and (b)
    /// honors that env var. The default ties meterability to
    /// [`Agent::llm_provider`] — a provider-less `GenericCli` can't be
    /// metered. An agent that speaks a provider but ignores the base-URL env
    /// overrides this to `false` so a metered spawn is never a silent bypass
    /// (the caller surfaces a "metered-but-not" notice instead of pointing
    /// the agent at a proxy URL it won't use).
    fn meterable(&self) -> bool {
        self.llm_provider().is_some()
    }

    /// Machine-readable runtime supported by this agent, if any.
    ///
    /// This is deliberately separate from [`Agent::spawn`]: a CLI can
    /// work perfectly in a PTY while lacking a stable structured mode.
    /// The daemon uses this capability to reject unsupported headless
    /// runs before trying provider-specific flags.
    fn structured_protocol(&self) -> Option<StructuredAgentProtocol> {
        None
    }

    /// Interactive shell/PTY behavior for this agent. The server owns the
    /// universal paste/settle/submit transaction; adapters only select a
    /// protocol. Simple and generic CLIs inherit [`PtyProtocol::LINE_ORIENTED`].
    fn pty_protocol(&self) -> PtyProtocol {
        PtyProtocol::default()
    }

    /// Provider-owned chooser interaction for credit recovery. Agents that
    /// do not expose a supported credit chooser leave this unavailable.
    fn credit_recovery_protocol(&self) -> Option<CreditRecoveryProtocol> {
        None
    }

    /// Provider guidance carried by a detected credit chooser.
    fn credit_exhausted_hint(&self, recent_output: &[u8]) -> Option<String> {
        let _ = recent_output;
        None
    }

    /// Encode a complete prompt interaction atomically. Most adapters should
    /// inherit this and declare [`Agent::pty_protocol`]; an unusual CLI may
    /// override this single method without having to coordinate independent
    /// prompt and submit hooks.
    fn encode_prompt(&self, prompt: &str, intent: PromptIntent) -> EncodedPrompt {
        self.pty_protocol().encode_prompt(prompt, intent)
    }

    /// The agent's in-composer "clear conversation context" command
    /// (`/clear` for Claude Code, `/new` for Codex), or `None` when
    /// the CLI has no such command. Drives the reset-agent action
    /// (#1204): the command is injected + submitted through the same
    /// settle-gated path snippets use, so the session, worktree, PTY
    /// and prompt history all survive — only the agent's conversation
    /// resets.
    fn clear_context_command(&self) -> Option<&'static str> {
        None
    }

    /// Whether this agent accepts an explicit MCP-server config file the
    /// daemon can inject (Claude's `--mcp-config`). Used to wire the lazybox
    /// coordination MCP server (#1420) into a spawn. Default: no.
    fn supports_mcp_config(&self) -> bool {
        false
    }

    /// Command + args to spawn a fresh session.
    fn spawn(&self, ctx: &SpawnCtx) -> Vec<String>;

    /// Command + args to resume the most recent session for this
    /// worktree. Default: same as `spawn`. Override when the agent has
    /// a `--continue`-style flag.
    fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
        self.spawn(ctx)
    }

    /// Command + args to resume a particular upstream conversation.
    /// Adapters that do not expose stable session ids inherit their
    /// existing cwd-based resume behavior.
    fn resume_session(&self, ctx: &SpawnCtx, provider_session_id: Option<&str>) -> Vec<String> {
        let _ = provider_session_id;
        self.resume(ctx)
    }

    /// Provider-owned authentication commands, when this CLI supports
    /// interactive account recovery.
    fn auth_commands(&self) -> Option<AgentAuthCommands> {
        None
    }

    /// Conservatively classify a provider-account authentication failure
    /// from recent PTY output.
    fn detect_auth_failure(&self, recent_output: &[u8]) -> Option<AuthFailure> {
        let _ = recent_output;
        None
    }

    /// Per-session credential-home isolation for this agent, when its CLI
    /// can relocate its credential directory through an environment
    /// variable. `None` (the default) leaves the agent on the machine-wide
    /// login, where a re-auth on one session cascades to every other.
    fn credential_isolation(&self) -> Option<CredentialIsolation> {
        None
    }

    /// Extra environment variables to seed into this agent's launch, on
    /// top of the inherited process env, for BOTH the interactive PTY
    /// spawn and the structured `exec` run. The daemon skips any key a
    /// higher-priority source (per-repo `env`) already set, so these are
    /// defaults, not overrides. Default: none.
    fn spawn_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Environment variables required by this agent's interactive PTY UI.
    /// These are applied after user-configured repository environment values,
    /// because they preserve terminal integration invariants rather than act
    /// as user-facing defaults. Structured runs do not receive them.
    fn pty_spawn_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Compatibility generation for the interactive PTY launch contract.
    /// A non-zero generation is persisted with a live backend session so a
    /// newer daemon can identify an older surviving process whose environment
    /// cannot be repaired in place. Generations are monotonic: bump this when
    /// `pty_spawn_env` changes in a way that requires restarting an
    /// already-running process.
    fn pty_launch_generation(&self) -> u32 {
        0
    }

    /// Prepare the environment so an UNATTENDED launch in `worktree`
    /// can't stall on a one-time interactive consent dialog. Called
    /// before spawning an autonomous session (the `w` / address-comments
    /// flows), where no human is present to answer such a dialog.
    ///
    /// Default no-op. Claude overrides it: Claude Code shows a
    /// workspace-trust dialog ("Do you trust the files in this folder?")
    /// for any directory it hasn't seen before — skipped only in
    /// non-interactive `-p` mode, which lazybox doesn't use — so an
    /// autonomous spawn in a freshly provisioned worktree would hang on
    /// it. A failure is fatal to the unattended spawn: proceeding would turn
    /// a concrete setup error into a terminal that silently waits forever.
    fn prepare_unattended(&self, worktree: &Path) -> std::io::Result<()> {
        let _ = worktree;
        Ok(())
    }

    /// Recognize an exact, safe-to-answer startup consent screen for an
    /// unattended launch. Runtime tool approvals and free-text questions must
    /// return `None`.
    fn unattended_startup_nudge(&self, _recent_output: &[u8]) -> Option<UnattendedPromptNudge> {
        None
    }

    /// Per-agent state detector. Inspect recent PTY output and return
    /// the agent's current [`AgentState`] — `Working` (streaming /
    /// running a tool), `InputNeeded` (paused on a prompt), or `Idle`
    /// (done / nothing happening) — or `None` when there's no
    /// confident determination.
    ///
    /// This is the per-agent strategy the issue calls for: lazybox's
    /// side panel never pattern-matches PTY output itself, it asks the
    /// active session's agent. Each agent kind recognises "working"
    /// differently (Claude's streaming pulser, Codex's spinner, …), so
    /// the detection vocabulary lives here, next to the agent, not in
    /// a global matcher.
    ///
    /// The default returns `None` — an unknown agent has no detector,
    /// so consumers render it as `Idle`. Crucially it must never
    /// default to `Working`: an agent that can't tell should look idle,
    /// not falsely busy.
    fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
        let _ = recent_output;
        None
    }

    /// The model + reasoning effort the agent is currently running,
    /// screen-scraped from its composer footer / status line as a compact
    /// display string (`"gpt-5.5 · xhigh"`). Drives the sidebar model badge
    /// and keeps it accurate when the user switches model *inside* the agent
    /// (the spawn tier can go stale). `None` — the default — means "no live
    /// reading"; the caller then falls back to the spawn-time tier label.
    ///
    /// Only agents that print their model in the PTY implement this: Codex
    /// draws `<model> <effort> · <cwd>`, so it reads its live value here;
    /// Claude is always spawned with an explicit `--model`, so its tier
    /// label already names the model and it keeps the default `None`.
    fn detect_model_effort(&self, recent_output: &[u8]) -> Option<String> {
        let _ = recent_output;
        None
    }

    /// Chunk-aware variant of [`Agent::detect_state`]. `last_chunk_start`
    /// is the byte offset within `recent_output` where the most recent
    /// PTY chunk begins. Agents whose detector reasons about marker
    /// recency (Claude and Codex) use it to recognize a full-screen repaint,
    /// where a live dialog and the bottom status bar arrive in ONE chunk and
    /// positional ordering alone misreads the dialog as stale. The
    /// default ignores the hint and delegates to `detect_state`.
    fn detect_state_chunked(
        &self,
        recent_output: &[u8],
        last_chunk_start: usize,
    ) -> Option<AgentState> {
        let _ = last_chunk_start;
        self.detect_state(recent_output)
    }

    /// Semantic quiet-screen observation consumed by the daemon.
    ///
    /// Existing detectors can keep returning [`AgentState`]; this default
    /// lifts that result into the shared observation contract. An adapter
    /// whose PTY detector recognizes free-text input can override this method
    /// and return [`AgentObservation::input_needed`] with the exact shape.
    fn detect_observation_chunked(
        &self,
        recent_output: &[u8],
        last_chunk_start: usize,
    ) -> Option<AgentObservation> {
        self.detect_state_chunked(recent_output, last_chunk_start)
            .map(AgentObservation::from_state)
    }

    /// High-confidence, current-chunk check for a blocking prompt.
    ///
    /// The daemon normally waits for the PTY to go quiet before running
    /// [`Agent::detect_state_chunked`]. That protects a streaming agent
    /// from stale prompt text in scrollback, but delays notification for
    /// full-screen approval dialogs and can miss one that keeps repainting.
    /// Agents with distinctive prompt chrome can opt into this fast path:
    /// only markers touched by the latest chunk may return their prompt shape.
    ///
    /// Default `None` preserves the quiet-only policy for adapters whose
    /// prompt vocabulary is not strong enough to classify while output is
    /// flowing.
    fn detect_input_needed_in_current_chunk(
        &self,
        recent_output: &[u8],
        last_chunk_start: usize,
    ) -> Option<PromptShape> {
        let _ = (recent_output, last_chunk_start);
        None
    }

    /// Typed current-chunk observation for blocking screens. The default
    /// preserves adapters that only distinguish ordinary input prompts.
    fn detect_blocked_in_current_chunk(
        &self,
        recent_output: &[u8],
        last_chunk_start: usize,
    ) -> Option<AgentObservation> {
        self.detect_input_needed_in_current_chunk(recent_output, last_chunk_start)
            .map(AgentObservation::input_needed)
    }

    /// Whether a `Working` PTY reading carries enough on-screen evidence
    /// to demote a hook-set `InputNeeded` once the hook stream has gone
    /// stale. A dialog on screen blocks Claude's hook stream (no tool
    /// calls fire while it waits), so "hooks stale + cached `?`" is the
    /// normal shape of a real unanswered dialog — demotion needs proof
    /// the dialog was answered (activity painted after its markers), not
    /// just a Working classification. Default `true`: agents without
    /// dialog-shaped prompts keep the plain stale-hook fallback.
    fn working_reading_supersedes_dialog(&self, recent_output: &[u8]) -> bool {
        let _ = recent_output;
        true
    }

    /// Tight "ready to receive a pasted prompt" check. Returns true
    /// when the agent's INPUT BOX is visibly drawn AND no
    /// permission gate / chooser is currently up.
    ///
    /// Distinct from `detect_state` because the binary
    /// Active/Asking state is too coarse — `Asking` includes both
    /// "Y/N permission gate" (paste would be eaten) AND "idle input
    /// box with a question in chat history" (paste is fine). This
    /// check vetoes only on a live permission gate / chooser, so a
    /// quiet input box reports ready immediately — no false-positive
    /// 60s inject wait on every spawn.
    ///
    /// Default `false` — agents that don't override never report
    /// ready, so the spawn-time injector falls back to its time-
    /// based settle. Built-ins (Claude, Codex, Cursor) override.
    fn detect_ready_for_prompt(&self, recent_output: &[u8]) -> bool {
        let _ = recent_output;
        false
    }

    /// Chunk-aware readiness — [`Agent::detect_ready_for_prompt`] with the
    /// daemon's chunk-boundary hint. Agents whose TUIs repaint continuously
    /// (Codex) override this to recognize composer chrome painted by the
    /// latest repaint frame itself, so spawn-time readiness fires in
    /// hundreds of milliseconds instead of riding the inject hard deadline
    /// while stale status-line bytes pin the whole-buffer positional read
    /// (issue #425). Default: ignore the hint and defer to the whole-buffer
    /// detector.
    fn detect_ready_for_prompt_chunked(
        &self,
        recent_output: &[u8],
        last_chunk_start: usize,
    ) -> bool {
        let _ = last_chunk_start;
        self.detect_ready_for_prompt(recent_output)
    }

    /// How lazybox keeps this agent's CLI current, out of band. The
    /// daemon runs the channel's commands in plain bounded
    /// subprocesses — never inside a live session PTY, where the
    /// agents' own self-updaters fail or churn (the reason lazybox
    /// suppresses them at spawn). The default `None` opts out: an
    /// agent with no known install channel (a `GenericCli` pointed at
    /// an arbitrary command) is never version-checked or updated.
    fn update_channel(&self) -> Option<crate::update::UpdateChannel> {
        None
    }

    /// Build the settings JSON to launch this agent with so it reports
    /// state through structured lifecycle hooks instead of (or
    /// alongside) PTY screen-scraping. `hook_command` is the shell
    /// command the agent should run on each lifecycle event — lazybox's
    /// `hook-ingest` helper, carrying the backend session key. `user_settings`
    /// is the user's own parsed settings, merged in so we don't clobber
    /// their hooks.
    ///
    /// One of two ways an agent wires up its authoritative state source
    /// (see also [`Agent::hook_command_args`]). This one is for agents
    /// that take a settings *file* — Claude launches with `--settings
    /// <path>`, whose contents this method produces. The daemon writes
    /// the returned JSON to a per-session file and sets
    /// [`SpawnCtx::hook_settings_path`].
    ///
    /// The default returns `None`: an agent that overrides neither this
    /// nor [`Agent::hook_command_args`] emits no authoritative signal, so
    /// the daemon keeps the lower-confidence PTY detector (`detect_state`
    /// and friends) as its only source.
    fn build_hook_settings(
        &self,
        hook_command: &str,
        user_settings: Option<&serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let _ = (hook_command, user_settings);
        None
    }

    /// Extra spawn-argv fragments that wire lazybox's authoritative-state
    /// hook command into an agent that configures hooks through CLI
    /// overrides rather than a settings file. `hook_command` is the same
    /// `hook-ingest` shell command Claude runs on each lifecycle event,
    /// already correlated to this terminal.
    ///
    /// The daemon appends the returned args to the spawn argv. The
    /// resulting hooks reach the daemon over the identical
    /// [`crate::hook`] → `IngestHook` path Claude's settings-file hooks
    /// use — the state machine consumes one normalized signal regardless
    /// of how the agent was configured to emit it.
    ///
    /// The default returns an empty vec (no argv-based hooks). Codex
    /// overrides it: its `hook_event_name` payloads are wire-compatible
    /// with Claude's, so [`crate::hook::parse_claude_hook`] parses both.
    fn hook_command_args(&self, hook_command: &str) -> Vec<String> {
        let _ = hook_command;
        Vec::new()
    }
}

/// Registry of known agents. Keyed by `Agent::id()`.
#[derive(Default, Clone)]
pub struct Registry {
    agents: HashMap<String, Arc<dyn Agent>>,
}

impl Registry {
    pub fn default_builtins() -> Self {
        let mut r = Self::default();
        r.register(Arc::new(builtins::Claude));
        r.register(Arc::new(builtins::Codex));
        r.register(Arc::new(builtins::Cursor));
        r
    }

    pub fn register(&mut self, agent: Arc<dyn Agent>) {
        self.agents.insert(agent.id().to_string(), agent);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(id).cloned()
    }

    /// Display badge for an agent id: the registered agent's declared
    /// [`Agent::badge`], or [`default_badge`] for an id this registry
    /// doesn't know (a YAML `GenericCli`). Lets a consumer resolve a
    /// badge from a bare id string without reaching for the fallback
    /// itself.
    pub fn badge_for(&self, id: &str) -> char {
        self.get(id)
            .map_or_else(|| default_badge(id), |a| a.badge())
    }

    /// Display name for an agent id: the registered agent's declared
    /// [`Agent::display_name`], or the bare id for one this registry
    /// doesn't know (a YAML `GenericCli`). Lets a consumer that can't
    /// name the `Agent` trait (the UI library, which reaches the registry
    /// through `lazybox-tui-core`) resolve a human label from an id.
    pub fn display_name_for(&self, id: &str) -> String {
        self.get(id)
            .map_or_else(|| id.to_string(), |a| a.display_name().to_string())
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.agents.keys().map(String::as_str)
    }
}

/// Shared pattern primitives for agent state detection.
///
/// The detection vocabulary — ANSI stripping, chooser/footer/status
/// matching, and the Claude state machine — lives in [`crate::detect`]
/// as pure functions over `&[u8]` so it can be exercised against
/// captured real PTY bytes, not just synthetic strings. Re-exported
/// here under the historical `agent::detect` path the built-ins and
/// tests reach for.
pub use crate::detect;

pub mod builtins {
    use super::*;

    /// How far back the simple-pattern agents (Codex, Cursor,
    /// GenericCli) look for a prompt marker. Their patterns carry no
    /// recency anchors like Claude's, so bounding the scan to the
    /// visible-screen tail is the only thing that lets an answered
    /// prompt stop matching once fresh output arrives.
    const PROMPT_TAIL_WINDOW: usize = 2 * 1024;

    /// Flags that make an unattended (`skip_permissions`) Claude launch
    /// start clean. `--dangerously-skip-permissions` bypasses
    /// tool-permission prompts; `--strict-mcp-config` makes Claude ignore
    /// every ambient MCP config (user / project / plugin `.mcp.json`) and
    /// load only servers from an explicit `--mcp-config` — which lazybox
    /// doesn't pass, so zero servers load. That forecloses the "⚠ N MCP
    /// server needs authentication · run /mcp" startup gate an autonomous
    /// spawn can't clear (issue #256): `--dangerously-skip-permissions`
    /// bypasses tool-permission checks but NOT an MCP server's OAuth/login
    /// gate. Interactive spawns keep their MCP servers.
    fn push_unattended_flags(argv: &mut Vec<String>, ctx: &SpawnCtx) {
        if ctx.skip_permissions {
            argv.push("--dangerously-skip-permissions".into());
            // #1183: strict MCP is opt-in now. Always-on it silently
            // disabled every user MCP server on autonomous spawns
            // ("works outside lazybox, not inside"). The #256 auth-gate
            // concern it addressed is opt-back-in via
            // `agent.strict_mcp: true`.
            if ctx.strict_mcp {
                argv.push("--strict-mcp-config".into());
            }
        }
    }

    fn push_read_only_flags(argv: &mut Vec<String>, ctx: &SpawnCtx) {
        if ctx.access == AgentRunAccess::ReadOnly {
            argv.extend([
                "--safe-mode".into(),
                "--strict-mcp-config".into(),
                "--tools".into(),
                "Read,Glob,Grep".into(),
                "--permission-mode".into(),
                "dontAsk".into(),
            ]);
        }
    }

    /// Append `--settings <path>` when the daemon generated a hooks
    /// settings file for this spawn. Claude's `--settings` accepts a
    /// file path and takes precedence over user/project settings — the
    /// daemon has already merged the user's hooks into that file (see
    /// [`crate::hook_settings`]), so nothing is clobbered.
    fn push_settings_flag(argv: &mut Vec<String>, ctx: &SpawnCtx) {
        if ctx.access == AgentRunAccess::Default
            && let Some(path) = &ctx.hook_settings_path
        {
            argv.push("--settings".into());
            argv.push(path.to_string_lossy().into_owned());
        }
    }

    #[derive(Default)]
    pub struct Claude;

    impl Agent for Claude {
        fn id(&self) -> &'static str {
            "claude"
        }
        fn clear_context_command(&self) -> Option<&'static str> {
            Some("/clear")
        }
        fn display_name(&self) -> &'static str {
            "Claude Code"
        }
        fn badge(&self) -> char {
            'C'
        }
        fn llm_provider(&self) -> Option<LlmProvider> {
            Some(LlmProvider::Anthropic)
        }
        fn structured_protocol(&self) -> Option<StructuredAgentProtocol> {
            Some(StructuredAgentProtocol::ClaudeStreamJson)
        }
        fn supports_mcp_config(&self) -> bool {
            true
        }
        fn pty_protocol(&self) -> PtyProtocol {
            PtyProtocol::GUARDED_COMPOSER
        }
        fn spawn(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["claude".into()];
            if ctx.access == AgentRunAccess::ReadOnly {
                push_read_only_flags(&mut argv, ctx);
            } else {
                push_unattended_flags(&mut argv, ctx);
            }
            push_settings_flag(&mut argv, ctx);
            argv
        }
        fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
            self.resume_session(ctx, None)
        }

        fn resume_session(&self, ctx: &SpawnCtx, provider_session_id: Option<&str>) -> Vec<String> {
            let mut argv = vec!["claude".into(), "--continue".into()];
            if let Some(session_id) = provider_session_id {
                argv[1] = "--resume".into();
                argv.push(session_id.into());
            }
            if ctx.access == AgentRunAccess::ReadOnly {
                push_read_only_flags(&mut argv, ctx);
            } else {
                push_unattended_flags(&mut argv, ctx);
            }
            push_settings_flag(&mut argv, ctx);
            argv
        }

        fn auth_commands(&self) -> Option<AgentAuthCommands> {
            Some(AgentAuthCommands {
                // Pin `--json`: it is the current default, but the status gate
                // scans for the JSON `"loggedIn": false` token, so relying on a
                // version-dependent default output format would silently
                // disable the gate if Claude ever flips it to text.
                status: vec![
                    "claude".into(),
                    "auth".into(),
                    "status".into(),
                    "--json".into(),
                ],
                logout: vec!["claude".into(), "auth".into(), "logout".into()],
                login: vec!["claude".into(), "auth".into(), "login".into()],
                signed_out_marker: Some("\"loggedIn\": false"),
            })
        }

        fn detect_auth_failure(&self, recent_output: &[u8]) -> Option<AuthFailure> {
            crate::detect::claude_auth_failure(recent_output)
        }

        fn pty_spawn_env(&self) -> Vec<(String, String)> {
            vec![(
                "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_string(),
                "1".to_string(),
            )]
        }

        fn pty_launch_generation(&self) -> u32 {
            1
        }

        fn prepare_unattended(&self, worktree: &Path) -> std::io::Result<()> {
            crate::claude_env::seed_unattended_env(worktree)?;
            // `--dangerously-skip-permissions` paints a bypass-mode consent
            // screen on every start that the flag itself doesn't clear; the
            // per-session `--settings` file seeds the same acceptance, but
            // that file isn't guaranteed (it needs a resolvable hook exe), so
            // persist it in user settings too. Log loudly on failure — this
            // is the difference between an autonomous spawn working and
            // hanging on a non-tool prompt.
            crate::claude_env::seed_skip_dangerous_mode_prompt()
        }

        fn unattended_startup_nudge(&self, recent_output: &[u8]) -> Option<UnattendedPromptNudge> {
            crate::detect::claude_unattended_startup_nudge(recent_output)
        }

        fn update_channel(&self) -> Option<crate::update::UpdateChannel> {
            Some(crate::update::claude_channel())
        }

        /// Wire lazybox's hook command into a settings file Claude
        /// launches with, merging the user's existing hooks. Delegates
        /// to [`crate::hook_settings::build_settings`].
        fn build_hook_settings(
            &self,
            hook_command: &str,
            user_settings: Option<&serde_json::Value>,
        ) -> Option<serde_json::Value> {
            Some(crate::hook_settings::build_settings(
                user_settings,
                hook_command,
            ))
        }

        /// Claude Code's three observable states. Delegates to the pure
        /// [`crate::detect::claude_state`] so the logic is exercisable
        /// against captured real PTY bytes, not just synthetic strings.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            detect::claude_state(recent_output)
        }

        /// Chunk-aware detection — threads the daemon's chunk-boundary
        /// hint into the recency model so a full-screen repaint
        /// (dialog + status bar in one chunk) keeps the dialog live.
        fn detect_state_chunked(
            &self,
            recent_output: &[u8],
            last_chunk_start: usize,
        ) -> Option<AgentState> {
            detect::claude_state_chunked(recent_output, last_chunk_start)
        }

        /// Stale-hook demotion evidence — see
        /// [`crate::detect::claude_working_supersedes_dialog`].
        fn working_reading_supersedes_dialog(&self, recent_output: &[u8]) -> bool {
            detect::claude_working_supersedes_dialog(recent_output)
        }

        /// Whether Claude is ready to receive a pasted prompt — input
        /// box drawn, no permission / trust gate up. Delegates to
        /// [`crate::detect::claude_ready_for_prompt`].
        fn detect_ready_for_prompt(&self, recent_output: &[u8]) -> bool {
            detect::claude_ready_for_prompt(recent_output)
        }
    }

    #[derive(Default)]
    pub struct Codex;

    /// Build a Codex CLI `-c` override that marks exactly this worktree as
    /// trusted. Codex's config override parser splits dotted keys literally,
    /// so a quoted path cannot safely be expressed as
    /// `projects."/path".trust_level=...`; replacing the `projects` table
    /// with a one-entry inline table works for paths containing dots too.
    fn codex_trusted_project_override(worktree: &Path) -> String {
        let path = serde_json::to_string(&worktree.to_string_lossy())
            .unwrap_or_else(|_| "\"\"".to_string());
        format!("projects={{{path}={{trust_level=\"trusted\"}}}}")
    }

    /// The flags that make an unattended (`skip_permissions`) Codex launch
    /// start clean, shared by [`Codex::spawn`] and [`Codex::resume`] so the
    /// two paths never drift. Empty unless `skip_permissions` is set.
    ///
    /// `--dangerously-bypass-hook-trust` is intentionally NOT here: it now
    /// rides with the injected lifecycle hooks in
    /// [`Codex::hook_command_args`] (needed for interactive spawns too, and
    /// pointless without hooks), so keeping it here as well would pass it
    /// twice on an autonomous hooked spawn.
    fn codex_unattended_flags(ctx: &SpawnCtx) -> Vec<String> {
        if !ctx.skip_permissions {
            return Vec::new();
        }
        vec![
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "-c".into(),
            codex_trusted_project_override(&ctx.worktree),
            "-c".into(),
            "check_for_update_on_startup=false".into(),
        ]
    }

    fn codex_read_only_flags(ctx: &SpawnCtx) -> Vec<String> {
        if ctx.access != AgentRunAccess::ReadOnly {
            return Vec::new();
        }
        vec![
            "--sandbox".into(),
            "read-only".into(),
            "--ask-for-approval".into(),
            "never".into(),
            "-c".into(),
            "mcp_servers={}".into(),
            "-c".into(),
            "hooks={}".into(),
        ]
    }

    /// Escape a string for a TOML basic (`"…"`) string. Codex's `-c`
    /// overrides are parsed as TOML, and the hook command embeds the
    /// lazybox binary path plus quoted arguments, so its inner `"` and
    /// `\` must be escaped or the override fails to parse.
    fn toml_escape(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// A Codex `-c` config override registering `command` as a hook on one
    /// lifecycle `event`. Mirrors the nested shape Codex's `hooks` table
    /// expects (`hooks.<Event> = [{ hooks = [{ type = "command", command
    /// = … }] }]`), the same matcher-group structure Claude's settings
    /// file uses.
    fn codex_hook_override(event: &str, command: &str) -> String {
        format!(
            "hooks.{event}=[{{hooks=[{{type=\"command\",command=\"{}\"}}]}}]",
            toml_escape(command)
        )
    }

    impl Agent for Codex {
        fn id(&self) -> &'static str {
            "codex"
        }
        fn clear_context_command(&self) -> Option<&'static str> {
            // Codex's composer command for starting a fresh conversation
            // in place.
            Some("/new")
        }
        fn display_name(&self) -> &'static str {
            "Codex"
        }
        fn badge(&self) -> char {
            'X'
        }
        fn llm_provider(&self) -> Option<LlmProvider> {
            Some(LlmProvider::OpenAI)
        }
        fn structured_protocol(&self) -> Option<StructuredAgentProtocol> {
            Some(StructuredAgentProtocol::CodexExecJson)
        }
        fn pty_protocol(&self) -> PtyProtocol {
            PtyProtocol::GUARDED_COMPOSER
        }
        fn credit_recovery_protocol(&self) -> Option<CreditRecoveryProtocol> {
            Some(CreditRecoveryProtocol::new(b"\r"))
        }
        fn credit_exhausted_hint(&self, recent_output: &[u8]) -> Option<String> {
            detect::codex_credit_exhausted_hint(recent_output)
        }
        fn spawn(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["codex".into()];
            if ctx.access == AgentRunAccess::ReadOnly {
                argv.extend(codex_read_only_flags(ctx));
            } else {
                argv.extend(codex_unattended_flags(ctx));
            }
            argv
        }

        /// Continue this worktree's most recent Codex session — the
        /// per-worktree analog of Claude's `--continue`. `codex resume
        /// --last` filters recorded sessions by cwd (only `--all` widens
        /// that), so a restored session reattaches its own conversation
        /// instead of starting blank.
        fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
            self.resume_session(ctx, None)
        }

        fn resume_session(&self, ctx: &SpawnCtx, provider_session_id: Option<&str>) -> Vec<String> {
            let mut argv = vec![
                "codex".into(),
                "resume".into(),
                provider_session_id.unwrap_or("--last").into(),
            ];
            if ctx.access == AgentRunAccess::ReadOnly {
                argv.extend(codex_read_only_flags(ctx));
            } else {
                argv.extend(codex_unattended_flags(ctx));
            }
            argv
        }

        fn auth_commands(&self) -> Option<AgentAuthCommands> {
            Some(AgentAuthCommands {
                status: vec!["codex".into(), "login".into(), "status".into()],
                logout: vec!["codex".into(), "logout".into()],
                login: vec!["codex".into(), "login".into()],
                // Codex isolates its login per session (its own CODEX_HOME), so
                // re-auth does a clean logout+login and never runs the status
                // gate; exit-code-only would suffice regardless.
                signed_out_marker: None,
            })
        }

        fn detect_auth_failure(&self, recent_output: &[u8]) -> Option<AuthFailure> {
            crate::detect::codex_auth_failure(recent_output)
        }

        /// Isolate each Codex session's login under its own `CODEX_HOME` so
        /// a re-auth (expired token / account switch) on one session never
        /// re-logs the machine-wide `~/.codex` and disrupts the fleet. The
        /// login lives in `auth.json` inside the home; `config.toml` carries
        /// the user's provider/model config, so both are seeded. Claude
        /// deliberately opts out: on macOS its OAuth credential lives in the
        /// login Keychain, not under `CLAUDE_CONFIG_DIR`, so relocating the
        /// config dir would not isolate the credential.
        fn credential_isolation(&self) -> Option<CredentialIsolation> {
            Some(CredentialIsolation {
                home_env: "CODEX_HOME",
                default_home: ".codex",
                seed_files: &["auth.json", "config.toml"],
            })
        }

        /// Suppress Homebrew's implicit self-update inside a spawned Codex
        /// session. Codex's Homebrew build shells out to
        /// `brew upgrade --cask codex` when the user accepts its on-launch
        /// update banner, and *any* `brew` invocation first triggers
        /// Homebrew's self-update (portable-ruby pour, tap refresh,
        /// "Auto-updated Homebrew!") unless suppressed — a heavy
        /// network+disk side effect the session never asked for (issue
        /// #355). `HOMEBREW_NO_AUTO_UPDATE=1` skips that implicit
        /// `brew update`; the `brew upgrade` an explicit `ctrl+u` runs
        /// still proceeds. The accepted cost is that brew also won't
        /// refresh its cached cask index, so an explicit upgrade against a
        /// long-stale cache can no-op — the price of not auto-updating on
        /// every fresh session.
        fn spawn_env(&self) -> Vec<(String, String)> {
            vec![("HOMEBREW_NO_AUTO_UPDATE".to_string(), "1".to_string())]
        }

        fn update_channel(&self) -> Option<crate::update::UpdateChannel> {
            Some(crate::update::codex_channel())
        }

        /// Wire lazybox's hook command into Codex through `-c` config
        /// overrides — Codex's authoritative state source, replacing the
        /// PTY screen-scraper as the primary signal. Codex's hooks (a
        /// near-clone of Claude's: PascalCase `hook_event_name`, JSON on
        /// stdin) fire `SessionStart` / `UserPromptSubmit` / `PreToolUse`
        /// / `PostToolUse` / `Stop` / … which map through the same
        /// [`crate::hook::hook_to_state`] transitions — so Codex reaches
        /// `Done` on a real `Stop` signal instead of a scraper topping
        /// out at `Idle`.
        ///
        /// `--dangerously-bypass-hook-trust` is required for the injected
        /// hooks to run: without persisted per-source trust Codex silently
        /// drops them (no prompt). lazybox is the hook source and vets it,
        /// and the flag bypasses only hook trust, never approvals/sandbox.
        fn hook_command_args(&self, hook_command: &str) -> Vec<String> {
            let mut args = vec!["--dangerously-bypass-hook-trust".to_string()];
            for event in crate::hook_settings::HOOKED_EVENTS {
                args.push("-c".to_string());
                args.push(codex_hook_override(event, hook_command));
            }
            args
        }

        /// Codex Code's three observable states. Delegates to the pure
        /// [`crate::detect::codex_state`] — its live `• Working
        /// (… esc to interrupt)` status line (`Working`), its approval /
        /// consent modals and the bare `[y/n]` family (`InputNeeded`),
        /// and the resting composer (`Idle`) — so the logic is
        /// exercisable against captured real PTY bytes.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            detect::codex_state(recent_output)
        }

        /// Chunk-aware detection — threads the daemon's chunk-boundary
        /// hint so a full-screen repaint (approval modal + an earlier
        /// status line in one chunk) keeps the modal live.
        fn detect_state_chunked(
            &self,
            recent_output: &[u8],
            last_chunk_start: usize,
        ) -> Option<AgentState> {
            detect::codex_state_chunked(recent_output, last_chunk_start)
        }

        /// Codex prints `<model> <effort> · <cwd>` in its composer footer,
        /// so its live model reading is the parsed footer (Codex ships no
        /// built-in tier menu, making this the only model signal it has).
        fn detect_model_effort(&self, recent_output: &[u8]) -> Option<String> {
            detect::codex_model_effort(recent_output)
        }

        /// Surface Codex approval and directory-trust dialogs as soon as
        /// their distinctive modal chrome is painted. The detector only
        /// accepts a marker touched by the newest PTY chunk, so an answered
        /// dialog lingering in scrollback cannot pin the session at `?`.
        fn detect_input_needed_in_current_chunk(
            &self,
            recent_output: &[u8],
            last_chunk_start: usize,
        ) -> Option<PromptShape> {
            detect::codex_input_needed_in_current_chunk(recent_output, last_chunk_start)
        }

        fn detect_blocked_in_current_chunk(
            &self,
            recent_output: &[u8],
            last_chunk_start: usize,
        ) -> Option<AgentObservation> {
            detect::codex_blocked_in_current_chunk(recent_output, last_chunk_start)
        }

        /// Whether Codex's composer is drawn and no approval / trust
        /// modal is up. Delegates to [`crate::detect::codex_ready_for_prompt`].
        fn detect_ready_for_prompt(&self, recent_output: &[u8]) -> bool {
            detect::codex_ready_for_prompt(recent_output)
        }

        /// Repaint-frame readiness — Codex's diff renderer never lets the
        /// whole-buffer positional read settle, so readiness is judged from
        /// composer chrome painted by the latest chunk. Delegates to
        /// [`crate::detect::codex_ready_for_prompt_chunked`].
        fn detect_ready_for_prompt_chunked(
            &self,
            recent_output: &[u8],
            last_chunk_start: usize,
        ) -> bool {
            detect::codex_ready_for_prompt_chunked(recent_output, last_chunk_start)
        }
    }

    #[derive(Default)]
    pub struct Cursor;

    impl Agent for Cursor {
        fn id(&self) -> &'static str {
            "cursor-agent"
        }
        fn display_name(&self) -> &'static str {
            "Cursor Agent"
        }
        fn badge(&self) -> char {
            'U'
        }
        fn llm_provider(&self) -> Option<LlmProvider> {
            Some(LlmProvider::OpenAI)
        }
        /// `cursor-agent` talks to Cursor's own backend and ignores
        /// `OPENAI_BASE_URL`, so routing it through the metering proxy
        /// captures nothing — it would show metered while it isn't. Exclude
        /// it from metering (it still gets plain gateway injection via
        /// [`Agent::llm_provider`]).
        fn meterable(&self) -> bool {
            false
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["cursor-agent".into()]
        }

        fn update_channel(&self) -> Option<crate::update::UpdateChannel> {
            Some(crate::update::cursor_channel())
        }

        /// Cursor uses the bare yes/no prompt family — no custom
        /// UI markers. Shares the standard `YN_PROMPT_PATTERNS`
        /// slice with Codex / GenericCli.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            let s = detect::strip_ansi_lossy(recent_output);
            let tail = detect::recent_tail(&s, PROMPT_TAIL_WINDOW);
            // Match only the bottom-of-screen prompt zone (see Codex).
            let prompt_zone = detect::last_nonempty_lines(tail, 5);
            if detect::contains_any(&prompt_zone, detect::YN_PROMPT_PATTERNS) {
                return Some(AgentState::InputNeeded);
            }
            // No Cursor "working" pulser is recognised yet — fall back
            // to `Idle`, never falsely busy.
            Some(AgentState::Idle)
        }
    }

    /// User-defined agent loaded from YAML. Kept minimal — spawn cmd +
    /// optional resume args + asking patterns. Lets users ship new
    /// agent integrations without code.
    #[derive(Debug, Clone)]
    pub struct GenericCli {
        pub id: String,
        pub display_name: String,
        pub spawn_cmd: Vec<String>,
        pub resume_cmd: Option<Vec<String>>,
        pub asking_patterns: Vec<String>,
    }

    impl Agent for GenericCli {
        fn id(&self) -> &str {
            &self.id
        }
        fn display_name(&self) -> &str {
            &self.display_name
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            self.spawn_cmd.clone()
        }
        fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
            self.resume_cmd.clone().unwrap_or_else(|| self.spawn(ctx))
        }
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            if self.asking_patterns.is_empty() {
                return None;
            }
            // YAML-supplied patterns flow through the shared
            // `contains_any` helper so the GenericCli matcher behaves
            // identically to the built-ins: ANSI-stripped first (a
            // colored prompt would otherwise split the marker), and
            // `Idle` on no-match — returning `None` left the cached
            // `InputNeeded` stuck forever once a prompt was answered,
            // since consumers only update on `Some` readings.
            let s = detect::strip_ansi_lossy(recent_output);
            let tail = detect::recent_tail(&s, PROMPT_TAIL_WINDOW);
            let refs: Vec<&str> = self.asking_patterns.iter().map(String::as_str).collect();
            if detect::contains_any(tail, &refs) {
                Some(AgentState::InputNeeded)
            } else {
                Some(AgentState::Idle)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::builtins::Claude;
    use super::{Agent, LlmProvider, SpawnCtx, StructuredAgentProtocol};
    use lazybox_ipc::AgentRunAccess;

    const SKIP_FLAG: &str = "--dangerously-skip-permissions";
    const STRICT_MCP_FLAG: &str = "--strict-mcp-config";

    #[test]
    fn builtin_agents_map_to_their_llm_provider() {
        assert_eq!(Claude.llm_provider(), Some(LlmProvider::Anthropic));
        assert_eq!(
            super::builtins::Codex.llm_provider(),
            Some(LlmProvider::OpenAI)
        );
        assert_eq!(
            super::builtins::Cursor.llm_provider(),
            Some(LlmProvider::OpenAI)
        );
    }

    #[test]
    fn meterable_excludes_agents_that_ignore_the_base_url_env() {
        // Claude/Codex honor their base-URL env → meterable. Cursor speaks a
        // provider but ignores it → not meterable (would show metered while
        // it isn't). GenericCli has no provider → not meterable.
        assert!(Claude.meterable());
        assert!(super::builtins::Codex.meterable());
        assert!(
            !super::builtins::Cursor.meterable(),
            "cursor-agent ignores OPENAI_BASE_URL, so it can't be metered",
        );
        let generic = super::builtins::GenericCli {
            id: "custom".into(),
            display_name: "Custom".into(),
            spawn_cmd: vec!["custom".into()],
            resume_cmd: None,
            asking_patterns: vec![],
        };
        assert!(!generic.meterable(), "no provider → not meterable");
    }

    #[test]
    fn builtins_declare_distinct_display_badges() {
        // #440: the display badge lives on the agent, not a hardcoded
        // sidebar match. Distinct letters guarantee two live agents in
        // one workspace never collapse onto one column.
        assert_eq!(Claude.badge(), 'C');
        assert_eq!(super::builtins::Codex.badge(), 'X');
        assert_eq!(super::builtins::Cursor.badge(), 'U');
    }

    #[test]
    fn generic_cli_badge_defaults_to_first_char() {
        let agent = super::builtins::GenericCli {
            id: "aider".into(),
            display_name: "Aider".into(),
            spawn_cmd: vec!["aider".into()],
            resume_cmd: None,
            asking_patterns: vec![],
        };
        assert_eq!(agent.badge(), 'A');
    }

    #[test]
    fn registry_badge_for_resolves_known_ids_and_falls_back_otherwise() {
        let reg = super::Registry::default_builtins();
        assert_eq!(reg.badge_for("codex"), 'X', "known id → declared badge");
        assert_eq!(reg.badge_for("cursor-agent"), 'U');
        // Unknown id (a YAML GenericCli the built-in registry never saw)
        // falls back to the same first-char rule the trait default uses.
        assert_eq!(reg.badge_for("aider"), 'A');
        assert_eq!(reg.badge_for(""), 'A', "empty id degenerates to 'A'");
    }

    #[test]
    fn only_adapted_builtins_advertise_a_structured_protocol() {
        assert_eq!(
            Claude.structured_protocol(),
            Some(StructuredAgentProtocol::ClaudeStreamJson)
        );
        assert_eq!(
            super::builtins::Codex.structured_protocol(),
            Some(StructuredAgentProtocol::CodexExecJson)
        );
        assert_eq!(super::builtins::Cursor.structured_protocol(), None);
    }

    #[test]
    fn generic_cli_has_no_inferable_provider() {
        let agent = super::builtins::GenericCli {
            id: "custom".into(),
            display_name: "Custom".into(),
            spawn_cmd: vec!["custom".into()],
            resume_cmd: None,
            asking_patterns: vec![],
        };
        assert_eq!(agent.llm_provider(), None);
        assert_eq!(agent.structured_protocol(), None);
    }

    #[test]
    fn builtins_advertise_update_channels_generic_opts_out() {
        assert!(Claude.update_channel().is_some());
        assert!(super::builtins::Codex.update_channel().is_some());
        assert!(super::builtins::Cursor.update_channel().is_some());
        let generic = super::builtins::GenericCli {
            id: "custom".into(),
            display_name: "Custom".into(),
            spawn_cmd: vec!["custom".into()],
            resume_cmd: None,
            asking_patterns: vec![],
        };
        assert!(generic.update_channel().is_none());
    }

    #[test]
    fn provider_base_url_env_names() {
        assert_eq!(LlmProvider::Anthropic.base_url_env(), "ANTHROPIC_BASE_URL");
        assert_eq!(LlmProvider::OpenAI.base_url_env(), "OPENAI_BASE_URL");
    }

    #[test]
    fn claude_spawn_carries_skip_flag_only_when_opted_in() {
        let claude = Claude;

        let off = SpawnCtx {
            skip_permissions: false,
            ..Default::default()
        };
        assert_eq!(claude.spawn(&off), vec!["claude".to_string()]);

        // #1183: skip-permissions no longer implies strict MCP — the
        // user's ambient MCP servers stay available by default.
        let on = SpawnCtx {
            skip_permissions: true,
            ..Default::default()
        };
        assert_eq!(
            claude.spawn(&on),
            vec!["claude".to_string(), SKIP_FLAG.to_string()],
            "unattended spawns inherit the user's MCP setup by default",
        );

        // Opting back in (`agent.strict_mcp: true`) restores the flag.
        let strict = SpawnCtx {
            skip_permissions: true,
            strict_mcp: true,
            ..Default::default()
        };
        assert_eq!(
            claude.spawn(&strict),
            vec![
                "claude".to_string(),
                SKIP_FLAG.to_string(),
                STRICT_MCP_FLAG.to_string()
            ]
        );
    }

    #[test]
    fn claude_resume_carries_skip_flag_only_when_opted_in() {
        let claude = Claude;

        let off = SpawnCtx {
            skip_permissions: false,
            ..Default::default()
        };
        assert_eq!(
            claude.resume(&off),
            vec!["claude".to_string(), "--continue".to_string()]
        );

        let on = SpawnCtx {
            skip_permissions: true,
            ..Default::default()
        };
        assert_eq!(
            claude.resume(&on),
            vec![
                "claude".to_string(),
                "--continue".to_string(),
                SKIP_FLAG.to_string(),
            ],
            "resume inherits the user's MCP setup by default (#1183)",
        );

        let strict = SpawnCtx {
            skip_permissions: true,
            strict_mcp: true,
            ..Default::default()
        };
        assert_eq!(
            claude.resume(&strict),
            vec![
                "claude".to_string(),
                "--continue".to_string(),
                SKIP_FLAG.to_string(),
                STRICT_MCP_FLAG.to_string()
            ]
        );
    }

    #[test]
    fn claude_appends_settings_flag_when_hook_path_set() {
        let claude = Claude;
        let ctx = SpawnCtx {
            hook_settings_path: Some(std::path::PathBuf::from("/run/hooks/settings-7.json")),
            ..Default::default()
        };
        assert_eq!(
            claude.spawn(&ctx),
            vec![
                "claude".to_string(),
                "--settings".to_string(),
                "/run/hooks/settings-7.json".to_string(),
            ]
        );
        assert_eq!(
            claude.resume(&ctx),
            vec![
                "claude".to_string(),
                "--continue".to_string(),
                "--settings".to_string(),
                "/run/hooks/settings-7.json".to_string(),
            ]
        );
    }

    #[test]
    fn claude_read_only_launch_disables_mutating_and_ambient_tools() {
        let claude = Claude;
        let ctx = SpawnCtx {
            skip_permissions: true,
            access: AgentRunAccess::ReadOnly,
            hook_settings_path: Some("/run/hooks/settings-7.json".into()),
            ..Default::default()
        };

        for argv in [claude.spawn(&ctx), claude.resume(&ctx)] {
            assert!(
                argv.windows(2)
                    .any(|args| args == ["--tools", "Read,Glob,Grep"])
            );
            assert!(
                argv.windows(2)
                    .any(|args| args == ["--permission-mode", "dontAsk"])
            );
            assert!(argv.iter().any(|arg| arg == "--safe-mode"));
            assert!(argv.iter().any(|arg| arg == "--strict-mcp-config"));
            assert!(!argv.iter().any(|arg| arg == SKIP_FLAG));
            assert!(!argv.iter().any(|arg| arg == "--settings"));
        }
    }

    #[test]
    fn claude_build_hook_settings_wires_command() {
        let claude = Claude;
        let settings = claude
            .build_hook_settings(
                "lazybox hook-ingest --backend-key lazybox-ws-claude-1-7",
                None,
            )
            .expect("claude supports hooks");
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "lazybox hook-ingest --backend-key lazybox-ws-claude-1-7"
        );
    }

    #[test]
    fn codex_resume_continues_last_cwd_session() {
        let codex = super::builtins::Codex;

        // Bare resume reattaches the cwd's most recent session; no
        // unattended flags unless opted in.
        let off = SpawnCtx {
            skip_permissions: false,
            ..Default::default()
        };
        assert_eq!(
            codex.resume(&off),
            vec![
                "codex".to_string(),
                "resume".to_string(),
                "--last".to_string(),
            ]
        );

        // The unattended flags ride resume identically to spawn, so a
        // restored autonomous session starts just as clean.
        let on = SpawnCtx {
            skip_permissions: true,
            worktree: std::path::PathBuf::from("/tmp/wt"),
            ..Default::default()
        };
        let resumed = codex.resume(&on);
        assert_eq!(resumed[..3], ["codex", "resume", "--last"]);
        assert!(resumed.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        // Hook trust is bypassed alongside the injected hooks
        // (`hook_command_args`), not in the unattended flags — so the bare
        // spawn/resume argv no longer carries it.
        assert!(!resumed.contains(&"--dangerously-bypass-hook-trust".to_string()));
        // The unattended tail matches spawn's exactly.
        assert_eq!(resumed[3..], codex.spawn(&on)[1..]);
    }

    #[test]
    fn codex_read_only_launch_enforces_sandbox_without_bypass() {
        let codex = super::builtins::Codex;
        let ctx = SpawnCtx {
            skip_permissions: true,
            access: AgentRunAccess::ReadOnly,
            ..Default::default()
        };

        for argv in [codex.spawn(&ctx), codex.resume(&ctx)] {
            assert!(
                argv.windows(2)
                    .any(|args| args == ["--sandbox", "read-only"])
            );
            assert!(
                argv.windows(2)
                    .any(|args| args == ["--ask-for-approval", "never"])
            );
            assert!(argv.windows(2).any(|args| args == ["-c", "mcp_servers={}"]));
            assert!(argv.windows(2).any(|args| args == ["-c", "hooks={}"]));
            assert!(
                !argv
                    .iter()
                    .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
            );
        }
    }

    #[test]
    fn codex_seeds_homebrew_auto_update_suppression() {
        assert_eq!(
            super::builtins::Codex.spawn_env(),
            vec![("HOMEBREW_NO_AUTO_UPDATE".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn codex_isolates_credentials_per_session_claude_does_not() {
        let iso = super::builtins::Codex
            .credential_isolation()
            .expect("codex isolates its login");
        assert_eq!(iso.home_env, "CODEX_HOME");
        assert_eq!(iso.default_home, ".codex");
        assert!(iso.seed_files.contains(&"auth.json"));
        // Claude's macOS credential lives in the Keychain, not under a
        // relocatable config dir, so it stays on the machine-wide login.
        assert!(Claude.credential_isolation().is_none());
    }

    #[test]
    fn claude_requires_inline_renderer_for_pty_scrollback() {
        assert_eq!(
            Claude.pty_spawn_env(),
            vec![(
                "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".to_string(),
                "1".to_string(),
            )]
        );
        assert_eq!(Claude.pty_launch_generation(), 1);
        assert!(Claude.spawn_env().is_empty());
    }

    #[test]
    fn other_agents_seed_no_spawn_env() {
        assert!(super::builtins::Cursor.spawn_env().is_empty());
        assert!(super::builtins::Cursor.pty_spawn_env().is_empty());
        let generic = super::builtins::GenericCli {
            id: "custom".into(),
            display_name: "Custom".into(),
            spawn_cmd: vec!["custom".into()],
            resume_cmd: None,
            asking_patterns: vec![],
        };
        assert!(generic.spawn_env().is_empty());
    }

    #[test]
    fn non_claude_agents_have_no_hook_settings() {
        assert!(
            super::builtins::Codex
                .build_hook_settings("x", None)
                .is_none()
        );
        assert!(
            super::builtins::Cursor
                .build_hook_settings("x", None)
                .is_none()
        );
    }
}
