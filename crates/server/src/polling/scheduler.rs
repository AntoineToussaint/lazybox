//! Per-repo round-robin scheduler for the GitHub PR sweep.
//!
//! Pre-fix every poll fired one `involves:USER` global search plus
//! `review-requested`, `is:merged`, and one query per watched repo —
//! easily 13+ paginated GraphQL calls a minute for a multi-repo
//! dogfood user. The big global search itself was the worst offender
//! because its cost grows with the user's involvement set: more
//! repos → bigger pagination tail → higher per-search cost.
//!
//! This module decides, per tick, whether to:
//! - run the global `involves:USER` sweep (cold start, or every K
//!   ticks so newly-touched repos still get discovered), and/or
//! - hit a small batch of the **stalest** known repos directly with
//!   `repo:owner/name`-scoped searches.
//!
//! The repo set lives in [`RoundRobinState::cursor`] (a field on
//! `TickState::round_robin`) and grows whenever a global sweep
//! returns PRs from a repo the cursor hadn't seen yet. A focused
//! repo ([`RoundRobinState::focused_repo`], populated by
//! `Command::FocusWorkspace` / `MarkRead`) is bumped to the front so
//! the workspace the user is staring at refreshes on the very next
//! tick instead of waiting its turn. Stale entries are pruned by
//! [`RoundRobinState::prune`] so the cursor stays bounded by the
//! user's *current* involvement set rather than their lifetime one.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default fan-out per warm tick. Three is the issue's recommended
/// starting point: enough that 9 known repos fully refresh inside a
/// 3-tick (≈3-minute) window, small enough that each tick stays
/// well under the 25s octocrab per-request cap even when paired
/// with the merged-sweep and review-requested companions.
pub const DEFAULT_ROUND_ROBIN_N: usize = 3;

/// How long a repo's cursor entry survives without being re-synced.
/// Past this window the entry is pruned, dropping the repo out of
/// the round-robin rotation entirely. Without this the cursor would
/// grow unbounded over a long-running daemon — a user who roams
/// across 200 repos over a quarter would end up paying a per-tick
/// sort cost proportional to their lifetime involvement set instead
/// of their CURRENT one.
pub const CURSOR_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 7);

/// Per-repo round-robin state. Owns the cursor map, the user's
/// current focus, and the monotonic tick counter the scheduler
/// reads. Lives inside `TickState` as a single field so the
/// scheduling-related bookkeeping stays co-located with the logic
/// in this module — adding TTL pruning or a dynamic-N knob doesn't
/// require touching the parent `TickState`.
#[derive(Debug, Default)]
pub struct RoundRobinState {
    /// Per-repo last-sync timestamp. Drives stalest-first ordering
    /// in [`pick_repos_for_tick`]. Pruned by [`Self::prune`] so a
    /// stale repo (no longer touched by polls) falls out of the
    /// rotation instead of being burned on every tick.
    pub cursor: HashMap<String, Instant>,
    /// Repo of the workspace the user is currently looking at,
    /// surfaced via `Command::FocusWorkspace`. Bumped to the front
    /// of the rotation by [`pick_repos_for_tick`].
    pub focused_repo: Option<String>,
    /// Monotonic poll-tick counter. Increments once per
    /// `sources_for` call. The scheduler runs the global
    /// `involves:USER` sweep on tick 0 (cold start) and again every
    /// `K` ticks, where `K ≈ ceil(cursor.len() / N)` so the per-repo
    /// branch and the global branch each cover the user's inbox at
    /// the same average rate.
    pub tick: u64,
}

impl RoundRobinState {
    /// Drop cursor entries older than [`CURSOR_TTL`]. Called once
    /// per tick from `sources_for` — cheaper than a separate task
    /// since we already hold the lock there, and the cursor is
    /// usually small (tens of entries).
    pub fn prune(&mut self, now: Instant) {
        self.cursor.retain(|_, last| {
            now.checked_duration_since(*last)
                .is_none_or(|d| d < CURSOR_TTL)
        });
    }

    /// Stamp `repo` as just-synced. Used both pre-fetch (for repos
    /// we're about to query — even a 0-result query advances the
    /// rotation) and post-fetch (for repos newly discovered in the
    /// global sweep's result set).
    pub fn record_sync(&mut self, repo: &str, at: Instant) {
        self.cursor.insert(repo.to_string(), at);
    }
}

/// The scheduler's per-tick verdict: which repos (if any) to query
/// directly, plus whether to also fire the global `involves:USER`
/// sweep this tick. `repos` is ordered — focused first, then stalest
/// — so the GhSource fan-out can pass it straight into `buffer_unordered`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRobinPick {
    /// Repos to query with `repo:owner/name`-scoped PR searches.
    /// Empty on cold-start ticks (we run the global sweep alone
    /// then) and on the K-th refresh tick when `run_global` is true
    /// (the global covers them).
    pub repos: Vec<String>,
    /// True when this tick should also fire the global
    /// `involves:USER` paginated search. Always true when the cursor
    /// is empty (cold start) or has fewer entries than the fan-out
    /// could fill from. Also true every K ticks (`K ≈ ceil(known/n)`)
    /// so a long-lived process eventually discovers repos the user
    /// just got pulled into.
    pub run_global: bool,
}

/// Decide what this tick should fetch.
///
/// Inputs:
/// - `known` — per-repo cursor. Empty = cold start.
/// - `focused` — repo of the workspace the user is actively viewing.
///   Bumped to the front of `repos` when present; never causes
///   `run_global` on its own.
/// - `tick` — monotonic counter incremented per `sources_for` call.
///   Used to determine the K-th refresh tick.
/// - `n` — fan-out budget. `0` is treated as [`DEFAULT_ROUND_ROBIN_N`]
///   so a misconfigured caller still gets something sensible.
///
/// Output rules, in order:
/// 1. **Cold start** (`known` empty): `repos=[]`, `run_global=true`.
///    Nothing to round-robin yet; let the global sweep populate the
///    cursor.
/// 2. **Small inbox** (`known.len() <= n`): `repos=known sorted by
///    stalest`, `run_global = (tick % 2 == 0)`. With ≤ N known repos
///    the per-repo branch already covers them every tick; the global
///    pass runs every other tick just for new-repo discovery.
/// 3. **Large inbox** (`known.len() > n`): `repos = N stalest (focus
///    first if present and in `known`)`, `run_global = (tick % K == 0)`
///    where `K = ceil(known.len() / n).max(2)`. Each warm tick advances
///    the rotation by N; the global pass fires often enough that the
///    round-robin doesn't miss new repos the user just got pulled into.
///
/// Determinism: ties on the same `last_synced_at` instant break by
/// repo name (lexicographic) so the order is reproducible — that's
/// what the unit tests assert against.
pub fn pick_repos_for_tick(
    known: &HashMap<String, Instant>,
    focused: Option<&str>,
    tick: u64,
    n: usize,
) -> RoundRobinPick {
    let n = if n == 0 { DEFAULT_ROUND_ROBIN_N } else { n };

    if known.is_empty() {
        return RoundRobinPick {
            repos: Vec::new(),
            run_global: true,
        };
    }

    let mut sorted: Vec<(&String, &Instant)> = known.iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));

    let focus_in_known = focused.filter(|f| known.contains_key(*f));

    let mut repos: Vec<String> = Vec::with_capacity(n);
    if let Some(focus) = focus_in_known {
        repos.push(focus.to_string());
    }
    for (repo, _) in sorted {
        if repos.len() >= n {
            break;
        }
        if Some(repo.as_str()) == focus_in_known {
            continue;
        }
        repos.push(repo.clone());
    }

    let run_global = if known.len() <= n {
        tick % 2 == 0
    } else {
        let k = known.len().div_ceil(n).max(2) as u64;
        tick % k == 0
    };

    RoundRobinPick { repos, run_global }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cursor(entries: &[(&str, u64)]) -> HashMap<String, Instant> {
        // Single `Instant::now()` snapshot so equal-`secs` entries
        // produce identical timestamps — `Instant::now()` advances
        // on each call, so naive per-entry sampling would defeat the
        // tie-breaking-by-name test (the test cares specifically
        // about the case where two repos have the SAME Instant).
        let base = Instant::now() - Duration::from_secs(3600);
        entries
            .iter()
            .map(|(repo, secs)| ((*repo).to_string(), base + Duration::from_secs(*secs)))
            .collect()
    }

    /// No cursor entries (fresh daemon) → run global, no per-repo
    /// fan-out. The global sweep populates the cursor for next tick.
    #[test]
    fn cold_start_runs_global_only() {
        let pick = pick_repos_for_tick(&HashMap::new(), None, 0, 3);
        assert_eq!(pick.repos, Vec::<String>::new());
        assert!(pick.run_global, "cold start MUST run the global sweep");
    }

    /// Cold-start applies even with a focused repo — focus alone
    /// can't bootstrap the cursor (the repo isn't known yet).
    #[test]
    fn cold_start_ignores_focus() {
        let pick = pick_repos_for_tick(&HashMap::new(), Some("a/b"), 5, 3);
        assert!(pick.repos.is_empty());
        assert!(pick.run_global);
    }

    /// Stalest-first ordering across a uniform cursor.
    #[test]
    fn warm_picks_stalest_first() {
        // a synced at t=10, b at t=20, c at t=30 — a is stalest.
        let known = cursor(&[("a/a", 10), ("b/b", 20), ("c/c", 30)]);
        let pick = pick_repos_for_tick(&known, None, 1, 2);
        assert_eq!(pick.repos, vec!["a/a".to_string(), "b/b".to_string()]);
    }

    /// Focused repo is bumped to the front even when it's not the
    /// stalest. Bump only fires when the focus is actually in `known`.
    #[test]
    fn focus_repo_is_bumped_to_front() {
        // c/c is the FRESHEST, so it'd normally be last in line.
        let known = cursor(&[("a/a", 10), ("b/b", 20), ("c/c", 30)]);
        let pick = pick_repos_for_tick(&known, Some("c/c"), 1, 2);
        assert_eq!(
            pick.repos,
            vec!["c/c".to_string(), "a/a".to_string()],
            "focused repo MUST appear first; second slot keeps the stalest of the rest",
        );
    }

    /// Focus that points outside the cursor doesn't deform the
    /// ordering — the cursor's stalest-first rotation wins.
    #[test]
    fn focus_outside_cursor_is_ignored() {
        let known = cursor(&[("a/a", 10), ("b/b", 20)]);
        let pick = pick_repos_for_tick(&known, Some("unknown/repo"), 1, 3);
        assert_eq!(pick.repos, vec!["a/a".to_string(), "b/b".to_string()]);
    }

    /// Large inbox runs the global every K ticks, where K = ceil(known/n).
    /// With 10 repos and n=3, K=4 (10/3 = 3.33 → ceil = 4).
    #[test]
    fn large_inbox_runs_global_every_k_ticks() {
        let entries: Vec<(&str, u64)> = (0..10)
            .map(|i| (Box::leak(format!("o/r{i}").into_boxed_str()) as &str, i))
            .collect();
        let known = cursor(&entries);
        // K should be ceil(10/3)=4, max(2)=4.
        let global_ticks: Vec<u64> = (0..16)
            .filter(|t| pick_repos_for_tick(&known, None, *t, 3).run_global)
            .collect();
        assert_eq!(
            global_ticks,
            vec![0, 4, 8, 12],
            "global sweep should run at tick 0 and every 4th tick thereafter"
        );
        // And every warm tick STILL covers 3 repos.
        for t in 1..16 {
            let p = pick_repos_for_tick(&known, None, t, 3);
            assert_eq!(p.repos.len(), 3, "tick {t} should fan out to N=3 repos");
        }
    }

    /// Small inbox (known ≤ n) runs the global every other tick: the
    /// per-repo branch alone covers existing repos, but global is
    /// still needed for new-repo discovery.
    #[test]
    fn small_inbox_runs_global_every_other_tick() {
        let known = cursor(&[("a/a", 10), ("b/b", 20)]);
        // With known=2 and n=3, every warm tick fans out both
        // repos; global runs at tick 0, 2, 4, ...
        for t in 0..6 {
            let p = pick_repos_for_tick(&known, None, t, 3);
            assert_eq!(
                p.repos.len(),
                2,
                "tick {t} fan-out should cover all 2 known repos"
            );
            assert_eq!(p.run_global, t % 2 == 0, "tick {t} global cadence");
        }
    }

    /// Tie on `last_synced_at` — break by repo name. Without this
    /// the per-tick rotation would be order-dependent on HashMap
    /// iteration, breaking the "predictable round-robin" guarantee.
    #[test]
    fn ties_break_by_repo_name() {
        let known = cursor(&[("b/b", 10), ("a/a", 10), ("c/c", 10)]);
        let pick = pick_repos_for_tick(&known, None, 1, 2);
        assert_eq!(pick.repos, vec!["a/a".to_string(), "b/b".to_string()]);
    }

    /// `n=0` falls back to the default — a misconfigured caller
    /// gets sane behavior rather than an empty `repos` list.
    #[test]
    fn zero_n_falls_back_to_default() {
        let known = cursor(&[("a/a", 10), ("b/b", 20), ("c/c", 30), ("d/d", 40)]);
        let pick = pick_repos_for_tick(&known, None, 1, 0);
        assert_eq!(pick.repos.len(), DEFAULT_ROUND_ROBIN_N);
    }

    /// Prune removes entries older than `CURSOR_TTL` and leaves the
    /// rest. Without the prune, a long-running daemon's cursor would
    /// grow unbounded as the user touches more repos over time.
    #[test]
    fn prune_drops_entries_past_ttl() {
        let mut state = RoundRobinState::default();
        let now = Instant::now();
        // `fresh` synced just now; `stale` synced past the TTL.
        state.cursor.insert("fresh/repo".to_string(), now);
        state.cursor.insert(
            "stale/repo".to_string(),
            now - CURSOR_TTL - Duration::from_secs(1),
        );
        state.prune(now);
        assert!(state.cursor.contains_key("fresh/repo"));
        assert!(
            !state.cursor.contains_key("stale/repo"),
            "entries older than CURSOR_TTL must be evicted"
        );
    }

    /// Prune is a no-op when every entry is within the window — must
    /// not accidentally drop the freshly-synced rotation.
    #[test]
    fn prune_leaves_fresh_entries_alone() {
        let mut state = RoundRobinState::default();
        let now = Instant::now();
        state.cursor.insert("a/a".to_string(), now);
        state
            .cursor
            .insert("b/b".to_string(), now - Duration::from_secs(60));
        state.prune(now);
        assert_eq!(state.cursor.len(), 2);
    }
}
