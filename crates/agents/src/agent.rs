//! The `Agent` trait and built-in implementations.

use crate::pty::{EncodedPrompt, PromptIntent, PtyProtocol};
use lazybox_ipc::AgentState;
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
    /// Path to a lazybox-generated settings file the agent should launch
    /// with, when the daemon has wired up structured lifecycle hooks
    /// for this spawn. Claude appends `--settings <path>`; agents
    /// without a settings flag ignore it. `None` when hooks aren't
    /// configured (non-Claude agent, or generation failed) — those fall
    /// back to PTY-based state detection.
    pub hook_settings_path: Option<PathBuf>,
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

pub use crate::pty::PromptShape;

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

pub trait Agent: Send + Sync {
    /// Stable id used in config and IPC (`"claude"`, `"codex"`, etc.).
    fn id(&self) -> &'static str;

    /// Human-readable display name.
    fn display_name(&self) -> &'static str;

    /// Single-letter badge for the sidebar runner column and any other
    /// compact "which agent is live here" indicator. Declared by the
    /// agent so identity lives in one place: the sidebar never
    /// special-cases a kind, and a new agent can pick a letter that
    /// doesn't collide instead of silently sharing the first char of a
    /// name that's already taken (Codex vs Claude, both `C`). The
    /// default derives the first char of [`Agent::id`] — fine for a
    /// unique leading letter, overridden when it would collide.
    fn badge(&self) -> char {
        self.id()
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('A')
    }

    /// Which upstream LLM API this agent speaks. Drives base-URL env
    /// injection when an LLM gateway is configured. The default `None`
    /// covers agents whose upstream lazybox can't infer (a `GenericCli`
    /// pointed at an arbitrary command) — they get no gateway injection.
    fn llm_provider(&self) -> Option<LlmProvider> {
        None
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

    /// Encode a complete prompt interaction atomically. Most adapters should
    /// inherit this and declare [`Agent::pty_protocol`]; an unusual CLI may
    /// override this single method without having to coordinate independent
    /// prompt and submit hooks.
    fn encode_prompt(&self, prompt: &str, intent: PromptIntent) -> EncodedPrompt {
        self.pty_protocol().encode_prompt(prompt, intent)
    }

    /// Command + args to spawn a fresh session.
    fn spawn(&self, ctx: &SpawnCtx) -> Vec<String>;

    /// Command + args to resume the most recent session for this
    /// worktree. Default: same as `spawn`. Override when the agent has
    /// a `--continue`-style flag.
    fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
        self.spawn(ctx)
    }

    /// Extra environment variables to seed into this agent's launch, on
    /// top of the inherited process env, for BOTH the interactive PTY
    /// spawn and the structured `exec` run. The daemon skips any key a
    /// higher-priority source (per-repo `env`) already set, so these are
    /// defaults, not overrides. Default: none.
    fn spawn_env(&self) -> Vec<(String, String)> {
        Vec::new()
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
    /// it. Best-effort: failures leave the spawn to proceed unchanged.
    fn prepare_unattended(&self, worktree: &Path) {
        let _ = worktree;
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
    /// The default returns `None`: most agents have no hook system, so
    /// the daemon writes no settings file and keeps PTY detection. Only
    /// Claude overrides this. The daemon writes the returned JSON to a
    /// per-session file and sets [`SpawnCtx::hook_settings_path`].
    fn build_hook_settings(
        &self,
        hook_command: &str,
        user_settings: Option<&serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let _ = (hook_command, user_settings);
        None
    }
}

/// Registry of known agents. Keyed by `Agent::id()`.
#[derive(Default, Clone)]
pub struct Registry {
    agents: HashMap<&'static str, Arc<dyn Agent>>,
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
        self.agents.insert(agent.id(), agent);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &&'static str> {
        self.agents.keys()
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
            argv.push("--strict-mcp-config".into());
        }
    }

    /// Append `--settings <path>` when the daemon generated a hooks
    /// settings file for this spawn. Claude's `--settings` accepts a
    /// file path and takes precedence over user/project settings — the
    /// daemon has already merged the user's hooks into that file (see
    /// [`crate::hook_settings`]), so nothing is clobbered.
    fn push_settings_flag(argv: &mut Vec<String>, ctx: &SpawnCtx) {
        if let Some(path) = &ctx.hook_settings_path {
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
        fn pty_protocol(&self) -> PtyProtocol {
            PtyProtocol::GUARDED_COMPOSER
        }
        fn spawn(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["claude".into()];
            push_unattended_flags(&mut argv, ctx);
            push_settings_flag(&mut argv, ctx);
            argv
        }
        fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["claude".into(), "--continue".into()];
            push_unattended_flags(&mut argv, ctx);
            push_settings_flag(&mut argv, ctx);
            argv
        }

        fn prepare_unattended(&self, worktree: &Path) {
            if let Err(e) = crate::claude_env::seed_unattended_env(worktree) {
                tracing::warn!(
                    worktree = %worktree.display(),
                    "claude: failed to prepare unattended env (trust/onboarding): {e}",
                );
            }
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
        let worktree = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
        let path = serde_json::to_string(&worktree.to_string_lossy())
            .unwrap_or_else(|_| "\"\"".to_string());
        format!("projects={{{path}={{trust_level=\"trusted\"}}}}")
    }

    impl Agent for Codex {
        fn id(&self) -> &'static str {
            "codex"
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
        fn spawn(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["codex".into()];
            if ctx.skip_permissions {
                argv.push("--dangerously-bypass-approvals-and-sandbox".into());
                argv.push("--dangerously-bypass-hook-trust".into());
                argv.push("-c".into());
                argv.push(codex_trusted_project_override(&ctx.worktree));
                argv.push("-c".into());
                argv.push("check_for_update_on_startup=false".into());
            }
            argv
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
        pub id: &'static str,
        pub display_name: &'static str,
        pub spawn_cmd: Vec<String>,
        pub resume_cmd: Option<Vec<String>>,
        pub asking_patterns: Vec<String>,
    }

    impl Agent for GenericCli {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.display_name
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
            id: "aider",
            display_name: "Aider",
            spawn_cmd: vec!["aider".into()],
            resume_cmd: None,
            asking_patterns: vec![],
        };
        assert_eq!(agent.badge(), 'A');
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
            id: "custom",
            display_name: "Custom",
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
            id: "custom",
            display_name: "Custom",
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

        let on = SpawnCtx {
            skip_permissions: true,
            ..Default::default()
        };
        assert_eq!(
            claude.spawn(&on),
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
    fn codex_seeds_homebrew_auto_update_suppression() {
        assert_eq!(
            super::builtins::Codex.spawn_env(),
            vec![("HOMEBREW_NO_AUTO_UPDATE".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn other_agents_seed_no_spawn_env() {
        assert!(Claude.spawn_env().is_empty());
        assert!(super::builtins::Cursor.spawn_env().is_empty());
        let generic = super::builtins::GenericCli {
            id: "custom",
            display_name: "Custom",
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
