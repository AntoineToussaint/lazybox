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

use std::collections::{HashMap, HashSet};
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
    /// Repositories removed solely because every stored row was cold.
    /// Their prior cursor position is retained so a snooze expiry or
    /// upstream reopen can restore the repo to the active rotation.
    suspended_cold: HashMap<String, Instant>,
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
        self.suspended_cold.retain(|_, last| {
            now.checked_duration_since(*last)
                .is_none_or(|d| d < CURSOR_TTL)
        });
    }

    /// Stamp `repo` as just-synced. Used both pre-fetch (for repos
    /// we're about to query — even a 0-result query advances the
    /// rotation) and post-fetch (for repos newly discovered in the
    /// global sweep's result set).
    pub fn record_sync(&mut self, repo: &str, at: Instant) {
        self.suspended_cold.remove(repo);
        self.cursor.insert(repo.to_string(), at);
    }

    pub fn update_engagement_repos(
        &mut self,
        cold_repos: &std::collections::HashSet<String>,
        active_repos: &std::collections::HashSet<String>,
        now: Instant,
    ) {
        for repo in active_repos {
            if let Some(last_sync) = self.suspended_cold.remove(repo) {
                self.cursor.insert(repo.clone(), last_sync);
            }
        }

        let fallback = now.checked_sub(Duration::from_secs(60 * 60)).unwrap_or(now);
        for repo in cold_repos {
            let last_sync = self.cursor.remove(repo).unwrap_or(fallback);
            self.suspended_cold.entry(repo.clone()).or_insert(last_sync);
        }
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

/// Per-tick scheduling entry point used by `sources_for`. Wraps
/// [`pick_repos_for_tick`] with the state mutations (prune, cursor
/// stamps, tick counter) that must only happen when the tick will
/// actually run a full sweep.
///
/// `will_full_sweep == false` means the fetch will take the
/// notifications fast path and never consult the pick, so we leave
/// the rotation untouched: stamping repos "synced" without querying
/// them would starve genuinely-stale repos, and advancing the tick
/// counter would evaluate the global-sweep modulus against ticks
/// that fetched nothing. The returned pick still sets
/// `run_global = true` so the heartbeat-failure promotion path
/// (incremental → full sweep mid-tick) runs the exhaustive global
/// sweep rather than an empty one.
pub fn plan_round_robin_tick(
    state: &mut RoundRobinState,
    forced: &HashSet<String>,
    will_full_sweep: bool,
    n: usize,
    now: Instant,
) -> RoundRobinPick {
    if !will_full_sweep {
        return RoundRobinPick {
            repos: Vec::new(),
            run_global: true,
        };
    }
    state.prune(now);
    let pick = pick_repos_for_tick(
        &state.cursor,
        state.focused_repo.as_deref(),
        forced,
        state.tick,
        n,
    );
    // Bump the cursor for repos we're about to query (even a
    // 0-result query advances the rotation — without it, an empty
    // repo stays "stalest" forever). This also lands forced/focused
    // repos in the cursor, so `known.len()` — and thus the global
    // sweep's `K = ceil(known.len()/n)` cadence — grows with the
    // session set: with many sessioned repos the discovery sweep fires
    // marginally less often, which is fine (those repos are genuinely
    // known and already fetched every tick).
    for repo in &pick.repos {
        state.record_sync(repo, now);
    }
    state.tick = state.tick.wrapping_add(1);
    pick
}

/// Decide what this tick should fetch.
///
/// Inputs:
/// - `known` — per-repo cursor. Empty = cold start.
/// - `focused` — repo of the workspace the user is actively viewing.
///   Force-included in every pick — even when it's absent from the
///   cursor or suspended cold — so a manual `Shift-R` while sitting on
///   a starved repo always re-fetches it. Never causes `run_global` on
///   its own.
/// - `forced` — repos that must be in EVERY pick regardless of the
///   round-robin budget (the session-anchored Tier 0: every repo
///   backing a workspace with a persisted session). Uncapped — bounded
///   naturally by how many worktrees the user has open. Each forced
///   repo costs one `repo:`-scoped PR search per full-sweep tick, so at
///   an extreme worktree count a single sweep's fan-out grows with the
///   session set; at the daily-driver scale (tens of worktrees) it
///   stays well inside the tick budget.
/// - `tick` — monotonic counter incremented per `sources_for` call.
///   Used to determine the K-th refresh tick.
/// - `n` — round-robin fan-out budget for the NON-forced tier. `0` is
///   treated as [`DEFAULT_ROUND_ROBIN_N`] so a misconfigured caller
///   still gets something sensible. `forced` repos are added on top of
///   these `n` slots; the `focused` repo instead RIDES one of them (it
///   is force-included, but consumes a round-robin slot rather than
///   adding to the fan-out).
///
/// Output rules:
/// 1. **Cold start** (`known` empty AND nothing forced/focused):
///    `repos=[]`, `run_global=true`. Nothing to round-robin yet; let
///    the global sweep populate the cursor.
/// 2. Otherwise `repos = focused ∪ forced ∪ (N stalest cursor entries
///    not already forced/focused)`. `run_global` follows the cursor
///    size: every other tick for a small inbox (`known.len() <= n`),
///    every `K = ceil(known.len() / n).max(2)` ticks for a large one,
///    so the round-robin doesn't miss new repos the user just got
///    pulled into.
///
/// Determinism: ties on the same `last_synced_at` instant break by
/// repo name (lexicographic), and forced repos are emitted in sorted
/// order, so the whole pick is reproducible — that's what the unit
/// tests assert against.
pub fn pick_repos_for_tick(
    known: &HashMap<String, Instant>,
    focused: Option<&str>,
    forced: &HashSet<String>,
    tick: u64,
    n: usize,
) -> RoundRobinPick {
    let n = if n == 0 { DEFAULT_ROUND_ROBIN_N } else { n };

    let mut repos: Vec<String> = Vec::new();
    let mut included: HashSet<&str> = HashSet::new();

    // Tier 0 — forced (sessioned) repos. Uncapped: every repo backing a
    // workspace with a persisted session is in every pick, bounded only
    // by how many worktrees the user has open. Emitted in name order so
    // the pick stays deterministic.
    let mut forced_sorted: Vec<&String> = forced.iter().collect();
    forced_sorted.sort();
    for repo in forced_sorted {
        if included.insert(repo.as_str()) {
            repos.push(repo.clone());
        }
    }

    // The focused repo is force-included even when it's absent from the
    // cursor (starved / suspended cold), so a manual Shift-R while
    // sitting on it always re-fetches. It rides the round-robin budget
    // rather than adding to it.
    let mut round_robin_budget = n;
    if let Some(focus) = focused
        && included.insert(focus)
    {
        repos.push(focus.to_string());
        round_robin_budget = round_robin_budget.saturating_sub(1);
    }

    if known.is_empty() {
        // Cold start: nothing to round-robin. Still emit any forced /
        // focused repos and run the global sweep to populate the cursor.
        return RoundRobinPick {
            repos,
            run_global: true,
        };
    }

    let mut sorted: Vec<(&String, &Instant)> = known.iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));

    let mut round_robin = 0usize;
    for (repo, _) in sorted {
        if round_robin >= round_robin_budget {
            break;
        }
        if included.contains(repo.as_str()) {
            continue;
        }
        repos.push(repo.clone());
        included.insert(repo.as_str());
        round_robin += 1;
    }

    let run_global = if known.len() <= n {
        tick.is_multiple_of(2)
    } else {
        let k = known.len().div_ceil(n).max(2) as u64;
        tick.is_multiple_of(k)
    };

    RoundRobinPick { repos, run_global }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn no_forced() -> HashSet<String> {
        HashSet::new()
    }

    fn forced(repos: &[&str]) -> HashSet<String> {
        repos.iter().map(|r| (*r).to_string()).collect()
    }

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
        let pick = pick_repos_for_tick(&HashMap::new(), None, &no_forced(), 0, 3);
        assert_eq!(pick.repos, Vec::<String>::new());
        assert!(pick.run_global, "cold start MUST run the global sweep");
    }

    /// A focused repo is fetched even on a cold start: the global sweep
    /// still runs to populate the cursor, but the repo the user is
    /// staring at gets its own `repo:` query rather than waiting for the
    /// sweep to discover it.
    #[test]
    fn cold_start_still_fetches_focus() {
        let pick = pick_repos_for_tick(&HashMap::new(), Some("a/b"), &no_forced(), 5, 3);
        assert_eq!(pick.repos, vec!["a/b".to_string()]);
        assert!(
            pick.run_global,
            "cold start MUST still run the global sweep"
        );
    }

    /// Sessioned (forced) repos are fetched on a cold start too, even
    /// before the cursor knows about them.
    #[test]
    fn cold_start_still_fetches_forced() {
        let pick = pick_repos_for_tick(&HashMap::new(), None, &forced(&["o/sessioned"]), 0, 3);
        assert_eq!(pick.repos, vec!["o/sessioned".to_string()]);
        assert!(pick.run_global);
    }

    #[test]
    fn cold_repos_leave_and_reenter_the_active_rotation() {
        let mut state = RoundRobinState {
            cursor: cursor(&[("cold/a", 1), ("warm/b", 2), ("cold/c", 3)]),
            ..RoundRobinState::default()
        };
        let cold = ["cold/a".to_string(), "cold/c".to_string()]
            .into_iter()
            .collect();
        let active = ["warm/b".to_string()].into_iter().collect();
        state.update_engagement_repos(&cold, &active, Instant::now());
        assert_eq!(
            state.cursor.keys().cloned().collect::<Vec<_>>(),
            vec!["warm/b".to_string()]
        );

        state.update_engagement_repos(
            &["cold/c".to_string()].into_iter().collect(),
            &["cold/a".to_string(), "warm/b".to_string()]
                .into_iter()
                .collect(),
            Instant::now(),
        );
        assert!(state.cursor.contains_key("cold/a"));
        assert!(state.cursor.contains_key("warm/b"));
        assert!(!state.cursor.contains_key("cold/c"));
    }

    #[test]
    fn cold_repo_without_a_cursor_is_restored_when_it_reopens() {
        let mut state = RoundRobinState::default();
        let now = Instant::now();
        state.update_engagement_repos(
            &["cold/a".to_string()].into_iter().collect(),
            &std::collections::HashSet::new(),
            now,
        );
        assert!(state.cursor.is_empty());

        state.update_engagement_repos(
            &std::collections::HashSet::new(),
            &["cold/a".to_string()].into_iter().collect(),
            now + Duration::from_secs(60),
        );
        assert_eq!(
            state.cursor.keys().cloned().collect::<Vec<_>>(),
            vec!["cold/a".to_string()]
        );
    }

    /// Stalest-first ordering across a uniform cursor.
    #[test]
    fn warm_picks_stalest_first() {
        // a synced at t=10, b at t=20, c at t=30 — a is stalest.
        let known = cursor(&[("a/a", 10), ("b/b", 20), ("c/c", 30)]);
        let pick = pick_repos_for_tick(&known, None, &no_forced(), 1, 2);
        assert_eq!(pick.repos, vec!["a/a".to_string(), "b/b".to_string()]);
    }

    /// Focused repo is bumped to the front even when it's not the
    /// stalest, and rides one of the N round-robin slots.
    #[test]
    fn focus_repo_is_bumped_to_front() {
        // c/c is the FRESHEST, so it'd normally be last in line.
        let known = cursor(&[("a/a", 10), ("b/b", 20), ("c/c", 30)]);
        let pick = pick_repos_for_tick(&known, Some("c/c"), &no_forced(), 1, 2);
        assert_eq!(
            pick.repos,
            vec!["c/c".to_string(), "a/a".to_string()],
            "focused repo MUST appear first; second slot keeps the stalest of the rest",
        );
    }

    /// A focused repo absent from the cursor (starved / suspended cold)
    /// is STILL fetched — this is the Shift-R-misses-the-focused-repo
    /// fix. It rides one round-robin slot; the rest keep rotating.
    #[test]
    fn focus_outside_cursor_is_still_fetched() {
        let known = cursor(&[("a/a", 10), ("b/b", 20)]);
        let pick = pick_repos_for_tick(&known, Some("unknown/repo"), &no_forced(), 1, 3);
        assert_eq!(
            pick.repos,
            vec![
                "unknown/repo".to_string(),
                "a/a".to_string(),
                "b/b".to_string()
            ],
        );
    }

    /// Every forced (sessioned) repo is included in every pick, uncapped
    /// — even far more than the round-robin budget N — and the N stalest
    /// non-forced repos still rotate on top.
    #[test]
    fn forced_repos_are_all_included_uncapped() {
        let known = cursor(&[
            ("cold/1", 10),
            ("cold/2", 20),
            ("cold/3", 30),
            ("cold/4", 40),
        ]);
        let sessioned = forced(&["s/a", "s/b", "s/c", "s/d", "s/e"]);
        let pick = pick_repos_for_tick(&known, None, &sessioned, 1, 2);
        for repo in ["s/a", "s/b", "s/c", "s/d", "s/e"] {
            assert!(
                pick.repos.contains(&repo.to_string()),
                "sessioned repo {repo} MUST be fetched every tick"
            );
        }
        // The two stalest non-forced cursor repos still rotate on top.
        assert!(pick.repos.contains(&"cold/1".to_string()));
        assert!(pick.repos.contains(&"cold/2".to_string()));
        assert_eq!(pick.repos.len(), 7);
    }

    /// A repo that is both focused and sessioned isn't double-counted,
    /// and doesn't steal an extra round-robin slot.
    #[test]
    fn focus_that_is_also_forced_is_not_double_counted() {
        let known = cursor(&[("a/a", 10), ("b/b", 20)]);
        let pick = pick_repos_for_tick(&known, Some("s/x"), &forced(&["s/x"]), 1, 2);
        assert_eq!(
            pick.repos,
            vec!["s/x".to_string(), "a/a".to_string(), "b/b".to_string()],
        );
    }

    /// Rate-budget regression: one tick's `repo:`-scoped fan-out is
    /// bounded by the session set plus the round-robin budget `n`, NOT
    /// by how many repos are in scope. A user with dozens of cold repos
    /// still pays only `sessioned + n` scoped searches per tick, and the
    /// focused repo rides a round-robin slot rather than inflating that.
    #[test]
    fn per_tick_fetch_count_is_bounded_by_sessions_plus_n() {
        // 50 cold repos in the cursor — past the reported 31-scoped scale.
        let entries: Vec<(&str, u64)> = (0..50)
            .map(|i| (Box::leak(format!("cold/r{i}").into_boxed_str()) as &str, i))
            .collect();
        let known = cursor(&entries);
        let sessioned = forced(&["s/a", "s/b", "s/c", "s/d", "s/e", "s/f"]);
        let n = 3;

        let pick = pick_repos_for_tick(&known, None, &sessioned, 7, n);
        assert_eq!(
            pick.repos.len(),
            sessioned.len() + n,
            "scoped fan-out must be sessioned + n, independent of the 50-repo cursor"
        );
        for repo in ["s/a", "s/b", "s/c", "s/d", "s/e", "s/f"] {
            assert!(pick.repos.contains(&repo.to_string()));
        }

        // A focused cursor repo consumes one of the `n` slots — it must
        // not push the per-tick count above the bound.
        let focused = pick_repos_for_tick(&known, Some("cold/r0"), &sessioned, 7, n);
        assert_eq!(focused.repos.len(), sessioned.len() + n);
        assert!(focused.repos.contains(&"cold/r0".to_string()));
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
            .filter(|t| pick_repos_for_tick(&known, None, &no_forced(), *t, 3).run_global)
            .collect();
        assert_eq!(
            global_ticks,
            vec![0, 4, 8, 12],
            "global sweep should run at tick 0 and every 4th tick thereafter"
        );
        // And every warm tick STILL covers 3 repos.
        for t in 1..16 {
            let p = pick_repos_for_tick(&known, None, &no_forced(), t, 3);
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
            let p = pick_repos_for_tick(&known, None, &no_forced(), t, 3);
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
        let pick = pick_repos_for_tick(&known, None, &no_forced(), 1, 2);
        assert_eq!(pick.repos, vec!["a/a".to_string(), "b/b".to_string()]);
    }

    /// `n=0` falls back to the default — a misconfigured caller
    /// gets sane behavior rather than an empty `repos` list.
    #[test]
    fn zero_n_falls_back_to_default() {
        let known = cursor(&[("a/a", 10), ("b/b", 20), ("c/c", 30), ("d/d", 40)]);
        let pick = pick_repos_for_tick(&known, None, &no_forced(), 1, 0);
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

    /// An incremental (no-full-sweep) tick must not advance the
    /// scheduler: no cursor stamps, no tick increment. Pre-fix, every
    /// 60s tick stamped its pick "synced" and bumped the counter even
    /// though the actual fetch only happens on full-sweep ticks
    /// (every ~10 min) — repos rotated out without ever being queried
    /// and the global-sweep modulus ran against an inflated counter.
    #[test]
    fn plan_skips_state_advance_when_no_full_sweep() {
        let mut state = RoundRobinState::default();
        let now = Instant::now();
        state
            .cursor
            .insert("a/a".into(), now - Duration::from_secs(60));
        state.tick = 3;

        let pick = plan_round_robin_tick(
            &mut state,
            &no_forced(),
            /*will_full_sweep=*/ false,
            3,
            now,
        );

        assert!(pick.repos.is_empty(), "no repos queried on a fast tick");
        assert!(
            pick.run_global,
            "promotion fallback must run the global sweep, not an empty one"
        );
        assert_eq!(state.tick, 3, "tick counter must not advance");
        assert_eq!(
            state.cursor.get("a/a"),
            Some(&(now - Duration::from_secs(60))),
            "cursor must not be stamped for repos that were never queried"
        );
    }

    /// A full-sweep tick advances the rotation exactly like the
    /// historical inline path: picked repos get stamped, the counter
    /// increments.
    #[test]
    fn plan_advances_state_on_full_sweep() {
        let mut state = RoundRobinState::default();
        let now = Instant::now();
        let stale = now - Duration::from_secs(600);
        state.cursor.insert("a/a".into(), stale);
        state.cursor.insert("b/b".into(), stale);
        state.tick = 1;

        let pick = plan_round_robin_tick(
            &mut state,
            &no_forced(),
            /*will_full_sweep=*/ true,
            3,
            now,
        );

        assert_eq!(pick.repos.len(), 2, "both known repos fan out");
        assert_eq!(state.tick, 2, "tick counter advances after the pick");
        for repo in ["a/a", "b/b"] {
            assert_eq!(
                state.cursor.get(repo),
                Some(&now),
                "queried repo must be stamped as just-synced"
            );
        }
    }

    /// A full-sweep tick fetches every forced (sessioned) repo even
    /// when the cursor has never seen it, and stamps it synced so it
    /// enters the active rotation.
    #[test]
    fn plan_includes_and_stamps_forced_repos() {
        let mut state = RoundRobinState::default();
        let now = Instant::now();
        state
            .cursor
            .insert("warm/a".into(), now - Duration::from_secs(600));

        let pick = plan_round_robin_tick(
            &mut state,
            &forced(&["s/one", "s/two"]),
            /*will_full_sweep=*/ true,
            3,
            now,
        );

        for repo in ["s/one", "s/two"] {
            assert!(
                pick.repos.contains(&repo.to_string()),
                "sessioned repo {repo} MUST be queried"
            );
            assert_eq!(
                state.cursor.get(repo),
                Some(&now),
                "queried sessioned repo must be stamped as just-synced"
            );
        }
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
