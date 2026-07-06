//! Tips — progressive feature-discovery hints (#115).
//!
//! A complement to the one-shot tour: where the tour runs once at
//! first launch and walks the onboarding user-story (inbox → work →
//! snippets → navigation → config), tips keep teaching the *long
//! tail* afterwards, surfaced just-in-time off the current UI state.
//!
//! A tip is deliberately quiet: it renders as a dim,
//! [`NoticeSeverity::Hint`](../../lazybox_tui/realm/components/footer)
//! footer notice that auto-fades — never a modal, never focus-
//! stealing. The model caps it to one tip per session and persists
//! which tips have shown so they don't repeat.
//!
//! Tips reference the action catalog ([`ActionKind`]) for their key
//! hint rather than hardcoding a key string, so a user's `action_keys`
//! remap flows through automatically (same single-source-of-truth
//! discipline as the footer / help panel).

use crate::action::{ActionDef, ActionKind};
use std::collections::BTreeMap;

/// Current UI state a tip keys off. Cheap to build each tick from the
/// model; only the dimensions any tip actually triggers on are
/// tracked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TipContext {
    /// An agent on some workspace is waiting on input.
    pub agent_waiting: bool,
    /// Some PR has failing / mixed CI.
    pub failing_ci: bool,
    /// The terminal pane currently holds focus.
    pub in_terminal: bool,
}

/// The live-state condition under which a tip becomes eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    AgentWaiting,
    FailingCi,
    InTerminal,
}

/// One catalog tip. `template` is filled at resolve time with the
/// effective key for `action` (so a remap is reflected), substituted
/// for the literal `{key}`.
struct Tip {
    /// Stable id persisted to `ui.tips_seen`. Never reuse across
    /// different tip content — it's how "already shown" is tracked.
    id: &'static str,
    trigger: Trigger,
    action: ActionKind,
    template: &'static str,
}

impl Tip {
    fn matches(&self, ctx: &TipContext) -> bool {
        match self.trigger {
            Trigger::AgentWaiting => ctx.agent_waiting,
            Trigger::FailingCi => ctx.failing_ci,
            Trigger::InTerminal => ctx.in_terminal,
        }
    }
}

/// The tip catalog, in priority order — [`next_tip`] surfaces the
/// first unseen tip whose trigger matches. Each tip keys off a live
/// state signal and points at the catalog action that addresses it,
/// so the hint is relevant rather than random. These cover the long
/// tail (cross-workspace jumps, terminal escape) deliberately left
/// out of the tour's onboarding story.
const TIPS: &[Tip] = &[
    Tip {
        id: "jump_to_asking",
        trigger: Trigger::AgentWaiting,
        action: ActionKind::JumpToAsking,
        template: "{key} jumps to the next agent waiting on input",
    },
    Tip {
        id: "jump_to_failing_ci",
        trigger: Trigger::FailingCi,
        action: ActionKind::JumpToFailingCi,
        template: "{key} jumps to the next failing PR",
    },
    Tip {
        id: "leave_terminal",
        trigger: Trigger::InTerminal,
        action: ActionKind::LeaveTerminal,
        template: "{key} returns from the terminal to the sidebar",
    },
];

/// A tip resolved to its display message, ready to flash as a footer
/// hint. `id` is what the caller records in `ui.tips_seen`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTip {
    pub id: String,
    pub message: String,
}

/// Pick the highest-priority tip eligible for the current state that
/// hasn't already been shown, resolving its key hint through the
/// effective binding (user `action_keys` override or catalog
/// default). Returns `None` when nothing applies — no matching state,
/// or every matching tip is already in `seen`.
pub fn next_tip(
    ctx: &TipContext,
    seen: &[String],
    overrides: &BTreeMap<String, String>,
) -> Option<ResolvedTip> {
    let tip = TIPS
        .iter()
        .filter(|t| t.matches(ctx))
        .find(|t| !seen.iter().any(|s| s == t.id))?;
    let key = ActionDef::for_kind(tip.action).effective_keys_display(overrides);
    Some(ResolvedTip {
        id: tip.id.to_string(),
        message: format!("tip: {}", tip.template.replace("{key}", &key)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_overrides() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn no_tip_when_no_state_matches() {
        let ctx = TipContext::default();
        assert!(next_tip(&ctx, &[], &no_overrides()).is_none());
    }

    #[test]
    fn agent_waiting_surfaces_the_jump_to_asking_tip() {
        let ctx = TipContext {
            agent_waiting: true,
            ..Default::default()
        };
        let tip = next_tip(&ctx, &[], &no_overrides()).expect("a tip");
        assert_eq!(tip.id, "jump_to_asking");
        // Key hint derives from the catalog (`!`), not a literal.
        assert!(tip.message.contains('!'), "{}", tip.message);
        assert!(tip.message.starts_with("tip: "));
    }

    #[test]
    fn failing_ci_surfaces_its_tip_with_catalog_key() {
        let ctx = TipContext {
            failing_ci: true,
            ..Default::default()
        };
        let tip = next_tip(&ctx, &[], &no_overrides()).expect("a tip");
        assert_eq!(tip.id, "jump_to_failing_ci");
        assert!(tip.message.contains("Shift-F"), "{}", tip.message);
    }

    #[test]
    fn in_terminal_surfaces_the_leave_terminal_tip() {
        let ctx = TipContext {
            in_terminal: true,
            ..Default::default()
        };
        let tip = next_tip(&ctx, &[], &no_overrides()).expect("a tip");
        assert_eq!(tip.id, "leave_terminal");
        assert!(tip.message.contains("]]]"), "{}", tip.message);
    }

    #[test]
    fn already_seen_tips_are_skipped() {
        let ctx = TipContext {
            agent_waiting: true,
            ..Default::default()
        };
        let seen = vec!["jump_to_asking".to_string()];
        assert!(
            next_tip(&ctx, &seen, &no_overrides()).is_none(),
            "the only matching tip was already shown",
        );
    }

    #[test]
    fn higher_priority_state_wins_when_several_match() {
        // Agent-waiting outranks failing-CI (earlier in the catalog).
        let ctx = TipContext {
            agent_waiting: true,
            failing_ci: true,
            in_terminal: true,
        };
        let tip = next_tip(&ctx, &[], &no_overrides()).expect("a tip");
        assert_eq!(tip.id, "jump_to_asking");
        // With it marked seen, the next-priority tip surfaces.
        let tip = next_tip(&ctx, &[tip.id], &no_overrides()).expect("a tip");
        assert_eq!(tip.id, "jump_to_failing_ci");
    }

    #[test]
    fn key_hint_follows_a_remap() {
        let ctx = TipContext {
            agent_waiting: true,
            ..Default::default()
        };
        let mut overrides = BTreeMap::new();
        overrides.insert("jump_to_asking".to_string(), "Ctrl-a".to_string());
        let tip = next_tip(&ctx, &[], &overrides).expect("a tip");
        assert!(tip.message.contains("Ctrl-a"), "{}", tip.message);
    }

    #[test]
    fn every_tip_references_a_real_catalog_action() {
        // A tip pointing at an ActionKind whose default_keys is a
        // presentation-only form would still render, but guard that
        // each referenced action has a non-empty effective key so the
        // `{key}` substitution never produces an empty hint.
        let overrides = no_overrides();
        for tip in TIPS {
            let key = ActionDef::for_kind(tip.action).effective_keys_display(&overrides);
            assert!(!key.is_empty(), "{} has no key hint", tip.id);
            assert!(
                tip.template.contains("{key}"),
                "{} template missing {{key}} placeholder",
                tip.id,
            );
        }
    }

    #[test]
    fn tip_ids_are_unique() {
        let mut ids: Vec<&str> = TIPS.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate tip id");
    }
}
