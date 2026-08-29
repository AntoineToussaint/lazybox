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

use lazybox_ipc::{AgentRunId, AgentUsage, ProviderQuota};

/// Cells in the `▓▓▓░░` progress bar.
const BAR_WIDTH: usize = 5;

/// Render a micro-USD cost as a compact dollar string: `$1.23`, and finer
/// precision (`$0.0042`) below a cent so a small metered session still shows
/// a non-zero figure instead of `$0.00`.
pub fn format_cost_micros(micros: u64) -> String {
    let dollars = micros as f64 / 1_000_000.0;
    if micros == 0 || dollars >= 0.01 {
        format!("${dollars:.2}")
    } else {
        format!("${dollars:.4}")
    }
}

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
    /// `agent id → committed tokens`, summed across finished turns.
    tokens: BTreeMap<String, u64>,
    /// Per-run high-water mark for the turn in flight, not yet committed.
    ///
    /// A single Claude turn reports usage more than once — one or more
    /// streaming `message_delta` events (each carrying the message's
    /// *cumulative* usage) plus a final `result` total — all arriving as
    /// `Event::AgentUsage`. Summing every event double-counts the turn.
    /// Instead we keep the turn's high-water mark here and commit it once,
    /// when the turn (or the run) ends, so the total gains each turn's
    /// authoritative figure exactly once.
    pending: HashMap<AgentRunId, u64>,
    /// `agent id → latest plan-quota` (5h + weekly utilization), from
    /// `AgentProviderQuota`. Distinct from `tokens` — that answers "what did
    /// this cost?", this answers "can I keep working?". Each update replaces
    /// the window(s) it carries, so a partial report (one window observed)
    /// keeps the other window's last value.
    quotas: HashMap<String, ProviderQuota>,
    /// `agent id → accumulated cost (micro-USD)`, from priced proxy usage.
    /// Only the metering proxy prices tokens, so this fills for metered
    /// sessions; `0`/absent means "not priced" (unknown model or unmetered).
    agent_cost_micros: BTreeMap<String, u64>,
    /// `session key → accumulated tokens`, the per-workspace half of
    /// `tokens`. Fed only by proxy-metered `AgentSessionUsage` that carried a
    /// session key, so a workspace shows its own draw independent of agent id.
    session_tokens: BTreeMap<String, u64>,
    /// `session key → accumulated cost (micro-USD)` — per-workspace cost,
    /// the headline number of the canary meter.
    session_cost_micros: BTreeMap<String, u64>,
}

impl UsageTracker {
    /// Record the `run_id → agent` binding a structured run announced, so
    /// its later usage events can be attributed.
    pub fn note_run(&mut self, run_id: AgentRunId, agent_id: impl Into<String>) {
        self.runs.insert(run_id, agent_id.into());
    }

    /// Observe one usage report for a run's in-flight turn, keeping the
    /// turn's high-water mark rather than adding — repeated cumulative
    /// reports within a turn must not multiply. A report for an unknown
    /// run (its start was missed) is dropped rather than bucketed under a
    /// placeholder.
    pub fn observe_usage(&mut self, run_id: AgentRunId, usage: &AgentUsage) {
        if !self.runs.contains_key(&run_id) {
            return;
        }
        let mark = self.pending.entry(run_id).or_default();
        *mark = (*mark).max(event_tokens(usage));
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

    /// Observe usage the metering proxy attributed straight to an agent
    /// (no structured run). Unlike [`Self::observe_usage`], each report is
    /// one distinct upstream response, so they *sum* rather than collapsing
    /// to a turn high-water mark: the proxy sees every request the agent
    /// makes — interactive terminal turns included — and none is a repeat
    /// of another. A zero-token report contributes nothing (and so never
    /// promotes the agent into [`Self::agents_with_usage`]).
    pub fn observe_session_usage(
        &mut self,
        agent_id: &str,
        session_key: Option<&str>,
        usage: &AgentUsage,
    ) {
        let tokens = event_tokens(usage);
        let cost = usage.cost_usd_micros.unwrap_or(0);
        if tokens > 0 {
            *self.tokens.entry(agent_id.to_string()).or_default() += tokens;
        }
        if cost > 0 {
            *self
                .agent_cost_micros
                .entry(agent_id.to_string())
                .or_default() += cost;
        }
        // Per-workspace attribution, when the proxy path carried a session
        // key (#per-session). The canary meter reads these.
        if let Some(key) = session_key {
            if tokens > 0 {
                *self.session_tokens.entry(key.to_string()).or_default() += tokens;
            }
            if cost > 0 {
                *self.session_cost_micros.entry(key.to_string()).or_default() += cost;
            }
        }
    }

    /// Tokens committed for `agent_id` this window (`0` when none).
    pub fn tokens_for(&self, agent_id: &str) -> u64 {
        self.tokens.get(agent_id).copied().unwrap_or(0)
    }

    /// Accumulated cost for `agent_id` this window in micro-USD (`0` when
    /// unpriced — no metered, model-known usage yet).
    pub fn cost_micros_for(&self, agent_id: &str) -> u64 {
        self.agent_cost_micros.get(agent_id).copied().unwrap_or(0)
    }

    /// Seed a session's accumulated cost from the daemon's persisted total
    /// on connect (#1389), so the per-workspace `$ METER · $cost` figure
    /// survives a restart. Overwrites rather than adds: hydration replays an
    /// authoritative baseline, and a mid-session reconnect must be idempotent
    /// rather than double the figure. A zero baseline mints nothing.
    pub fn hydrate_session_cost(&mut self, session_key: &str, micros: u64) {
        if micros > 0 {
            self.session_cost_micros
                .insert(session_key.to_string(), micros);
        }
    }

    /// Tokens metered for one workspace/session (`0` when none).
    pub fn tokens_for_session(&self, session_key: &str) -> u64 {
        self.session_tokens.get(session_key).copied().unwrap_or(0)
    }

    /// Accumulated cost for one workspace/session in micro-USD (`0` when
    /// unpriced or unmetered) — the per-session headline.
    pub fn cost_micros_for_session(&self, session_key: &str) -> u64 {
        self.session_cost_micros
            .get(session_key)
            .copied()
            .unwrap_or(0)
    }

    /// Whether any workspace has metered cost — gates showing the per-session
    /// column at all.
    pub fn has_session_cost(&self) -> bool {
        self.session_cost_micros.values().any(|c| *c > 0)
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

    /// Record a plan-quota report for `agent_id`, merging window-by-window:
    /// a report carrying only the 5h window updates that window and leaves
    /// the weekly one at its last observed value (and vice-versa), so the
    /// widget never blinks a window back to unknown on a partial update.
    pub fn note_quota(&mut self, agent_id: &str, quota: ProviderQuota) {
        let slot = self.quotas.entry(agent_id.to_string()).or_default();
        if quota.five_hour.is_some() {
            slot.five_hour = quota.five_hour;
        }
        if quota.weekly.is_some() {
            slot.weekly = quota.weekly;
        }
    }

    /// The latest plan-quota for `agent_id`, if any window has been observed.
    pub fn quota_for(&self, agent_id: &str) -> Option<ProviderQuota> {
        self.quotas
            .get(agent_id)
            .copied()
            .filter(|quota| !quota.is_empty())
    }

    /// Agent ids that have an observed plan-quota — the other half of the
    /// display set, so "can I keep working?" shows even for a provider that
    /// has reported quota but no committed tokens yet.
    pub fn agents_with_quota(&self) -> impl Iterator<Item = &str> {
        self.quotas
            .iter()
            .filter(|(_, quota)| !quota.is_empty())
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
    /// A plan-quota utilization figure (` · 5h 45%`) — the "can I keep
    /// working?" headroom signal, accented distinctly from the cost figure.
    Quota,
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
    /// Pre-formatted 5-hour plan-quota fragment (`"45%"` or `"45% · 2h"`) —
    /// the "can I keep working?" signal, distinct from the token total.
    /// Pre-formatted because the reset countdown needs a clock this
    /// ratatui-free crate deliberately doesn't hold; the caller builds it.
    pub quota_5h: Option<String>,
    /// Pre-formatted weekly plan-quota fragment.
    pub quota_weekly: Option<String>,
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
            quota_5h: None,
            quota_weekly: None,
        }
    }

    /// Attach pre-formatted plan-quota fragments (see the field docs).
    pub fn with_quota(mut self, quota_5h: Option<String>, quota_weekly: Option<String>) -> Self {
        self.quota_5h = quota_5h;
        self.quota_weekly = quota_weekly;
        self
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
        // Plan-quota — the "can I keep working?" signal. Rendered after the
        // token figure so one glance answers both "what did this cost?" and
        // "how much headroom is left?". The `Quota` kind lets the theme
        // accent it distinctly from the cost figure.
        if let Some(five_hour) = &self.quota_5h {
            out.push((" · 5h ".into(), UsageSpanKind::Meta));
            out.push((five_hour.clone(), UsageSpanKind::Quota));
        }
        if let Some(weekly) = &self.quota_weekly {
            out.push((" · wk ".into(), UsageSpanKind::Meta));
            out.push((weekly.clone(), UsageSpanKind::Quota));
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
    fn every_token_leg_counts() {
        let mut tracker = UsageTracker::default();
        tracker.note_run(AgentRunId(7), "claude");
        tracker.observe_usage(
            AgentRunId(7),
            &AgentUsage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                cache_creation_input_tokens: Some(4),
                cache_read_input_tokens: Some(8),
                cost_usd_micros: None,
            },
        );
        tracker.commit_turn(&AgentRunId(7));
        assert_eq!(tracker.tokens_for("claude"), 15);
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
    fn proxy_session_usage_sums_per_agent_across_responses() {
        // Proxy-observed usage has no structured run: every report is a
        // distinct upstream response, so they accumulate rather than
        // collapsing to a turn high-water mark.
        let mut tracker = UsageTracker::default();
        tracker.observe_session_usage("codex", None, &usage(1_000, 200));
        tracker.observe_session_usage("codex", None, &usage(500, 100));
        assert_eq!(tracker.tokens_for("codex"), 1_800);
        let agents: Vec<&str> = tracker.agents_with_usage().collect();
        assert_eq!(agents, vec!["codex"]);
    }

    #[test]
    fn proxy_zero_token_report_is_ignored() {
        let mut tracker = UsageTracker::default();
        tracker.observe_session_usage("claude", None, &usage(0, 0));
        assert_eq!(tracker.tokens_for("claude"), 0);
        assert_eq!(tracker.agents_with_usage().count(), 0);
    }

    #[test]
    fn session_usage_accrues_cost_and_tokens_per_workspace() {
        let mut tracker = UsageTracker::default();
        let mut priced = usage(1_000, 200);
        priced.cost_usd_micros = Some(1_500_000); // $1.50
        tracker.observe_session_usage("claude", Some("ws-a"), &priced);

        let mut priced2 = usage(500, 100);
        priced2.cost_usd_micros = Some(500_000); // $0.50
        tracker.observe_session_usage("claude", Some("ws-a"), &priced2);

        // A second workspace stays independent.
        let mut priced3 = usage(200, 50);
        priced3.cost_usd_micros = Some(250_000);
        tracker.observe_session_usage("claude", Some("ws-b"), &priced3);

        assert_eq!(tracker.tokens_for_session("ws-a"), 1_800);
        assert_eq!(tracker.cost_micros_for_session("ws-a"), 2_000_000);
        assert_eq!(tracker.cost_micros_for_session("ws-b"), 250_000);
        // The agent-level roll-up spans both workspaces.
        assert_eq!(tracker.cost_micros_for("claude"), 2_250_000);
        assert!(tracker.has_session_cost());
        assert_eq!(format_cost_micros(2_000_000), "$2.00");
        assert_eq!(format_cost_micros(250_000), "$0.25");
        assert_eq!(format_cost_micros(4_200), "$0.0042");
    }

    #[test]
    fn hydrate_seeds_session_cost_and_is_idempotent() {
        // A restart replays the daemon's persisted per-session total: it
        // seeds the figure (not add), and a reconnect replaying the same
        // baseline doesn't double it.
        let mut tracker = UsageTracker::default();
        tracker.hydrate_session_cost("ws-a", 2_000_000);
        assert_eq!(tracker.cost_micros_for_session("ws-a"), 2_000_000);
        assert!(tracker.has_session_cost());
        // Live usage after hydration still accrues on top of the baseline.
        let mut priced = usage(100, 20);
        priced.cost_usd_micros = Some(500_000);
        tracker.observe_session_usage("claude", Some("ws-a"), &priced);
        assert_eq!(tracker.cost_micros_for_session("ws-a"), 2_500_000);
        // A reconnect replays the (now larger) baseline: overwrite, not add.
        tracker.hydrate_session_cost("ws-a", 2_500_000);
        assert_eq!(tracker.cost_micros_for_session("ws-a"), 2_500_000);
        // A zero baseline mints nothing.
        tracker.hydrate_session_cost("ws-b", 0);
        assert_eq!(tracker.cost_micros_for_session("ws-b"), 0);
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
    fn plan_quota_renders_after_the_token_figure() {
        // The "can I keep working?" fragments trail the cost figure — one
        // glance answers both questions.
        let summary = UsageSummary::new("Claude", 128_000, None, None)
            .with_quota(Some("45% · 2h".into()), Some("60%".into()));
        assert_eq!(summary.text(), "Claude 128k used · 5h 45% · 2h · wk 60%");
        // The quota percentages carry the distinct Quota accent, not Figure.
        assert!(
            summary
                .spans()
                .iter()
                .any(|(text, kind)| text == "45% · 2h" && *kind == UsageSpanKind::Quota)
        );
    }

    #[test]
    fn quota_is_omitted_when_absent() {
        // Default (no with_quota) renders exactly as before — no trailing
        // fragments, no Quota spans.
        let summary = UsageSummary::new("Claude", 128_000, None, None);
        assert_eq!(summary.text(), "Claude 128k used");
        assert!(
            summary
                .spans()
                .iter()
                .all(|(_, kind)| *kind != UsageSpanKind::Quota)
        );
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
