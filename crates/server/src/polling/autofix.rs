//! Stateful guards for auto-fix: cooldown + max-attempts, persisted in
//! the store's kv table so they survive daemon restarts.
//!
//! The *pure* "should we touch this PR" decision lives in
//! `lazybox_core::autofix`. This module adds the part that needs state:
//! once the pure layer says "yes, fix the CI on this PR", we still
//! have to decide whether we've already tried too many times, or tried
//! too recently. Without this guard a chronically-red PR would re-spawn
//! an agent every full sweep forever.
//!
//! ## Storage shape
//!
//! One kv entry per `(workspace, failure-kind)`:
//! `autofix:<session-key>:<ci|conflict>` → JSON [`AttemptRecord`]. CI
//! failures and merge conflicts keep separate budgets so a flaky test
//! can't starve the conflict-resolution allowance.
//!
//! ## Window semantics
//!
//! `max_attempts` is measured over a rolling `window` (default 24h)
//! anchored at the first attempt in the window. Once `window` elapses
//! since that anchor, the whole record resets and the PR gets a fresh
//! budget — so a PR that broke, got fixed, and broke again next week
//! isn't permanently locked out.

use chrono::{DateTime, Utc};
use lazybox_core::{AutoFixKind, AutoFixSettings};
use lazybox_store::Store;
use serde::{Deserialize, Serialize};

/// Persisted per-(workspace, kind) attempt bookkeeping.
///
/// `#[serde(default)]` on the container so a record written by an older
/// build (missing a field added later) still deserializes — the absent
/// field falls back to its `Default` instead of failing the parse and
/// silently resetting the whole counter. Keep every field `Default`-able.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct AttemptRecord {
    /// Attempts made within the current window.
    attempts: u32,
    /// When the current window started (the first attempt's time).
    /// `None` before the first attempt.
    window_start: Option<DateTime<Utc>>,
    /// When the most recent attempt fired — drives the cooldown gate.
    last_attempt: Option<DateTime<Utc>>,
    /// Whether we've already posted the "budget exhausted" notice, so
    /// it surfaces exactly once per window instead of every sweep.
    #[serde(default)]
    exhausted_notified: bool,
}

/// Outcome of consulting (and possibly updating) the attempt record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptDecision {
    /// Go ahead — this is attempt `attempt` of `max`. The record has
    /// already been incremented + persisted.
    Proceed { attempt: u32, max: u32 },
    /// Within the cooldown gap since the last attempt — skip silently.
    Cooldown,
    /// Budget exhausted within the window. `notify` is `true` exactly
    /// once (the first exhausted sweep) so the caller can surface it
    /// without spamming a comment every poll.
    Exhausted { notify: bool },
}

/// Build the kv key for a `(workspace, kind)` attempt record. Distinct
/// from `AutoFixKind::store_key`, which yields just the kind
/// discriminant this interpolates.
fn record_key(session_key: &str, kind: AutoFixKind) -> String {
    format!("autofix:{session_key}:{}", kind.store_key())
}

/// `std::time::Duration` → `chrono::Duration`, saturating to
/// `Duration::MAX` on the only failure mode (overflow at ~292M years).
/// Saturating — not snapping to some small default — keeps a
/// pathologically-large configured window/cooldown behaving as
/// "effectively never" instead of silently shrinking it.
fn to_chrono(d: std::time::Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

fn load(store: &dyn Store, key: &str) -> AttemptRecord {
    store
        .get_kv(key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn persist(store: &dyn Store, key: &str, rec: &AttemptRecord) {
    match serde_json::to_string(rec) {
        Ok(json) => {
            if let Err(e) = store.set_kv(key, &json) {
                tracing::warn!("autofix: failed to persist attempt record for {key}: {e}");
            }
        }
        Err(e) => tracing::warn!("autofix: failed to serialize attempt record for {key}: {e}"),
    }
}

/// Consult the cooldown + max-attempts guard for `(session_key, kind)`
/// and, when it returns [`AttemptDecision::Proceed`], record the
/// attempt. `now` is injected so this is deterministically testable.
pub fn check_and_record(
    store: &dyn Store,
    session_key: &str,
    kind: AutoFixKind,
    settings: &AutoFixSettings,
    now: DateTime<Utc>,
) -> AttemptDecision {
    // `max_attempts: 0` is effectively "disabled" — never attempt, and
    // never post the "budget exhausted" notice (there was no attempt to
    // exhaust). Short-circuit before touching the store so we don't
    // write a record or surface a nonsensical "0 attempts" comment.
    if settings.max_attempts == 0 {
        return AttemptDecision::Exhausted { notify: false };
    }

    let key = record_key(session_key, kind);
    let mut rec = load(store, &key);

    // Window reset: a never-started record, or one whose window has
    // fully elapsed, starts fresh (counter + cooldown + notify flag).
    let window = to_chrono(settings.window);
    let window_expired = rec
        .window_start
        .is_none_or(|start| now.signed_duration_since(start) >= window);
    if window_expired {
        rec = AttemptRecord::default();
    }

    // Budget exhausted within the window.
    if rec.attempts >= settings.max_attempts {
        let notify = !rec.exhausted_notified;
        if notify {
            rec.exhausted_notified = true;
            persist(store, &key, &rec);
        }
        return AttemptDecision::Exhausted { notify };
    }

    // Cooldown since the last attempt.
    if let Some(last) = rec.last_attempt
        && now.signed_duration_since(last) < to_chrono(settings.cooldown)
    {
        return AttemptDecision::Cooldown;
    }

    // Proceed — record the attempt.
    if rec.window_start.is_none() {
        rec.window_start = Some(now);
    }
    rec.attempts += 1;
    rec.last_attempt = Some(now);
    persist(store, &key, &rec);
    AttemptDecision::Proceed {
        attempt: rec.attempts,
        max: settings.max_attempts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_store::MemoryStore;
    use std::time::Duration;

    fn settings() -> AutoFixSettings {
        AutoFixSettings {
            enabled: true,
            opt_out_labels: vec![],
            max_attempts: 3,
            cooldown: Duration::from_secs(3600),
            window: Duration::from_secs(24 * 3600),
        }
    }

    fn t0() -> DateTime<Utc> {
        "2026-05-28T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn first_attempt_proceeds() {
        let store = MemoryStore::new();
        let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &settings(), t0());
        assert_eq!(d, AttemptDecision::Proceed { attempt: 1, max: 3 });
    }

    #[test]
    fn second_attempt_within_cooldown_is_blocked() {
        let store = MemoryStore::new();
        let s = settings();
        check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, t0());
        // 10 minutes later — well inside the 1h cooldown.
        let later = t0() + chrono::Duration::minutes(10);
        let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, later);
        assert_eq!(d, AttemptDecision::Cooldown);
    }

    #[test]
    fn attempt_proceeds_after_cooldown_clears() {
        let store = MemoryStore::new();
        let s = settings();
        check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, t0());
        let later = t0() + chrono::Duration::minutes(61);
        let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, later);
        assert_eq!(d, AttemptDecision::Proceed { attempt: 2, max: 3 });
    }

    #[test]
    fn exhausts_after_max_attempts_and_notifies_once() {
        let store = MemoryStore::new();
        let s = settings();
        // Three attempts, each past the cooldown.
        for i in 0..3 {
            let when = t0() + chrono::Duration::hours(i * 2);
            let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, when);
            assert!(matches!(d, AttemptDecision::Proceed { .. }), "attempt {i}");
        }
        // Fourth → exhausted, notify true exactly once.
        let when = t0() + chrono::Duration::hours(7);
        let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, when);
        assert_eq!(d, AttemptDecision::Exhausted { notify: true });
        let when = t0() + chrono::Duration::hours(9);
        let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, when);
        assert_eq!(d, AttemptDecision::Exhausted { notify: false });
    }

    #[test]
    fn window_expiry_resets_the_budget() {
        let store = MemoryStore::new();
        let s = settings();
        for i in 0..3 {
            let when = t0() + chrono::Duration::hours(i * 2);
            check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, when);
        }
        // Exhausted within the window.
        assert!(matches!(
            check_and_record(
                &store,
                "ws",
                AutoFixKind::CiFailure,
                &s,
                t0() + chrono::Duration::hours(7)
            ),
            AttemptDecision::Exhausted { .. }
        ));
        // >24h after the window started → fresh budget.
        let next_day = t0() + chrono::Duration::hours(25);
        let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, next_day);
        assert_eq!(d, AttemptDecision::Proceed { attempt: 1, max: 3 });
    }

    #[test]
    fn ci_and_conflict_budgets_are_independent() {
        let store = MemoryStore::new();
        let s = settings();
        check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, t0());
        // Conflict on the same workspace starts at attempt 1 — separate key.
        let d = check_and_record(&store, "ws", AutoFixKind::MergeConflict, &s, t0());
        assert_eq!(d, AttemptDecision::Proceed { attempt: 1, max: 3 });
    }

    #[test]
    fn max_attempts_zero_is_silently_disabled() {
        let store = MemoryStore::new();
        let s = AutoFixSettings {
            max_attempts: 0,
            ..settings()
        };
        // Never proceeds, never notifies (no attempt to "exhaust"),
        // and writes nothing to the store.
        let d = check_and_record(&store, "ws", AutoFixKind::CiFailure, &s, t0());
        assert_eq!(d, AttemptDecision::Exhausted { notify: false });
        assert_eq!(store.get_kv("autofix:ws:ci").unwrap(), None);
    }

    #[test]
    fn distinct_workspaces_are_independent() {
        let store = MemoryStore::new();
        let s = settings();
        check_and_record(&store, "ws-a", AutoFixKind::CiFailure, &s, t0());
        let d = check_and_record(&store, "ws-b", AutoFixKind::CiFailure, &s, t0());
        assert_eq!(d, AttemptDecision::Proceed { attempt: 1, max: 3 });
    }
}
