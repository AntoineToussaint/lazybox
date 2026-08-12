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

/// New tokens a usage event reports: input, output, and cache *creation*.
/// Cache *reads* are deliberately excluded — they re-read
/// already-counted context every turn and balloon with conversation
/// length, so folding them in would let a long session's re-read dominate
/// the total and read far higher than the work actually drawn from the
/// plan window. Cache creation stays because it is genuinely new content
/// written to the cache, bounded by the turn's real input.
fn event_tokens(usage: &AgentUsage) -> u64 {
    usage.input_tokens.unwrap_or(0)
        + usage.output_tokens.unwrap_or(0)
        + usage.cache_creation_input_tokens.unwrap_or(0)
}

/// Running token totals per agent id, joined from the two disjoint daemon
/// signals (see the module docs). Cheap to clone-free query each render.
#[derive(Debug, Default, Clone)]
pub struct UsageTracker {
    /// `run_id → agent id`, from `AgentRunStarted`. A usage event only
    /// carries its `run_id`, so this is the sole way to know which
    /// provider it draws down.
    runs: HashMap<AgentRunId, String>,
    /// `agent id → committed tokens`, summed across finished turns.
    tokens: BTreeMap<String, u64>,
    /// Latest usage reported for the turn in flight, not yet committed.
    ///
    /// A single Claude turn reports usage more than once — one or more
    /// streaming `message_delta` events (each carrying the message's
    /// *cumulative* usage) plus a final `result` total — all arriving as
    /// `Event::AgentUsage`. Summing every event double-counts the turn.
    /// The authoritative per-turn figure is the `result` total, which the
    /// daemon emits last (immediately before `AgentTurnFinished`). So we
    /// keep the *latest* report here and commit it once, when the turn (or
    /// the run) ends — the total gains each turn's `result` figure exactly
    /// once, without assuming any ordering between the report magnitudes.
    pending: HashMap<AgentRunId, u64>,
}

impl UsageTracker {
    /// Record the `run_id → agent` binding a structured run announced, so
    /// its later usage events can be attributed.
    pub fn note_run(&mut self, run_id: AgentRunId, agent_id: impl Into<String>) {
        self.runs.insert(run_id, agent_id.into());
    }

    /// Observe one usage report for a run's in-flight turn, keeping only
    /// the latest — repeated cumulative reports within a turn must not
    /// multiply, and the last report is the authoritative `result` total
    /// (see [`Self::pending`]). A report for an unknown run (its start was
    /// missed) is dropped rather than bucketed under a placeholder.
    pub fn observe_usage(&mut self, run_id: AgentRunId, usage: &AgentUsage) {
        if !self.runs.contains_key(&run_id) {
            return;
        }
        self.pending.insert(run_id, event_tokens(usage));
    }

    /// Commit the in-flight turn's high-water mark into its agent total
    /// and reset the turn accumulator (`AgentTurnFinished`). The next turn
    /// on the same run starts fresh, so a multi-turn run accrues each
    /// turn's figure.
    pub fn commit_turn(&mut self, run_id: &AgentRunId) {
        self.commit_pending(run_id);
    }

    /// Forget a finished run's binding, committing any turn still in
    /// flight first (a run that dies mid-turn never sent
    /// `AgentTurnFinished`). The accumulated total stays — the window it
    /// counts toward outlives the individual run.
    pub fn finish_run(&mut self, run_id: &AgentRunId) {
        self.commit_pending(run_id);
        self.runs.remove(run_id);
    }

    fn commit_pending(&mut self, run_id: &AgentRunId) {
        if let Some(pending) = self.pending.remove(run_id)
            && let Some(agent) = self.runs.get(run_id)
        {
            *self.tokens.entry(agent.clone()).or_default() += pending;
        }
    }

    /// Tokens committed for `agent_id` this window (`0` when none).
    pub fn tokens_for(&self, agent_id: &str) -> u64 {
        self.tokens.get(agent_id).copied().unwrap_or(0)
    }

    /// Agent ids that have accrued any committed usage this window — the
    /// data-driven half of the summary's display set, so structured-run
    /// totals surface even for an agent with no live interactive terminal.
    pub fn agents_with_usage(&self) -> impl Iterator<Item = &str> {
        self.tokens
            .iter()
            .filter(|(_, tokens)| **tokens > 0)
            .map(|(agent, _)| agent.as_str())
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
        tracker.observe_usage(AgentRunId(1), &usage(100, 20));
        tracker.commit_turn(&AgentRunId(1));
        tracker.observe_usage(AgentRunId(2), &usage(5, 5));
        tracker.commit_turn(&AgentRunId(2));

        assert_eq!(tracker.tokens_for("claude"), 120);
        assert_eq!(tracker.tokens_for("codex"), 10);
        assert_eq!(tracker.tokens_for("cursor"), 0);
    }

    #[test]
    fn repeated_cumulative_reports_in_a_turn_count_once() {
        // One Claude turn reports usage twice: a streaming `message_delta`
        // (output so far) then the final `result` total. Summing them
        // double-counts; the high-water mark counts the turn once.
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(1), "claude");
        tracker.observe_usage(AgentRunId(1), &usage(0, 500)); // message_delta
        tracker.observe_usage(AgentRunId(1), &usage(1_000, 500)); // result total
        tracker.commit_turn(&AgentRunId(1));
        assert_eq!(tracker.tokens_for("claude"), 1_500);
    }

    #[test]
    fn latest_report_is_committed_not_the_largest() {
        // The final `result` is authoritative even if a mid-turn report
        // happened to read higher — we commit the last report, not a
        // high-water mark, so no ordering-of-magnitude assumption is made.
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(1), "claude");
        tracker.observe_usage(AgentRunId(1), &usage(0, 900));
        tracker.observe_usage(AgentRunId(1), &usage(400, 200)); // result total
        tracker.commit_turn(&AgentRunId(1));
        assert_eq!(tracker.tokens_for("claude"), 600);
    }

    #[test]
    fn multi_turn_run_accrues_each_turn() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(1), "claude");
        tracker.observe_usage(AgentRunId(1), &usage(1_000, 500));
        tracker.commit_turn(&AgentRunId(1));
        tracker.observe_usage(AgentRunId(1), &usage(900, 500));
        tracker.commit_turn(&AgentRunId(1));
        assert_eq!(tracker.tokens_for("claude"), 2_900);
    }

    #[test]
    fn counts_new_tokens_but_not_cache_reads() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(7), "claude");
        tracker.observe_usage(
            AgentRunId(7),
            &AgentUsage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                cache_creation_input_tokens: Some(4),
                // Re-read context — excluded, or a long session's re-reads
                // would dominate the total.
                cache_read_input_tokens: Some(1_000_000),
                cost_usd_micros: None,
            },
        );
        tracker.commit_turn(&AgentRunId(7));
        assert_eq!(tracker.tokens_for("claude"), 7);
    }

    #[test]
    fn usage_for_an_unknown_run_is_dropped() {
        let mut tracker = UsageTracker::default();
        tracker.observe_usage(AgentRunId(99), &usage(500, 500));
        tracker.commit_turn(&AgentRunId(99));
        assert_eq!(tracker.tokens_for("claude"), 0);
    }

    #[test]
    fn finishing_a_run_commits_the_in_flight_turn_then_stops_attribution() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(1), "claude");
        tracker.observe_usage(AgentRunId(1), &usage(100, 0));
        // A run that dies mid-turn never sent `AgentTurnFinished`; the
        // in-flight turn is still committed.
        tracker.finish_run(&AgentRunId(1));
        // A stray late usage event for the finished run no longer lands.
        tracker.observe_usage(AgentRunId(1), &usage(100, 0));
        tracker.commit_turn(&AgentRunId(1));
        assert_eq!(tracker.tokens_for("claude"), 100);
    }

    #[test]
    fn agents_with_usage_lists_only_those_with_committed_tokens() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(1), "claude");
        tracker.note_run(AgentRunId(2), "codex");
        tracker.observe_usage(AgentRunId(1), &usage(100, 0));
        tracker.commit_turn(&AgentRunId(1));
        // Codex observed a zero-token report — it must not appear.
        tracker.observe_usage(AgentRunId(2), &usage(0, 0));
        tracker.commit_turn(&AgentRunId(2));
        let agents: Vec<&str> = tracker.agents_with_usage().collect();
        assert_eq!(agents, vec!["claude"]);
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
