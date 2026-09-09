//! The one context-hygiene policy, shared by both enforcement points.
//!
//! A large share of an agent's input-token bill is mechanical: a 900-line file
//! read on turn 3 rides in every request for the rest of the session. lazybox
//! blocks that in two places — the metering proxy rewrites old tool results for
//! every agent it fronts, and Claude's `PreToolUse` hook denies the read before
//! it happens — and both must reach the *same* verdict on the *same* block and
//! emit the *same* bytes for it. Two enforcement points disagreeing is worse
//! than one: the model would see a file condensed one way through the hook and
//! another way through the proxy, and every such divergence is a prompt-cache
//! miss paid for twice.
//!
//! So the decision ([`ContextHygiene::eligibility`]), the cache identity
//! ([`cache_key`]) and the rendered bytes ([`render_condensed`]) all live here,
//! and the enforcement points own only their own mechanics.
//!
//! Byte stability is the load-bearing property throughout. Claude Code re-sends
//! the whole conversation each turn with prompt-cache breakpoints, so a
//! condensed block whose bytes drift between turns invalidates the cached prefix
//! and costs more than it saved.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Store kv prefix for cached condensations. Both enforcement points read and
/// write this space, keyed by [`cache_key`], so a block condensed by the hook is
/// never re-condensed by the proxy.
pub const KV_PREFIX_CONDENSE: &str = "condense:";

/// Opening bytes of a rendered condensation. Recognizing our own output is what
/// makes rewriting monotone: a block already carrying this marker is passed
/// through untouched instead of being summarized again.
pub const CONDENSED_MARKER: &str = "[condensed by lazybox: ";

/// Rollout state of the context-hygiene block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompactionMode {
    /// Nothing is measured, decided, or rewritten.
    Off,
    /// Decide as if compacting, log what *would* be rewritten and what it would
    /// save, and send the original bytes. The default: rewriting an agent's
    /// context is load-bearing for correctness, not just accounting, so it earns
    /// its way from evidence rather than starting on.
    #[default]
    Shadow,
    /// Decide and rewrite.
    On,
}

impl CompactionMode {
    /// Whether the policy should be evaluated at all. `Shadow` says yes — it
    /// differs from `On` in what happens to the bytes, not in what gets decided.
    pub fn evaluates(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether a verdict may actually change the bytes an agent sends or sees,
    /// or block a tool call. **This, not the verdict, is the permission
    /// check** — `evaluates()` is true in `Shadow` too.
    pub fn rewrites(self) -> bool {
        matches!(self, Self::On)
    }
}

/// What kind of material a block holds. The kind picks the condense prompt, the
/// re-read affordance, and (via [`cache_key`]) part of the cache identity, so
/// the same bytes read as a file and captured as command output stay distinct
/// entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CondenseKind {
    FileRead { path: String },
    CommandOutput { command: String },
    Diff,
}

impl CondenseKind {
    /// The subject named in the rendered header — a path, a command, or the
    /// literal `diff`.
    pub fn label(&self) -> &str {
        match self {
            Self::FileRead { path } => path,
            Self::CommandOutput { command } => command,
            Self::Diff => "diff",
        }
    }

    /// How the model gets back to the real bytes. Load-bearing, not decoration:
    /// condensation is only safe because an explicit, always-permitted re-read
    /// path exists for the moment the model needs real content to edit.
    pub fn reread_hint(&self) -> &'static str {
        match self {
            Self::FileRead { .. } => "re-read the file for full content",
            Self::CommandOutput { .. } => "re-run the command for full output",
            Self::Diff => "re-read the diff for full content",
        }
    }

    fn discriminant(&self) -> &'static str {
        match self {
            Self::FileRead { .. } => "file",
            Self::CommandOutput { .. } => "command",
            Self::Diff => "diff",
        }
    }
}

/// What an enforcement point knows about one candidate block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultFacts<'a> {
    /// `None` when the wire shape did not classify — an unrecognized block is
    /// never rewritten.
    pub kind: Option<&'a CondenseKind>,
    pub lines: usize,
    /// Position counted back from the newest tool result in the conversation:
    /// `0` is the most recent. The proxy counts these off the request body; the
    /// hook is deciding about a read that has not happened yet and so passes
    /// `usize::MAX` via [`ToolResultFacts::pending`].
    pub from_end: usize,
    /// The block already carries [`CONDENSED_MARKER`].
    pub already_condensed: bool,
}

impl<'a> ToolResultFacts<'a> {
    /// Facts for material that is about to enter the conversation rather than
    /// already sitting in it — the `PreToolUse` case. Such a block has no
    /// position yet, and the recency window exists to protect blocks the model
    /// is mid-task on, which a read it has not yet performed is not.
    pub fn pending(kind: &'a CondenseKind, lines: usize) -> Self {
        Self {
            kind: Some(kind),
            lines,
            from_end: usize::MAX,
            already_condensed: false,
        }
    }
}

/// The verdict. `Skip` carries its reason because shadow mode's whole job is
/// reporting *why* a block was or wasn't eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    Condense,
    Skip(SkipReason),
}

impl Eligibility {
    /// Whether the policy considers this block condensable. Eligibility is not
    /// permission: see [`ContextHygiene::eligibility`] for why acting on this
    /// alone rewrites context in shadow mode.
    pub fn is_condense(self) -> bool {
        matches!(self, Self::Condense)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `mode: off`.
    Disabled,
    /// Inside the recency window — the model is plausibly mid-task on it, and
    /// edits need real content.
    WithinRecencyWindow,
    /// Under `min_lines`; condensing it would not repay the cache miss.
    BelowLineFloor,
    /// The wire shape did not classify the block. Pass through on doubt.
    KindNotEligible,
    /// Already ours. Re-condensing would change bytes that must not change.
    AlreadyCondensed,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::WithinRecencyWindow => "within-recency-window",
            Self::BelowLineFloor => "below-line-floor",
            Self::KindNotEligible => "kind-not-eligible",
            Self::AlreadyCondensed => "already-condensed",
        }
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every knob the epic's two enforcement points and the summarizer share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextHygiene {
    pub mode: CompactionMode,
    /// Tool results this close to the newest are never touched, whatever their
    /// size. The model is likely mid-task on them and an edit needs the real
    /// bytes.
    pub keep_recent: usize,
    /// Line floor for eligibility. Below it a rewrite cannot repay the one
    /// deliberate cache miss it costs on the turn it happens.
    pub min_lines: usize,
    /// Let agents that expose a pre-tool decision hook block the read before it
    /// happens, instead of relying on the proxy to rewrite it afterwards.
    /// Ignored by agents with no such hook.
    pub hook_intercept: bool,
    /// Model for the condense call when the serving agent declares no cheap
    /// tier. Unset means fall back to the agent's own low tier only, and skip
    /// condensing when it has none.
    pub condense_model: Option<String>,
    /// Bumped by hand when the condense prompt changes. It is part of
    /// [`cache_key`], so a new prompt yields new keys rather than silently
    /// different output under old ones.
    pub prompt_version: u32,
    /// Ceiling on one condense call. Expiry means the original bytes go
    /// through; an agent is never stalled on the summarizer.
    pub condense_timeout_ms: u64,
    /// Input cap for one condense call — a multi-megabyte blob is truncated
    /// rather than sent whole to a cheap model.
    pub condense_input_cap_bytes: usize,
}

impl Default for ContextHygiene {
    fn default() -> Self {
        Self {
            mode: CompactionMode::default(),
            keep_recent: 4,
            min_lines: 350,
            hook_intercept: true,
            condense_model: None,
            prompt_version: 1,
            condense_timeout_ms: 15_000,
            condense_input_cap_bytes: 256 * 1024,
        }
    }
}

impl ContextHygiene {
    /// The single verdict both enforcement points ask for.
    ///
    /// It answers **"is this block eligible"** — never "may I rewrite it".
    /// `Shadow` deliberately returns [`Eligibility::Condense`] for a block it
    /// would condense, because that is the whole point of a shadow mode:
    /// #1606's instrumentation wants the real verdict on every block while
    /// nothing on the wire changes. So a caller that acts on
    /// [`Eligibility::is_condense`] alone will act in shadow mode. Gate on
    /// [`CompactionMode::rewrites`] before touching bytes or denying a tool
    /// call — a hook that skips that check denies reads in the shipped default
    /// configuration.
    ///
    /// Order matters for what shadow mode reports: the cheap structural reasons
    /// are checked before the size floor, so a block inside the recency window
    /// is reported as such rather than as merely small.
    pub fn eligibility(&self, facts: &ToolResultFacts<'_>) -> Eligibility {
        if !self.mode.evaluates() {
            return Eligibility::Skip(SkipReason::Disabled);
        }
        if facts.already_condensed {
            return Eligibility::Skip(SkipReason::AlreadyCondensed);
        }
        if facts.kind.is_none() {
            return Eligibility::Skip(SkipReason::KindNotEligible);
        }
        if facts.from_end < self.keep_recent {
            return Eligibility::Skip(SkipReason::WithinRecencyWindow);
        }
        if facts.lines < self.min_lines {
            return Eligibility::Skip(SkipReason::BelowLineFloor);
        }
        Eligibility::Condense
    }

    /// Cache identity for one condensation. Content-addressed over everything
    /// that can change the output, so a hit is byte-identical by construction
    /// and a prompt or model change lands on fresh keys instead of colliding
    /// with stale ones.
    pub fn cache_key(&self, input: &str, kind: &CondenseKind, model: &str) -> String {
        cache_key(input, kind, model, self.prompt_version)
    }

    /// Full store key, ready for the kv.
    pub fn cache_kv_key(&self, input: &str, kind: &CondenseKind, model: &str) -> String {
        format!("{KV_PREFIX_CONDENSE}{}", self.cache_key(input, kind, model))
    }
}

/// Content-addressed cache key: `hash(input, kind, model, prompt_version)`.
///
/// SHA-256, hex, over length-prefixed fields. Both the strength and the framing
/// are deliberate. A hit is served back into an agent's context verbatim, so a
/// collision would put one file's summary under another file's header — and the
/// inputs here are unbounded, attacker-adjacent tool output, not the small
/// closed set `Snippet::content_hash`'s FNV-1a/64 covers. Length prefixes mean
/// no field's content can impersonate a boundary and forge another entry's key.
pub fn cache_key(input: &str, kind: &CondenseKind, model: &str, prompt_version: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt_version.to_le_bytes());
    for field in [model, kind.discriminant(), kind.label(), input] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Render a condensation into the bytes that go on the wire.
///
/// A pure function of its arguments, which is the byte-stability contract: the
/// same cached summary renders identically on every turn and from either
/// enforcement point, so the prompt-cache prefix stays intact after the one
/// turn on which the block was first condensed.
pub fn render_condensed(kind: &CondenseKind, original_lines: usize, summary: &str) -> String {
    let summary = summary.trim();
    let condensed_lines = if summary.is_empty() {
        0
    } else {
        summary.lines().count()
    };
    format!(
        "{CONDENSED_MARKER}{}, {original_lines} lines → {condensed_lines} lines; {}]\n{summary}",
        kind.label(),
        kind.reread_hint(),
    )
}

/// Whether this block is already one of ours.
pub fn is_condensed(text: &str) -> bool {
    text.starts_with(CONDENSED_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(kind: &'a CondenseKind, lines: usize, from_end: usize) -> ToolResultFacts<'a> {
        ToolResultFacts {
            kind: Some(kind),
            lines,
            from_end,
            already_condensed: false,
        }
    }

    fn file() -> CondenseKind {
        CondenseKind::FileRead {
            path: "src/lib.rs".into(),
        }
    }

    #[test]
    fn defaults_match_the_documented_policy() {
        let policy = ContextHygiene::default();
        assert_eq!(policy.mode, CompactionMode::Shadow);
        assert_eq!(policy.keep_recent, 4);
        assert_eq!(policy.min_lines, 350);
        assert!(policy.hook_intercept);
        assert_eq!(policy.prompt_version, 1);
    }

    /// Shadow decides everything `On` decides and rewrites nothing — that split
    /// is what makes it a safe default.
    #[test]
    fn shadow_evaluates_but_does_not_rewrite() {
        assert!(CompactionMode::Shadow.evaluates());
        assert!(!CompactionMode::Shadow.rewrites());
        assert!(CompactionMode::On.evaluates());
        assert!(CompactionMode::On.rewrites());
        assert!(!CompactionMode::Off.evaluates());
        assert!(!CompactionMode::Off.rewrites());
    }

    /// The trap, pinned: shadow returns a real `Condense` verdict, so
    /// `is_condense()` is eligibility and never permission. An enforcement
    /// point must consult `rewrites()` before acting, or it enforces in the
    /// shipped default configuration.
    #[test]
    fn shadow_yields_a_condense_verdict_that_is_not_permission_to_act() {
        let kind = file();
        let policy = ContextHygiene {
            mode: CompactionMode::Shadow,
            ..Default::default()
        };
        assert_eq!(
            policy.eligibility(&facts(&kind, 900, 7)),
            Eligibility::Condense,
            "shadow must reach the same verdict On would"
        );
        assert!(
            !policy.mode.rewrites(),
            "but must not permit acting on it — this pair is the whole contract"
        );
    }

    #[test]
    fn a_big_old_block_is_eligible() {
        let kind = file();
        let policy = ContextHygiene::default();
        assert_eq!(
            policy.eligibility(&facts(&kind, 900, 7)),
            Eligibility::Condense
        );
    }

    /// The recency window is absolute: size never buys past it, because edits
    /// need real content.
    #[test]
    fn the_recency_window_beats_any_size() {
        let kind = file();
        let policy = ContextHygiene::default();
        for from_end in 0..policy.keep_recent {
            assert_eq!(
                policy.eligibility(&facts(&kind, 100_000, from_end)),
                Eligibility::Skip(SkipReason::WithinRecencyWindow),
                "from_end {from_end} is inside keep_recent"
            );
        }
        assert_eq!(
            policy.eligibility(&facts(&kind, 100_000, policy.keep_recent)),
            Eligibility::Condense,
            "the first block past the window is eligible"
        );
    }

    #[test]
    fn the_line_floor_is_inclusive_at_min_lines() {
        let kind = file();
        let policy = ContextHygiene::default();
        assert_eq!(
            policy.eligibility(&facts(&kind, policy.min_lines - 1, 9)),
            Eligibility::Skip(SkipReason::BelowLineFloor)
        );
        assert_eq!(
            policy.eligibility(&facts(&kind, policy.min_lines, 9)),
            Eligibility::Condense
        );
    }

    #[test]
    fn off_skips_everything() {
        let kind = file();
        let policy = ContextHygiene {
            mode: CompactionMode::Off,
            ..Default::default()
        };
        assert_eq!(
            policy.eligibility(&facts(&kind, 100_000, 99)),
            Eligibility::Skip(SkipReason::Disabled)
        );
    }

    /// Monotone rewriting: our own output is never a candidate, so a block
    /// condensed on turn *k* keeps identical bytes on every turn after.
    #[test]
    fn an_already_condensed_block_is_never_recondensed() {
        let kind = file();
        let policy = ContextHygiene::default();
        let facts = ToolResultFacts {
            already_condensed: true,
            ..facts(&kind, 100_000, 42)
        };
        assert_eq!(
            policy.eligibility(&facts),
            Eligibility::Skip(SkipReason::AlreadyCondensed)
        );
    }

    /// Pass through on doubt: an unrecognized wire shape is data we don't
    /// understand, and rewriting it would be guessing.
    #[test]
    fn an_unclassified_block_is_skipped() {
        let policy = ContextHygiene::default();
        let facts = ToolResultFacts {
            kind: None,
            lines: 100_000,
            from_end: 42,
            already_condensed: false,
        };
        assert_eq!(
            policy.eligibility(&facts),
            Eligibility::Skip(SkipReason::KindNotEligible)
        );
    }

    /// A read that has not happened yet has no position, so the recency window
    /// cannot exempt it — otherwise the hook could never intercept anything.
    #[test]
    fn a_pending_read_is_outside_the_recency_window() {
        let kind = file();
        let policy = ContextHygiene::default();
        assert_eq!(
            policy.eligibility(&ToolResultFacts::pending(&kind, 400)),
            Eligibility::Condense
        );
        assert_eq!(
            policy.eligibility(&ToolResultFacts::pending(&kind, 40)),
            Eligibility::Skip(SkipReason::BelowLineFloor)
        );
    }

    #[test]
    fn rendering_is_byte_stable_and_self_describing() {
        let kind = file();
        let rendered = render_condensed(&kind, 900, "fn main() {}\n// two lines");
        assert_eq!(
            rendered,
            "[condensed by lazybox: src/lib.rs, 900 lines → 2 lines; \
             re-read the file for full content]\nfn main() {}\n// two lines"
        );
        assert_eq!(
            rendered,
            render_condensed(&kind, 900, "fn main() {}\n// two lines")
        );
        assert!(is_condensed(&rendered));
    }

    /// The proxy recognizes what the hook produced and vice versa — that
    /// mutual recognition is what stops the two layers double-summarizing.
    #[test]
    fn every_kind_renders_a_recognizable_block_with_a_reread_path() {
        for kind in [
            file(),
            CondenseKind::CommandOutput {
                command: "cargo test".into(),
            },
            CondenseKind::Diff,
        ] {
            let rendered = render_condensed(&kind, 500, "summary");
            assert!(is_condensed(&rendered), "{kind:?} must be recognizable");
            assert!(
                rendered.contains(kind.reread_hint()),
                "{kind:?} must name its re-read path"
            );
        }
        assert!(!is_condensed("just some file content"));
    }

    /// Every field that can change the output is in the key; nothing else is.
    #[test]
    fn the_cache_key_covers_input_kind_model_and_prompt_version() {
        let kind = file();
        let base = cache_key("body", &kind, "claude-haiku-4-5", 1);

        assert_eq!(base, cache_key("body", &kind, "claude-haiku-4-5", 1));
        assert_ne!(base, cache_key("other", &kind, "claude-haiku-4-5", 1));
        assert_ne!(base, cache_key("body", &kind, "claude-haiku-4-5", 2));
        assert_ne!(base, cache_key("body", &kind, "claude-sonnet-5", 1));
        assert_ne!(
            base,
            cache_key(
                "body",
                &CondenseKind::CommandOutput {
                    command: "src/lib.rs".into()
                },
                "claude-haiku-4-5",
                1
            ),
            "same label, different kind must not collide"
        );
        assert_ne!(
            base,
            cache_key(
                "body",
                &CondenseKind::FileRead {
                    path: "src/main.rs".into()
                },
                "claude-haiku-4-5",
                1
            )
        );
    }

    /// Fields are length-prefixed, so no field's content can impersonate a
    /// boundary and make two different condensations share a cache entry.
    #[test]
    fn adjacent_fields_cannot_be_confused() {
        assert_ne!(
            cache_key("b", &CondenseKind::FileRead { path: "ab".into() }, "m", 1),
            cache_key("b", &CondenseKind::FileRead { path: "a".into() }, "mb", 1),
        );
    }

    #[test]
    fn the_kv_key_is_prefixed() {
        let policy = ContextHygiene::default();
        let kind = file();
        let key = policy.cache_kv_key("body", &kind, "claude-haiku-4-5");
        assert!(key.starts_with(KV_PREFIX_CONDENSE));
        assert!(key.ends_with(&policy.cache_key("body", &kind, "claude-haiku-4-5")));
    }

    #[test]
    fn an_empty_summary_renders_zero_lines() {
        assert!(render_condensed(&file(), 900, "   \n  ").contains("900 lines → 0 lines"));
    }

    #[test]
    fn the_policy_round_trips_through_yaml() {
        let policy = ContextHygiene {
            mode: CompactionMode::On,
            keep_recent: 2,
            condense_model: Some("claude-haiku-4-5".into()),
            ..Default::default()
        };
        let yaml = serde_json::to_string(&policy).expect("serialize");
        let back: ContextHygiene = serde_json::from_str(&yaml).expect("deserialize");
        assert_eq!(policy, back);
    }

    /// A config written before this block existed must load with the whole
    /// policy defaulted, not fail.
    #[test]
    fn an_absent_block_loads_as_the_default_policy() {
        let back: ContextHygiene = serde_json::from_str("{}").expect("deserialize");
        assert_eq!(back, ContextHygiene::default());
    }
}
