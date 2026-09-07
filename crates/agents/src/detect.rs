//! Pure, testable PTY-output detection over `&[u8]`.
//!
//! lazybox wraps every agent in tmux and infers the agent's state by
//! screen-scraping the PTY byte stream. tmux paints by absolute cursor
//! position, so the bytes lazybox sees arrive ANSI-laden and temporally
//! reordered — a single visual line can land as
//! `<cursor-move>❯<cursor-move> <cursor-move>1.<…>Yes`. Detection is
//! therefore inherently heuristic; the only way to keep it honest is to
//! make every step a pure function with no IO so it can be exercised
//! against captured real bytes (see `tests/detect_fixtures.rs`) as well
//! as synthetic strings.
//!
//! Everything here takes raw `&[u8]` (or its `strip_ansi_lossy`'d
//! `&str`) and returns a value — no `self`, no mutation, no clock. The
//! `Agent` impls in [`crate::agent`] are thin wrappers that call into
//! these functions.
//!
//! The one observation hook is level-gated `tracing`: `claude_state_of`
//! emits a single `trace` record of *why* a state was chosen (which
//! branch fired, the recency offsets, the matched consent phrase, a
//! bounded tail of the buffer). It never affects the returned value, so
//! the functions stay pure for testing; flip it on with
//! `RUST_LOG=lazybox_agents=trace` to debug a misclassification against
//! `/tmp/lazybox.log`.

use crate::AuthFailure;
use crate::agent::AgentObservation;
use crate::pty::PromptShape;
use lazybox_ipc::AgentState;

/// Standard bare yes/no prompt markers. Used by every CLI that doesn't
/// have a custom approval UI (Codex, Cursor, most GenericCli configs).
/// Order doesn't matter — substring search.
pub const YN_PROMPT_PATTERNS: &[&str] = &["[y/n]", "(y/n)", "[Y/n]", "[y/N]"];

/// Phrases the Codex TUI renders ONLY inside a blocking approval / consent
/// modal — never in the idle composer or streamed chat. Matched against the
/// space-free buffer (see `compact_lower`) so the cursor-positioned status
/// bar, which arrives with its inter-word gaps stripped, still matches.
/// Lowercase; compared against the lowercased buffer.
///
/// Coverage, by modal (captured from a real `codex` 0.142 session):
///   - command approval → "Would you like to run the following command?"
///   - file-edit approval → "Would you like to make the following edits?"
///     (both collapse to the "would you like to" stem)
///   - any approval modal → "Press enter to confirm or esc to cancel"
///   - directory-trust gate → "Do you trust the contents of this directory?"
///     / "Press enter to continue"
pub const CODEX_PROMPT_PHRASES: &[&str] = &[
    "would you like to",
    "press enter to confirm",
    "press enter to continue",
    "do you trust the contents",
];

/// Credit-exhaustion copy emitted by Codex. A phrase is only classified as
/// blocking when the same screen also exposes the provider's Wait option;
/// prose in the transcript that merely discusses credits remains ordinary
/// output.
pub const CODEX_CREDIT_EXHAUSTED_PHRASES: &[&str] = &[
    "your workspace is out of credits",
    "you've reached your workspace credit limit",
    "you hit your spend cap set in your workspace",
];

const CODEX_WAIT_FOR_CREDIT_PHRASES: &[&str] = &["wait for credit"];

/// Standalone phrases that are unambiguously Claude blocking on user
/// input — no chat context realistically produces them, so matching one
/// is enough confidence to flag `InputNeeded`. Lowercase; matched
/// against the lowercased buffer so phrasing variants ("Do you want to
/// Proceed?" vs "Do you want to proceed?") both fire without expanding
/// the table.
///
/// Each entry maps to one prompt shape. Keep in sync with the
/// fixture-driven test suite so every documented shape has explicit
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
pub const CLAUDE_STANDALONE_PROMPT_PHRASES: &[&str] = &[
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

/// Yes/no choice markers for the paired question + choice branch and
/// the dialog-marker scan — the option labels a Claude approval dialog
/// renders (`1. Yes`) plus the bare `(y/n)` family.
const CLAUDE_CHOICE_MARKERS: &[&str] = &["1. Yes", "1) Yes", "(y/n)", "[y/n]"];

/// Startup interstitials an unattended (`--dangerously-skip-permissions`)
/// spawn CANNOT clear on its own, so the run stalls silently instead of
/// doing the work (issue #256). `--dangerously-skip-permissions` bypasses
/// tool-permission checks but NOT an MCP server's OAuth/login gate, and no
/// human is present to `run /mcp`. Treated as `InputNeeded` so the blocked
/// workspace flashes for attention rather than the auto-fix just dying.
///
/// Phrased singular + plural (`1 MCP server needs` / `2 MCP servers need`).
/// The `MCP server(s) … authentication` stem is distinctive enough that no
/// chat prose realistically produces it; keeping the `needs authentication`
/// half anchors on the operative words even if the styled `⚠`/count prefix
/// or the `· run /mcp` suffix fragments out of the detect window.
pub const CLAUDE_BLOCKING_INTERSTITIAL_PHRASES: &[&str] = &[
    "mcp server needs authentication",
    "mcp servers need authentication",
];

/// Phrases Claude Code renders ONLY when it has hit its provider usage /
/// monthly / weekly limit and paused on the "limit reached — Wait?" prompt
/// (issues #847, #1337). No normal chat output produces these as a live
/// gate, so — like [`CLAUDE_BLOCKING_INTERSTITIAL_PHRASES`] — a single
/// match is enough confidence to classify the distinct
/// [`AgentState::LimitReached`] rather than a generic `InputNeeded`.
/// Lowercase; matched against the space-free buffer so the
/// cursor-positioned banner (whose inter-word gaps arrive as cursor moves
/// rather than space bytes) still matches. The reset countdown Claude
/// prints alongside (`resets 3pm`) is deliberately NOT required here —
/// the wording of the time varies and the banner alone is the block.
///
/// The weekly-limit banner (`You've hit your weekly limit · resets …`)
/// uses "hit" rather than "reached" and a "weekly" period, and drops the
/// user into the `/rate-limit-options` numbered menu — none of which the
/// original three phrases caught, so the block read as a generic chooser.
///
/// The newer spend-limit / session-limit banners (#1452) drop the numbered
/// chooser entirely for an auto-continue footer
/// (`Continuing automatically at 3:10pm · esc to cancel`):
///   `You've hit your individual spend limit · … · your session limit
///    resets 3:10pm (America/New_York)`
///   `Usage limit reached · continuing automatically at 3:10pm · esc or
///    type to cancel`
/// "individual spend limit" / "session limit resets" are neither "usage"
/// nor "weekly"/"monthly", so the earlier phrases missed them. They stay
/// full, banner-specific fragments for the same reason every other phrase
/// here does: a single match flips state to `LimitReached` (which blocks
/// prompt injection and can fire the opt-in auto-`Wait` keystroke), so a
/// bare noun like "spend limit" — ordinary vocabulary for an agent working
/// on billing / cost-cap code — must NOT be a trigger.
///
/// The auto-continue FOOTER (`continuing automatically at 3:10pm · esc or
/// type to cancel`) is matched separately by
/// [`CLAUDE_USAGE_LIMIT_AUTO_CONTINUE_PHRASES`] and routed to the calm
/// [`AgentState::AwaitingReset`] (#1504) — Claude resumes on its own, so
/// pressing auto-`Wait` into that cancel-on-keypress composer would be
/// wrong. Its reset time is recovered in [`parse_usage_limit_reset`] via
/// the `continuing automatically at <time>` fallback.
pub const CLAUDE_USAGE_LIMIT_PHRASES: &[&str] = &[
    "usage limit reached",
    "reached your usage limit",
    "monthly limit reached",
    "hit your usage limit",
    "hit your weekly limit",
    // The per-seat spend cap on a team/enterprise plan (`You've hit your
    // individual spend limit · run /usage-credits to ask your admin …`);
    // `session limit resets` is that banner's own countdown line (#1452).
    "hit your individual spend limit",
    "hit your spend limit",
    "session limit resets",
    "/rate-limit-options",
];

/// The auto-continue form of the usage-limit block: instead of parking on
/// a Wait/Exit chooser, newer Claude Code builds print
/// `Usage limit reached · continuing automatically at 1:10pm · esc or type
/// to cancel` and resume by themselves when the limit resets. Two things
/// distinguish it from [`CLAUDE_USAGE_LIMIT_PHRASES`]:
///
/// - The composer stays alive beneath the banner (typing cancels the
///   wait), so a resting `? for shortcuts` / bypass footer painted after
///   the phrase does NOT mean the banner is stale scrollback. Gating this
///   shape on the resting footer — the way the chooser form must be — is
///   exactly how it went undetected: every such block read as Idle and
///   quiet-settled to `Done`.
/// - Nothing needs pressing, and the agent will pick its work back up on
///   its own, so it classifies straight to the calm
///   [`AgentState::AwaitingReset`] (`💤 parked, will resume`) rather than
///   the alerting `LimitReached` — which would also fire the opt-in
///   auto-Wait keystroke into a composer where a keystroke is the cancel.
///
/// Only a live working anchor painted after the phrase clears it (the
/// agent resumed).
pub const CLAUDE_USAGE_LIMIT_AUTO_CONTINUE_PHRASES: &[&str] =
    &["continuing automatically at", "continuing automatically in"];

/// Best-effort extraction of the reset time Claude prints alongside a
/// usage-limit block (`… resets 3pm`, `… resets at 3:00pm`, `… resets in
/// 2h`) — the "time-to-reset" a proactive usage indicator surfaces
/// (#1012). Returns a short, display-ready hint (`"3pm"`, `"3:00pm"`,
/// `"2h"`) or `None` when the banner carries no parseable time.
///
/// Parsed from the compacted (space-free, lowercased) buffer, the only
/// form that survives tmux's cursor-positioned repaint — the banner's
/// inter-word gaps arrive as cursor moves, not space bytes, so `resets
/// 3pm` reaches lazybox as `resets3pm`. Every reset keyword occurrence is
/// tried and the MOST RECENT (highest offset) one that parses a time wins,
/// so a prior episode's stale `resets 3pm` still in the window can't shadow
/// the current banner's countdown. Recency is compared across ALL keywords,
/// not keyword-first, so an auto-continue tail whose own `resets` line
/// scrolled out (only `continuing automatically at 1:10pm` left) still wins
/// over an older `resets`. The chooser's own "Wait until it resets" line
/// also contains the word but is followed by a newline, not a digit, so it
/// never captures. Deliberately
/// conservative — only a leading digit run plus `:` and the am/pm + h/m/s/d
/// time letters, stopping at the first byte outside that set — so the
/// trailing `∙` / newline / `❯ 1. wait` never bleeds in, and an
/// unrecognised phrasing (`resets tomorrow`) yields `None` rather than a
/// garbled hint. The auto-continue banner carries no `resets` word — it
/// states the reset as `continuing automatically at 3:10pm` — so the
/// specific `continuing automatically at/in <time>` keywords are tried as
/// a fallback. The block still surfaces without a countdown: the documented
/// degraded path where no usage API exists.
pub fn parse_usage_limit_reset(recent_output: &[u8]) -> Option<String> {
    let s = strip_ansi_lossy(recent_output);
    let compact = compact_lower(&s);
    // Only meaningful under a live limit banner — never mine a stray
    // "resets" out of ordinary scrollback. Matched against the space-free
    // buffer (patterns compacted the same way), so the cursor-positioned
    // banner still matches.
    last_compact_match_pos(&compact, CLAUDE_USAGE_LIMIT_PHRASES)
        .or_else(|| last_compact_match_pos(&compact, CLAUDE_USAGE_LIMIT_AUTO_CONTINUE_PHRASES))?;
    // Prefer the banner's own `resets 3pm` countdown, but fall back to the
    // auto-continue form's `continuing automatically at 1:10pm` (same reset)
    // for a window holding only that line — the spend-limit line above it
    // scrolled out. Across all three keywords, the MOST RECENT match that
    // parses a time wins (max offset), so neither an older episode's stale
    // `resets` nor the chooser's token-less "Wait until it resets" line can
    // shadow the live banner's countdown. The specific `continuing
    // automatically at/in` keywords are used (not a bare `automatically`) so
    // ordinary prose like "the deploy continues automatically 2h after
    // merge" can never be mined as a reset time.
    let compact = compact.as_str();
    [
        "resets",
        "continuingautomaticallyat",
        "continuingautomaticallyin",
    ]
    .into_iter()
    .flat_map(|keyword| {
        compact.rmatch_indices(keyword).filter_map(move |(i, kw)| {
            reset_token(&compact[i + kw.len()..]).map(|token| (i, token))
        })
    })
    .max_by_key(|(i, _)| *i)
    .map(|(_, token)| token)
}

/// Month abbreviations a date-style reset leads with (`resets Aug 30 at
/// 2pm`), which the compacted buffer delivers as `resetsaug30at2pm`.
const MONTH_ABBREVS: &[&str] = &[
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// The reset hint immediately after a `resets` keyword, or `None` when
/// what follows is neither a clock time nor a calendar date. Skips an
/// `at`/`in` connective (`resets at 3pm`, `resets in 2h`).
fn reset_token(after: &str) -> Option<String> {
    let after = after
        .strip_prefix("at")
        .or_else(|| after.strip_prefix("in"))
        .unwrap_or(after);
    time_token(after).or_else(|| date_token(after))
}

/// A digit-led clock token (`3pm`, `3:00pm`, `2h`) at the start of `after`.
/// Deliberately conservative — a leading digit run plus `:` and the am/pm +
/// h/m/s/d time letters, stopping at the first byte outside that set — so a
/// trailing `∙` / newline / `❯ 1. wait` never bleeds in.
fn time_token(after: &str) -> Option<String> {
    let token: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, ':' | 'a' | 'p' | 'm' | 'h' | 's' | 'd'))
        .take(12)
        .collect();
    (token.len() >= 2 && token.starts_with(|c: char| c.is_ascii_digit())).then_some(token)
}

/// A calendar-date reset (`resets Aug 30 at 2pm` → `resetsaug30at2pm`) —
/// the weekly-limit banner's form, which `time_token` can't read because it
/// leads with a month name, not a digit (#1337). Emits a spaced,
/// display-ready hint (`aug 30 at 2pm`); the trailing clock time is
/// optional, so `resets Aug 30` still yields `aug 30`.
fn date_token(after: &str) -> Option<String> {
    let month = MONTH_ABBREVS.iter().find(|m| after.starts_with(**m))?;
    // Tolerate a full month name (`august`) after the 3-letter anchor.
    let rest = after[month.len()..].trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let day: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .take(2)
        .collect();
    if day.is_empty() {
        return None;
    }
    let mut hint = format!("{month} {day}");
    let rest = rest[day.len()..]
        .strip_prefix("at")
        .or_else(|| rest[day.len()..].strip_prefix("in"))
        .unwrap_or(&rest[day.len()..]);
    if let Some(time) = time_token(rest) {
        hint.push_str(" at ");
        hint.push_str(&time);
    }
    Some(hint)
}

/// Substring "any-of" match. Plain text in; bytes should be passed
/// through [`strip_ansi_lossy`] first so escape sequences don't split
/// the markers.
pub fn contains_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| text.contains(p))
}

/// Two-stage check: at least one `choice` marker AND at least one
/// `question` phrase. Pairing raises confidence — neither alone is
/// enough to distinguish "agent is asking" from "the agent's chat
/// output mentions the same phrase."
pub fn contains_paired(text: &str, choices: &[&str], questions: &[&str]) -> bool {
    contains_any(text, choices) && contains_any(text, questions)
}

pub fn codex_auth_failure(recent_output: &[u8]) -> Option<AuthFailure> {
    let text = strip_ansi_lossy(recent_output).to_ascii_lowercase();
    let tail = recent_tail(&text, 8 * 1024);
    // "access token could not be refreshed" is distinctive enough that the
    // exact re-auth directive can vary — Codex prints "please sign in again"
    // and "please log out and sign in again". Anchor on the loose "sign in
    // again" (covers both) so a directive-wording tweak upstream can't
    // silently defeat the in-place re-auth.
    let refresh_rejected =
        tail.contains("access token could not be refreshed") && tail.contains("sign in again");
    let login_required = tail.contains("not logged in. run `codex login`")
        || tail.contains("not logged in. run codex login")
        || tail.contains("not authenticated. run `codex login`")
        || tail.contains("not authenticated. run codex login");
    let provider_rejected = (tail.contains("authentication failed")
        || tail.contains("authentication error"))
        && tail.contains("sign in again")
        && (tail.contains("codex") || tail.contains("chatgpt"));
    (refresh_rejected || login_required || provider_rejected).then_some(AuthFailure {
        reason: "Codex authentication is no longer valid.",
    })
}

pub fn claude_auth_failure(recent_output: &[u8]) -> Option<AuthFailure> {
    let text = strip_ansi_lossy(recent_output).to_ascii_lowercase();
    let tail = recent_tail(&text, 8 * 1024);
    let login_required = tail.contains("not authenticated. run `claude auth login`")
        || tail.contains("not authenticated. run claude auth login")
        || tail.contains("not authenticated. run /login")
        || tail.contains("not logged in. run `claude auth login`")
        || tail.contains("not logged in. run claude auth login")
        || tail.contains("not logged in. run /login");
    let oauth_expired = (tail.contains("oauth") || tail.contains("authentication"))
        && (tail.contains("expired") || tail.contains("invalid"))
        && (tail.contains("please log in again")
            || tail.contains("please login again")
            || tail.contains("please run /login")
            || tail.contains("please run `claude auth login`")
            || tail.contains("please run claude auth login"));
    let startup_failure = (tail.contains("authentication failed")
        || tail.contains("authentication error"))
        && (tail.contains("run /login") || tail.contains("claude auth login"));
    (login_required || oauth_expired || startup_failure).then_some(AuthFailure {
        reason: "Claude Code authentication is no longer valid.",
    })
}

/// Claude Code's three observable states, in one detector. Ordered
/// most-specific first; the priority is deliberate — a live permission
/// chooser outranks the working status line, which outranks the quiet
/// input box.
///
/// - **`InputNeeded`** — ONLY structural prompt markers, split into two
///   confidence tiers (see `classify`): WEAK chooser shapes (selection
///   arrow + numbered options, or an `Esc to cancel` footer + options)
///   gated on the full composer footer, and STRONG phrase shapes (a
///   standalone `do you want to …` consent phrase, or a paired
///   question + yes/no) gated only on the RELIABLE resting footer so a
///   live command-approval dialog — whose footer carries the otherwise
///   composer-like `Tab to amend` — still fires. Freeform conversational
///   asks ("Want me to …?", a line that merely ends in `?`) are NOT
///   flagged — they were the dominant false-positive source and fired
///   spurious desktop notifications.
/// - **`Working`** — Claude paints a live status line ONLY while busy:
///   `✦ Gusting… (2m 2s · ↓ 7.2k tokens · …)`. Its presence is the
///   reliable "working" pulser — far more robust than the old
///   `esc to interrupt`-only match, which the newer phase suffix
///   sometimes replaces.
/// - **`Idle`** — the input box is drawn with nothing pending, or the
///   output is plain non-interactive text.
///
/// Returning `Some(_)` on every path (rather than `None`) lets the
/// daemon notice every transition between the three states; without it
/// the cached state would stick at its last value.
pub fn claude_state(recent_output: &[u8]) -> Option<AgentState> {
    // Always `Some`. Strip + compact once and hand both forms to the
    // classifier; every path classifies, so the caller can diff against
    // the cached state to notice transitions.
    let s = strip_ansi_lossy(recent_output);
    Some(claude_state_of(&s, &compact_lower(&s), None))
}

/// Chunk-aware variant of [`claude_state`]. `last_chunk_start` is the
/// byte offset within `recent_output` where the most recent PTY chunk
/// begins (the daemon's rolling detect buffer is older chunks followed
/// by the chunk that just arrived).
///
/// Why the hint matters: the classifier suppresses a dialog marker that
/// sits ABOVE a live working status anchor — positionally "the agent
/// already moved past the prompt." But a full-screen repaint (tmux
/// redraws top-to-bottom) delivers a live dialog AND the bottom status
/// bar in ONE chunk, with the status bar last. Position alone then
/// misreads the live dialog as stale. When both the prompt marker and
/// the work anchor land inside the most recent chunk, the work anchor
/// is not allowed to suppress the dialog.
pub fn claude_state_chunked(recent_output: &[u8], last_chunk_start: usize) -> Option<AgentState> {
    let mark = last_chunk_start.min(recent_output.len());
    let (s, s_mark) = strip_ansi_lossy_marked(recent_output, mark);
    let (compact, compact_mark) = compact_lower_marked(&s, s_mark);
    Some(claude_state_of(&s, &compact, Some(compact_mark)))
}

/// Classify Claude's state from an already-stripped buffer `s` and its
/// space-free form `compact` (see [`compact_lower`]), then emit one
/// `trace`-level record of why. Returns `AgentState` directly (never the
/// outer `Option`): every path classifies. Both forms are passed in so
/// the public entry points build them once and share them — this runs on
/// every PTY chunk.
///
/// Why two forms:
/// - `compact` (lowercased, spaces removed) is what every footer /
///   status-bar marker is matched against. tmux/Claude paint the bottom
///   status bar by absolute cursor position, so a footer phrase
///   (`? for shortcuts`, `Esc to cancel`, the `esc to interrupt` hint)
///   reaches lazybox as `?forshortcuts` / `esctocancel` / `esctointerrupt`
///   — the inter-word gaps are cursor moves, not space bytes — while the
///   SAME phrase in scrollback keeps its spaces. Comparing the space-free
///   form matches either rendering; the spaced literal silently never
///   matched the live footer, which is why readiness once never signalled
///   and the injector rode its 10s deadline. All recency offsets are
///   computed in `compact` so they're directly comparable.
/// - raw `s` is kept only for the arrow scan, whose ASCII form (`> 1.`)
///   depends on the space `compact` strips.
///
/// `last_chunk_start` is the chunk-boundary hint in `compact`'s offset
/// space (see [`claude_state_chunked`]); `None` keeps the pure
/// positional rules.
fn claude_state_of(s: &str, compact: &str, last_chunk_start: Option<usize>) -> AgentState {
    let decision = classify(s, compact, last_chunk_start);
    decision.log(compact);
    decision.state
}

/// Which branch of [`classify`] selected `InputNeeded`. Captured so the
/// decision is loggable and unit-testable without a `tracing`
/// subscriber; `None` for the `Working`/`Idle` outcomes (their reason is
/// the state itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// A startup blocker an unattended spawn can't clear — an MCP server
    /// awaiting interactive auth (`CLAUDE_BLOCKING_INTERSTITIAL_PHRASES`).
    BlockingInterstitial,
    /// A provider usage / monthly-limit block
    /// (`CLAUDE_USAGE_LIMIT_PHRASES`) — the distinct `LimitReached` state.
    UsageLimit,
    /// The auto-continue form of the limit block
    /// (`CLAUDE_USAGE_LIMIT_AUTO_CONTINUE_PHRASES`) — Claude parks itself
    /// and resumes at reset, so the calm `AwaitingReset`.
    UsageLimitAutoContinue,
    /// Selection arrow + numbered options, more recent than the composer.
    StructuralChooser,
    /// `Esc to cancel` permission footer + numbered options.
    EscToCancelFooter,
    /// A [`CLAUDE_STANDALONE_PROMPT_PHRASES`] consent phrase.
    StandalonePhrase,
    /// Bare yes/no: a choice marker paired with a question phrase.
    PairedYesNo,
}

/// Full classification result: the chosen state plus the recency offsets
/// and matched markers that drove it. [`classify`] returns it so
/// [`claude_state_of`] can both hand back the state and emit a single
/// `trace` record of *why* (issue #2: instrument the decision before
/// tweaking the heuristic). Tests assert on `trigger` directly.
struct Decision {
    state: AgentState,
    trigger: Option<Trigger>,
    /// Composer footer offset gating the WEAK chooser shapes — includes
    /// the ambiguous `Tab to amend`. See [`idle_box_pos`].
    idle_pos: Option<usize>,
    /// RELIABLE end-of-turn anchor gating the STRONG phrase shapes —
    /// `? for shortcuts` / bypass only. See [`resting_composer_pos`].
    resting_pos: Option<usize>,
    chooser_pos: Option<usize>,
    choice_pos: Option<usize>,
    question_pos: Option<usize>,
    work_pos: Option<usize>,
    phrase_pos: Option<usize>,
    matched_phrase: Option<&'static str>,
}

impl Decision {
    /// Emit the decision at `trace` (off in normal runs; flip on with
    /// `RUST_LOG=lazybox_agents=trace`). `tracing` evaluates the field
    /// expressions only when the callsite is enabled, so the bounded
    /// `tail` slice is built solely while debugging.
    fn log(&self, compact: &str) {
        tracing::trace!(
            state = ?self.state,
            trigger = ?self.trigger,
            idle_pos = ?self.idle_pos,
            resting_pos = ?self.resting_pos,
            chooser_pos = ?self.chooser_pos,
            choice_pos = ?self.choice_pos,
            question_pos = ?self.question_pos,
            work_pos = ?self.work_pos,
            phrase_pos = ?self.phrase_pos,
            matched_phrase = ?self.matched_phrase,
            tail = %compact_tail(compact),
            "claude_state_of",
        );
    }
}

/// The classifier. Ordered most-specific first; the priority is
/// deliberate. The branches split into two confidence tiers that gate on
/// DIFFERENT end-of-turn anchors — the crux of keeping the `?` honest:
///
/// - **WEAK shapes** (a bare selection arrow + numbered options, or an
///   `Esc to cancel` footer + numbered options) gate on [`idle_box_pos`],
///   which includes the `Tab to amend` composer footer. A numbered list
///   the agent typed into its prose, or a multi-line prompt lazybox
///   injected into the composer, satisfies "arrow + 1./2." but is NOT a
///   live chooser — the `Tab to amend` footer below it proves the
///   composer is in control, so these stay `Idle`.
/// - **STRONG shapes** (a standalone `do you want to …` consent phrase,
///   or a paired question + yes/no) gate on [`resting_composer_pos`] —
///   `? for shortcuts` / bypass ONLY, never `Tab to amend`. These phrases
///   never appear in an idle composer or injected prose, so the only
///   reason to suppress one is a RELIABLE end-of-turn footer above which
///   it's stale scrollback (#191). A real Claude command-approval dialog
///   renders the `Esc to cancel · Tab to amend · ctrl+e to explain`
///   footer — which carries `Tab to amend` — so gating these on
///   `idle_box_pos` (as the chooser shapes do) wrongly read the live
///   `Do you want to proceed?` prompt as stale and dropped the `?`
///   entirely. Bash approvals always carry a consent phrase, so the
///   strong tier catches them regardless of that ambiguous footer.
fn classify(s: &str, compact: &str, last_chunk_start: Option<usize>) -> Decision {
    let idle_pos = idle_box_pos(compact);
    let resting_pos = resting_composer_pos(compact);
    let chooser_pos = chooser_pos(compact);
    let work_pos = working_status_pos(compact);

    // Same-chunk rule. tmux full repaints paint the screen top-to-
    // bottom, so a live dialog and the bottom status bar land in ONE
    // chunk with the work anchor LAST — position alone would read the
    // dialog as already answered. When both the prompt marker and the
    // work anchor arrived inside the most recent chunk, the work anchor
    // must not suppress the dialog. `None` (no chunk hint) keeps the
    // pure positional ordering rule.
    let work_anchor_against =
        |marker: Option<usize>| work_anchor_for(marker, work_pos, last_chunk_start);

    // A live chooser / permission dialog REPLACES the input box, so its
    // markers are the bottom-most thing on screen. When the composer
    // footer is MORE RECENT (further down the buffer) than the chooser
    // markers, the arrow + numbered shape is scrollback — a parked `❯`
    // prompt above an agent's prose `1.`/`2.` list, or an injected
    // multi-line prompt sitting in the composer — NOT a live chooser.
    //
    // The live working status line is an end-of-prompt anchor too: tmux
    // repaints the `esc to interrupt` / token-counter line continuously
    // while the agent is busy, but a static prompt footer is never
    // re-sent. A prompt marker ABOVE the working anchor is therefore an
    // already-answered prompt the agent has moved past, not a live gate
    // — without this, the stale marker pins InputNeeded for as long as
    // it stays in the detect window. (Subject to the same-chunk rule
    // above: a full repaint delivers both in one chunk.)
    // A live counter line — spinner glyph + a ticking elapsed timer +
    // token counter (see `is_live_counter_line`) — is painted ONLY while
    // the agent is actively working; a blocking dialog freezes that timer,
    // so Claude never renders one beneath a live prompt. When such a line
    // is MORE RECENT than the chooser markers, the agent has moved past
    // any numbered shape above it: the "chooser" is scrollback (a git log,
    // a numbered prose list), not a live gate. Unlike the generic work
    // anchor this is NOT relaxed by the same-chunk rule — a startup full
    // repaint delivers the scrollback and the ticking status line together
    // (#96), and the ticking line must still win over the weak shapes. The
    // STRONG consent-phrase / paired-yes-no tiers keep the same-chunk rule,
    // so a real dialog repainted alongside a frozen status preview is
    // unaffected.
    let live_counter_pos = last_line_pos(compact, is_live_counter_line);
    let chooser_live = marker_at_least_as_recent(
        chooser_pos,
        idle_pos
            .max(work_anchor_against(chooser_pos))
            .max(live_counter_pos),
    );

    // tmux paints the screen by absolute cursor position, so the bytes
    // lazybox sees arrive in TEMPORAL order: a single visual line
    // `❯ 1. Yes` can land in the buffer as
    // `<cursor-move>❯<cursor-move> <cursor-move>1.<…>Yes`, and strip_ansi
    // removes the CSI runs but NOT the absolute positioning gap that may
    // be filled with other content. So strict substring matching for
    // `❯ 1.` fails even when the prompt is visually on screen — the
    // matcher pairs the arrow `❯` ANYWHERE with a numbered-choice shape
    // (`1.`/`1)` AND `2.`/`2)`). The required second option filters chat
    // lists that merely start with `1.`. The ASCII arrow is accepted on
    // any option (`> 1.`, `> 2.`, …) since j/k moves the cursor.
    let has_arrow = s.contains('❯') || has_ascii_chooser_arrow(s);
    let has_chooser = has_numbered_chooser_options(s);

    let (phrase_pos, matched_phrase) =
        match last_compact_match(compact, CLAUDE_STANDALONE_PROMPT_PHRASES) {
            Some((pos, phrase)) => (Some(pos), Some(phrase)),
            None => (None, None),
        };
    let choice_pos = last_compact_match_pos(compact, CLAUDE_CHOICE_MARKERS);
    let question_pos = last_compact_match_pos(
        compact,
        &[
            "Do you want",
            "Allow Claude",
            "Allow Bash",
            "Approve",
            "Continue?",
            "Proceed?",
        ],
    );

    let mut d = Decision {
        state: AgentState::Idle,
        trigger: None,
        idle_pos,
        resting_pos,
        chooser_pos,
        choice_pos,
        question_pos,
        work_pos,
        phrase_pos,
        matched_phrase,
    };

    // When the detect window carries NO recency anchor at all — no
    // composer footer (idle or resting) and no live working line — a
    // bare arrow + numbered list can't be positively placed as the
    // bottom-most live dialog. `chooser_live` is vacuously true in that
    // case (nothing to out-rank), so an agent merely PRINTING prose like
    // "Here are the options:\n1. …\n2. …\n❯ pick one" would read as a
    // live gate. Require a corroborating dialog signal before trusting
    // the weak arrow+list shape with no anchor to lean on: a consent /
    // question phrase, a Yes/no choice marker, the Esc-to-cancel footer,
    // OR the structural tell that the selection arrow sits DIRECTLY on a
    // numbered option (`❯ 1.` / `> 1.`) — a real chooser shape that prose
    // ("❯ pick one") never produces. The last one rescues a chooser with
    // custom option labels whose other chrome fragmented out of the
    // window.
    let has_recency_anchor = idle_pos.is_some() || resting_pos.is_some() || work_pos.is_some();
    let corroborated = question_pos.is_some()
        || choice_pos.is_some()
        || compact.contains("esctocancel")
        || arrow_on_numbered_option(s);

    // A startup interstitial an unattended spawn can't clear (an MCP
    // server needing interactive auth) outranks every prompt shape: the
    // agent is wedged before it can do any work. Unlike the consent
    // phrases this is NOT gated on the composer footer — the warning
    // renders ALONGSIDE the drawn composer at startup, so a more-recent
    // `? for shortcuts` doesn't mean it was cleared. Only a live working
    // anchor painted after it (the agent streaming = it got past the gate)
    // suppresses it, keeping a resolved-then-working session from pinning
    // InputNeeded (issue #256).
    let blocker_pos = last_compact_match_pos(compact, CLAUDE_BLOCKING_INTERSTITIAL_PHRASES);
    if marker_at_least_as_recent(blocker_pos, work_anchor_against(blocker_pos)) {
        d.state = AgentState::InputNeeded;
        d.trigger = Some(Trigger::BlockingInterstitial);
        return d;
    }

    // A provider usage-limit block (#847) is a distinct blocked state, not
    // a generic prompt: classify it as `LimitReached` so it gets its own
    // pill / jump / filter / bulk-resume. Placed BEFORE the chooser and
    // consent branches — Claude's limit prompt itself renders a numbered
    // "Wait / Exit" chooser, which would otherwise read as a plain
    // `InputNeeded`.
    //
    // Gated like the STRONG consent phrases (`resting_pos`), NOT like the
    // interstitial (work-anchor only): the limit prompt is a blocking
    // dialog that REPLACES the composer, so a live block has no resting
    // `? for shortcuts` / bypass footer beneath it. When the same phrase
    // sits ABOVE a redrawn resting footer it's stale scrollback — the
    // agent finished a turn whose output merely MENTIONED a usage limit
    // ("you've reached your usage limit before …") — and must stay Idle.
    // Erring toward this false-negative rather than a false-positive
    // matters here because a positive can fire the opt-in auto-`Wait`
    // keystroke; a stray keystroke into an idle composer is worse than
    // missing a block the user can still act on manually. A live working
    // anchor painted after the phrase suppresses it the same way (the
    // agent resumed = the block cleared).
    // The auto-continue form of the same block (`Usage limit reached ·
    // continuing automatically at 1:10pm · esc or type to cancel`) parks
    // itself and resumes at reset, so it is the calm `AwaitingReset`, not
    // the alerting chooser state. Gated like the interstitial (work anchor
    // ONLY): the composer stays live beneath this banner — typing is the
    // cancel — so Claude keeps its resting footer painted under it, and
    // gating on `resting_pos` is precisely why this shape used to read as
    // stale scrollback and quiet-settle to `Done`. Placed before the
    // chooser form so the shared "usage limit reached" prefix never
    // classifies this banner as `LimitReached` (which would fire the
    // opt-in auto-Wait keystroke into the cancel-on-keypress composer).
    let auto_continue_pos =
        last_compact_match_pos(compact, CLAUDE_USAGE_LIMIT_AUTO_CONTINUE_PHRASES);
    if marker_at_least_as_recent(auto_continue_pos, work_anchor_against(auto_continue_pos)) {
        d.state = AgentState::AwaitingReset;
        d.trigger = Some(Trigger::UsageLimitAutoContinue);
        return d;
    }

    let limit_pos = last_compact_match_pos(compact, CLAUDE_USAGE_LIMIT_PHRASES);
    if marker_at_least_as_recent(limit_pos, resting_pos.max(work_anchor_against(limit_pos))) {
        d.state = AgentState::LimitReached;
        d.trigger = Some(Trigger::UsageLimit);
        return d;
    }

    // WEAK: arrow + numbered options, gated on the full composer footer
    // (incl. `Tab to amend`) so injected prose / parked prompts stay Idle.
    if has_arrow && has_chooser && chooser_live && (has_recency_anchor || corroborated) {
        d.state = AgentState::InputNeeded;
        d.trigger = Some(Trigger::StructuralChooser);
        return d;
    }

    // WEAK: `Esc to cancel` permission footer + numbered options — the
    // arrow-less dialog shape (older builds, or the arrow fragmented out
    // of the detect window). Same `idle_box_pos` recency gate.
    if compact.contains("esctocancel") && has_chooser && chooser_live {
        d.state = AgentState::InputNeeded;
        d.trigger = Some(Trigger::EscToCancelFooter);
        return d;
    }

    // STRONG: a standalone consent phrase, gated on the RELIABLE resting
    // footer only. Such a phrase never appears in an idle composer or
    // injected prose, so the sole reason to suppress it is a genuine
    // end-of-turn footer (`? for shortcuts` / bypass) below which it's
    // stale scrollback (#191) — or a live working anchor painted after
    // it, which proves the agent already moved past the prompt. It must
    // NOT be gated by `Tab to amend`, which the live command-approval
    // dialog itself renders.
    if marker_at_least_as_recent(phrase_pos, resting_pos.max(work_anchor_against(phrase_pos))) {
        d.state = AgentState::InputNeeded;
        d.trigger = Some(Trigger::StandalonePhrase);
        return d;
    }

    // STRONG: a question phrase paired with a yes/no choice marker — the
    // arrow-less bash/approval shape whose options are `1. Yes` / `(y/n)`.
    // Both must be present, and the more-recent of the two at least as
    // recent as the reliable resting footer.
    let paired_pos = choice_pos.max(question_pos);
    if choice_pos.is_some()
        && question_pos.is_some()
        && marker_at_least_as_recent(paired_pos, resting_pos.max(work_anchor_against(paired_pos)))
    {
        d.state = AgentState::InputNeeded;
        d.trigger = Some(Trigger::PairedYesNo);
        return d;
    }

    // Working: Claude paints a live status line ONLY while busy. The
    // append-only buffer still holds a finished agent's last status line,
    // but Claude redraws the composer footer AFTER it — so treat the
    // agent as working only when the status line is the more recent of
    // the two. Gated on `idle_box_pos` (incl. `Tab to amend`): a stale
    // `esc to interrupt` under a redrawn composer footer must read Idle,
    // not forever-busy.
    if let Some(work_pos) = work_pos
        && idle_pos.is_none_or(|ip| work_pos > ip)
    {
        d.state = AgentState::Working;
        return d;
    }

    // Background shells outlive the model turn: after the turn's
    // `esc to interrupt` status is gone, Claude keeps painting
    // `✻ Crunched for 2m 44s · 4 shells still running` while shells the
    // agent launched during the turn keep executing. That is real work in
    // flight — the agent is not `Done` — but the resting composer footer
    // beneath it would otherwise read as finished and let the quiet timer
    // settle it to `Done` mid-run (#1136). Treat it as `Working` until the
    // shells drain and the line disappears.
    //
    // Liveness is judged by STATUS-LINE RECENCY — the same idiom every
    // other branch uses — adapted to a status line that, unlike
    // `esc to interrupt`, renders ABOVE its own composer footer (so a bare
    // positional compare against the footer never fires). A shells line is
    // live only when:
    //   1. it is the most recent status frame — no NEWER spinner-glyph line
    //      has been painted over it. A drain either repaints a different
    //      `✻ …` status (compacting, a summary) or, failing that, redraws
    //      the composer; the former is caught here, and
    //   2. at most its own composer footer sits below it. When the shells
    //      finish and Claude redraws the resting composer with no new status
    //      line, a SECOND `? for shortcuts` stacks below the now-stale line
    //      and this guard settles it.
    // Placed after every `InputNeeded` branch, so a shell awaiting approval
    // (or any real prompt) still wins.
    //
    // Residual (the one case neither guard separates): a shells line that
    // was re-painting BELOW its footer, then drained straight to rest with
    // no fresh footer and no newer status line, leaves a byte window
    // identical to a live single-frame shells line. We take the #1136-safe
    // side and stay Working; the stale line settles once it scrolls out of
    // the ~16 KiB detect window.
    if let Some(shells_pos) = last_line_pos(compact, is_shells_running_line)
        && last_line_pos(compact, is_spinner_status_line) == Some(shells_pos)
        && resting_footers_after(compact, shells_pos) <= 1
    {
        d.state = AgentState::Working;
        return d;
    }

    // Nothing pending and not streaming: the input box is drawn and
    // quiet, or the output is plain non-interactive text.
    d
}

/// Claude is ready to receive a pasted prompt when its input box (the
/// composer) is drawn AND no permission chooser / Y-N gate / folder-
/// trust dialog is up.
///
/// This reuses the [`claude_state`] model rather than re-deriving the
/// prompt markers: readiness is "the composer is visible and the state
/// isn't `InputNeeded`." Deriving it from the shared state model is what
/// fixes the spawn-time injection delay — the old check required the
/// FULL idle footer (`Esc to cancel` AND `Tab to amend` paired), which
/// silently never matched the newer composer footer (`? for shortcuts`
/// alone) or a banner mid-render, so the ready signal never fired and
/// the injector rode its hard deadline. Keying off "composer drawn + not
/// asking" fires the moment Claude is genuinely idle at the prompt.
///
/// Returning true means "paste arrives in the input buffer, not as a
/// Y/N answer." Returning false means "wait — the banner isn't done OR a
/// gate is up."
pub fn claude_ready_for_prompt(recent_output: &[u8]) -> bool {
    // Strip + compact once and share both with the state classifier —
    // this runs on every output chunk while the spawn-time injector polls
    // for readiness, so re-stripping or re-compacting the (up to 16 KiB)
    // buffer is wasted work on a hot path.
    let s = strip_ansi_lossy(recent_output);
    let compact = compact_lower(&s);
    // A live chooser / permission / standalone-consent prompt means a
    // paste would be eaten by the gate. The state model already
    // recognises every one of those shapes. Use `classify` directly (not
    // `claude_state_of`) so the spawn-time injector's readiness polling
    // doesn't double-emit the decision trace on every chunk.
    let decision = classify(&s, &compact, None);
    // `AwaitingReset` too: under the auto-continue banner the composer is
    // live but any keystroke CANCELS the wait, so a paste there would
    // abort the parked work rather than queue behind it.
    if matches!(
        decision.state,
        AgentState::InputNeeded
            | AgentState::LimitReached
            | AgentState::CreditExhausted
            | AgentState::AwaitingReset
    ) {
        return false;
    }
    // Folder-trust prompts don't always render as a numbered chooser
    // (older builds, alt phrasings), so `claude_state` can read them as
    // Idle. Veto explicitly — pasting the work prompt into the trust
    // dialog is the original "y eats my prompt" race. Matched space-free
    // like every other footer phrase.
    if compact.contains("trustthisfolder") || compact.contains("doyoutrustthefiles") {
        return false;
    }
    // The composer must actually be on screen. The boot banner is also
    // `Idle`, so requiring a composer footer marker excludes it — we
    // don't want to paste into the banner before the input box exists.
    // `idle_pos` is exactly that footer offset (`idle_box_pos`: `Tab to
    // amend` / `? for shortcuts` / bypass `shift+tab to cycle`) the
    // classifier already computed above, so reuse it rather than re-scan
    // the buffer for the same three markers on this per-chunk hot path.
    decision.idle_pos.is_some()
}

/// Recognize only startup consent gates whose affirmative answer was already
/// authorized by an unattended launch. This deliberately excludes ordinary
/// tool approvals and free-text questions: the fallback nudge is for a failed
/// one-time trust/consent seed, never a general auto-approve mechanism.
pub fn claude_unattended_startup_nudge(
    recent_output: &[u8],
) -> Option<crate::agent::UnattendedPromptNudge> {
    use crate::agent::{UnattendedPromptKind, UnattendedPromptNudge};

    let screen = strip_ansi_lossy(recent_output);
    let compact = compact_lower(&screen);
    let trust = last_compact_match_pos(
        &compact,
        &[
            "do you trust the files in this folder",
            "do you trust this folder",
        ],
    );
    let bypass = last_compact_match_pos(
        &compact,
        &["bypass permissions mode", "bypass permissions warning"],
    );
    let (marker, kind) = match (trust, bypass) {
        (Some(trust), Some(bypass)) if bypass > trust => {
            (Some(bypass), UnattendedPromptKind::PermissionBypassConsent)
        }
        (Some(trust), _) => (Some(trust), UnattendedPromptKind::WorkspaceTrust),
        (_, Some(bypass)) => (Some(bypass), UnattendedPromptKind::PermissionBypassConsent),
        _ => return None,
    };

    // A newer idle/working anchor proves this phrase is scrollback. The
    // revalidation immediately before writing runs this same test against a
    // fresh backend snapshot, closing the detect→answer race.
    let cleared = idle_box_pos(&compact).max(working_status_pos(&compact));
    if !marker_at_least_as_recent(marker, cleared) {
        return None;
    }
    let marker = marker.expect("matched startup marker");
    let tail = &compact[marker..];
    let affirmative_chooser =
        tail.contains("1.yes") && (tail.contains("2.no") || tail.contains("esctocancel"));
    affirmative_chooser.then_some(UnattendedPromptNudge {
        kind,
        response: b"1",
    })
}

/// Whether a `Working` reading carries on-screen proof the agent moved
/// PAST a dialog: an affirmative live status anchor (see
/// `working_status_pos`) strictly more recent than the most recent
/// dialog marker still in the buffer.
///
/// `false` when there's no working anchor — or when the buffer holds no
/// dialog marker at all. The caller is the daemon's stale-hook
/// fallback: a dialog on screen BLOCKS Claude's hook stream (no tool
/// calls fire while it waits), so "hooks stale + cached `InputNeeded`"
/// is the normal shape of a real unanswered dialog, not a broken
/// pipeline. Demoting that `?` needs evidence the dialog was answered —
/// activity painted after its markers — not just a Working
/// classification over a marker-free window.
pub fn claude_working_supersedes_dialog(recent_output: &[u8]) -> bool {
    let s = strip_ansi_lossy(recent_output);
    let compact = compact_lower(&s);
    let Some(work) = working_status_pos(&compact) else {
        return false;
    };
    dialog_marker_pos(&compact).is_some_and(|marker| work > marker)
}

/// Most recent dialog-shaped marker in the compacted buffer: chooser
/// markers, a standalone consent phrase, a yes/no choice marker, or the
/// `Esc to cancel` dialog footer. `None` when nothing dialog-shaped is
/// in the window.
fn dialog_marker_pos(compact: &str) -> Option<usize> {
    [
        chooser_pos(compact),
        last_compact_match_pos(compact, CLAUDE_STANDALONE_PROMPT_PHRASES),
        last_compact_match_pos(compact, CLAUDE_CHOICE_MARKERS),
        last_compact_match_pos(compact, CLAUDE_USAGE_LIMIT_PHRASES),
        last_compact_match_pos(compact, CLAUDE_USAGE_LIMIT_AUTO_CONTINUE_PHRASES),
        compact.rfind("esctocancel"),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Lowercased copy of `s` with ASCII spaces removed. tmux/Claude render
/// the bottom status bar by absolute cursor position, so a footer phrase
/// reaches lazybox with its inter-word gaps as cursor moves rather than
/// space bytes (`? for shortcuts` → `?forshortcuts`), while the same
/// phrase printed into scrollback keeps its spaces. Matching against
/// this form catches a marker in either rendering. Newlines are
/// preserved so the per-line working-counter scan still sees line
/// boundaries.
fn compact_lower(s: &str) -> String {
    compact_lower_marked(s, s.len()).0
}

/// [`compact_lower`] that also translates one byte offset (`mark`, in
/// `s`'s offset space) into the compacted output's offset space —
/// returns `(compacted, compacted_mark)`. Used to carry the
/// chunk-boundary hint through compaction in a single pass instead of
/// compacting the prefix a second time on the per-chunk hot path.
fn compact_lower_marked(s: &str, mark: usize) -> (String, usize) {
    let mut out = String::with_capacity(s.len());
    let mut out_mark = None;
    for (i, c) in s.char_indices() {
        if i >= mark && out_mark.is_none() {
            out_mark = Some(out.len());
        }
        if c == ' ' {
            continue;
        }
        out.extend(c.to_lowercase());
    }
    let out_mark = out_mark.unwrap_or(out.len());
    (out, out_mark)
}

/// Byte offset of the most-recent occurrence of any pattern in `patterns`
/// within the space-free buffer (each pattern compacted the same way —
/// lowercase, spaces removed — so a human-readable constant table written
/// WITH spaces matches the spaceless wire form), or `None` if none match.
/// The `max` of the per-pattern `rfind` offsets is the recency anchor the
/// standalone-consent and paired yes/no branches compare against the idle
/// composer footer, exactly as the chooser branches use [`chooser_pos`].
///
/// One scratch buffer is reused across patterns (cleared, not
/// reallocated) so a scan over an N-entry table costs roughly one
/// allocation total rather than N — this is on the per-chunk detection
/// hot path.
fn last_compact_match_pos(compact: &str, patterns: &[&str]) -> Option<usize> {
    last_compact_match(compact, patterns).map(|(pos, _)| pos)
}

/// Like [`last_compact_match_pos`] but also returns WHICH pattern carried
/// the most-recent match, so the decision trace can name the exact
/// consent phrase that fired (issue #2) rather than just its offset.
fn last_compact_match<'a>(compact: &str, patterns: &[&'a str]) -> Option<(usize, &'a str)> {
    let mut needle = String::new();
    patterns
        .iter()
        .filter_map(|p| {
            needle.clear();
            needle.extend(p.chars().filter(|c| *c != ' ').flat_map(char::to_lowercase));
            compact.rfind(needle.as_str()).map(|pos| (pos, *p))
        })
        .max_by_key(|(pos, _)| *pos)
}

/// Last `MAX_TAIL` chars of the compacted buffer, snapped to a char
/// boundary — the bounded, already-redacted (lowercased, space-free)
/// slice that fed the decision, logged for debugging without dumping the
/// whole 16 KiB detect window.
fn compact_tail(compact: &str) -> &str {
    const MAX_TAIL: usize = 200;
    if compact.len() <= MAX_TAIL {
        return compact;
    }
    let mut start = compact.len() - MAX_TAIL;
    while start < compact.len() && !compact.is_char_boundary(start) {
        start += 1;
    }
    &compact[start..]
}

/// Whether a prompt marker at `marker_pos` is at least as recent (as far
/// down the append-only buffer) as the idle composer footer at
/// `idle_pos`. `None` marker → not present, so not live. `None` footer →
/// no idle anchor, so any present marker is live. This is the recency
/// gate shared by every prompt branch: the chooser shapes, the
/// `Esc to cancel` permission footer, the standalone consent phrases, and
/// the paired yes/no fallback. Without it a prompt phrase left in
/// scrollback reads as InputNeeded until it scrolls out of the detect
/// window even though the turn has finished and the composer is redrawn.
fn marker_at_least_as_recent(marker_pos: Option<usize>, idle_pos: Option<usize>) -> bool {
    marker_pos.is_some_and(|mp| idle_pos.is_none_or(|ip| mp >= ip))
}

/// The same-chunk rule shared by both detectors' classifiers. tmux full
/// repaints paint the screen top-to-bottom, so a live dialog and the bottom
/// status bar can land in ONE chunk with the work anchor LAST — position
/// alone would then read the dialog as already answered. When BOTH the prompt
/// `marker` and the `work_pos` anchor arrived inside the most recent chunk
/// (offset `>= last_chunk_start`), return `None` so the work anchor can't
/// suppress the dialog. `None` chunk hint (or either offset absent) keeps the
/// pure positional ordering by returning `work_pos` unchanged.
fn work_anchor_for(
    marker: Option<usize>,
    work_pos: Option<usize>,
    last_chunk_start: Option<usize>,
) -> Option<usize> {
    match (marker, work_pos, last_chunk_start) {
        (Some(m), Some(w), Some(cs)) if m >= cs && w >= cs => None,
        _ => work_pos,
    }
}

/// Detect Claude's ASCII selection-arrow at ANY numbered option:
/// `> 0.`–`> 9.` or `> 0)`–`> 9)`. `windows(4)` walks the byte stream
/// once with no allocations. ASCII-only sentinels (`>`, ` `, digit, `.`,
/// `)`) are safe to scan against raw bytes — these values never appear
/// inside a multi-byte UTF-8 sequence.
///
/// Scanned against the RAW (un-compacted) buffer: the arrow is `>` then a
/// literal space, and [`compact_lower`] strips exactly that space, so the
/// shape only survives pre-compaction. The chooser-recency anchor
/// therefore leans on the numbered options (which survive compaction),
/// not this arrow — see [`chooser_pos`].
fn has_ascii_chooser_arrow(s: &str) -> bool {
    s.as_bytes().windows(4).any(|w| {
        w[0] == b'>' && w[1] == b' ' && w[2].is_ascii_digit() && (w[3] == b'.' || w[3] == b')')
    })
}

/// True when a selection arrow sits DIRECTLY on a numbered option:
/// `❯ 1.` / `❯1.` / `> 1.` (and `)` variants). This is the structural
/// tell of a real chooser regardless of option labels — prose that
/// merely contains an arrow ("❯ pick one") never matches because the
/// arrow isn't followed by a `<digit><.|)>`. Used as a corroboration
/// signal so a custom-label chooser whose other chrome scrolled out of
/// the window is still recognised. Scanned on the RAW buffer (the arrow's
/// trailing space survives) like [`has_ascii_chooser_arrow`].
fn arrow_on_numbered_option(s: &str) -> bool {
    if has_ascii_chooser_arrow(s) {
        return true;
    }
    s.match_indices('❯').any(|(i, _)| {
        let rest = s[i + '❯'.len_utf8()..].trim_start();
        let mut chars = rest.chars();
        matches!(
            (chars.next(), chars.next()),
            (Some(d), Some(p)) if d.is_ascii_digit() && (p == '.' || p == ')')
        )
    })
}

/// Byte offset of the most recent chooser marker — the unicode selection
/// arrow `❯` or a numbered option (`1.`/`1)`/`2.`/`2)`) — or `None` when
/// no chooser shape is present. The MAX of the marker offsets is the
/// recency anchor compared against the idle composer footer in
/// [`claude_state_of`]: a live chooser's markers sit below (after) any
/// stale footer, while a parked-prompt arrow above a prose list sits
/// above it.
///
/// Called only with the compacted buffer, so it shares
/// [`last_option_marker_pos`]'s digit guard — a version/decimal `N.`
/// (`v2.1.159`) is NOT a marker, matching the `has_chooser` gate exactly
/// so the two can't disagree. The ASCII arrow `> N.` is intentionally
/// absent here: its space is gone post-compaction, so the numbered
/// options carry the anchor instead.
fn chooser_pos(compact: &str) -> Option<usize> {
    [
        compact.rfind('❯'),
        last_option_marker_pos(compact, b'1'),
        last_option_marker_pos(compact, b'2'),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Numbered-chooser shape: at least one `1.` / `1)` AND at least one
/// `2.` / `2)` option marker in the buffer (see [`last_option_marker_pos`]
/// for the digit guard that excludes version/decimal numbers). The
/// required `2.` / `2)` is what distinguishes a chooser from a chat list
/// that merely happens to start with `1.`.
fn has_numbered_chooser_options(s: &str) -> bool {
    has_option_marker(s, b'1') && has_option_marker(s, b'2')
}

/// True if `s` contains an option marker for `digit` — see
/// [`last_option_marker_pos`] for the exact shape and the digit guard.
fn has_option_marker(s: &str, digit: u8) -> bool {
    last_option_marker_pos(s, digit).is_some()
}

/// Byte offset of the LAST `<digit>.` / `<digit>)` whose delimiter is not
/// immediately followed by another ASCII digit (which would make it part
/// of a number rather than an option label), or `None`.
///
/// The digit guard is what keeps a decimal or dotted version number from
/// reading as a chooser: Claude's boot banner `Claude Code v2.1.159` has
/// `2.1`/`1.1` that satisfy a bare `1.`+`2.` test, and the composer prompt
/// glyph `❯` satisfies the arrow test — so a freshly-spawned IDLE composer
/// (banner still in the detection window) once misread as a live chooser →
/// `InputNeeded`, masking Working/Idle and blocking spawn-time readiness.
/// A real option (`1. Yes`, `2. No`) is followed by a space or letter,
/// never a digit, so the guard keeps every genuine chooser while rejecting
/// `7.2k`, `4.8`, and `2.1.159`. `rposition` returns the most recent match
/// so the offset is comparable against the other recency anchors.
/// ASCII-only sentinels are safe against raw bytes — none appear inside a
/// multi-byte UTF-8 sequence.
fn last_option_marker_pos(s: &str, digit: u8) -> Option<usize> {
    let b = s.as_bytes();
    b.windows(2).enumerate().rposition(|(i, w)| {
        w[0] == digit
            && (w[1] == b'.' || w[1] == b')')
            && !b.get(i + 2).is_some_and(u8::is_ascii_digit)
    })
}

/// Byte offset of the most recent live "working" status-line marker in
/// `compact` (the lowercased, space-free buffer — see [`compact_lower`]),
/// or `None` when nothing in the buffer says Claude is busy.
///
/// Claude renders a status line ONLY while streaming / running a tool:
/// `✦ Gusting… (2m 2s · ↓ 7.2k tokens · thinking some more)` or
/// `✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)`. We anchor on
/// its two stable shapes — the `esc to interrupt` interrupt hint (which
/// arrives spaceless as `esctointerrupt` when Claude paints it in the
/// status bar), and the live counter line in its full affirmative shape
/// (see [`is_live_counter_line`]). A line that merely mentions `tokens`
/// beside a `·` separator — a tool preview, a dialog's command echo —
/// must NOT anchor "working": the work anchor suppresses live dialog
/// markers, so a loose match here reads a real permission prompt as
/// already answered on a full repaint.
fn working_status_pos(compact: &str) -> Option<usize> {
    // Use the offset of the `esctointerrupt` MARKER itself (not the line
    // start) so the recency anchor lands where the status text actually
    // is — a status line that shares a wrapped/concatenated line with an
    // earlier dialog footer (`Esc to cancel✻ …`) must anchor at the
    // spinner text, not back at the dialog. The line-shape check just
    // gates OUT prose that merely mentions "esc to interrupt".
    let interrupt =
        last_line_pos(compact, is_interrupt_status_line).and(compact.rfind("esctointerrupt"));
    let counter = last_line_pos(compact, is_live_counter_line);
    [interrupt, counter].into_iter().flatten().max()
}

/// The `esc to interrupt` hint, but only when it sits on a line shaped
/// like Claude's live status bar rather than prose. The status bar
/// renders the hint as a STANDALONE token — it ends the line, or is
/// followed by a non-letter: a closing `)` (`(esc to interrupt)`), a
/// thinking-level dot (`esc to interrupt●high`), or a `·` separator.
/// Prose like "press esc to interrupt me while I work" continues with a
/// letter (`…interruptme…` after compaction), so it no longer pins the
/// agent to Working forever. (An earlier form required a spinner/timer/`·`
/// ON the same line, which wrongly rejected the real captured wire shape
/// where the interrupt hint lands on its own bottom line with no counter.)
fn is_interrupt_status_line(line: &str) -> bool {
    const HINT: &str = "esctointerrupt";
    match line.find(HINT) {
        Some(idx) => match line[idx + HINT.len()..].chars().next() {
            None => true,                  // ends the line
            Some(c) => !c.is_alphabetic(), // `)`, `·`, `●`, digit, …
        },
        None => false,
    }
}

/// Spinner glyphs Claude cycles through at the head of its live status
/// line (`✻ Cogitating…`, `✦ Gusting…`, `✽ Running…`, …). Matched as an
/// any-of set since the glyph rotates per frame.
///
/// `*` is deliberately NOT here: it's a plain ASCII bullet, so a markdown
/// line the agent prints (`* Done in (2s · 5 tokens)`) would otherwise
/// satisfy `is_live_counter_line` and read as Working. A genuinely-busy
/// agent is still caught by the `esc to interrupt` hint, which its status
/// bar always carries, so dropping `*` costs no real detection.
const WORKING_SPINNER_GLYPHS: &[char] = &['✢', '✳', '✶', '✻', '✽', '✺', '✦', '✧'];

/// The affirmative live-counter shape (real fixture lines:
/// `✻ Simmering… (10s · ↓ 137 tokens)`,
/// `✦ Gusting… (2m 2s · ↓ 7.2k tokens · thinking some more)`):
/// a spinner glyph plus the parenthesized elapsed timer plus the token
/// counter, all on one line of the compacted buffer. `esc to interrupt`
/// lines are matched separately in `working_status_pos`.
fn is_live_counter_line(line: &str) -> bool {
    line.contains('·')
        && line.contains("tokens")
        && line.contains(WORKING_SPINNER_GLYPHS)
        && has_elapsed_counter(line)
}

/// `(7s`, `(2m2s`, `(1h2m` — the opening of Claude's elapsed counter as
/// it appears in the compacted (space-free) buffer: `(`, one or more
/// digits, then a unit letter.
fn has_elapsed_counter(line: &str) -> bool {
    let b = line.as_bytes();
    for i in 0..b.len().saturating_sub(2) {
        if b[i] != b'(' || !b[i + 1].is_ascii_digit() {
            continue;
        }
        let mut j = i + 2;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j < b.len() && matches!(b[j], b's' | b'm' | b'h') {
            return true;
        }
    }
    false
}

/// Byte offset of the most recent idle input-box footer in `compact`.
/// Claude draws this footer only once it's done and waiting at the
/// prompt, so it's the recency anchor that beats a stale status line
/// still sitting in the append-only buffer.
///
/// The `shift+tab to cycle` mode indicator is the footer's bypass /
/// accept-edits / plan-mode form — lazybox launches agents with
/// `--dangerously-skip-permissions`, so the live footer reads
/// `bypass permissions on (shift+tab to cycle)` and never `? for
/// shortcuts`. Without this marker a just-finished agent has no idle
/// anchor, the stale `esc to interrupt` status line never gets evicted,
/// and the working glyph sticks ON forever (#179).
fn idle_box_pos(compact: &str) -> Option<usize> {
    [
        compact.rfind("tabtoamend"),
        compact.rfind("?forshortcuts"),
        compact.rfind("shift+tabtocycle"),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Byte offset of the most recent RELIABLE end-of-turn footer in
/// `compact` — `? for shortcuts` (default) or the bypass-mode
/// `shift+tab to cycle`. Unlike [`idle_box_pos`] this deliberately
/// EXCLUDES `Tab to amend`.
///
/// `Tab to amend` is ambiguous: Claude renders it both for an idle
/// composer holding draft text AND in a live command-approval dialog
/// (`Esc to cancel · Tab to amend · ctrl+e to explain`). The chooser
/// shapes can tolerate that ambiguity — a numbered list above a
/// `Tab to amend` footer is prose, not a live chooser. But the STRONG
/// consent-phrase shapes cannot: a real `Do you want to proceed?`
/// approval carries exactly that footer, so gating those on
/// `idle_box_pos` read the live prompt as stale scrollback and dropped
/// the `?`. `? for shortcuts` / bypass appear ONLY when the turn is over
/// and the composer is at rest, so they're the only trustworthy "this
/// consent phrase is now scrollback" anchor.
fn resting_composer_pos(compact: &str) -> Option<usize> {
    [
        compact.rfind("?forshortcuts"),
        compact.rfind("shift+tabtocycle"),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// A live "N shells still running" status line — Claude paints it while
/// background shells the agent launched during the turn keep executing
/// after the model finished (`✻ Crunched for 2m 44s · 4 shells still
/// running`). Compacted, "shells still running" → "shellsstillrunning" and
/// the singular "shell still running" → "shellstillrunning"; both carry
/// "stillrunning" alongside "shell".
///
/// Anchored on a spinner glyph, exactly like [`is_live_counter_line`]: the
/// status bar always leads with one of [`WORKING_SPINNER_GLYPHS`], but the
/// agent's own PROSE about a shell ("the dev shell is still running in the
/// background", "left the smoke tests still running") never does. Without
/// that anchor the two bare substrings pin the agent to Working on ordinary
/// end-of-turn prose — the false-Working mirror of the very bug this fixes,
/// and more frequent than the background-shells case it detects.
fn is_shells_running_line(line: &str) -> bool {
    line.contains(WORKING_SPINNER_GLYPHS) && line.contains("shell") && line.contains("stillrunning")
}

/// A line led by one of Claude's live [`WORKING_SPINNER_GLYPHS`] — the shape
/// every status frame shares (`✻ Crunched…`, `✻ Compacting…`,
/// `✻ Simmering…`) and that a shells-running line is one instance of.
/// Compared by recency against the last shells line: when a NEWER
/// spinner-status line exists, a fresh frame has been painted over the
/// shells status, so the shells line is stale. Prose never carries these
/// glyphs (see [`is_shells_running_line`]), so this matches only real
/// status frames.
fn is_spinner_status_line(line: &str) -> bool {
    line.contains(WORKING_SPINNER_GLYPHS)
}

/// How many resting composer footers (`? for shortcuts` / bypass `shift+tab
/// to cycle`) start after byte offset `pos`. Secondary staleness guard for
/// the shells-running branch: when the shells drain STRAIGHT to rest with no
/// newer status line (so the [`is_spinner_status_line`] recency check can't
/// see the transition), Claude still redraws the composer, stacking a SECOND
/// `? for shortcuts` below the now-stale line — `≥2 after` settles it, while
/// a live frame carries only its own footer (`≤1 after`). Excludes
/// `Tab to amend`, which a live command-approval dialog also renders.
fn resting_footers_after(compact: &str, pos: usize) -> usize {
    let mut count = 0;
    let mut offset = 0;
    for line in compact.split_inclusive('\n') {
        if offset > pos && (line.contains("?forshortcuts") || line.contains("shift+tabtocycle")) {
            count += 1;
        }
        offset += line.len();
    }
    count
}

/// Start offset of the last line satisfying `pred`. Walks the buffer
/// once, keeping line boundaries (`split_inclusive`) so the returned
/// offset is comparable against `rfind` positions in the same string.
fn last_line_pos(s: &str, pred: impl Fn(&str) -> bool) -> Option<usize> {
    let mut best = None;
    let mut offset = 0;
    for line in s.split_inclusive('\n') {
        if pred(line) {
            best = Some(offset);
        }
        offset += line.len();
    }
    best
}

/// Last `max` bytes of `s`, snapped forward to a char boundary. The
/// simple-pattern agents (Codex, Cursor, GenericCli) match only this
/// recent tail of the detect window so a prompt that scrolled past it
/// stops matching — without the bound, a stale `[y/n]` anywhere in the
/// 16 KiB window pins `InputNeeded` long after the user answered.
pub fn recent_tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// The last `n` non-empty lines of `s`, joined with `\n`. An interactive
/// CLI's prompt awaiting input sits at the BOTTOM of the screen (the
/// cursor parks there), so the simple-pattern agents scan only this zone
/// for their `[y/n]`-style markers — a `[y/n]` that merely appeared
/// earlier in an echoed command, a diff, or a doc no longer fires a
/// spurious InputNeeded. `n` of a few lines tolerates several trailing
/// helper / key-hint lines a CLI may print under the prompt, while a
/// `[y/n]` buried deeper in scrollback (more output below it) is still
/// excluded.
pub fn last_nonempty_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Fingerprint of a PTY chunk's *meaningful* content: a hash of the
/// letter stream left after dropping ANSI escapes, digits, whitespace,
/// punctuation, and symbol glyphs. Spinner frames (braille or star
/// glyphs), elapsed-time and token-counter ticks, clocks, and progress
/// bars all reduce to an unchanged letter stream — or to nothing at all
/// (`None`) — so comparing successive chunk fingerprints separates real
/// output from repaint churn. `None` (no letters in the chunk: pure
/// cursor/erase churn or an animation-only repaint) is never meaningful.
pub fn content_fingerprint(bytes: &[u8]) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let s = strip_ansi_lossy(bytes);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut any = false;
    for c in s.chars().filter(|c| c.is_alphabetic()) {
        c.hash(&mut hasher);
        any = true;
    }
    any.then(|| hasher.finish())
}

/// Filter out ANSI escape sequences, then UTF-8-decode the remainder.
///
/// Earlier this function pushed `bytes[i] as char` — that mangled
/// multi-byte UTF-8 glyphs like Claude's choice arrow `❯` (U+276F, 3
/// bytes) into three separate Latin-1 chars, so any pattern containing
/// the glyph silently failed to match. We keep the raw glyph bytes
/// untouched and decode once at the end so the patterns can search for
/// it literally.
///
/// Every `ESC`-introduced run is dropped wholesale via `skip_escape`;
/// the parser handles the four families tmux/Claude emit (CSI, OSC, the
/// DCS/SOS/PM/APC string family, and charset designators) so none of
/// their payload bytes leak into the searched text. Malformed or
/// truncated sequences are consumed safely to the end of the buffer
/// rather than panicking — a half-arrived chunk is the common case on a
/// live PTY.
pub fn strip_ansi_lossy(bytes: &[u8]) -> String {
    strip_ansi_lossy_marked(bytes, bytes.len()).0
}

/// [`strip_ansi_lossy`] that also translates one input byte offset
/// (`mark`) into the stripped output's offset space — returns
/// `(stripped, stripped_mark)`. Carries the chunk-boundary hint through
/// stripping in the same single pass. The lossy-decode fallback can
/// shift offsets slightly (replacement chars resize); the mark is
/// clamped and snapped forward to a char boundary, which is fine for a
/// recency heuristic.
fn strip_ansi_lossy_marked(bytes: &[u8], mark: usize) -> (String, usize) {
    let mut filtered: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut out_mark = None;
    let mut i = 0;
    while i < bytes.len() {
        if i >= mark && out_mark.is_none() {
            out_mark = Some(filtered.len());
        }
        if bytes[i] == 0x1b {
            i = skip_escape(bytes, i);
        } else {
            filtered.push(bytes[i]);
            i += 1;
        }
    }
    let out_mark = out_mark.unwrap_or(filtered.len());
    // Happy path (valid UTF-8, the norm) moves the buffer into the String
    // with no copy; only a malformed chunk pays for the lossy fallback.
    let text = match String::from_utf8(filtered) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    let mut out_mark = out_mark.min(text.len());
    while out_mark < text.len() && !text.is_char_boundary(out_mark) {
        out_mark += 1;
    }
    (text, out_mark)
}

/// Advance past one escape sequence whose `ESC` (0x1b) is at `start`.
/// Returns the index of the first byte *after* the sequence — i.e. the
/// next byte the caller should treat as content. Recognises the four
/// families that actually reach lazybox through tmux:
///
///   - **CSI** `ESC [ … <final 0x40–0x7e>` — SGR colour, cursor moves.
///   - **OSC** `ESC ] … <BEL | ST>` — window title, hyperlinks.
///   - **string** `ESC (P|X|^|_) … <BEL | ST>` — DCS / SOS / PM / APC.
///     tmux wraps app passthrough in DCS, so the payload must be dropped,
///     not leaked into the matched text as stray characters.
///   - **charset** `ESC (|)|*|+ <one byte>` — G0–G3 designators.
///
/// Anything else is treated as a two-byte escape (`ESC c`, `ESC =`,
/// `ESC 7`, …): drop `ESC` plus the single introducer. A lone trailing
/// `ESC` consumes just itself.
fn skip_escape(bytes: &[u8], start: usize) -> usize {
    debug_assert_eq!(bytes.get(start), Some(&0x1b));
    let mut i = start + 1;
    let Some(&intro) = bytes.get(i) else {
        return i; // lone trailing ESC
    };
    i += 1;
    match intro {
        b'[' => {
            // CSI: parameter / intermediate bytes until a final byte.
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // consume the final byte
            }
            i
        }
        b']' | b'P' | b'X' | b'^' | b'_' => skip_string_terminated(bytes, i),
        b'(' | b')' | b'*' | b'+' => (i + 1).min(bytes.len()), // + one charset byte
        _ => i,                                                // two-byte escape, already consumed
    }
}

/// Advance past the body of a string-terminated escape (OSC / DCS / SOS /
/// PM / APC) starting at `i` (the first byte after the introducer),
/// consuming the terminator. The body ends at BEL (0x07) or ST
/// (`ESC \`); an unterminated body runs to the end of the buffer.
fn skip_string_terminated(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            0x07 => return i + 1,                                     // BEL
            0x1b if bytes.get(i + 1) == Some(&b'\\') => return i + 2, // ST
            _ => i += 1,
        }
    }
    i
}

/// How far back the Codex bare-`[y/n]` fallback scans for a bottom-of-
/// screen prompt. Mirrors the `builtins::PROMPT_TAIL_WINDOW` the simple-
/// pattern agents use: their markers carry no recency anchor, so bounding
/// the scan to the visible-screen tail is what lets an answered prompt stop
/// matching once fresh output arrives.
const CODEX_PROMPT_TAIL_WINDOW: usize = 2 * 1024;

/// Codex's three observable states, in one detector — the per-agent
/// equivalent of [`claude_state`]. Codex renders INLINE (no alt-screen), so
/// the daemon's append-only detect window ends with whatever Codex last
/// repainted; recency is therefore the byte offset of each marker, exactly
/// as for Claude.
///
/// - **`InputNeeded`** — a live approval / consent modal. Two signals, both
///   gated on recency so an ANSWERED modal still lingering in the window
///   stops matching once Codex repaints below it: a
///   [`CODEX_PROMPT_PHRASES`] phrase, or the chooser shape `› 1.` (the
///   selection arrow `›` sitting directly on a numbered option). Plus the
///   bare `[y/n]` / `approve?` family scanned in the bottom-of-screen zone
///   for prompts that carry no Codex chrome.
/// - **`Working`** — Codex paints a live status line while busy:
///   `• Working (3s · esc to interrupt)`. The `esc to interrupt` hint is the
///   reliable pulser (same token Claude uses), recognised only when it's
///   more recent than the resting composer footer so a finished turn's stale
///   status line reads `Idle`, not forever-busy.
/// - **`Idle`** — composer at rest, or plain non-interactive output.
///
/// Always `Some(_)`, like [`claude_state`], so the daemon can diff against
/// the cached state to notice every transition.
pub fn codex_state(recent_output: &[u8]) -> Option<AgentState> {
    let s = strip_ansi_lossy(recent_output);
    Some(codex_state_of(&s, &compact_lower(&s), None))
}

/// Chunk-aware variant of [`codex_state`] — the per-agent equivalent of
/// [`claude_state_chunked`]. `last_chunk_start` is the byte offset within
/// `recent_output` where the most recent PTY chunk begins. A tmux/full-
/// screen repaint can deliver a live approval modal and an earlier status
/// line in one chunk; the hint keeps the work anchor from suppressing a
/// prompt that arrived in the same repaint.
pub fn codex_state_chunked(recent_output: &[u8], last_chunk_start: usize) -> Option<AgentState> {
    let mark = last_chunk_start.min(recent_output.len());
    let (s, s_mark) = strip_ansi_lossy_marked(recent_output, mark);
    let (compact, compact_mark) = compact_lower_marked(&s, s_mark);
    Some(codex_state_of(&s, &compact, Some(compact_mark)))
}

/// Fast-path detector for unmistakable Codex approval chrome painted by the
/// latest PTY chunk. Unlike [`codex_state_chunked`], this deliberately does
/// not classify `Working` or `Idle`: it returns the interaction shape only
/// when a blocking modal marker overlaps the current chunk, so the daemon can
/// surface `InputNeeded` immediately without re-reading a stale prompt from
/// scrollback while the agent streams.
///
/// A small prefix from the previous chunk is retained so split writes such as
/// `"Press enter to con"` + `"firm"` still match. Requiring the match to END
/// after `last_chunk_start` is the stale-marker guard.
pub fn codex_input_needed_in_current_chunk(
    recent_output: &[u8],
    last_chunk_start: usize,
) -> Option<PromptShape> {
    let mark = last_chunk_start.min(recent_output.len());
    let (s, s_mark) = strip_ansi_lossy_marked(recent_output, mark);
    let (compact, compact_mark) = compact_lower_marked(&s, s_mark);
    codex_input_needed_in_current_chunk_from(&s, s_mark, &compact, compact_mark)
}

/// [`codex_input_needed_in_current_chunk`] over an already stripped (`s`) and
/// compacted (`compact`) window plus their chunk-boundary marks. Callers that
/// have already paid for the ANSI strip / compaction — the per-chunk pump path
/// runs on every repaint frame — reuse those buffers instead of scanning the
/// 16 KiB window a second time.
fn codex_input_needed_in_current_chunk_from(
    s: &str,
    s_mark: usize,
    compact: &str,
    compact_mark: usize,
) -> Option<PromptShape> {
    let phrase_touched = CODEX_PROMPT_PHRASES.iter().any(|phrase| {
        let needle: String = phrase
            .chars()
            .filter(|c| *c != ' ')
            .flat_map(char::to_lowercase)
            .collect();
        compact
            .rfind(&needle)
            .is_some_and(|pos| pos + needle.len() > compact_mark)
    });
    if phrase_touched {
        return Some(PromptShape::Chooser);
    }

    let arrow_touched = codex_arrow_option_pos(compact).is_some_and(|pos| {
        // `›` is three UTF-8 bytes, followed by one ASCII digit and one
        // ASCII delimiter (`.` or `)`).
        pos + '›'.len_utf8() + 2 > compact_mark
    });
    if arrow_touched {
        return Some(PromptShape::Chooser);
    }

    // Bare prompt families do not have Codex's modal chrome. Keep their
    // existing bottom-of-screen guard and additionally require the latest
    // chunk to touch the marker.
    let prompt_zone = last_nonempty_lines(recent_tail(s, CODEX_PROMPT_TAIL_WINDOW), 5);
    let bare_prompt_touched = YN_PROMPT_PATTERNS.iter().any(|pattern| {
        s.rfind(pattern)
            .is_some_and(|pos| pos + pattern.len() > s_mark && prompt_zone.contains(pattern))
    }) || s
        .rfind("approve?")
        .is_some_and(|pos| pos + "approve?".len() > s_mark && prompt_zone.contains("approve?"));
    bare_prompt_touched.then_some(PromptShape::Chooser)
}

/// Typed counterpart to [`codex_input_needed_in_current_chunk`]. Credit
/// exhaustion wins over the generic chooser classification when the newest
/// chunk completes either half of the provider screen.
pub fn codex_blocked_in_current_chunk(
    recent_output: &[u8],
    last_chunk_start: usize,
) -> Option<AgentObservation> {
    let mark = last_chunk_start.min(recent_output.len());
    let (s, s_mark) = strip_ansi_lossy_marked(recent_output, mark);
    let (compact, compact_mark) = compact_lower_marked(&s, s_mark);

    let footer_pos = codex_footer_pos(&compact);
    if codex_state_from(&s, &compact, Some(compact_mark), footer_pos) == AgentState::CreditExhausted
        && (compact_match_touched(&compact, CODEX_CREDIT_EXHAUSTED_PHRASES, compact_mark)
            || compact_match_touched(&compact, CODEX_WAIT_FOR_CREDIT_PHRASES, compact_mark))
    {
        return Some(AgentObservation::from_state(AgentState::CreditExhausted));
    }

    // Reuse the strip / compaction computed above rather than re-scanning the
    // 16 KiB window inside `codex_input_needed_in_current_chunk` — this runs on
    // every repaint frame of a live session.
    codex_input_needed_in_current_chunk_from(&s, s_mark, &compact, compact_mark)
        .map(AgentObservation::input_needed)
}

pub fn codex_credit_exhausted_hint(recent_output: &[u8]) -> Option<String> {
    let stripped = strip_ansi_lossy(recent_output);
    let compact = compact_lower(&stripped);
    codex_credit_exhausted_pos(&compact)?;
    if compact.contains("askyourworkspaceownertoaddmore")
        || compact.contains("askyourworkspaceownertorefill")
    {
        Some("ask your workspace owner to add credits".into())
    } else if compact.contains("spendcap") {
        Some("increase your workspace spend cap".into())
    } else {
        Some("add credits or switch subscription".into())
    }
}

/// Whether Codex is ready to receive a pasted prompt: the composer footer is
/// drawn AND no approval / trust modal is up. Like [`claude_ready_for_prompt`],
/// derived from the shared state model — "composer visible and not asking" —
/// rather than re-deriving the markers. Lets the spawn-time injector land the
/// work prompt the moment Codex is idle instead of riding the settle timer,
/// and never into a directory-trust gate.
pub fn codex_ready_for_prompt(recent_output: &[u8]) -> bool {
    let s = strip_ansi_lossy(recent_output);
    let compact = compact_lower(&s);
    codex_ready_for_prompt_from(&s, &compact)
}

/// [`codex_ready_for_prompt`] over an already stripped (`s`) and compacted
/// (`compact`) detect window. Callers that have paid for the ANSI strip /
/// compaction once — the per-chunk pump path runs on every repaint frame —
/// reuse those buffers instead of scanning the 16 KiB window twice.
fn codex_ready_for_prompt_from(s: &str, compact: &str) -> bool {
    // Compute the composer-footer offset once and feed it to BOTH the
    // readiness gate (is the composer drawn at all?) and the state decision.
    // This runs on every chunk while the spawn-time injector polls for
    // readiness, so sharing the offset avoids a second `rfind` scan for the
    // footer that `codex_state_of` would otherwise repeat internally.
    let footer_pos = codex_footer_pos(compact);
    footer_pos.is_some() && codex_state_from(s, compact, None, footer_pos) == AgentState::Idle
}

/// Chunk-aware companion to [`codex_ready_for_prompt`] (issue #425).
///
/// [`codex_ready_for_prompt`] reads readiness *positionally* over the whole
/// append-only detect window: the composer footer must be the byte-most-recent
/// marker. Codex's diff renderer defeats that against a live session — spinner
/// ticks and status-line fragments keep landing *after* the last full footer
/// paint, and the compacted (whitespace-free) buffer can even reconstruct a
/// stale `esc to interrupt` from unrelated fragments. The positional read then
/// stays pinned on `Working` long after Codex is resting at its composer, and
/// the spawn-time injector rides its hard deadline instead of firing in
/// hundreds of milliseconds.
///
/// This variant judges the *current repaint frame* — the bytes of the latest
/// PTY chunk (`last_chunk_start`) — the same way
/// [`codex_input_needed_in_current_chunk`] does for approvals. Codex is ready
/// when the frame itself paints composer chrome:
///
/// - the composer footer (`<model> <effort> · <cwd>`), or
/// - the composer arrow (`›` followed by prompt text — a chooser's `› 1.`
///   never matches), which Codex repaints on every placeholder rotation while
///   the composer is usable,
///
/// AND the frame carries no approval/consent marker, no working status line
/// painted after that composer chrome, and no bare `[y/n]` prompt parked in
/// the visible tail. The footer must additionally have been painted at least
/// once somewhere in the buffer — a lone arrow on a half-drawn boot screen is
/// not proof the composer exists yet.
///
/// Frames without composer evidence fall back to the whole-buffer positional
/// read, so this is strictly an acceleration: a live approval modal or trust
/// gate (whose frames carry their own chrome, and which never rotate
/// placeholders while parked) can never become ready through it.
pub fn codex_ready_for_prompt_chunked(recent_output: &[u8], last_chunk_start: usize) -> bool {
    let mark = last_chunk_start.min(recent_output.len());
    let (s, s_mark) = strip_ansi_lossy_marked(recent_output, mark);
    let (compact, compact_mark) = compact_lower_marked(&s, s_mark);

    let frame = &compact[compact_mark.min(compact.len())..];
    let frame_working = codex_working_pos(frame);
    // Composer evidence painted by THIS frame, most-conservative first:
    // - footer: trust it unless the same frame painted a working status
    //   line after it (mirrors the whole-buffer positional rule);
    // - arrow: a placeholder-rotation frame is tiny and carries no
    //   footer, so accept it only when the frame shows no working status
    //   line at all — a busy full repaint (status line + composer) must
    //   keep falling through to the positional read.
    let composer_painted = codex_footer_pos(frame)
        .is_some_and(|fp| !frame_working.is_some_and(|wp| wp > fp))
        || (codex_composer_arrow_pos(frame).is_some() && frame_working.is_none());
    if composer_painted
        && codex_footer_pos(&compact).is_some()
        && codex_prompt_pos(frame).is_none()
        && codex_credit_exhausted_pos(frame).is_none()
        && !codex_bare_prompt_in_tail(&s)
    {
        return true;
    }

    // Fall back to the whole-buffer positional read, reusing the strip /
    // compaction already computed above — this runs on every repaint frame,
    // so a second 16 KiB ANSI strip here would double the pump's per-chunk
    // cost on a continuously-repainting session (issue #629 watchdog stress).
    codex_ready_for_prompt_from(&s, &compact)
}

/// Byte offset of the most recent Codex *composer* arrow in `compact` — `›`
/// followed by prompt text (the rotating placeholder or the user's typed
/// draft). The complement of [`codex_arrow_option_pos`]: only the LAST arrow
/// is classified, and an arrow sitting on a numbered chooser option
/// (`›1.` / `›1)`) is rejected, so a modal's selection arrow never reads as
/// composer chrome.
fn codex_composer_arrow_pos(compact: &str) -> Option<usize> {
    let (i, _) = compact.match_indices('\u{203a}').next_back()?;
    let mut chars = compact[i + '\u{203a}'.len_utf8()..].chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() => Some(i),
        _ => None,
    }
}

/// Bare yes/no family parked at the bottom of the visible screen — Codex
/// prompts (or a GenericCli-style bare prompt) with no distinctive chrome.
/// Scoped to the visible tail so a `[y/n]` echoed earlier in scrollback
/// doesn't false-fire.
fn codex_bare_prompt_in_tail(s: &str) -> bool {
    let prompt_zone = last_nonempty_lines(recent_tail(s, CODEX_PROMPT_TAIL_WINDOW), 5);
    contains_any(&prompt_zone, YN_PROMPT_PATTERNS) || prompt_zone.contains("approve?")
}

/// Classify Codex's state from the stripped buffer `s` and its space-free
/// form `compact`. `last_chunk_start` is the chunk-boundary hint in
/// `compact`'s offset space (`None` keeps the pure positional rules).
fn codex_state_of(s: &str, compact: &str, last_chunk_start: Option<usize>) -> AgentState {
    codex_state_from(s, compact, last_chunk_start, codex_footer_pos(compact))
}

/// [`codex_state_of`] with the composer-footer offset supplied by the caller.
/// The readiness check ([`codex_ready_for_prompt`]) already computes it, so
/// threading it in lets that per-chunk hot path avoid a second `rfind` pass
/// over the buffer for the same footer.
fn codex_state_from(
    s: &str,
    compact: &str,
    last_chunk_start: Option<usize>,
    footer_pos: Option<usize>,
) -> AgentState {
    let work_pos = codex_working_pos(compact);
    let prompt_pos = codex_prompt_pos(compact);
    let credit_pos = codex_credit_exhausted_pos(compact);

    // Same-chunk rule (see [`work_anchor_for`]): on a full repaint the
    // approval modal and an earlier status line land in ONE chunk; the work
    // anchor must not then suppress a prompt that arrived in the same paint.
    let work_against = |marker: Option<usize>| work_anchor_for(marker, work_pos, last_chunk_start);

    if marker_at_least_as_recent(credit_pos, footer_pos.max(work_against(credit_pos))) {
        return AgentState::CreditExhausted;
    }

    // A live approval / consent modal is the bottom-most marker — more recent
    // than the resting footer and any status line painted above it.
    if marker_at_least_as_recent(prompt_pos, footer_pos.max(work_against(prompt_pos))) {
        return AgentState::InputNeeded;
    }

    // Bare yes/no family at the bottom of the screen — Codex prompts (or a
    // GenericCli-style bare prompt) that carry no distinctive chrome.
    if codex_bare_prompt_in_tail(s) {
        return AgentState::InputNeeded;
    }

    // Working: the live `esc to interrupt` status line, more recent than the
    // resting composer footer. Deliberately positional with NO same-chunk
    // relaxation — a finished turn repaints its composer footer BELOW the now-
    // stale status line, and "never falsely busy" outranks catching the very
    // first frame after submit (the spinner re-paints the hint within a frame
    // or two, and the daemon's working hysteresis bridges the gap).
    if work_pos.is_some_and(|wp| footer_pos.is_none_or(|fp| wp > fp)) {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Byte offset of the most recent Codex composer footer in `compact` — the
/// `<model> <effort> · <cwd>` status line Codex draws beneath the input box
/// at rest and while streaming. Anchored on the middle-dot separator (U+00B7,
/// distinct from the U+2022 bullet the status line uses) immediately followed
/// by the cwd path (`·/…` or `·~…`). It's the recency anchor that evicts a
/// finished turn's stale `esc to interrupt` and that proves the composer is
/// drawn for the readiness check.
///
/// Assumes a Unix-shaped cwd — an absolute `/…` or home-relative `~…` path,
/// which every lazybox worktree is (`~/.lazybox/v2/worktrees/…`). A footer
/// whose cwd rendered without that leading `/`/`~` would miss this anchor; no
/// such case exists on the macOS/Linux hosts lazybox targets.
fn codex_footer_pos(compact: &str) -> Option<usize> {
    [compact.rfind("\u{b7}/"), compact.rfind("\u{b7}~")]
        .into_iter()
        .flatten()
        .max()
}

/// Reasoning-effort tokens Codex prints as the second word of its
/// `<model> <effort>` composer footer. Matched as whole words (exact
/// equality, so order and prefix overlaps don't matter). `default` and
/// `max` are the labels Codex uses for a model's own default and its
/// maximum reasoning setting; the rest are the standard effort levels.
const CODEX_EFFORT_TOKENS: &[&str] = &[
    "minimal", "low", "medium", "high", "xhigh", "max", "default", "none",
];

/// Model-name prefixes Codex renders in its composer footer. The footer
/// glues the rotating placeholder text directly onto the model name (the
/// cursor move between the composer row and the footer row leaves no byte
/// between them — see the fixtures: `…@filenamegpt-5.5 xhigh · /repo`), so
/// the model is isolated by anchoring on its own prefix rather than by
/// tokenizing. Codex runs OpenAI models only — `gpt-*` today, the
/// `o1`/`o3`/`o4` and `codex-*` families historically.
const CODEX_MODEL_PREFIXES: &[&str] = &["gpt-", "o1", "o3", "o4", "codex-"];

/// Extract Codex's live `<model> <effort>` from the composer footer as a
/// compact display string (`"gpt-5.5 · xhigh"`), or `None` when the footer
/// isn't on screen or its trailing token isn't a recognised effort.
///
/// Unlike the state detectors this reads the ANSI-stripped-but-NOT-compacted
/// buffer: Codex paints the footer with real space bytes
/// (`gpt-5.5 xhigh · /repo`), so the model/effort split survives — whereas
/// `compact_lower` would glue them into `gpt-5.5xhigh`. The `<effort>` word
/// is validated against `CODEX_EFFORT_TOKENS` (so a stray `foo · /bar` in
/// output can't be mistaken for a footer) and the model is cut from the
/// rightmost `CODEX_MODEL_PREFIXES` anchor to drop the placeholder text
/// glued in front of it. Cheap enough to ride the daemon's per-settle
/// detection.
pub fn codex_model_effort(recent_output: &[u8]) -> Option<String> {
    let s = strip_ansi_lossy(recent_output);
    // The footer is `<model> <effort> · <cwd>`; find the bottom-most `·`
    // followed by an absolute or home-relative path (the same cwd anchor
    // `codex_footer_pos` uses, but on the spaced form so the tokens before
    // it keep their separators). `rev` picks the most recent footer, so a
    // stale one in scrollback never wins.
    let dot = s
        .match_indices('\u{b7}')
        .rev()
        .find(|(i, _)| {
            let rest = s[i + '\u{b7}'.len_utf8()..].trim_start();
            rest.starts_with('/') || rest.starts_with('~')
        })
        .map(|(i, _)| i)?;
    let head = s[..dot].trim_end();
    // The effort is the last whitespace-delimited word; require it to be a
    // known effort so an arbitrary `word · /path` line can't false-match.
    let (before_effort, effort) = head.rsplit_once(char::is_whitespace)?;
    if !CODEX_EFFORT_TOKENS.contains(&effort) {
        return None;
    }
    // The model name is glued to the placeholder text, so cut it from the
    // rightmost recognised model prefix rather than by tokenizing.
    // `before_effort` keeps whatever whitespace separated the model from the
    // effort (`rsplit_once` only removes the final char), so trim it off the
    // model slice — otherwise a multi-space footer leaks padding into the name.
    let model = CODEX_MODEL_PREFIXES
        .iter()
        .filter_map(|p| before_effort.rfind(p))
        .max()
        .map(|start| before_effort[start..].trim_end())?;
    Some(format!("{model} · {effort}"))
}

/// Byte offset of Codex's most recent live "working" status line in `compact`
/// — the `esc to interrupt` hint on a status-bar-shaped line (reusing
/// [`is_interrupt_status_line`], which rejects prose that merely mentions the
/// phrase). Codex renders `• Working (3s · esc to interrupt)` only while busy.
fn codex_working_pos(compact: &str) -> Option<usize> {
    last_line_pos(compact, is_interrupt_status_line).and(compact.rfind("esctointerrupt"))
}

/// Byte offset of the most recent Codex approval-modal marker in `compact` —
/// a [`CODEX_PROMPT_PHRASES`] phrase or the chooser shape `› <digit>`. The
/// `max` is the recency anchor compared against the composer footer and work
/// status line in [`codex_state_of`].
fn codex_prompt_pos(compact: &str) -> Option<usize> {
    [
        last_compact_match_pos(compact, CODEX_PROMPT_PHRASES),
        codex_arrow_option_pos(compact),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// A credit banner is actionable only while its Wait option is visible.
/// Requiring both signals rejects chat prose, release notes, and stale banner
/// text after the chooser has disappeared.
fn codex_credit_exhausted_pos(compact: &str) -> Option<usize> {
    let exhausted = last_compact_match_pos(compact, CODEX_CREDIT_EXHAUSTED_PHRASES)?;
    let wait = last_compact_match_pos(compact, CODEX_WAIT_FOR_CREDIT_PHRASES)?;
    Some(exhausted.min(wait))
}

fn compact_match_touched(compact: &str, patterns: &[&str], mark: usize) -> bool {
    patterns.iter().any(|pattern| {
        let needle: String = pattern
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        compact
            .rfind(&needle)
            .is_some_and(|pos| pos + needle.len() > mark)
    })
}

/// Byte offset of the most recent Codex selection arrow sitting directly on a
/// numbered option — `›1.` / `›1)` in the space-free buffer (rendered
/// `› 1. Yes` on screen). The idle composer's placeholder is `›` followed by
/// prompt text (a letter), so requiring a digit after the arrow keeps it from
/// matching a resting composer. `›` is U+203A — Codex's chooser arrow, not
/// Claude's `❯`.
fn codex_arrow_option_pos(compact: &str) -> Option<usize> {
    compact.match_indices('\u{203a}').rev().find_map(|(i, _)| {
        let mut chars = compact[i + '\u{203a}'.len_utf8()..].chars();
        matches!(
            (chars.next(), chars.next()),
            (Some(d), Some(p)) if d.is_ascii_digit() && (p == '.' || p == ')')
        )
        .then_some(i)
    })
}

/// Compact fragment length taken from the pasted prompt for echo matching.
/// Long enough to be unmistakable in a repaint, short enough to fit the
/// visible composer even on a narrow terminal.
const PASTE_ECHO_PROBE_LEN: usize = 32;

/// Minimum probe length — a shorter fragment ("ok", "fix it") is too likely
/// to occur in unrelated repaint output to serve as paste evidence.
const PASTE_ECHO_PROBE_MIN: usize = 8;

/// Composer placeholder chrome agents render when a large paste is collapsed
/// instead of echoed verbatim (Claude: `[Pasted text #1 +12 lines]`, Codex:
/// `[Pasted Content …]`). Compact-lowercase, like every probe.
pub const PASTE_PLACEHOLDER_PROBES: &[&str] = &["pastedtext", "pastedcontent"];

/// Build the compact (lowercased, whitespace-free) probe that recognizes the
/// paste's echo in the composer: the TAIL of the prompt, because a composer
/// keeps its cursor at the end of the inserted text, so for a paste larger
/// than the visible box it's the tail that stays on screen. `None` when the
/// prompt is too short to be distinctive — the caller then relies on the
/// placeholder probes and the quiet-window fallback.
pub fn paste_echo_probe(prompt: &str) -> Option<String> {
    // Raw ESC bytes are dropped to mirror the framing sanitizer — the
    // delivered (and therefore echoed) text never contains them.
    let compact: Vec<char> = prompt
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\x1b')
        .flat_map(char::to_lowercase)
        .collect();
    (compact.len() >= PASTE_ECHO_PROBE_MIN).then(|| {
        compact[compact.len().saturating_sub(PASTE_ECHO_PROBE_LEN)..]
            .iter()
            .collect()
    })
}

/// Whether the output produced since a bracketed paste already echoes that
/// paste — the composer re-rendered with the pasted text (or its collapsed
/// placeholder). This is the content-based settle signal for TUIs that never
/// go output-quiet (issue #425): once the echo is visible, the paste has been
/// processed and the submit keystroke can be sent immediately, without
/// waiting for a global quiet window a repainting status line never allows.
pub fn paste_echo_observed(output: &[u8], probes: &[String]) -> bool {
    if probes.is_empty() {
        return false;
    }
    // Full whitespace removal — NOT `compact_lower`, which preserves
    // newlines: a composer soft-wraps the echoed paste at the terminal
    // width, so the echo of a single-line prompt can arrive split across
    // rendered lines. Probes are built with the same normalization
    // ([`paste_echo_probe`]).
    let compact: String = strip_ansi_lossy(output)
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    probes
        .iter()
        .any(|p| !p.is_empty() && compact.contains(p.as_str()))
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the pure primitives. The agent-level tests in
    //! `tests/agents.rs` and the real-byte corpus in
    //! `tests/detect_fixtures.rs` cover composition and live wire shapes;
    //! these pin the building blocks — above all `strip_ansi_lossy`, the
    //! `pub` function every detector sits on, which until now was only
    //! exercised transitively.
    use super::*;

    #[test]
    fn strip_passes_plain_text_through_unchanged() {
        assert_eq!(strip_ansi_lossy(b""), "");
        assert_eq!(strip_ansi_lossy(b"hello world"), "hello world");
    }

    #[test]
    fn strip_removes_csi_sgr_and_cursor_moves() {
        // SGR colour + erase-line + cursor home, interleaved with text.
        assert_eq!(strip_ansi_lossy(b"\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi_lossy(b"\x1b[2Khello"), "hello");
        assert_eq!(strip_ansi_lossy(b"a\x1b[1;2Hb"), "ab");
    }

    #[test]
    fn strip_preserves_multibyte_glyphs() {
        // The regression this function was rewritten for: the chooser
        // arrow `❯` (U+276F, 3 bytes) must survive intact so patterns can
        // match it literally. Wrap it in CSI runs to mimic a real repaint.
        let raw = b"\x1b[38;5;2m\xe2\x9d\xaf\x1b[0m 1. Yes";
        assert_eq!(strip_ansi_lossy(raw), "❯ 1. Yes");
    }

    #[test]
    fn strip_removes_osc_terminated_by_bel_or_st() {
        // OSC 0 (window title) — both terminators occur in the wild.
        assert_eq!(strip_ansi_lossy(b"\x1b]0;title\x07keep"), "keep");
        assert_eq!(strip_ansi_lossy(b"\x1b]0;title\x1b\\keep"), "keep");
    }

    #[test]
    fn strip_drops_dcs_string_family_payload() {
        // tmux wraps app passthrough in DCS (`ESC P … ST`). Pre-hardening
        // the introducer was dropped but the payload leaked as text,
        // which could inject stray `tokens`/digits into the matched
        // buffer. The whole run — including the body — must vanish.
        assert_eq!(strip_ansi_lossy(b"\x1bPtmux;data\x1b\\visible"), "visible");
        // SOS / PM / APC share the string terminator.
        assert_eq!(strip_ansi_lossy(b"\x1b^priv\x07ok"), "ok");
        assert_eq!(strip_ansi_lossy(b"\x1b_app\x1b\\ok"), "ok");
    }

    #[test]
    fn strip_drops_charset_designator_second_byte() {
        // `ESC ( B` selects the ASCII charset for G0. Pre-hardening the
        // `B` leaked; now the full two-byte designator is dropped.
        assert_eq!(strip_ansi_lossy(b"\x1b(Bplain"), "plain");
        assert_eq!(strip_ansi_lossy(b"\x1b)0graphics"), "graphics");
    }

    #[test]
    fn strip_drops_two_byte_escapes() {
        // `ESC c` (full reset) and friends carry no payload.
        assert_eq!(strip_ansi_lossy(b"\x1bcfresh"), "fresh");
    }

    // ── content_fingerprint: churn vs progress (#398) ─────────────

    #[test]
    fn spinner_frames_share_a_fingerprint() {
        // A spinner repaint changes only the glyph (braille or Claude's
        // star set) and the counters — the letter stream is identical,
        // so successive frames must fingerprint the same.
        let f1 = "\x1b[2K✻ Gusting… (2m 2s · ↓ 7.2k tokens · esc to interrupt)".as_bytes();
        let f2 = "\x1b[2K✦ Gusting… (2m 3s · ↓ 7.4k tokens · esc to interrupt)".as_bytes();
        let f3 = "\x1b[2K⠋ Gusting… (2m 4s · ↓ 7.9k tokens · esc to interrupt)".as_bytes();
        assert_eq!(content_fingerprint(f1), content_fingerprint(f2));
        assert_eq!(content_fingerprint(f2), content_fingerprint(f3));
    }

    #[test]
    fn real_content_changes_the_fingerprint() {
        let spinner = "✻ Working (12s · esc to interrupt)".as_bytes();
        let prose = "✻ Working (13s · esc to interrupt)\nWrote src/main.rs".as_bytes();
        assert_ne!(content_fingerprint(spinner), content_fingerprint(prose));
    }

    #[test]
    fn letterless_churn_has_no_fingerprint() {
        // Pure cursor/erase churn, a clock, a progress bar: no letters,
        // so no fingerprint — never counts as meaningful.
        assert_eq!(content_fingerprint(b"\x1b[1;2H\x1b[2K"), None);
        assert_eq!(content_fingerprint(b"12:03:45"), None);
        assert_eq!(content_fingerprint("███░░ 45%".as_bytes()), None);
        assert_eq!(content_fingerprint("⠋".as_bytes()), None);
    }

    #[test]
    fn strip_handles_truncated_sequences_without_panicking() {
        // Half-arrived chunks are the common case on a live PTY. None of
        // these may panic; the dangling escape is consumed to the end.
        assert_eq!(strip_ansi_lossy(b"text\x1b"), "text"); // lone trailing ESC
        assert_eq!(strip_ansi_lossy(b"text\x1b[31"), "text"); // CSI, no final byte
        assert_eq!(strip_ansi_lossy(b"text\x1b]0;unterminated"), "text"); // OSC, no terminator
        assert_eq!(strip_ansi_lossy(b"text\x1bPdcs-no-st"), "text"); // DCS, no terminator
    }

    #[test]
    fn strip_replaces_invalid_utf8_without_panicking() {
        // The lossy fallback arm: a stray continuation byte (0x80) is not
        // valid UTF-8 and must become U+FFFD, not panic or truncate.
        assert_eq!(strip_ansi_lossy(b"ab\x80cd"), "ab\u{fffd}cd");
        // Valid multi-byte glyphs still survive the (happy) move path.
        assert_eq!(strip_ansi_lossy("héllo".as_bytes()), "héllo");
    }

    #[test]
    fn ready_vetoes_trust_folder_prompt_matched_space_free() {
        // The trust dialog's footer arrives spaceless from the status bar;
        // the veto must still fire so a paste can't land in it.
        assert!(!claude_ready_for_prompt(
            b"Do you trust the files in this folder?\n? for shortcuts"
        ));
    }

    #[test]
    fn ascii_chooser_arrow_detects_any_option_and_ignores_short_buffers() {
        assert!(has_ascii_chooser_arrow("> 1. Yes"));
        assert!(has_ascii_chooser_arrow("foo\n> 3) No"));
        assert!(!has_ascii_chooser_arrow(">1.")); // no space — not the arrow
        assert!(!has_ascii_chooser_arrow("> a.")); // not a digit
        assert!(!has_ascii_chooser_arrow(">")); // shorter than the 4-byte window
    }

    #[test]
    fn numbered_chooser_requires_both_first_and_second_option() {
        assert!(has_numbered_chooser_options("1. Yes\n2. No"));
        assert!(has_numbered_chooser_options("1) a\n2) b"));
        assert!(!has_numbered_chooser_options("1. only one option")); // no `2.`/`2)`
    }

    #[test]
    fn numbered_chooser_ignores_version_and_decimal_numbers() {
        // The `N.` of a version string / decimal is followed by another
        // digit, so it must NOT read as an option label. Claude's boot
        // banner carries `Claude Code v2.1.159`, which together with the
        // composer's `❯` prompt glyph used to misfire an idle composer
        // as a live chooser.
        assert!(!has_numbered_chooser_options("Claude Code v2.1.159"));
        assert!(!has_numbered_chooser_options(
            "Opus 4.8 · 1.2k tokens · 7.2k"
        ));
        // A genuine chooser whose options happen to sit beside a version
        // banner still fires — the option `N.` is followed by a space.
        assert!(has_numbered_chooser_options("v2.1.159\n1. Yes\n2. No"));
    }

    #[test]
    fn last_line_pos_returns_start_offset_of_final_match() {
        // Two matching lines — the offset of the LATER one wins, and it's
        // a byte offset comparable against `rfind` in the same string.
        let s = "x marks\nfirst hit\nfiller\nsecond hit\n";
        let pos = last_line_pos(s, |l| l.contains("hit")).unwrap();
        assert_eq!(&s[pos..pos + "second hit".len()], "second hit");
        assert_eq!(last_line_pos(s, |l| l.contains("absent")), None);
    }

    #[test]
    fn chooser_recency_distinguishes_parked_prompt_from_live_chooser() {
        // Parked `❯` prompt in the composer ABOVE a prose numbered list,
        // with the composer footer the most recent marker → scrollback,
        // not a live chooser → Idle. (The dominant #156 false positive.)
        let parked = "Here's the plan:\n1. refactor\n2. test\n❯ ship it\n? for shortcuts";
        assert_eq!(claude_state(parked.as_bytes()), Some(AgentState::Idle));

        // The SAME markers, but the chooser renders BELOW a now-stale
        // composer footer → it's live → InputNeeded.
        let live = "? for shortcuts\nAllow Bash this command?\n❯ 1. Yes\n2. No\nEsc to cancel";
        assert_eq!(claude_state(live.as_bytes()), Some(AgentState::InputNeeded));

        // No composer footer at all (full-screen dialog) → always live.
        let bare = "Allow Bash?\n❯ 1. Yes\n2. No\nEsc to cancel";
        assert_eq!(claude_state(bare.as_bytes()), Some(AgentState::InputNeeded));
    }

    #[test]
    fn mcp_auth_interstitial_reads_as_input_needed() {
        // The exact startup blocker from issue #256: an autonomous spawn
        // can't clear an MCP server's auth gate, so surface it (flash +
        // attention) instead of the run silently dying. Note the warning
        // renders ABOVE the drawn composer footer — a more-recent
        // `? for shortcuts` must NOT read it as cleared.
        let blocked = "⚠ 1 MCP server needs authentication · run /mcp\n? for shortcuts";
        assert_eq!(
            claude_state(blocked.as_bytes()),
            Some(AgentState::InputNeeded)
        );
        // The composer never reports ready while wedged, so the injector
        // can't paste the work prompt into a blocked session.
        assert!(!claude_ready_for_prompt(blocked.as_bytes()));

        // Plural phrasing fires too.
        let plural = "⚠ 2 MCP servers need authentication · run /mcp\n? for shortcuts";
        assert_eq!(
            claude_state(plural.as_bytes()),
            Some(AgentState::InputNeeded)
        );

        // Once the agent got past the gate and is streaming (a live status
        // line painted AFTER the warning), it's no longer pinned.
        let recovered = "⚠ 1 MCP server needs authentication · run /mcp\n✻ (8s · 412 tokens · esc to interrupt)";
        assert_eq!(
            claude_state(recovered.as_bytes()),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn background_shells_running_read_as_working() {
        // #1136: the model turn ended (no `esc to interrupt`), but shells
        // the agent launched are still executing. The resting composer sits
        // below the status line; without the shells signal this settles to
        // Done mid-run. The live frame pairs the status line with exactly
        // one composer footer, so it reads Working.
        let live = "✻ Crunched for 2m 44s · 4 shells still running\n? for shortcuts";
        assert_eq!(claude_state(live.as_bytes()), Some(AgentState::Working));

        // Singular phrasing ("1 shell still running") fires too.
        let single = "✻ Crunched for 12s · 1 shell still running\n? for shortcuts";
        assert_eq!(claude_state(single.as_bytes()), Some(AgentState::Working));

        // Bypass-mode footer instead of `? for shortcuts`.
        let bypass = "✻ Crunched for 30s · 2 shells still running\nbypass permissions on (shift+tab to cycle)";
        assert_eq!(claude_state(bypass.as_bytes()), Some(AgentState::Working));
    }

    #[test]
    fn stale_shells_line_does_not_pin_working() {
        // The shells finished and Claude repainted the composer again, so a
        // SECOND `? for shortcuts` now sits below the now-stale status line.
        // Two footers after it → scrollback, not a live frame → back to
        // Idle so the quiet timer can settle it to Done.
        let stale = "✻ Crunched for 2m 44s · 4 shells still running\n? for shortcuts\n✓ all shells finished — summary below\n? for shortcuts";
        assert_eq!(claude_state(stale.as_bytes()), Some(AgentState::Idle));
    }

    #[test]
    fn a_prompt_still_wins_over_background_shells() {
        // The shells-Working branch sits AFTER every InputNeeded branch, so
        // a screen that independently reads InputNeeded stays InputNeeded
        // even with shells running. Uses the interstitial shape (guaranteed
        // InputNeeded) so the test asserts branch ordering, not a fragile
        // hand-built dialog.
        let blocked = "⚠ 1 MCP server needs authentication · run /mcp\n✻ 2 shells still running\n? for shortcuts";
        assert_eq!(
            claude_state(blocked.as_bytes()),
            Some(AgentState::InputNeeded)
        );
    }

    #[test]
    fn prose_about_a_running_shell_is_not_working() {
        // #1136 regression: the shells signal must anchor on the live
        // status bar (a spinner glyph), NOT two bare substrings. An agent's
        // own end-of-turn prose that happens to mention a shell "still
        // running" carries no spinner glyph, so it must stay Idle — pinning
        // it to Working would be the false-Working mirror of the bug being
        // fixed and would stall the settle-gated inject path forever.
        let adjacent = "I started the dev server; the shell is still running in the background.\n? for shortcuts";
        assert_eq!(claude_state(adjacent.as_bytes()), Some(AgentState::Idle));

        // The two substrings need not even be adjacent — a bare
        // `contains("shell") && contains("stillrunning")` matched this too.
        let split =
            "Reinstalled the shells and left the smoke tests still running.\n? for shortcuts";
        assert_eq!(claude_state(split.as_bytes()), Some(AgentState::Idle));
    }

    #[test]
    fn background_shells_ticking_below_footer_reads_working() {
        // The realistic live shape: the status line re-paints (spinner
        // animating / count changing) AFTER the composer footer was drawn
        // once, so the newest shells tick lands BELOW the footer in the
        // append-only buffer. It is still the most recent status frame and
        // carries no footer beneath it → Working. (A bare positional compare
        // against the footer would already pass here; this pins the shape so
        // a future refactor can't regress it.)
        let ticking = "✻ Crunched for 2m 44s · 4 shells still running\n? for shortcuts\n✻ Crunched for 2m 45s · 4 shells still running";
        assert_eq!(claude_state(ticking.as_bytes()), Some(AgentState::Working));
    }

    #[test]
    fn newer_status_frame_supersedes_stale_shells() {
        // The stale case the footer count alone missed: the shells line was
        // re-painting below its footer, then drained and Claude painted a
        // DIFFERENT `✻ …` status frame (here: compacting) over it — one
        // footer, not two, sits below the stale shells line. The status-line
        // recency guard catches it because a newer spinner-status line now
        // outranks the shells line, so the agent no longer reads Working off
        // a scrollback shells line.
        let superseded = "✻ Crunched for 2m 44s · 4 shells still running\n✻ Compacting conversation…\n? for shortcuts";
        assert_eq!(claude_state(superseded.as_bytes()), Some(AgentState::Idle));
    }

    #[test]
    fn usage_limit_prompt_reads_as_limit_reached() {
        // The #847 block: Claude hit its usage cap and paused on the
        // "limit reached — Wait?" prompt. It must classify as the distinct
        // `LimitReached`, NOT a generic `InputNeeded`, even though the
        // prompt renders a numbered Wait/Exit chooser (which alone would
        // read `InputNeeded`). The reset countdown rides along but isn't
        // required for the match. A live block REPLACES the composer, so
        // there's no resting footer beneath the chooser.
        let blocked =
            "Claude usage limit reached ∙ resets 3pm\n❯ 1. Wait until it resets\n  2. Exit";
        assert_eq!(
            claude_state(blocked.as_bytes()),
            Some(AgentState::LimitReached),
        );
        // A limit-blocked composer never reports ready — a pasted prompt
        // would be eaten by the Wait/Exit gate.
        assert!(!claude_ready_for_prompt(blocked.as_bytes()));

        // The "reached your usage limit" phrasing fires the same way.
        let alt = "You've reached your usage limit for the month.";
        assert_eq!(claude_state(alt.as_bytes()), Some(AgentState::LimitReached));
    }

    #[test]
    fn weekly_limit_rate_limit_options_menu_reads_as_limit_reached() {
        // #1337: the current weekly-limit banner uses "hit" (not "reached")
        // and a "weekly" period, then drops into the `/rate-limit-options`
        // numbered menu. None of the original phrases caught it, so the
        // block read as a generic `InputNeeded` chooser and Shift-K found
        // nothing to resume. The exact banner + menu must classify as
        // `LimitReached`.
        let blocked = "You've hit your weekly limit · resets Aug 30 at 2pm (America/New_York)\n\n\
             /rate-limit-options\n\n\
             What do you want to do?\n\
             ❯ 1. Stop and wait for limit to reset\n  \
             2. Switch to usage credits\n  \
             3. Switch to Team plan";
        assert_eq!(
            claude_state(blocked.as_bytes()),
            Some(AgentState::LimitReached),
        );
        assert!(!claude_ready_for_prompt(blocked.as_bytes()));
        // The date-style reset is extracted for the badge hint.
        assert_eq!(
            parse_usage_limit_reset(blocked.as_bytes()),
            Some("aug 30 at 2pm".into()),
        );
    }

    /// The auto-continue form of the block, verbatim from a spend-capped
    /// team seat: no chooser — Claude parks itself and resumes at reset,
    /// and the composer stays live beneath the banner (typing cancels).
    /// The resting footer painted under it therefore does NOT make the
    /// banner stale, and the shape must classify as the calm
    /// `AwaitingReset` — not `Idle` (the bug: every such block
    /// quiet-settled to `Done` and lazybox showed nothing), and not the
    /// alerting `LimitReached` (nothing to press, and auto-Wait's Enter
    /// would land in a cancel-on-keypress composer).
    #[test]
    fn auto_continue_limit_banner_reads_as_awaiting_reset() {
        let parked = "⎿  You've hit your individual spend limit · run /usage-credits to ask \
             your admin for a higher limit · your session limit resets 1:10pm \
             (America/New_York)\n\
             /usage-credits to request more usage from your admin.\n\n\
             ⏺ Usage limit reached · continuing automatically at 1:10pm · esc or type to cancel\n\n\
             ✻ Brewed for 3m 28s · d\n\n\
             ❯ \n\
             ? for shortcuts";
        assert_eq!(
            claude_state(parked.as_bytes()),
            Some(AgentState::AwaitingReset),
        );
        // A keystroke cancels the wait, so the composer is not injectable.
        assert!(!claude_ready_for_prompt(parked.as_bytes()));
        // The badge hint comes from the banner's own `resets 1:10pm`.
        assert_eq!(
            parse_usage_limit_reset(parked.as_bytes()),
            Some("1:10pm".into()),
        );

        // Only the auto-continue line left in the window (the spend-limit
        // line scrolled out): still parked, and the hint falls back to the
        // `continuing automatically at …` time.
        let tail = "⏺ Usage limit reached · continuing automatically at 1:10pm · esc or type to cancel\n\
             bypass permissions on (shift+tab to cycle)";
        assert_eq!(
            claude_state(tail.as_bytes()),
            Some(AgentState::AwaitingReset),
        );
        assert_eq!(
            parse_usage_limit_reset(tail.as_bytes()),
            Some("1:10pm".into()),
        );

        // The reset happened and Claude resumed: a live working line
        // painted after the banner clears it.
        let resumed = format!("{parked}\n✻ Working (3s · esc to interrupt)");
        assert_eq!(claude_state(resumed.as_bytes()), Some(AgentState::Working),);

        // The spend-limit wording on its own (an older build that still
        // parks on the chooser) is the alerting block.
        let chooser = "You've hit your individual spend limit · resets 1:10pm\n\
             ❯ 1. Wait until it resets\n  2. Exit";
        assert_eq!(
            claude_state(chooser.as_bytes()),
            Some(AgentState::LimitReached),
        );
    }

    #[test]
    fn spend_limit_auto_continue_banner_reads_as_awaiting_reset() {
        // #1452 discovered these newer individual-spend-limit / session-limit
        // banners: "individual spend limit" / "session limit resets" match
        // none of the older phrases, so the block read as generic Idle
        // output. #1504 refines the classification — the auto-continue FOOTER
        // means Claude resumes on its own, so it is the calm `AwaitingReset`,
        // NOT the alerting `LimitReached` (whose opt-in auto-`Wait` keystroke
        // would land in a composer where a keystroke cancels the wait). The
        // reset time is still mined from `resets 3:10pm`.
        let blocked = "You've hit your individual spend limit · run /usage-credits to ask your\n\
             admin for a higher limit · your session limit resets 3:10pm (America/New_York)\n\
             Continuing automatically at 3:10pm · esc to cancel";
        assert_eq!(
            claude_state(blocked.as_bytes()),
            Some(AgentState::AwaitingReset),
        );
        assert!(!claude_ready_for_prompt(blocked.as_bytes()));
        assert_eq!(
            parse_usage_limit_reset(blocked.as_bytes()),
            Some("3:10pm".into()),
        );

        // The auto-continue variant of the plain usage-limit banner: the
        // `esc or type to cancel` footer must NOT be read as a resting
        // composer that demotes the matched phrase to stale scrollback.
        let auto =
            "Usage limit reached · continuing automatically at 3:10pm · esc or type to cancel";
        assert_eq!(
            claude_state(auto.as_bytes()),
            Some(AgentState::AwaitingReset)
        );
        assert!(!claude_ready_for_prompt(auto.as_bytes()));
        assert_eq!(
            parse_usage_limit_reset(auto.as_bytes()),
            Some("3:10pm".into()),
        );

        // Ordinary prose like "the deploy continues automatically 2h after
        // merge" must never be mined as a reset time: the specific
        // `continuing automatically at/in` keywords don't match it, so only
        // the live footer's `3:10pm` parses.
        let noisy = "the deploy continues automatically 2h after merge\n\
             Usage limit reached · continuing automatically at 3:10pm · esc to cancel";
        assert_eq!(
            parse_usage_limit_reset(noisy.as_bytes()),
            Some("3:10pm".into()),
        );
    }

    #[test]
    fn spend_limit_prose_while_working_is_not_limit_reached() {
        // A bare "spend limit" was NOT added as a trigger phrase precisely
        // so an agent working on billing / cost-cap code (or on #1452
        // itself) isn't misread. Full tmux repaint delivers the prose
        // mention AND the live status bar in one chunk; the same-chunk rule
        // nulls the work anchor, so a broad trigger phrase here would flip
        // to LimitReached and block prompt injection. It must stay Working.
        let buf = "I'll enforce the spend limit in the billing module now.\n\
                   ✻ Working… (5s · ↓ 200 tokens · esc to interrupt)";
        assert_eq!(
            claude_state_chunked(buf.as_bytes(), 0),
            Some(AgentState::Working),
        );
    }

    #[test]
    fn a_usage_limit_phrase_above_a_resting_composer_is_stale_scrollback() {
        // Regression for the false-positive the review caught: a finished
        // turn whose OUTPUT merely mentioned a usage limit, now at rest
        // with the composer footer redrawn BELOW the phrase, must read
        // Idle — not LimitReached (which would flash a spurious pill and,
        // with auto-`Wait` on, submit a stray keystroke into the idle
        // composer). Gating the limit branch on the resting footer, like
        // the STRONG consent phrases, is what suppresses it.
        let stale = "You've reached your usage limit before, but you're fine now.\n? for shortcuts";
        assert_eq!(claude_state(stale.as_bytes()), Some(AgentState::Idle));
        // The bypass-mode footer lazybox actually runs under
        // (`--dangerously-skip-permissions`) suppresses it too.
        let bypass =
            "earlier you reached your usage limit\nbypass permissions on (shift+tab to cycle)";
        assert_eq!(claude_state(bypass.as_bytes()), Some(AgentState::Idle));
        // #1337: the weekly-limit wording gets the same gate — a resting
        // composer redrawn below the banner means the block already cleared.
        let stale_weekly = "You've hit your weekly limit · resets Aug 30 at 2pm\n? for shortcuts";
        assert_eq!(
            claude_state(stale_weekly.as_bytes()),
            Some(AgentState::Idle)
        );
        // #1452: the spend-limit banner wording gets the same gate — a
        // finished turn whose prose merely mentioned the individual spend
        // limit, now at rest, must not flash a spurious block.
        let stale_spend = "You've hit your individual spend limit earlier today.\n? for shortcuts";
        assert_eq!(claude_state(stale_spend.as_bytes()), Some(AgentState::Idle));
    }

    #[test]
    fn parses_reset_time_from_the_limit_banner() {
        // The banner's reset countdown is the "time-to-reset" the proactive
        // indicator surfaces (#1012). The chooser line below it ("Wait
        // until it resets") also holds the word `resets` but is followed by
        // a newline, so the banner's `resets 3pm` is what captures.
        let blocked =
            "Claude usage limit reached ∙ resets 3pm\n❯ 1. Wait until it resets\n  2. Exit";
        assert_eq!(
            parse_usage_limit_reset(blocked.as_bytes()),
            Some("3pm".into())
        );

        // `at` / `in` connectives and a clock time are skipped/kept.
        assert_eq!(
            parse_usage_limit_reset(b"usage limit reached. resets at 3:00pm"),
            Some("3:00pm".into()),
        );
        assert_eq!(
            parse_usage_limit_reset(b"reached your usage limit - resets in 2h"),
            Some("2h".into()),
        );

        // #1337: a calendar-date reset ("resets Aug 30 at 2pm") is read
        // despite leading with a month name rather than a digit. The
        // trailing clock time is optional.
        assert_eq!(
            parse_usage_limit_reset(b"You've hit your weekly limit resets Aug 30 at 2pm"),
            Some("aug 30 at 2pm".into()),
        );
        assert_eq!(
            parse_usage_limit_reset(b"You've hit your weekly limit, resets Sep 3"),
            Some("sep 3".into()),
        );
    }

    #[test]
    fn parses_the_most_recent_reset_when_an_older_one_lingers() {
        // Two limit episodes still inside the ~16 KiB detect window: an
        // older `resets 3pm` above the current banner. The forward scan
        // returned the FIRST match (`3pm`, the stale one); the recency scan
        // must return the CURRENT banner's time instead.
        let two_episodes = "usage limit reached ∙ resets 3pm\n\
             …later, a new episode…\n\
             usage limit reached ∙ resets 5pm";
        assert_eq!(
            parse_usage_limit_reset(two_episodes.as_bytes()),
            Some("5pm".into()),
        );

        // Cross-keyword recency: the current auto-continue banner's own
        // `resets` line scrolled out, leaving only `continuing automatically
        // at 1:10pm`, while a stale `resets 3pm` from a prior block lingers
        // ABOVE it. Keyword-first ("resets" before "continuing") would pick
        // the stale `3pm`; comparing recency across all keywords picks the
        // live `1:10pm`.
        let stale_resets_then_autocontinue = "usage limit reached ∙ resets 3pm\n\
             ⏺ Usage limit reached · continuing automatically at 1:10pm · esc or type to cancel";
        assert_eq!(
            parse_usage_limit_reset(stale_resets_then_autocontinue.as_bytes()),
            Some("1:10pm".into()),
        );
    }

    #[test]
    fn reset_time_degrades_to_none_when_absent_or_unparseable() {
        // No banner at all: never mine a stray "resets" out of scrollback.
        assert_eq!(
            parse_usage_limit_reset(b"git resets the branch to HEAD"),
            None
        );
        // Banner present but a phrasing the conservative scan can't read —
        // the documented degraded path (block shows without a countdown).
        assert_eq!(
            parse_usage_limit_reset("usage limit reached ∙ resets tomorrow".as_bytes()),
            None,
        );
        assert_eq!(parse_usage_limit_reset(b"usage limit reached"), None);
    }

    #[test]
    fn usage_limit_clears_once_the_agent_resumes() {
        // After a re-auth / reset the agent streams again: a live working
        // status line painted AFTER the banner supersedes the block, so it
        // no longer pins `LimitReached`.
        let recovered =
            "Claude usage limit reached ∙ resets 3pm\n✻ (8s · 412 tokens · esc to interrupt)";
        assert_eq!(
            claude_state(recovered.as_bytes()),
            Some(AgentState::Working),
        );
    }

    #[test]
    fn ordinary_prose_mentioning_a_limit_is_not_limit_reached() {
        // The phrase must be distinctive enough that chat prose about
        // limits doesn't trip it — only the exact banner wording matches.
        let prose = "I checked and you have not reached any limit yet.\n? for shortcuts";
        assert_eq!(claude_state(prose.as_bytes()), Some(AgentState::Idle));

        // #1337: the weekly-limit phrase is anchored on the banner's full
        // "hit your weekly limit" — a bare "weekly limit" mention (a
        // rate-limiter the agent just wrote about) must NOT read as a block.
        let bare = "The GitHub API weekly limit is 5000 requests.\n? for shortcuts";
        assert_eq!(claude_state(bare.as_bytes()), Some(AgentState::Idle));
    }

    #[test]
    fn chooser_pos_is_the_most_recent_marker() {
        // `chooser_pos` is only ever called with the compacted (space-free)
        // buffer, so these cases are shaped that way.
        assert_eq!(chooser_pos("nomarkershere"), None);
        // Arrow after the numbered options → arrow offset wins.
        let s = "1.a\n2.b\n❯x";
        assert_eq!(chooser_pos(s), s.rfind('❯'));
        // Version/decimal `N.` is NOT a marker — shares the digit guard
        // with `has_option_marker`, so it can't disagree with `has_chooser`.
        assert_eq!(chooser_pos("opus4.8·v2.1.159"), None);
        // An ASCII-only chooser still anchors on its numbered options once
        // the arrow's space has been compacted away.
        let ascii = "allowbash?\n>1.yes\n>2.no";
        assert_eq!(chooser_pos(ascii), ascii.rfind("2."));
    }

    #[test]
    fn working_status_position_reflects_recency_order() {
        // The recency guard in `claude_state_of` compares these two
        // offsets in the space-free buffer, as the callers pass it.
        // Status line AFTER the idle footer → working is the more recent.
        let work_recent =
            compact_lower("esc to cancel · tab to amend\n✻ (8s · 412 tokens · esc to interrupt)");
        assert!(working_status_pos(&work_recent) > idle_box_pos(&work_recent));
        // Idle footer AFTER a now-stale status line → footer is the more
        // recent, so the agent reads as done rather than forever-busy.
        let idle_recent = compact_lower("✻ (esc to interrupt)\ndone\n? for shortcuts");
        assert!(working_status_pos(&idle_recent) < idle_box_pos(&idle_recent));
    }

    #[test]
    fn bypass_mode_idle_footer_evicts_stale_working_status() {
        // #179: lazybox spawns Claude with `--dangerously-skip-permissions`,
        // so its idle composer footer is the bypass-mode mode line —
        // `bypass permissions on (shift+tab to cycle) · ← for agents` —
        // NOT `? for shortcuts`. A just-finished agent's last
        // `esc to interrupt` status line still sits earlier in the
        // append-only buffer; the more-recent bypass footer must register
        // as an idle anchor so the working glyph returns to idle instead
        // of sticking ON forever. Bytes carry the cursor-positioned
        // spacing tmux produces (`(shift+tab` ‖ `to` ‖ `cycle)`).
        let buf = concat!(
            "\x1b[24;1H\x1b[2K\x1b[35m✻\x1b[0m \x1b[1mCogitating\x1b[0m… ",
            "\x1b[2m(7s · ↑ 318 tokens · esc to interrupt)\x1b[0m",
            "\x1b[40;1H\x1b[2K\x1b[7mT\x1b[27m\x1b[2mry \"how do I log an error?\"\x1b[22m",
            "\x1b[41;3H\x1b[38;5;211mbypass\x1b[10Gpermissions\x1b[22Gon",
            "\x1b[38;5;246m (shift+tab\x1b[36Gto\x1b[39Gcycle)\x1b[49G · \x1b[53Gfor\x1b[57Gagents\x1b[0m",
        );
        assert_eq!(claude_state(buf.as_bytes()), Some(AgentState::Idle));

        // The bypass footer is also a composer-visible signal, so the
        // spawn-time injector treats the screen as ready for a paste.
        assert!(claude_ready_for_prompt(buf.as_bytes()));
    }

    #[test]
    fn compact_lower_strips_spaces_and_lowercases_but_keeps_newlines() {
        assert_eq!(compact_lower("? For Shortcuts"), "?forshortcuts");
        assert_eq!(compact_lower("Esc to cancel"), "esctocancel");
        assert_eq!(compact_lower("a b\nc d"), "ab\ncd");
    }

    #[test]
    fn last_compact_match_pos_matches_across_stripped_spacing() {
        // The live footer arrives spaceless; the constant table is
        // written with spaces. Both compact to the same form.
        let compact = compact_lower("…doyouwanttoproceed?…");
        assert!(last_compact_match_pos(&compact, &["Do you want to proceed?"]).is_some());
        assert_eq!(
            last_compact_match_pos(&compact, &["Do you want to delete"]),
            None
        );
        // The offset is the most recent match across the whole table, so
        // it's comparable against the other recency anchors.
        let two = compact_lower("do you want to proceed?\nlater do you want to retry?");
        let pos = last_compact_match_pos(&two, CLAUDE_STANDALONE_PROMPT_PHRASES).unwrap();
        assert_eq!(pos, two.rfind("doyouwanttoretry").unwrap());
    }

    #[test]
    fn standalone_consent_phrase_in_scrollback_is_gated_by_idle_footer() {
        // #191: a finished turn whose detect window still holds a
        // standalone consent phrase ABOVE the most-recent composer footer
        // is stale scrollback, not a live prompt → Idle. The same phrase
        // with no footer below it (a real prompt) still fires.
        let stale = "Do you want to proceed?\nProceeding…\ndone\n? for shortcuts";
        assert_eq!(claude_state(stale.as_bytes()), Some(AgentState::Idle));

        let live = "? for shortcuts\nDo you want to proceed?";
        assert_eq!(claude_state(live.as_bytes()), Some(AgentState::InputNeeded));

        // No composer footer at all → the phrase is the only marker → live.
        let bare = "some bash output\nDo you want to proceed?";
        assert_eq!(claude_state(bare.as_bytes()), Some(AgentState::InputNeeded));
    }

    #[test]
    fn paired_yes_no_in_scrollback_is_gated_by_idle_footer() {
        // #191: the bare yes/no pairing (`Do you want…` + `1. Yes`) left
        // above the most-recent composer footer is stale → Idle; the same
        // pairing below a stale footer is a live prompt → InputNeeded.
        // `Approve` is a paired question marker but NOT a standalone
        // phrase, so this exercises the paired branch's gate specifically.
        let stale = "Approve this command?\n1. Yes\n2. No\nok done\n? for shortcuts";
        assert_eq!(claude_state(stale.as_bytes()), Some(AgentState::Idle));

        let live = "? for shortcuts\nApprove this?\n1. Yes\n2. No";
        assert_eq!(claude_state(live.as_bytes()), Some(AgentState::InputNeeded));
    }

    #[test]
    fn resting_composer_pos_excludes_tab_to_amend() {
        // `? for shortcuts` / bypass anchor; `Tab to amend` does NOT, since
        // it's also the live command-approval dialog footer.
        assert!(resting_composer_pos(&compact_lower("? for shortcuts")).is_some());
        assert!(
            resting_composer_pos(&compact_lower("bypass permissions on (shift+tab to cycle)"))
                .is_some()
        );
        assert_eq!(
            resting_composer_pos(&compact_lower(
                "Esc to cancel · Tab to amend · ctrl+e to explain"
            )),
            None,
        );
    }

    #[test]
    fn bash_approval_with_amend_footer_is_input_needed() {
        // The real false NEGATIVE: a live Bash-command approval whose
        // footer is the full `Esc to cancel · Tab to amend · ctrl+e to
        // explain` form (NOT the short `Esc to cancel` the detector once
        // assumed). The footer carries `Tab to amend`, so `idle_box_pos`
        // mistook it for an idle composer and the recency gate read the
        // live `❯ 1. Yes` chooser as stale scrollback → Idle, dropping the
        // `?`. The standalone consent phrase fires on the reliable resting
        // anchor (absent here) and rescues it.
        let buf = concat!(
            "Bash command\n",
            "  for c in git-ops gh-provider; do echo $c; done\n",
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel · Tab to amend · ctrl+e to explain",
        );
        assert_eq!(claude_state(buf.as_bytes()), Some(AgentState::InputNeeded));
        assert!(!claude_ready_for_prompt(buf.as_bytes()));
    }

    #[test]
    fn injected_prompt_list_above_amend_footer_stays_idle() {
        // The guard the `Tab to amend` anchor must keep: a multi-line
        // prompt (numbered list) sitting in the composer before submit —
        // or a prose plan the agent printed — wears the `Tab to amend`
        // footer too, but carries NO consent phrase and NO `1. Yes`. The
        // weak chooser shapes gate on `idle_box_pos` (incl. `Tab to
        // amend`), so this stays Idle instead of flashing a spurious `?`.
        let buf = concat!(
            "Here's the plan:\n",
            "  1. Refactor the parser\n",
            "  2. Add tests\n",
            "│ > \n",
            "Esc to cancel · Tab to amend · ctrl+e to explain",
        );
        assert_eq!(claude_state(buf.as_bytes()), Some(AgentState::Idle));
    }

    #[test]
    fn classify_records_which_branch_fired() {
        // The instrumentation (issue #2): the trigger names the branch so
        // a misclassification is bisectable from the trace. Each shape
        // exercises a distinct tier.
        let trigger = |s: &str| classify(s, &compact_lower(s), None).trigger;

        // Weak structural chooser (arrow + options, no resting footer).
        assert_eq!(
            trigger("Allow Bash?\n❯ 1. Yes\n2. No\nEsc to cancel"),
            Some(Trigger::StructuralChooser),
        );
        // Weak esc-to-cancel footer (arrow-less numbered dialog).
        assert_eq!(
            trigger("1. Approve\n2. Skip\n3. Cancel\nEsc to cancel"),
            Some(Trigger::EscToCancelFooter),
        );
        // Strong standalone consent phrase, and it names the entry.
        let d = classify(
            "Do you want to overwrite the file?",
            &compact_lower("Do you want to overwrite the file?"),
            None,
        );
        assert_eq!(d.trigger, Some(Trigger::StandalonePhrase));
        assert_eq!(d.matched_phrase, Some("do you want to overwrite"));
        // Strong paired question + yes/no (`Approve` is not a standalone).
        assert_eq!(
            trigger("Approve this command?\n1. Yes\n2. No"),
            Some(Trigger::PairedYesNo),
        );
        // Idle / Working carry no InputNeeded trigger.
        assert_eq!(trigger("just plain output"), None);
        assert_eq!(
            trigger("✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)"),
            None,
        );
    }

    #[test]
    fn working_counter_requires_affirmative_shape() {
        // The full live shape — spinner + elapsed + token counter —
        // anchors working.
        let live = compact_lower("✻ Simmering… (10s · ↓ 137 tokens)");
        assert!(working_status_pos(&live).is_some());
        let live2 = compact_lower("✦ Gusting… (2m 2s · ↓ 7.2k tokens · thinking some more)");
        assert!(working_status_pos(&live2).is_some());
        // `esc to interrupt` alone is affirmative too.
        assert!(working_status_pos(&compact_lower("✻ (esc to interrupt)")).is_some());

        // A line that merely mentions `tokens` beside a `·` separator —
        // a tool preview or a dialog's command echo — is NOT a working
        // anchor. Pre-tightening it suppressed live dialog markers on a
        // full repaint.
        let echo = compact_lower("Bash(echo 'count · 137 tokens used')");
        assert_eq!(working_status_pos(&echo), None);
        // Spinner but no elapsed counter: still not affirmative.
        let no_elapsed = compact_lower("✻ summary · 137 tokens total");
        assert_eq!(working_status_pos(&no_elapsed), None);
    }

    #[test]
    fn prose_mentioning_esc_to_interrupt_is_not_working() {
        // Regression: `working_status_pos` matched the bare substring
        // `esctointerrupt` anywhere, so prose or a commit message that
        // mentioned it pinned the agent to Working forever.
        assert_eq!(
            claude_state(b"? for shortcuts\nYou can press esc to interrupt me while I work.\n"),
            Some(AgentState::Idle),
        );
        assert_eq!(
            working_status_pos(&compact_lower("Add esc to interrupt handler to the runner")),
            None,
        );
        // But the real status-bar shapes still anchor working — including
        // the lone interrupt line with NO counter/spinner on it (the
        // captured wire shape: a thinking-level dot follows the hint).
        // The earlier "require scaffolding on the same line" form wrongly
        // read these as Idle while the agent was busy.
        assert!(working_status_pos(&compact_lower("✻ (esc to interrupt)")).is_some());
        assert!(working_status_pos(&compact_lower("esc to interrupt●high")).is_some());
        assert_eq!(
            claude_state("⏺ Compacting conversation…\nesc to interrupt●high".as_bytes()),
            Some(AgentState::Working),
        );
    }

    #[test]
    fn markdown_bullet_with_timer_is_not_working() {
        // Regression: `*` was in the spinner-glyph set, so a markdown
        // bullet that happened to carry a parenthesized timer + "tokens"
        // read as a live counter line.
        assert_eq!(
            working_status_pos(&compact_lower("* Done in (2s · 5 tokens)")),
            None,
        );
        assert_eq!(
            claude_state("? for shortcuts\n* Done in (2s · 5 tokens)\n".as_bytes()),
            Some(AgentState::Idle),
        );
    }

    #[test]
    fn prose_numbered_list_without_anchor_is_not_input_needed() {
        // Regression: with no composer footer and no working line in the
        // window, a bare arrow + numbered list read as a live chooser and
        // fired InputNeeded (+ a desktop notification). An agent merely
        // printing options must stay Idle unless a real dialog signal
        // (consent phrase / choice marker / Esc-to-cancel) corroborates.
        assert_eq!(
            claude_state(b"Here are the options:\n1. first\n2. second\n\xe2\x9d\xaf pick one\n"),
            Some(AgentState::Idle),
        );
        // A real dialog carrying a consent question is still detected.
        assert_eq!(
            claude_state("Do you want to proceed?\n\u{276f} 1. Yes\n2. No\n".as_bytes()),
            Some(AgentState::InputNeeded),
        );
        // A real chooser with CUSTOM labels (no "Yes", no question
        // phrase, no Esc-to-cancel in window) is still detected: the
        // arrow sits directly on a numbered option, which prose never
        // produces. Without this corroboration it wrongly read Idle.
        assert_eq!(
            claude_state("Which approach?\n\u{276f} 1. Rewrite\n2. Patch\n3. Skip\n".as_bytes()),
            Some(AgentState::InputNeeded),
        );
    }

    #[test]
    fn same_chunk_full_repaint_keeps_dialog_over_statusbar() {
        // A tmux full repaint delivers the live dialog AND the bottom
        // status bar in ONE chunk, status bar last. Position alone reads
        // the dialog as already answered; the chunk hint must keep it
        // live.
        let repaint = concat!(
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel\n",
            "✻ Simmering… (10s · ↓ 137 tokens · esc to interrupt)",
        );
        // Whole buffer arrived as one chunk → InputNeeded.
        assert_eq!(
            claude_state_chunked(repaint.as_bytes(), 0),
            Some(AgentState::InputNeeded)
        );
        // The ordering rule is intact when the work anchor arrives in a
        // LATER chunk than the dialog: the agent moved past the prompt.
        let boundary = repaint.rfind('✻').unwrap();
        assert_eq!(
            claude_state_chunked(repaint.as_bytes(), boundary),
            Some(AgentState::Working)
        );
        // No chunk hint at all keeps the pure positional rule.
        assert_eq!(claude_state(repaint.as_bytes()), Some(AgentState::Working));
    }

    #[test]
    fn same_chunk_live_counter_beats_scrollback_chooser() {
        // #96: right after `w`, a tmux full repaint delivers scrollback
        // that LOOKS chooser-shaped (a git log whose `(#91)`/`(#92)`/`(#93)`
        // refs compact to `1)`/`2)`/`3)` option markers, plus the composer
        // `❯`) AND the live ticking status line — all in ONE chunk. The
        // same-chunk rule lets a dialog out-rank the status bar, so the
        // weak chooser wrongly fired InputNeeded while Claude was just
        // running commands. A live counter line (spinner + elapsed timer +
        // token counter) is never painted beneath a live gate, so its
        // presence below the markers proves the agent is working.
        let repaint = concat!(
            "  04c758e Cache-bust README hero image\n",
            "  0704b78 Refresh README (#91)\n",
            "  f1a0773 Instrument event-pipeline counters (#92) (#93)\n",
            "  … +19 lines (ctrl+o to expand)\n",
            "❯\n",
            "✳ Clauding… (52s · ↑ 2.0k tokens)",
        );
        // Whole buffer arrived as one chunk → must read Working, not the
        // weak chooser's InputNeeded.
        assert_eq!(
            claude_state_chunked(repaint.as_bytes(), 0),
            Some(AgentState::Working)
        );
        // The pure positional path agrees (the counter is most recent).
        assert_eq!(claude_state(repaint.as_bytes()), Some(AgentState::Working));

        // The guard the same-chunk rule still keeps: a STRONG consent
        // phrase in the same repaint is a real dialog and stays
        // InputNeeded even with a (frozen) status preview below it.
        let real_dialog = concat!(
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel\n",
            "✳ Clauding… (52s · ↑ 2.0k tokens)",
        );
        assert_eq!(
            claude_state_chunked(real_dialog.as_bytes(), 0),
            Some(AgentState::InputNeeded)
        );
    }

    #[test]
    fn working_supersedes_dialog_requires_activity_after_markers() {
        // Working anchor painted AFTER the dialog markers → the dialog
        // was answered, demotion is evidenced.
        let answered = concat!(
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel\n",
            "user picked yes\n",
            "✻ Simmering… (10s · ↓ 137 tokens · esc to interrupt)",
        );
        assert!(claude_working_supersedes_dialog(answered.as_bytes()));

        // A bare working line with NO dialog markers in the window
        // proves nothing about the dialog the hook reported → false.
        assert!(!claude_working_supersedes_dialog(
            "✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)".as_bytes()
        ));
        // No working anchor at all → false.
        assert!(!claude_working_supersedes_dialog(
            "Do you want to proceed?\n❯ 1. Yes\n2. No".as_bytes()
        ));
        // Dialog markers more recent than the working anchor → the
        // dialog is the live thing on screen → false.
        let dialog_last = concat!(
            "✻ Simmering… (10s · ↓ 137 tokens)\n",
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel",
        );
        assert!(!claude_working_supersedes_dialog(dialog_last.as_bytes()));
    }

    #[test]
    fn marked_strip_and_compact_translate_offsets() {
        // The mark lands after the escape-laden prefix; both passes must
        // map it into their output offset spaces.
        let bytes = b"\x1b[31mAB CD\x1b[0mEF";
        let mark = bytes.iter().position(|b| *b == b'E').unwrap();
        let (s, s_mark) = strip_ansi_lossy_marked(bytes, mark);
        assert_eq!(s, "AB CDEF");
        assert_eq!(&s[s_mark..], "EF");
        let (compact, c_mark) = compact_lower_marked(&s, s_mark);
        assert_eq!(compact, "abcdef");
        assert_eq!(&compact[c_mark..], "ef");
        // Mark at / past the end clamps to the end.
        assert_eq!(strip_ansi_lossy_marked(b"xy", 99).1, 2);
        assert_eq!(compact_lower_marked("xy", 99).1, 2);
    }

    #[test]
    fn compact_tail_is_bounded_and_char_aligned() {
        assert_eq!(compact_tail("short"), "short");
        let long = "x".repeat(500);
        assert_eq!(compact_tail(&long).len(), 200);
        // Snapping forward to a char boundary never panics on multi-byte
        // glyphs straddling the cut.
        let multi = format!("{}❯ 1. yes", "a".repeat(199));
        let tail = compact_tail(&multi);
        assert!(tail.len() <= 200);
    }

    #[test]
    fn work_anchor_for_suppresses_only_same_chunk_anchor() {
        // Both the prompt marker and the work anchor landed in the most
        // recent chunk (offset >= start) → the work anchor is nulled so a
        // same-repaint status line can't out-rank the live prompt.
        assert_eq!(work_anchor_for(Some(10), Some(20), Some(5)), None);
        // The marker predates the chunk boundary → positional rule stands,
        // the work anchor is returned unchanged.
        assert_eq!(work_anchor_for(Some(3), Some(20), Some(5)), Some(20));
        // No chunk hint → always positional.
        assert_eq!(work_anchor_for(Some(10), Some(20), None), Some(20));
        // No work anchor at all → nothing to return regardless of the hint.
        assert_eq!(work_anchor_for(Some(10), None, Some(5)), None);
    }

    #[test]
    fn codex_footer_pos_anchors_on_middle_dot_then_path() {
        // The `<model> <effort> · <cwd>` composer footer, compacted: the
        // U+00B7 separator immediately followed by an absolute or home path.
        assert!(codex_footer_pos("gpt-5.5xhigh·/repo").is_some());
        assert!(codex_footer_pos("gpt-5.5xhigh·~/proj").is_some());
        // A U+00B7 used as a status-line separator (`· esc to interrupt`)
        // carries no path after it, so it is NOT a footer.
        assert_eq!(codex_footer_pos("•running(1s·esctointerrupt)"), None);
        // Recency: the most recent footer wins.
        let two = "·/old\nwork\n·/new";
        assert_eq!(codex_footer_pos(two), two.rfind("·/new"));
    }

    #[test]
    fn codex_model_effort_extracts_model_and_effort_from_footer() {
        // The footer keeps real spaces; the rotating placeholder is glued
        // directly onto the model name (no separating byte survives), so the
        // model is cut from its own prefix. Shapes taken from the real
        // fixtures (`codex_real_*`).
        assert_eq!(
            codex_model_effort(b"...@filenamegpt-5.5 xhigh \xc2\xb7 /private/tmp/x"),
            Some("gpt-5.5 · xhigh".to_string())
        );
        assert_eq!(
            codex_model_effort(b"Summarize recent commitsgpt-5.6-sol max \xc2\xb7 /repo"),
            Some("gpt-5.6-sol · max".to_string())
        );
        assert_eq!(
            codex_model_effort(b"...gpt-5.5 default \xc2\xb7 /repo"),
            Some("gpt-5.5 · default".to_string())
        );
        // Home-relative cwd anchors too.
        assert_eq!(
            codex_model_effort(b"gpt-5.5 high \xc2\xb7 ~/proj"),
            Some("gpt-5.5 · high".to_string())
        );
    }

    #[test]
    fn codex_model_effort_trims_multi_space_padding_from_the_model() {
        // `rsplit_once` only drops the final separator, so a footer that
        // pads model→effort with more than one space would otherwise leak
        // the extra spaces into the model name (`"gpt-5.5   · xhigh"`).
        assert_eq!(
            codex_model_effort(b"gpt-5.5   xhigh \xc2\xb7 /repo"),
            Some("gpt-5.5 · xhigh".to_string())
        );
        assert_eq!(
            codex_model_effort(b"...@filegpt-5.6-sol  \t max \xc2\xb7 /repo"),
            Some("gpt-5.6-sol · max".to_string())
        );
    }

    #[test]
    fn codex_model_effort_is_none_without_a_recognised_footer() {
        // No footer at all.
        assert_eq!(codex_model_effort(b"just some agent output"), None);
        // A `word · /path` line whose trailing token isn't a known effort
        // must not be mistaken for the footer.
        assert_eq!(
            codex_model_effort(b"see the file gpt-5.5 \xc2\xb7 /path"),
            None
        );
        assert_eq!(
            codex_model_effort(b"gpt-5.5 turbocharged \xc2\xb7 /repo"),
            None
        );
        // A middle-dot not followed by a path (the status-line separator)
        // is not a footer.
        assert_eq!(
            codex_model_effort(b"gpt-5.5 high \xc2\xb7 esc to interrupt"),
            None
        );
        // A valid effort but no recognisable model prefix → nothing to show.
        assert_eq!(codex_model_effort(b"mystery high \xc2\xb7 /repo"), None);
    }

    #[test]
    fn codex_model_effort_prefers_the_most_recent_footer() {
        // A stale footer earlier in the buffer must lose to the bottom-most
        // one (the live composer's).
        let two = "oldgpt-5.5 low \u{b7} /old\nworkgpt-5.6-sol max \u{b7} /new";
        assert_eq!(
            codex_model_effort(two.as_bytes()),
            Some("gpt-5.6-sol · max".to_string())
        );
    }

    #[test]
    fn codex_arrow_option_pos_requires_digit_after_arrow() {
        // `› 1.` / `› 2.` chooser shape (compacted `›1.`) → a live chooser.
        assert!(codex_arrow_option_pos("›1.yes\n›2.no").is_some());
        // The resting composer placeholder is `›` + prompt TEXT (a letter),
        // not `› <digit>`, so it must NOT read as a chooser.
        assert_eq!(codex_arrow_option_pos("›improvedocumentation"), None);
        // Most recent arrow-on-digit wins.
        let s = "›1.a\n›2.b";
        assert_eq!(codex_arrow_option_pos(s), s.rfind("›2."));
    }

    #[test]
    fn codex_working_pos_ignores_prose_mentioning_interrupt() {
        // Status-bar shape: the hint is followed by `)` → a live status line.
        assert!(codex_working_pos("•running(1s·esctointerrupt)").is_some());
        // Prose continuing with a letter after the hint is not a status line.
        assert_eq!(codex_working_pos("pressesctointerruptmewhileiwork"), None);
    }
}
