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

use lazybox_ipc::AgentState;

/// Standard bare yes/no prompt markers. Used by every CLI that doesn't
/// have a custom approval UI (Codex, Cursor, most GenericCli configs).
/// Order doesn't matter — substring search.
pub const YN_PROMPT_PATTERNS: &[&str] = &["[y/n]", "(y/n)", "[Y/n]", "[y/N]"];

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
    let work_anchor_against = |marker: Option<usize>| -> Option<usize> {
        match (marker, work_pos, last_chunk_start) {
            (Some(m), Some(w), Some(cs)) if m >= cs && w >= cs => None,
            _ => work_pos,
        }
    };

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
    if classify(&s, &compact, None).state == AgentState::InputNeeded {
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
    input_box_visible(&compact)
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
        compact.rfind("esctocancel"),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Whether Claude's input box (composer) footer is on screen. Claude
/// draws one of these footers ONLY once it's done streaming and waiting
/// at the prompt: the long form `Esc to cancel · Tab to amend · …`, the
/// short newer form `? for shortcuts`, or — in any non-default
/// permission mode — the mode indicator `… on (shift+tab to cycle)`.
/// Lazybox spawns every agent with `--dangerously-skip-permissions`, so
/// that last form (`bypass permissions on (shift+tab to cycle)`) is the
/// footer it actually sees; `? for shortcuts` is never drawn in that
/// mode. Any of the three is proof the composer is drawn. `compact` is
/// the space-free buffer (see [`compact_lower`]) because the live footer
/// is painted by absolute cursor position and arrives spaceless
/// (`?forshortcuts`, `shift+tabtocycle`).
fn input_box_visible(compact: &str) -> bool {
    compact.contains("tabtoamend")
        || compact.contains("?forshortcuts")
        || compact.contains("shift+tabtocycle")
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
}
