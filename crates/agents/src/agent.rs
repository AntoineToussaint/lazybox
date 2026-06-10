//! The `Agent` trait and built-in implementations.

use lazybox_ipc::AgentState;
use std::collections::HashMap;
use std::path::PathBuf;
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
    /// support a bypass flag (Claude → `--dangerously-skip-permissions`).
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

pub trait Agent: Send + Sync {
    /// Stable id used in config and IPC (`"claude"`, `"codex"`, etc.).
    fn id(&self) -> &'static str;

    /// Human-readable display name.
    fn display_name(&self) -> &'static str;

    /// Command + args to spawn a fresh session.
    fn spawn(&self, ctx: &SpawnCtx) -> Vec<String>;

    /// Command + args to resume the most recent session for this
    /// worktree. Default: same as `spawn`. Override when the agent has
    /// a `--continue`-style flag.
    fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
        self.spawn(ctx)
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

    /// Whether the spawn-time prompt injector must hold off pasting
    /// until `detect_ready_for_prompt` reports ready. Agents with an
    /// authoritative readiness detector (Claude — input box drawn AND
    /// no folder-trust / permission gate up) override to `true`, so
    /// the time-based settle fallback can never paste the work-context
    /// prompt into a still-visible trust dialog.
    ///
    /// Agents that rely on the default always-false
    /// `detect_ready_for_prompt` keep `false`: for them the detector
    /// never reports ready, so gating the injector on it would stall
    /// every inject to the hard deadline. They keep the first-output +
    /// settle path instead.
    fn inject_requires_ready(&self) -> bool {
        false
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

    /// Encode a prompt as bytes the daemon should write to the PTY.
    /// Most agents accept plain text + a newline; some need bracketed
    /// paste or specific control sequences.
    fn inject_prompt(&self, prompt: &str) -> Vec<u8> {
        let mut bytes = prompt.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }

    /// Bytes to write AFTER `inject_prompt`, separated by a brief
    /// delay, to commit/submit the prompt. Returns `None` when
    /// `inject_prompt` already includes the submit keystroke — the
    /// default, which works for any CLI where Enter both terminates
    /// the line and submits it.
    ///
    /// Required by agents whose input area batches rapid byte
    /// arrival as a paste (Claude Code): Enter inside a paste blob
    /// is interpreted as a soft line break in the input buffer, not
    /// as a submit. Sending Enter separately, after the paste batch
    /// has settled, triggers the actual submit.
    fn inject_submit(&self) -> Option<Vec<u8>> {
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
        fn spawn(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["claude".into()];
            if ctx.skip_permissions {
                argv.push("--dangerously-skip-permissions".into());
            }
            push_settings_flag(&mut argv, ctx);
            argv
        }
        fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["claude".into(), "--continue".into()];
            if ctx.skip_permissions {
                argv.push("--dangerously-skip-permissions".into());
            }
            push_settings_flag(&mut argv, ctx);
            argv
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

        /// Claude Code's input area batches rapid byte arrival as a
        /// paste. The prompt body is wrapped in explicit bracketed-
        /// paste markers (`ESC[200~` … `ESC[201~`) so Claude's paste
        /// detection is deterministic — without them it relies on
        /// arrival timing, and a write that coalesces with the later
        /// `\r` can swallow the submit as a soft line break. No `\r`
        /// is included here: the trailing Enter is sent separately by
        /// `inject_submit` after a brief delay, once the paste batch
        /// has settled, so it's unambiguously a keystroke.
        ///
        /// Any literal `\n` inside the prompt stays a line break in
        /// Claude's input box, which is what we want for multi-
        /// paragraph instructions.
        fn inject_prompt(&self, prompt: &str) -> Vec<u8> {
            let mut bytes = Vec::with_capacity(prompt.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(prompt.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        }

        /// Send `\r` (Enter) separately from the paste body. Without
        /// the gap, Claude treats the whole blob as a paste and the
        /// trailing `\r` becomes a soft line break instead of a
        /// submit — the prompt sits in the input box waiting on a
        /// keystroke. Sending `\r` after the gap fires Enter as an
        /// independent keystroke and submits the paste.
        fn inject_submit(&self) -> Option<Vec<u8>> {
            Some(vec![b'\r'])
        }

        /// Claude Code's three observable states. Delegates to the pure
        /// [`crate::detect::claude_state`] so the logic is exercisable
        /// against captured real PTY bytes, not just synthetic strings.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            detect::claude_state(recent_output)
        }

        /// Whether Claude is ready to receive a pasted prompt — input
        /// box drawn, no permission / trust gate up. Delegates to
        /// [`crate::detect::claude_ready_for_prompt`].
        fn detect_ready_for_prompt(&self, recent_output: &[u8]) -> bool {
            detect::claude_ready_for_prompt(recent_output)
        }

        fn inject_requires_ready(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    pub struct Codex;

    impl Agent for Codex {
        fn id(&self) -> &'static str {
            "codex"
        }
        fn display_name(&self) -> &'static str {
            "Codex"
        }
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["codex".into()]
        }

        /// Codex CLI uses the standard `[y/n]` family plus a custom
        /// `approve?` phrasing. Declarative — both groups flow
        /// through the shared `super::detect` helpers so a new
        /// Codex prompt phrasing just appends to the slice.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            let s = detect::strip_ansi_lossy(recent_output);
            let tail = detect::recent_tail(&s, PROMPT_TAIL_WINDOW);
            if detect::contains_any(tail, detect::YN_PROMPT_PATTERNS)
                || detect::contains_any(tail, &["approve?"])
            {
                return Some(AgentState::InputNeeded);
            }
            // No Codex "working" pulser is recognised yet, so fall back
            // to `Idle` rather than `Working` — never falsely busy.
            Some(AgentState::Idle)
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
        fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
            vec!["cursor-agent".into()]
        }

        /// Cursor uses the bare yes/no prompt family — no custom
        /// UI markers. Shares the standard `YN_PROMPT_PATTERNS`
        /// slice with Codex / GenericCli.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            let s = detect::strip_ansi_lossy(recent_output);
            let tail = detect::recent_tail(&s, PROMPT_TAIL_WINDOW);
            if detect::contains_any(tail, detect::YN_PROMPT_PATTERNS) {
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
    use super::{Agent, SpawnCtx};

    const SKIP_FLAG: &str = "--dangerously-skip-permissions";

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
            vec!["claude".to_string(), SKIP_FLAG.to_string()]
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
                SKIP_FLAG.to_string()
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
            .build_hook_settings("lazybox hook-ingest --backend-key lazybox-ws-claude-1-7", None)
            .expect("claude supports hooks");
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "lazybox hook-ingest --backend-key lazybox-ws-claude-1-7"
        );
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
