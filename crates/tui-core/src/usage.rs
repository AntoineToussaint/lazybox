//! Always-visible per-provider usage accounting (#1059).
//!
//! The reactive layer (`⏳ N limited` + the escalating alert, #1024)
//! only surfaces *once* an agent hits its provider limit. This module is
//! the proactive baseline that layer escalates from: a running token
//! total per agent, rendered as a compact `Claude ▓▓▓░░ 62% · 76k left`
//! header widget that is visible before any limit is reached.
//!
//! Two independent daemon signals feed it, joined here:
//!   - [`lazybox_ipc::AgentUsage`] token counts, attributed to an agent
//!     id via the `run_id → agent` map that `AgentRunStarted` establishes
//!     (the usage event itself carries only a `run_id`).
//!   - the reset countdown parsed from a usage-limit banner
//!     (`AgentUsageLimit.reset_hint`), folded in per provider by the
//!     caller as the "time-to-reset" fragment.
//!
//! Percentages need a per-agent token budget for the plan window; OAuth
//! plans expose no such number, so it is configuration
//! (`ui.usage_budgets`). Absent a budget the widget degrades to the bare
//! token total — "show what's known" — rather than inventing a
//! denominator.

use std::collections::{BTreeMap, HashMap};

use lazybox_ipc::{AgentRunId, AgentUsage};

/// Cells in the `▓▓▓░░` progress bar.
const BAR_WIDTH: usize = 5;

/// Every token field a usage event reports, summed. A running proxy for
/// "tokens processed against the plan window" — input, output, and both
/// cache legs all draw down the same allowance.
fn event_tokens(usage: &AgentUsage) -> u64 {
    usage.input_tokens.unwrap_or(0)
        + usage.output_tokens.unwrap_or(0)
        + usage.cache_creation_input_tokens.unwrap_or(0)
        + usage.cache_read_input_tokens.unwrap_or(0)
}

/// Running token totals per agent id, joined from the two disjoint daemon
/// signals (see the module docs). Cheap to clone-free query each render.
#[derive(Debug, Default, Clone)]
pub struct UsageTracker {
    /// `run_id → agent id`, from `AgentRunStarted`. A usage event only
    /// carries its `run_id`, so this is the sole way to know which
    /// provider it draws down.
    runs: HashMap<AgentRunId, String>,
    /// `agent id → accumulated tokens`.
    tokens: BTreeMap<String, u64>,
}

impl UsageTracker {
    /// Record the `run_id → agent` binding a structured run announced, so
    /// its later usage events can be attributed.
    pub fn note_run(&mut self, run_id: AgentRunId, agent_id: impl Into<String>) {
        self.runs.insert(run_id, agent_id.into());
    }

    /// Fold one usage event into its run's agent total. A usage event for
    /// an unknown run (its start was missed) is dropped rather than
    /// bucketed under a placeholder — an unattributable total is worse
    /// than a slightly low one.
    pub fn add_usage(&mut self, run_id: AgentRunId, usage: &AgentUsage) {
        if let Some(agent) = self.runs.get(&run_id) {
            *self.tokens.entry(agent.clone()).or_default() += event_tokens(usage);
        }
    }

    /// Forget a finished run's binding. The accumulated total stays — the
    /// window it counts toward outlives the individual run.
    pub fn finish_run(&mut self, run_id: &AgentRunId) {
        self.runs.remove(run_id);
    }

    /// Tokens accumulated for `agent_id` this window (`0` when none).
    pub fn tokens_for(&self, agent_id: &str) -> u64 {
        self.tokens.get(agent_id).copied().unwrap_or(0)
    }
}

/// Style role for one rendered fragment. The render layer maps these to
/// theme colours; keeping them symbolic lets this crate stay ratatui-free
/// while still owning the widget's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSpanKind {
    /// The agent's display name (`Claude`).
    Label,
    /// A filled bar cell (`▓`).
    BarFilled,
    /// An empty bar cell (`░`).
    BarEmpty,
    /// The headline figure — a percentage or a bare token count.
    Figure,
    /// Dim connective / remaining text (` · 76k left`).
    Meta,
    /// The reset countdown (` · resets 3pm`) — carries the limit accent.
    Reset,
}

/// One provider's usage, ready to render. Built by [`UsageSummary::new`];
/// [`UsageSummary::spans`] / [`UsageSummary::text`] turn it into the
/// widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSummary {
    pub label: String,
    pub tokens: u64,
    /// Plan-window token budget, when configured. Its presence is what
    /// unlocks the bar + percentage; absent, only the token total shows.
    pub budget: Option<u64>,
    /// Reset countdown fragment (`"3pm"`, `"2h"`), shown while known.
    pub reset: Option<String>,
}

impl UsageSummary {
    pub fn new(
        label: impl Into<String>,
        tokens: u64,
        budget: Option<u64>,
        reset: Option<String>,
    ) -> Self {
        Self {
            label: label.into(),
            tokens,
            budget: budget.filter(|b| *b > 0),
            reset,
        }
    }

    /// Percent of the budget consumed, clamped to 100. `None` without a
    /// budget.
    pub fn pct(&self) -> Option<u8> {
        self.budget
            .map(|budget| ((self.tokens.saturating_mul(100)) / budget).min(100) as u8)
    }

    /// Tokens left before the budget, saturating at 0. `None` without a
    /// budget.
    pub fn remaining(&self) -> Option<u64> {
        self.budget.map(|budget| budget.saturating_sub(self.tokens))
    }

    /// Filled bar cells for the current percentage (rounded).
    fn bar_filled(&self) -> Option<usize> {
        self.pct()
            .map(|pct| ((pct as usize * BAR_WIDTH) + 50) / 100)
    }

    /// The widget as `(text, kind)` fragments, in render order. The
    /// caller styles each fragment by its kind and joins them.
    pub fn spans(&self) -> Vec<(String, UsageSpanKind)> {
        let mut out = vec![
            (self.label.clone(), UsageSpanKind::Label),
            (" ".into(), UsageSpanKind::Meta),
        ];
        match (self.bar_filled(), self.pct(), self.remaining()) {
            (Some(filled), Some(pct), Some(remaining)) => {
                let filled = filled.min(BAR_WIDTH);
                if filled > 0 {
                    out.push(("▓".repeat(filled), UsageSpanKind::BarFilled));
                }
                if filled < BAR_WIDTH {
                    out.push(("░".repeat(BAR_WIDTH - filled), UsageSpanKind::BarEmpty));
                }
                out.push((format!(" {pct}%"), UsageSpanKind::Figure));
                out.push((
                    format!(" · {} left", format_tokens(remaining)),
                    UsageSpanKind::Meta,
                ));
            }
            _ => {
                out.push((format_tokens(self.tokens), UsageSpanKind::Figure));
                out.push((" used".into(), UsageSpanKind::Meta));
            }
        }
        if let Some(reset) = &self.reset {
            out.push((" · ".into(), UsageSpanKind::Meta));
            out.push((format!("resets {reset}"), UsageSpanKind::Reset));
        }
        out
    }

    /// The rendered widget as a plain string — the exact concatenation of
    /// [`Self::spans`]. Used by tests and any non-styled surface.
    pub fn text(&self) -> String {
        self.spans().into_iter().map(|(text, _)| text).collect()
    }
}

/// Compact token count: `999`, `7.2k`, `128k`, `3.4M`. One decimal only
/// where it changes the reading (below 10k / 10M); larger magnitudes drop
/// it so the header stays narrow.
pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let k = n as f64 / 1_000.0;
        if n < 10_000 {
            format!("{k:.1}k")
        } else {
            format!("{}k", n / 1_000)
        }
    } else {
        let m = n as f64 / 1_000_000.0;
        if n < 10_000_000 {
            format!("{m:.1}M")
        } else {
            format!("{}M", n / 1_000_000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> AgentUsage {
        AgentUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cost_usd_micros: None,
        }
    }

    #[test]
    fn usage_is_attributed_to_the_run_s_agent() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(1), "claude");
        tracker.note_run(AgentRunId(2), "codex");
        tracker.add_usage(AgentRunId(1), &usage(100, 20));
        tracker.add_usage(AgentRunId(1), &usage(30, 10));
        tracker.add_usage(AgentRunId(2), &usage(5, 5));

        assert_eq!(tracker.tokens_for("claude"), 160);
        assert_eq!(tracker.tokens_for("codex"), 10);
        assert_eq!(tracker.tokens_for("cursor"), 0);
    }

    #[test]
    fn every_token_leg_counts() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(7), "claude");
        tracker.add_usage(
            AgentRunId(7),
            &AgentUsage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                cache_creation_input_tokens: Some(4),
                cache_read_input_tokens: Some(8),
                cost_usd_micros: None,
            },
        );
        assert_eq!(tracker.tokens_for("claude"), 15);
    }

    #[test]
    fn usage_for_an_unknown_run_is_dropped() {
        let mut tracker = UsageTracker::default();
        tracker.add_usage(AgentRunId(99), &usage(500, 500));
        assert_eq!(tracker.tokens_for("claude"), 0);
    }

    #[test]
    fn finishing_a_run_keeps_the_total_but_stops_attribution() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(1), "claude");
        tracker.add_usage(AgentRunId(1), &usage(100, 0));
        tracker.finish_run(&AgentRunId(1));
        // A stray late usage event for the finished run no longer lands.
        tracker.add_usage(AgentRunId(1), &usage(100, 0));
        assert_eq!(tracker.tokens_for("claude"), 100);
    }

    #[test]
    fn budget_summary_renders_bar_percent_and_remaining() {
        let summary = UsageSummary::new("Claude", 124_000, Some(200_000), None);
        assert_eq!(summary.pct(), Some(62));
        assert_eq!(summary.remaining(), Some(76_000));
        assert_eq!(summary.text(), "Claude ▓▓▓░░ 62% · 76k left");
    }

    #[test]
    fn budget_summary_folds_in_the_reset_hint() {
        let summary = UsageSummary::new("Codex", 160_000, Some(200_000), Some("3pm".into()));
        assert_eq!(summary.text(), "Codex ▓▓▓▓░ 80% · 40k left · resets 3pm");
    }

    #[test]
    fn zero_usage_still_renders_a_full_widget() {
        let summary = UsageSummary::new("Claude", 0, Some(200_000), None);
        assert_eq!(summary.pct(), Some(0));
        assert_eq!(summary.text(), "Claude ░░░░░ 0% · 200k left");
    }

    #[test]
    fn without_a_budget_the_widget_degrades_to_a_token_total() {
        let summary = UsageSummary::new("Claude", 128_000, None, Some("2h".into()));
        assert_eq!(summary.pct(), None);
        assert_eq!(summary.remaining(), None);
        assert_eq!(summary.text(), "Claude 128k used · resets 2h");
    }

    #[test]
    fn a_zero_budget_is_treated_as_unknown() {
        let summary = UsageSummary::new("Claude", 100, Some(0), None);
        assert_eq!(summary.budget, None);
        assert_eq!(summary.text(), "Claude 100 used");
    }

    #[test]
    fn usage_over_budget_clamps_to_full() {
        let summary = UsageSummary::new("Claude", 300_000, Some(200_000), None);
        assert_eq!(summary.pct(), Some(100));
        assert_eq!(summary.remaining(), Some(0));
        assert_eq!(summary.text(), "Claude ▓▓▓▓▓ 100% · 0 left");
    }

    #[test]
    fn token_formatting_stays_compact() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(7_250), "7.2k");
        assert_eq!(format_tokens(128_000), "128k");
        assert_eq!(format_tokens(3_400_000), "3.4M");
        assert_eq!(format_tokens(42_000_000), "42M");
    }
}
