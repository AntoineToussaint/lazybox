//! The `Agent` trait and built-in implementations.

use pilot_ipc::AgentState;
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
    /// ("no-permission" / bypass mode) so it runs unattended. Set only
    /// for pilot-spawned autonomous sessions; honored by agents that
    /// support a bypass flag (Claude → `--dangerously-skip-permissions`).
    /// Agents without one ignore it.
    pub skip_permissions: bool,
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

    /// Inspect recent PTY output and return an updated state, or None
    /// if no confident determination. Used when the agent has no hooks
    /// (Codex, Cursor) or as a fallback when hooks miss a transition.
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
    /// box with a question in chat history" (paste is fine).
    /// `detect_state`'s loose heuristics (last line ends with `?`)
    /// power the `?` sidebar pill, where false positives are
    /// harmless. The inject path can't tolerate them — a false
    /// Asking made the inject wait 60s on every spawn before
    /// shipping the paste.
    ///
    /// Default `false` — agents that don't override never report
    /// ready, so the spawn-time injector falls back to its time-
    /// based settle. Built-ins (Claude, Codex, Cursor) override.
    fn detect_ready_for_prompt(&self, recent_output: &[u8]) -> bool {
        let _ = recent_output;
        false
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
/// Every agent's `detect_state` walks a small set of well-known
/// markers in the recent PTY output to classify "is the agent
/// asking the user something?" The vocabulary repeats:
///
/// - **Bare yes/no markers** (`[y/n]`, `(y/n)`, …) — used by Codex,
///   Cursor, and most YAML-configured CLIs. Single substring match
///   is enough confidence: these don't show up in chat output.
///
/// - **Paired patterns** — at least one *choice marker* AND at
///   least one *question phrase*. Used by Claude: "Do you want to"
///   alone is too weak (chat output could include it), so we pair
///   it with `1. Yes` / `(y/n)` / `[y/n]` to raise confidence.
///
/// - **Footer markers** — UI footers some agents render ONLY while
///   waiting on input (Claude's `Esc to cancel · Tab to amend`).
///   The most reliable signal: when present, the agent is asking.
///
/// Adding a new built-in agent should be a config-style declaration
/// — declare the agent's pattern shape using these helpers — rather
/// than writing yet another bespoke matcher with its own
/// substring-soup logic.
pub mod detect {
    /// Standard bare yes/no prompt markers. Used by every CLI that
    /// doesn't have a custom approval UI (Codex, Cursor, most
    /// GenericCli configs). Order doesn't matter — substring search.
    pub const YN_PROMPT_PATTERNS: &[&str] = &["[y/n]", "(y/n)", "[Y/n]", "[y/N]"];

    /// Substring "any-of" match. Plain text in; bytes should be
    /// passed through `strip_ansi_lossy` first so escape sequences
    /// don't split the markers.
    pub fn contains_any(text: &str, patterns: &[&str]) -> bool {
        patterns.iter().any(|p| text.contains(p))
    }

    /// Two-stage check: at least one `choice` marker AND at least
    /// one `question` phrase. Pairing raises confidence — neither
    /// alone is enough to distinguish "agent is asking" from "the
    /// agent's chat output mentions the same phrase."
    pub fn contains_paired(text: &str, choices: &[&str], questions: &[&str]) -> bool {
        contains_any(text, choices) && contains_any(text, questions)
    }

    /// Conversational "asking" detection — issue #58.
    ///
    /// A SEPARATE code path from the structural permission-dialog
    /// detection (issue #26). The structural path keys off harness
    /// UI shape: the `❯ 1. … 2. …` chooser arrow, the `Esc to
    /// cancel` footer, numbered-option menus. This path keys off
    /// Claude's OWN conversational text — the model ending its turn
    /// with a freeform question and parking on it:
    ///
    ///   "Want me to proceed?"
    ///   "Should I also update the tests?"
    ///   "Let me know if you'd like me to continue."
    ///   "Which would you prefer?"
    ///
    /// There's no menu and no footer here, so the structural matchers
    /// never fire. Today those sessions look idle in pilot when
    /// they're actually waiting on the user.
    ///
    /// ## Tuning: recall over precision
    ///
    /// A false positive costs one needlessly-flagged session (cheap —
    /// the user glances and dismisses). A false negative leaves a
    /// session sitting idle indefinitely while it actually waits on
    /// input (expensive). So this detector deliberately leans toward
    /// flagging: any last conversational line ending in `?`, or
    /// containing a known confirmation/clarification phrase, counts.
    pub mod conversational {
        /// Confirmation / clarification phrases that signal Claude is
        /// soliciting input even when the line does NOT end in `?`
        /// (e.g. "Let me know if you'd like me to continue."). Matched
        /// case-insensitively as a substring of the last conversational
        /// line. Documented and auditable in one place; the
        /// fixture suite (`PROMPT_FIXTURES` in `tests/agents.rs`) pins
        /// one shape per phrase class.
        ///
        /// Lowercase. Some entries carry a trailing space to avoid
        /// matching inside longer words ("should i " won't fire on
        /// "shoulder"); the `?`-terminated forms ("Should I?") are
        /// caught by the ends-with-`?` rule instead. Entries are the
        /// shortest distinctive stem of each shape — "would you like"
        /// also catches "would you like me to …", "let me know" also
        /// catches "let me know if / whether / how / your thoughts" —
        /// so we don't carry redundant longer variants.
        pub const CONVERSATIONAL_ASK_PHRASES: &[&str] = &[
            "want me to",
            "should i ",
            "shall i ",
            "do you want",
            "would you like",
            "let me know",
            "which one",
            "which would you prefer",
            "ok to ",
            "okay to ",
        ];

        /// True when `text` (post-ANSI-strip) ends parked on a
        /// conversational ask. Scans the tail of the buffer from the
        /// bottom, skips Claude's rendered UI chrome (input-box
        /// borders, the empty prompt line, the footer hint strip), and
        /// tests the last line of actual conversational content.
        pub fn is_conversational_ask(text: &str) -> bool {
            // Bound to the tail (~2 KiB) so a flush full of streamed
            // output doesn't drag the heuristic back to a question
            // from several screens ago. Round `tail_start` UP to the
            // next char boundary — Claude renders multi-byte glyphs
            // (`●`, box-drawing `─`, the middle-dot `·`) and slicing
            // mid-sequence would panic.
            let mut tail_start = text.len().saturating_sub(2048);
            while tail_start < text.len() && !text.is_char_boundary(tail_start) {
                tail_start += 1;
            }
            text[tail_start..]
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !is_chrome_line(l))
                .is_some_and(line_is_ask)
        }

        /// A single conversational line is an ask when it ends with a
        /// question mark (the model parked on a question) or contains
        /// one of the known confirmation/clarification phrases. The
        /// `?` check runs first so the common case allocates nothing.
        fn line_is_ask(line: &str) -> bool {
            if line.ends_with('?') {
                return true;
            }
            let lower = line.to_lowercase();
            CONVERSATIONAL_ASK_PHRASES.iter().any(|p| lower.contains(p))
        }

        /// Lines that are Claude's rendered UI chrome rather than
        /// conversational content. The ask sits ABOVE the input box in
        /// the transcript, so we skip the box borders, the empty
        /// prompt line, and the footer hint / mode-indicator strip to
        /// reach it. A line of actual prose never starts with a
        /// box-drawing glyph and never carries these footer markers.
        ///
        /// This is load-bearing for RECALL: the footer sits below the
        /// input box, so the bottom-up scan hits it first. A footer
        /// line we fail to recognise as chrome becomes the apparent
        /// "last content line" and masks the real ask above it — a
        /// false negative, the expensive failure mode. Hence the
        /// generous marker list.
        fn is_chrome_line(line: &str) -> bool {
            if line.is_empty() {
                return true;
            }
            let lower = line.to_lowercase();
            if lower.contains("esc to cancel")
                || lower.contains("tab to amend")
                || lower.contains("for shortcuts")
                || lower.contains("ctrl+")
                || lower.contains("shift+")
                || lower.contains("bypass permissions")
                || lower.contains("accept edits")
                || lower.contains("auto-accept")
                || lower.contains("plan mode on")
            {
                return true;
            }
            // Leading glyphs that only ever open a chrome line: the
            // input-box borders (`╭│╰` …), the tool-output gutter
            // (`⎿`), the mode-indicator arrow (`⏵⏵ accept edits`), and
            // the bare prompt marker (`>`).
            matches!(
                line.chars().next(),
                Some(
                    '╭' | '╮'
                        | '╰'
                        | '╯'
                        | '│'
                        | '─'
                        | '┌'
                        | '┐'
                        | '└'
                        | '┘'
                        | '>'
                        | '⎿'
                        | '⏵'
                )
            )
        }
    }
}

pub mod builtins {
    use super::*;

    /// Standalone phrases that are unambiguously Claude blocking on
    /// user input — no chat context realistically produces them, so
    /// matching one is enough confidence to flag `Asking`. Lowercase;
    /// matched against `s.to_lowercase()` so phrasing variants
    /// ("Do you want to Proceed?" vs "Do you want to proceed?") both
    /// fire without expanding the table.
    ///
    /// Each entry maps to one prompt shape. Keep in sync with the
    /// fixture-driven test suite (`PROMPT_FIXTURES` in
    /// `tests/agents.rs`) so every documented shape has explicit
    /// coverage. Shapes covered, by tool:
    ///   - Write tool          → "do you want to create"
    ///   - Edit / MultiEdit    → "do you want to make this edit"
    ///   - file overwrites     → "do you want to overwrite"
    ///   - bash / file deletes → "do you want to delete"
    ///   - Bash approval       → "do you want to allow"
    ///   - plan-mode exit      → "do you want to proceed?"
    ///   - continue chat       → "do you want to continue?"
    ///   - apply diff/patch    → "do you want to apply"
    ///   - tool consent        → "do you want to enable"
    ///   - retry on failure    → "do you want to retry"
    ///   - settings edit       → "do you want to edit its own settings"
    const CLAUDE_STANDALONE_PROMPT_PHRASES: &[&str] = &[
        "do you want to proceed?",
        "do you want to continue?",
        "do you want to apply",
        "do you want to allow",
        "do you want to enable",
        "do you want to retry",
        "do you want to create",
        "do you want to make this edit",
        "do you want to overwrite",
        "do you want to delete",
        "do you want to edit its own settings",
    ];

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
            argv
        }
        fn resume(&self, ctx: &SpawnCtx) -> Vec<String> {
            let mut argv = vec!["claude".into(), "--continue".into()];
            if ctx.skip_permissions {
                argv.push("--dangerously-skip-permissions".into());
            }
            argv
        }

        /// Claude Code's input area batches rapid byte arrival as a
        /// paste. We deliberately return JUST the prompt bytes here
        /// (no trailing `\r`) so `\r` doesn't get folded into the
        /// paste blob — inside a paste, Enter is interpreted as a
        /// soft line break, not a submit. The trailing `\r` is sent
        /// separately by `inject_submit` after a brief delay, once
        /// the paste batch has settled.
        ///
        /// Any literal `\n` inside the prompt stays a line break in
        /// Claude's input box, which is what we want for multi-
        /// paragraph instructions.
        fn inject_prompt(&self, prompt: &str) -> Vec<u8> {
            prompt.as_bytes().to_vec()
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

        /// Claude Code's interactive prompt UI is recognisable by a
        /// stable footer line (`Esc to cancel · Tab to amend · …`)
        /// plus paired question phrasings. Both signals run through
        /// the shared `super::detect` helpers so adding a new
        /// pattern is a one-line declarative change, not another
        /// hand-rolled substring soup.
        ///
        /// Returning `Some(Active)` on the default path (rather than
        /// `None`) lets the daemon notice the Asking → Active
        /// transition when the user hits a choice; without it the
        /// cached state would stay Asking forever.
        fn detect_state(&self, recent_output: &[u8]) -> Option<AgentState> {
            let s = strip_ansi_lossy(recent_output);

            // Pilot wraps every agent in tmux. The tmux client paints
            // the screen by absolute cursor position, so the bytes
            // pilot sees arrive in TEMPORAL order: a single visual
            // line `❯ 1. Yes` can land in the buffer as
            // `<cursor-move>❯<cursor-move> <cursor-move>1.<…>Yes` and
            // strip_ansi removes the CSI runs but NOT the absolute
            // positioning gap that may be filled with other content.
            // So strict substring matching for `❯ 1.` fails even when
            // the prompt is visually on screen.
            //
            // Generalized matcher: the arrow `❯` ANYWHERE in the
            // buffer paired with a numbered-choice shape (any `1.` /
            // `1)` followed by a text option AND any `2.` / `2)`)
            // catches every chooser claude renders today: permission
            // prompts, tool-approval, multi-choice continuations,
            // custom yes/no — anything with at least two numbered
            // options. The `2.` / `2)` second requirement filters out
            // chat lists that happen to start with `1.` (the second
            // option is the giveaway: a chooser is always followed
            // immediately by `2.`).
            //
            // ASCII arrow accepted on ANY option (`> 1.`, `> 2.`,
            // `> 3.`, …) — the user moves the cursor with j/k, and
            // claude re-renders the arrow at the new option. Earlier
            // we only matched `> 1.`, which silently missed the Write
            // permission dialog when the default selection was
            // option 2 ("Yes, and allow Claude to edit its own
            // settings for this session").
            let has_arrow = s.contains('❯') || has_ascii_chooser_arrow(&s);
            let has_chooser = has_numbered_chooser_options(&s);
            if has_arrow && has_chooser {
                return Some(AgentState::Asking);
            }

            // Footer marker — Claude renders the textarea hint while
            // editing a prompt. Paired so a chat message that quotes
            // "Esc to cancel" doesn't false-trigger.
            if super::detect::contains_paired(&s, &["Esc to cancel"], &["Tab to amend"]) {
                return Some(AgentState::Asking);
            }

            // Permission-chooser footer: `Esc to cancel` rendered
            // alongside a numbered-options shape (`1.` + `2.`). The
            // permission dialog uses a SHORTER footer than the input
            // box ("Esc to cancel" alone, without "Tab to amend"), so
            // the input-box paired check above doesn't fire. Pairing
            // `Esc to cancel` with the chooser shape is still high-
            // confidence — both halves are UI-specific markers that
            // never co-occur in chat output. This is the path that
            // catches the Write/Edit/Bash permission dialog even when
            // the ASCII arrow lands on option 2 or 3 (so the
            // arrow+options branch above misses it) and the
            // `do you want to <verb>` standalone phrase scrolled off
            // the detection window's tail.
            if s.contains("Esc to cancel") && has_chooser {
                return Some(AgentState::Asking);
            }

            // Standalone high-confidence prompts. These exact phrases
            // appear only when claude is blocking on user input —
            // there's no realistic chat context that produces them.
            // Lowercase comparison so phrasing variants ("Do you
            // want to proceed?" vs "Do you want to Proceed?") both
            // match without expanding the table. Without this the
            // bash-permission prompt was missed entirely on long
            // outputs where the `❯` glyph got pushed off the
            // detection window's tail.
            let lower = s.to_lowercase();
            if CLAUDE_STANDALONE_PROMPT_PHRASES
                .iter()
                .any(|p| lower.contains(p))
            {
                return Some(AgentState::Asking);
            }

            // Paired fallback for bare yes/no prompts (no arrow UI) +
            // older prompt shapes. Question phrases ANY-ed with choice
            // markers — both must be present in the buffer.
            if super::detect::contains_paired(
                &s,
                &["1. Yes", "1) Yes", "(y/n)", "[y/n]"],
                &[
                    "Do you want",
                    "Allow Claude",
                    "Allow Bash",
                    "Approve",
                    "Continue?",
                    "Proceed?",
                ],
            ) {
                return Some(AgentState::Asking);
            }

            // Conversational asks (issue #58) — a SEPARATE code path
            // from the structural matchers above. Everything before
            // this point keys off harness UI shape (chooser arrow,
            // `Esc to cancel` footer, numbered menus). This last
            // branch keys off Claude's own conversational text: the
            // model ending its turn parked on a freeform question
            // ("Want me to proceed?", "Should I also do X?", "Let me
            // know if you'd like me to continue."). No menu, no
            // footer — so none of the structural branches fire, and
            // these sessions previously looked idle. Tuned for recall
            // (see `detect::conversational`).
            if super::detect::conversational::is_conversational_ask(&s) {
                return Some(AgentState::Asking);
            }

            Some(AgentState::Active)
        }

        /// Claude is ready to receive a pasted prompt when:
        ///   1. The input-box footer is on screen (`Esc to cancel` +
        ///      `Tab to amend` paired markers — Claude renders these
        ///      ONLY while waiting at the prompt input).
        ///   2. No permission chooser / Y-N gate is currently up
        ///      (the high-confidence `❯ 1. ... 2. ...` arrow-plus-
        ///      numbered-options pattern, OR known trust-folder
        ///      strings).
        ///
        /// Returning true means "paste arrives in the input buffer,
        /// not as a Y/N answer." Returning false means "wait — the
        /// banner isn't done OR a permission gate is up."
        fn detect_ready_for_prompt(&self, recent_output: &[u8]) -> bool {
            let s = strip_ansi_lossy(recent_output);
            // Input-box must be visible.
            let has_input_box =
                super::detect::contains_paired(&s, &["Esc to cancel"], &["Tab to amend"]);
            if !has_input_box {
                return false;
            }
            // Permission / chooser must NOT be visible. Same arrow-
            // plus-numbered-options pattern `detect_state` uses for
            // high-confidence Asking detection, plus the trust-folder
            // string Claude shows on first run.
            let has_arrow = s.contains('❯') || has_ascii_chooser_arrow(&s);
            let has_chooser = has_numbered_chooser_options(&s);
            if has_arrow && has_chooser {
                return false;
            }
            // Permission-chooser footer signal — same shape
            // `detect_state` uses to flag Asking when the ASCII arrow
            // sits on option 2/3 instead of option 1.
            if s.contains("Esc to cancel") && has_chooser {
                return false;
            }
            let lower = s.to_lowercase();
            if lower.contains("trust this folder")
                || lower.contains("do you trust the files")
                || lower.contains("do you want to proceed?")
                || lower.contains("do you want to allow")
                || lower.contains("do you want to create")
                || lower.contains("do you want to make this edit")
            {
                return false;
            }
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
            let s = strip_ansi_lossy(recent_output);
            if super::detect::contains_any(&s, super::detect::YN_PROMPT_PATTERNS)
                || super::detect::contains_any(&s, &["approve?"])
            {
                return Some(AgentState::Asking);
            }
            Some(AgentState::Active)
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
            let s = strip_ansi_lossy(recent_output);
            if super::detect::contains_any(&s, super::detect::YN_PROMPT_PATTERNS) {
                return Some(AgentState::Asking);
            }
            Some(AgentState::Active)
        }
    }


    /// Detect Claude's ASCII selection-arrow at ANY numbered option:
    /// `> 0.`–`> 9.` or `> 0)`–`> 9)`.
    ///
    /// Earlier we only matched `> 1.` literally — that quietly missed
    /// permission dialogs whose default selection is option 2
    /// ("Yes, and allow Claude to edit its own settings for this
    /// session") because the cursor renders as `> 2.`, not `> 1.`.
    /// `windows(4)` walks the byte stream once with no allocations.
    /// ASCII-only sentinels (`>`, ` `, digit, `.`, `)`) are safe to
    /// scan against raw bytes — these values never appear inside a
    /// multi-byte UTF-8 sequence.
    fn has_ascii_chooser_arrow(s: &str) -> bool {
        s.as_bytes().windows(4).any(|w| {
            w[0] == b'>' && w[1] == b' ' && w[2].is_ascii_digit() && (w[3] == b'.' || w[3] == b')')
        })
    }

    /// Numbered-chooser shape: at least one `1.` / `1)` AND at least
    /// one `2.` / `2)` in the buffer. The chooser is always followed
    /// by `2.` — that's what distinguishes it from a chat list that
    /// happens to start with `1.`. Used by every Claude detection
    /// branch that needs to know "is there a chooser on screen?"
    /// (arrow + options, `Esc to cancel` + options, and the symmetric
    /// `detect_ready_for_prompt` veto).
    fn has_numbered_chooser_options(s: &str) -> bool {
        (s.contains("1.") || s.contains("1)")) && (s.contains("2.") || s.contains("2)"))
    }

    fn strip_ansi_lossy(bytes: &[u8]) -> String {
        // Filter out ANSI CSI / OSC escape sequences in-place, then
        // UTF-8-decode the remainder. Earlier this function pushed
        // `bytes[i] as char` — that mangled multi-byte UTF-8 glyphs
        // like Claude's choice arrow `❯` (U+276F, 3 bytes) into three
        // separate Latin-1 chars, so any pattern containing the
        // glyph silently failed to match. We need the raw glyph
        // preserved so the patterns can search for it literally.
        let mut filtered: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                let intro = bytes[i];
                i += 1;
                if intro == b'[' {
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                } else if intro == b']' {
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == 0x07 {
                        i += 1;
                    }
                }
                continue;
            }
            filtered.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&filtered).into_owned()
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
            // `contains_any` helper so the GenericCli matcher
            // behaves identically to the built-ins.
            let text = String::from_utf8_lossy(recent_output);
            let refs: Vec<&str> = self.asking_patterns.iter().map(String::as_str).collect();
            if super::detect::contains_any(&text, &refs) {
                Some(AgentState::Asking)
            } else {
                None
            }
        }
    }
}
