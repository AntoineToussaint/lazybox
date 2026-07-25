//! `RateBudget` — two-layer rate limiting for the GitHub client.
//!
//! ## Why two layers
//!
//! 1. **Local guard rail.** A self-imposed cap on requests per minute,
//!    independent of what GitHub allows. The point isn't to mirror
//!    GitHub's policy — it's a circuit breaker. If lazybox has a bug
//!    and starts looping, we want it to stop on its own before
//!    burning the user's daily quota.
//!
//!    Set conservatively at 30 req/min (way more than the steady-state
//!    needs, way less than what's required to do real damage). When
//!    the local budget hits zero, requests fail-fast with a Reason
//!    the polling layer surfaces — instead of the request blasting
//!    GitHub.
//!
//! 2. **Remote awareness.** Each GraphQL response carries
//!    `rateLimit { remaining, resetAt }`. We capture the latest
//!    observation; when remaining drops below `LOW_THRESHOLD`
//!    (default 100), we pause polling until reset. The user keeps a
//!    safety margin instead of running their token dry.
//!
//! ## Lifecycle
//!
//! - `RateBudget::new()` builds a default budget.
//! - `try_acquire()` is called *before* each GraphQL request; on
//!   `Err(reason)` the caller doesn't fire the request.
//! - `observe(remote)` is called *after* a successful response so the
//!   budget tracks GitHub's reality.
//! - `snapshot()` returns the current state for status bar / logs.
//!
//! Shared between `GhClient` instances + the polling task via
//! `Arc<Mutex<RateBudget>>`.

use chrono::{DateTime, Utc};
use std::time::Instant;

/// Soft limit on local-bucket failures. When `remaining` reported by
/// GitHub falls under this, we treat it as "low" and back off.
pub const LOW_THRESHOLD: u32 = 100;

/// Default local-bucket capacity. 30 requests bursting; refill at
/// the same rate per minute. Steady-state polling at 60s intervals
/// uses 1–2 of these; the rest is headroom for the setup wizard's
/// detection / scope-listing calls.
pub const DEFAULT_CAPACITY: u32 = 30;

/// Default refill rate (tokens per minute). Matches `DEFAULT_CAPACITY`
/// so the bucket is full after a minute of idle.
pub const DEFAULT_REFILL_PER_MIN: f64 = 30.0;

#[derive(Debug, Clone)]
pub struct RemoteRateLimit {
    pub remaining: u32,
    pub limit: u32,
    pub reset_at: DateTime<Utc>,
    pub observed_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// Local self-imposed cap reached. Caller should NOT make the
    /// request. `wait_secs` is how long until the bucket has at
    /// least one token again.
    LocalBudgetExhausted { wait_secs: u64 },
    /// GitHub's reported `remaining` is at or below `LOW_THRESHOLD`.
    /// Caller should NOT make the request until `reset_at`.
    RemoteLow {
        remaining: u32,
        reset_at: DateTime<Utc>,
    },
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalBudgetExhausted { wait_secs } => write!(
                f,
                "lazybox's local rate budget is empty (wait {wait_secs}s) — \
                 we throttle ourselves to avoid runaway loops"
            ),
            Self::RemoteLow {
                remaining,
                reset_at,
            } => write!(
                f,
                "GitHub rate limit low ({remaining} remaining, resets {reset_at})"
            ),
        }
    }
}

impl std::error::Error for AcquireError {}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub local_available: f64,
    pub local_capacity: u32,
    pub remote: Option<RemoteRateLimit>,
}

pub struct RateBudget {
    capacity: u32,
    available: f64,
    refill_per_sec: f64,
    last_refill: Instant,
    remote: Option<RemoteRateLimit>,
    /// When set, force the next `try_acquire` to fail with this
    /// error. Used in tests; production callers ignore.
    #[cfg(test)]
    force_fail: Option<AcquireError>,
}

impl RateBudget {
    pub fn new(capacity: u32, refill_per_min: f64) -> Self {
        Self {
            capacity,
            available: capacity as f64,
            refill_per_sec: refill_per_min / 60.0,
            last_refill: Instant::now(),
            remote: None,
            #[cfg(test)]
            force_fail: None,
        }
    }

    pub fn default_for_lazybox() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_REFILL_PER_MIN)
    }

    fn refill(&mut self) {
        self.refill_at(Instant::now());
    }

    /// Refill math with the clock passed in — the parameterized-now
    /// pattern (mirrors `pick_repos_for_tick(known, now)` in the
    /// server's scheduler) so the per-minute arithmetic is unit-
    /// testable without sleeping. A `now` earlier than `last_refill`
    /// (monotonic clocks shouldn't produce one, but a stale snapshot
    /// clone could) contributes zero elapsed time rather than
    /// draining the bucket.
    fn refill_at(&mut self, now: Instant) {
        let elapsed = now
            .checked_duration_since(self.last_refill)
            .unwrap_or_default()
            .as_secs_f64();
        self.available = (self.available + elapsed * self.refill_per_sec).min(self.capacity as f64);
        self.last_refill = now;
    }

    /// Try to spend one token AND check remote rate limit. Returns
    /// `Ok(())` when the caller may make the request; `Err(reason)`
    /// otherwise.
    ///
    /// Order of checks: remote first (cheap, no state mutation),
    /// then local. So the surfaced reason matches the actual
    /// blocker.
    pub fn try_acquire(&mut self) -> Result<(), AcquireError> {
        self.try_acquire_at(Instant::now())
    }

    /// [`try_acquire`](Self::try_acquire) with the monotonic clock
    /// passed in — lets tests drive the local-bucket refill math
    /// deterministically. The remote-window check still reads the
    /// wall clock (`Utc::now`): GitHub's `reset_at` is a wall-clock
    /// deadline and has no meaningful `Instant` representation.
    fn try_acquire_at(&mut self, now: Instant) -> Result<(), AcquireError> {
        #[cfg(test)]
        if let Some(forced) = self.force_fail.take() {
            return Err(forced);
        }

        // 1. Remote check.
        if let Some(remote) = &self.remote {
            let wall_now = Utc::now();
            if remote.remaining <= LOW_THRESHOLD && remote.reset_at > wall_now {
                return Err(AcquireError::RemoteLow {
                    remaining: remote.remaining,
                    reset_at: remote.reset_at,
                });
            }
        }

        // 2. Local refill + spend.
        self.refill_at(now);
        if self.available >= 1.0 {
            self.available -= 1.0;
            Ok(())
        } else {
            // Compute how long until one token is available.
            let needed = 1.0 - self.available;
            let wait_secs = (needed / self.refill_per_sec).ceil() as u64;
            Err(AcquireError::LocalBudgetExhausted { wait_secs })
        }
    }

    /// Record the most recent remote rate-limit observation. Call
    /// this after every successful GraphQL response.
    pub fn observe(&mut self, remote: RemoteRateLimit) {
        self.remote = Some(remote);
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut clone = self.clone_for_snapshot();
        clone.refill();
        Snapshot {
            local_available: clone.available,
            local_capacity: self.capacity,
            remote: self.remote.clone(),
        }
    }

    fn clone_for_snapshot(&self) -> Self {
        Self {
            capacity: self.capacity,
            available: self.available,
            refill_per_sec: self.refill_per_sec,
            last_refill: self.last_refill,
            remote: self.remote.clone(),
            #[cfg(test)]
            force_fail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn fresh_bucket_allows_one_acquire() {
        let mut b = RateBudget::new(2, 60.0);
        assert!(b.try_acquire().is_ok());
        assert!(b.try_acquire().is_ok());
        // Third should fail — bucket exhausted.
        match b.try_acquire() {
            Err(AcquireError::LocalBudgetExhausted { .. }) => {}
            other => panic!("expected LocalBudgetExhausted, got {other:?}"),
        }
    }

    #[test]
    fn low_remote_blocks_even_with_local_tokens() {
        let mut b = RateBudget::new(100, 60.0);
        b.observe(RemoteRateLimit {
            remaining: 5,
            limit: 5000,
            reset_at: Utc::now() + Duration::seconds(60),
            observed_at: Instant::now(),
        });
        match b.try_acquire() {
            Err(AcquireError::RemoteLow { remaining, .. }) => assert_eq!(remaining, 5),
            other => panic!("expected RemoteLow, got {other:?}"),
        }
    }

    #[test]
    fn expired_remote_low_doesnt_block() {
        let mut b = RateBudget::new(100, 60.0);
        b.observe(RemoteRateLimit {
            remaining: 0,
            limit: 5000,
            // Reset already passed → don't honor the old observation.
            reset_at: Utc::now() - Duration::seconds(1),
            observed_at: Instant::now(),
        });
        assert!(b.try_acquire().is_ok());
    }

    #[test]
    fn snapshot_reports_local_available() {
        let b = RateBudget::new(10, 60.0);
        let s = b.snapshot();
        assert!((s.local_available - 10.0).abs() < 0.01);
        assert_eq!(s.local_capacity, 10);
    }

    // ── refill math (parameterized now — issue: `refill` read
    // `Instant::now()` internally so none of this was testable) ──

    /// Drain a bucket to zero at a fixed `now` without letting the
    /// wall clock refill anything mid-drain.
    fn drain_at(b: &mut RateBudget, now: Instant) {
        while b.try_acquire_at(now).is_ok() {}
    }

    #[test]
    fn refill_exact_after_sixty_seconds() {
        // 30 tokens/min: a fully drained bucket is exactly full again
        // after 60s — no more, no less.
        let now = Instant::now();
        let mut b = RateBudget::new(30, 30.0);
        drain_at(&mut b, now);
        assert!(b.available < 1.0, "bucket should be drained");
        b.refill_at(now + std::time::Duration::from_secs(60));
        assert!(
            (b.available - 30.0).abs() < 1e-6,
            "60s at 30/min refills exactly to capacity, got {}",
            b.available
        );
    }

    #[test]
    fn refill_partial_is_proportional() {
        // 30/min = 0.5/s → 10s refills 5 tokens.
        let now = Instant::now();
        let mut b = RateBudget::new(30, 30.0);
        drain_at(&mut b, now);
        let leftover = b.available; // fractional remainder < 1.0
        b.refill_at(now + std::time::Duration::from_secs(10));
        assert!(
            (b.available - (leftover + 5.0)).abs() < 1e-6,
            "10s at 0.5 tok/s should add exactly 5 tokens, got {}",
            b.available
        );
    }

    #[test]
    fn refill_zero_elapsed_changes_nothing() {
        let now = Instant::now();
        let mut b = RateBudget::new(30, 30.0);
        assert!(b.try_acquire_at(now).is_ok());
        let before = b.available;
        b.refill_at(now);
        assert!(
            (b.available - before).abs() < 1e-9,
            "no time passed → no refill (got {} vs {})",
            b.available,
            before
        );
    }

    #[test]
    fn refill_clamps_at_capacity() {
        // An hour of idle must not overfill past capacity.
        let now = Instant::now();
        let mut b = RateBudget::new(30, 30.0);
        assert!(b.try_acquire_at(now).is_ok());
        b.refill_at(now + std::time::Duration::from_secs(3600));
        assert!(
            (b.available - 30.0).abs() < 1e-9,
            "refill clamps at capacity, got {}",
            b.available
        );
    }

    #[test]
    fn refill_tolerates_now_before_last_refill() {
        // A `now` older than `last_refill` contributes zero elapsed
        // time instead of panicking or going negative.
        let now = Instant::now();
        let mut b = RateBudget::new(30, 30.0);
        b.refill_at(now + std::time::Duration::from_secs(5));
        let before = b.available;
        b.refill_at(now);
        assert!((b.available - before).abs() < 1e-9);
    }

    #[test]
    fn local_wait_secs_matches_refill_rate() {
        // Drained bucket at 30/min: the surfaced wait for one token
        // is ceil(1 token / 0.5 tok-per-s) = 2s.
        let now = Instant::now();
        let mut b = RateBudget::new(30, 30.0);
        drain_at(&mut b, now);
        // Consume the fractional leftover so `needed` is deterministic:
        // available is in [0,1) after the drain; wait covers 1-available.
        match b.try_acquire_at(now) {
            Err(AcquireError::LocalBudgetExhausted { wait_secs }) => {
                assert!(
                    (1..=2).contains(&wait_secs),
                    "one-token wait at 0.5 tok/s is 1-2s, got {wait_secs}"
                );
            }
            other => panic!("expected LocalBudgetExhausted, got {other:?}"),
        }
    }

    /// The 429/secondary-limit feedback path: the client observes a
    /// throttle response as `remaining: 0, reset_at: now + retry_after`
    /// — from then on `try_acquire` must refuse admission until the
    /// window passes, even with a full local bucket.
    #[test]
    fn observed_throttle_blocks_admission_until_reset() {
        let mut b = RateBudget::new(30, 30.0);
        b.observe(RemoteRateLimit {
            remaining: 0,
            limit: 5000,
            reset_at: Utc::now() + Duration::seconds(30),
            observed_at: Instant::now(),
        });
        match b.try_acquire() {
            Err(AcquireError::RemoteLow {
                remaining,
                reset_at,
            }) => {
                assert_eq!(remaining, 0);
                assert!(reset_at > Utc::now());
            }
            other => panic!("expected RemoteLow after a 429 observation, got {other:?}"),
        }
    }

    #[test]
    fn observe_updates_remote() {
        let mut b = RateBudget::new(10, 60.0);
        let r = RemoteRateLimit {
            remaining: 4500,
            limit: 5000,
            reset_at: Utc::now() + Duration::seconds(3600),
            observed_at: Instant::now(),
        };
        b.observe(r);
        let s = b.snapshot();
        assert_eq!(s.remote.unwrap().remaining, 4500);
    }
}
