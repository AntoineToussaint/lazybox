//! Shared GitHub API admission control and accounting.
//!
//! GitHub exposes independent primary budgets for GraphQL points and
//! each REST resource bucket. Secondary limits, however, apply across
//! both APIs. `RateBudget` models those facts in one lock shared by all
//! `GhClient` clones. Scheduled work receives a sustainable per-tick
//! allowance that protects a configurable reserve; interactive work is
//! admitted against GitHub's real remaining capacity and the emergency
//! floor only.

use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Emergency backstop. Proactive background admission normally stops
/// substantially earlier at the configured reserve.
pub const LOW_THRESHOLD: u32 = 100;

pub const DEFAULT_CAPACITY: u32 = 30;
pub const DEFAULT_REFILL_PER_MIN: f64 = 30.0;
pub const DEFAULT_BACKGROUND_SHARE: f64 = 0.55;

/// Guaranteed per-tick background allowance while external usage
/// contends for the shared token. GitHub's token is also spent by
/// interactive `gh` and spawned agents; when that external burn is high
/// the sustainable-rate and headroom math can drive the daemon's own
/// tick allowance to zero, stalling sync indefinitely even while the
/// primary budget is perfectly healthy (#782). This floor keeps sync
/// *slowing* under contention instead of stopping — enough to admit a
/// complete background sweep unit each tick. It only relaxes the soft
/// per-tick throttle: the hard `RemoteLow` / `ReserveProtected` guards in
/// [`RateBudget::admit`] still protect the real reserve, so the floor can
/// never push actual spend past the reserve.
pub const MIN_BACKGROUND_TICK_ALLOWANCE: u32 = 30;
pub const DEFAULT_SECONDARY_PAUSE: Duration = Duration::from_secs(60);
const MAX_SECONDARY_PAUSE: Duration = Duration::from_secs(15 * 60);
const LATENCY_SAMPLE_CAPACITY: usize = 1024;

/// Baseline spacing between the *start* of consecutive GitHub
/// requests. GitHub's secondary (abuse) limit keys on burst rate and
/// concurrency, not the primary 5000/h budget, so a sweep that fires
/// its whole allowance back-to-back trips it with thousands of primary
/// points still unspent. The concurrency gate bounds in-flight
/// requests; this bounds how fast new ones may launch.
pub(crate) const DEFAULT_MIN_REQUEST_GAP: Duration = Duration::from_millis(200);
/// Ceiling on the adaptive gap so a hot token never stalls a request
/// for longer than the poll cadence would tolerate.
const MAX_REQUEST_GAP: Duration = Duration::from_secs(5);
/// External requests/sec on the shared token at which spacing doubles.
/// Feeds the measured `ext/h` burn back into daemon pacing: heavy
/// interactive/agent `gh` usage widens the daemon's own gaps so its
/// requests don't stack on top of an already-bursting token.
const EXTERNAL_CONTENTION_REF: f64 = 1.0;
/// Cap on the external-contention widening factor.
const MAX_CONTENTION_MULT: f64 = 4.0;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ApiResource {
    Graphql,
    Rest(String),
}

impl ApiResource {
    pub fn rest(resource: impl Into<String>) -> Self {
        Self::Rest(resource.into())
    }

    pub fn key(&self) -> &str {
        match self {
            Self::Graphql => "graphql",
            Self::Rest(resource) => resource,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPriority {
    Interactive,
    Focused,
    Sessioned,
    Recent,
    Cold,
}

impl RequestPriority {
    fn is_scheduled(self) -> bool {
        !matches!(self, Self::Interactive)
    }
}

#[derive(Debug, Clone)]
pub struct RemoteRateLimit {
    pub remaining: u32,
    pub limit: u32,
    pub reset_at: DateTime<Utc>,
    pub observed_at: Instant,
}

/// Durable slice of the rate/throttle state — everything an admission
/// decision needs that is anchored to wall-clock time (and so survives a
/// process restart) rather than to a monotonic `Instant`. The local
/// token bucket and latency samples are deliberately omitted: they refill
/// or repopulate within seconds and mean nothing across a restart. The
/// server persists this per authenticated user and reloads it at startup
/// so a fresh daemon resumes respecting the primary budget, secondary
/// cooldown, external-usage estimate, and backoff level it already
/// learned instead of re-bursting into the same throttle.
/// Keyed by primary-bucket name (`core`, `graphql`, `search`, …) so the
/// serialized form is deterministic across ticks — an unchanged budget
/// re-serializes byte-for-byte, which the poller relies on to skip
/// redundant writes.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedRateState {
    #[serde(default)]
    pub resources: BTreeMap<String, PersistedResource>,
    #[serde(default)]
    pub secondary_circuit: Option<PersistedCooldown>,
    #[serde(default)]
    pub primary_circuits: BTreeMap<String, PersistedCooldown>,
    #[serde(default)]
    pub secondary_failures: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedResource {
    pub remaining: u32,
    pub limit: u32,
    pub used: u32,
    pub reset_at: DateTime<Utc>,
    /// Estimated non-daemon burn (interactive `gh`, spawned agents) on
    /// the shared token, in points per second. Carrying it over keeps
    /// contention awareness from resetting to zero on restart.
    pub external_burn_per_sec: f64,
}

/// A persisted circuit-breaker window — the wall-clock instant traffic may
/// resume and why it paused. Used for both the single global secondary
/// cooldown and each per-resource primary circuit (keyed by resource in
/// [`PersistedRateState::primary_circuits`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedCooldown {
    pub reason: String,
    pub retry_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    LocalBudgetExhausted {
        wait_secs: u64,
    },
    RemoteLow {
        remaining: u32,
        reset_at: DateTime<Utc>,
    },
    ReserveProtected {
        resource: String,
        remaining: u32,
        reserve: u32,
        reset_at: DateTime<Utc>,
    },
    TickAllowanceExhausted {
        resource: String,
        allowance: u32,
        spent: u32,
        wait_secs: u64,
    },
    CircuitOpen {
        reason: String,
        retry_at: DateTime<Utc>,
    },
}

impl AcquireError {
    /// True when this refusal is lazybox's OWN governor pacing itself
    /// (local bucket empty, per-tick background allowance spent, or the
    /// soft reserve protecting a healthy budget) rather than a limit
    /// GitHub imposed (`RemoteLow`, `CircuitOpen`). A voluntary backoff
    /// under shared-token contention must never be reported to the user
    /// as a connection/token failure (#782).
    pub fn is_self_imposed(&self) -> bool {
        matches!(
            self,
            Self::LocalBudgetExhausted { .. }
                | Self::TickAllowanceExhausted { .. }
                | Self::ReserveProtected { .. }
        )
    }

    pub fn retry_after_secs(&self, now: DateTime<Utc>) -> u64 {
        match self {
            Self::LocalBudgetExhausted { wait_secs }
            | Self::TickAllowanceExhausted { wait_secs, .. } => (*wait_secs).max(1),
            Self::RemoteLow { reset_at, .. }
            | Self::ReserveProtected { reset_at, .. }
            | Self::CircuitOpen {
                retry_at: reset_at, ..
            } => reset_at
                .signed_duration_since(now)
                .to_std()
                .unwrap_or_default()
                .as_secs()
                .max(1),
        }
    }
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalBudgetExhausted { wait_secs } => write!(
                f,
                "lazybox's local rate budget is empty (wait {wait_secs}s)"
            ),
            Self::RemoteLow {
                remaining,
                reset_at,
            } => write!(
                f,
                "GitHub rate limit low ({remaining} remaining, resets {reset_at})"
            ),
            Self::ReserveProtected {
                resource,
                remaining,
                reserve,
                reset_at,
            } => write!(
                f,
                "GitHub {resource} reserve protected ({remaining} remaining, \
                 {reserve} reserved, resets {reset_at})"
            ),
            Self::TickAllowanceExhausted {
                resource,
                allowance,
                spent,
                wait_secs,
            } => write!(
                f,
                "GitHub {resource} background allowance spent \
                 ({spent}/{allowance}, retry in {wait_secs}s)"
            ),
            Self::CircuitOpen { reason, retry_at } => {
                write!(f, "GitHub traffic paused until {retry_at}: {reason}")
            }
        }
    }
}

impl std::error::Error for AcquireError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountingSnapshot {
    pub requests: u64,
    pub graphql_points: u64,
    pub rest_points: u64,
    pub bytes: u64,
    pub duration_ms: u64,
    pub conditional_hits: u64,
    pub forecast_points: u64,
    pub forecast_error_points: i64,
    pub items: u64,
    pub duplicates: u64,
}

#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    pub resource: String,
    pub remaining: u32,
    pub limit: u32,
    pub used: u32,
    pub reset_at: DateTime<Utc>,
    pub blocked_reason: Option<String>,
    pub eligible_at: Option<DateTime<Utc>>,
    pub reserve: u32,
    pub allowance: u32,
    pub scheduled: u32,
    pub external_burn_per_hour: f64,
}

#[derive(Debug, Clone)]
pub struct OperationSnapshot {
    pub class: String,
    pub forecast: u32,
    pub unit_forecast: u32,
    pub last_actual: Option<u32>,
    pub material_forecast_errors: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub local_available: f64,
    pub local_capacity: u32,
    /// Compatibility view of the GraphQL primary budget.
    pub remote: Option<RemoteRateLimit>,
    pub background_share: f64,
    pub resources: Vec<ResourceSnapshot>,
    pub tick: AccountingSnapshot,
    pub total: AccountingSnapshot,
    pub request_p50_ms: Option<u64>,
    pub request_p95_ms: Option<u64>,
    pub request_p99_ms: Option<u64>,
    pub circuit_reason: Option<String>,
    pub retry_at: Option<DateTime<Utc>>,
    pub operations: Vec<OperationSnapshot>,
}

impl Snapshot {
    pub fn compact(&self) -> String {
        let resources = self
            .resources
            .iter()
            .map(|resource| {
                format!(
                    "{} {}/{} reset={} reserve={} allowance={}/{} ext={:.0}/h{}{}",
                    resource.resource,
                    resource.remaining,
                    resource.limit,
                    resource.reset_at.to_rfc3339(),
                    resource.reserve,
                    resource.scheduled,
                    resource.allowance,
                    resource.external_burn_per_hour,
                    resource
                        .eligible_at
                        .map(|at| format!(" eligible={at}"))
                        .unwrap_or_default(),
                    resource
                        .blocked_reason
                        .as_ref()
                        .map(|reason| format!(" blocked={reason}"))
                        .unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let retry = self
            .retry_at
            .map(|at| format!(" · paused until {at}"))
            .unwrap_or_default();
        format!(
            "share={:.0}% · {} · tick req={} gql={} rest={} bytes={} changed={} dedup={} p95={}ms{}",
            self.background_share * 100.0,
            resources,
            self.tick.requests,
            self.tick.graphql_points,
            self.tick.rest_points,
            self.tick.bytes,
            self.tick.items,
            self.tick.duplicates,
            self.request_p95_ms.unwrap_or(0),
            retry,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundPlan {
    pub graphql_points: u32,
    pub rest_core_points: u32,
    pub graphql_budget_current: bool,
    pub pressure: bool,
    pub next_eligible_at: Option<DateTime<Utc>>,
    pub tick_interval: Duration,
}

impl BackgroundPlan {
    pub fn admits_complete_graphql_unit(&self, manual: bool, required_points: u32) -> bool {
        manual || (self.graphql_budget_current && self.graphql_points >= required_points)
    }
}

#[derive(Debug, Clone, Default)]
struct Accounting {
    requests: u64,
    graphql_points: u64,
    rest_points: u64,
    bytes: u64,
    duration_ms: u64,
    conditional_hits: u64,
    forecast_points: u64,
    forecast_error_points: i64,
    items: u64,
    duplicates: u64,
}

impl Accounting {
    fn snapshot(&self) -> AccountingSnapshot {
        AccountingSnapshot {
            requests: self.requests,
            graphql_points: self.graphql_points,
            rest_points: self.rest_points,
            bytes: self.bytes,
            duration_ms: self.duration_ms,
            conditional_hits: self.conditional_hits,
            forecast_points: self.forecast_points,
            forecast_error_points: self.forecast_error_points,
            items: self.items,
            duplicates: self.duplicates,
        }
    }
}

#[derive(Debug, Clone)]
struct ResourceState {
    remaining: u32,
    limit: u32,
    used: u32,
    reset_at: DateTime<Utc>,
    observed_at: Instant,
    external_burn_per_sec: f64,
    external_baseline_used: u32,
    external_baseline_remaining: u32,
    external_baseline_at: Instant,
    external_baseline_local_completed: u64,
    /// Set when the baseline was seeded from persisted state rather than a
    /// live response. The next live observation re-anchors the baseline
    /// without emitting a burn sample: diffing a live reading against a
    /// pre-restart baseline would fold all downtime usage into one tiny
    /// interval and produce a phantom external-burn spike.
    external_baseline_pending: bool,
}

#[derive(Debug, Clone)]
struct OperationState {
    forecast: u32,
    unit_forecast: u32,
    last_actual: Option<u32>,
    material_forecast_errors: u64,
}

/// Minimum spacing between Interactive requests allowed through an open
/// secondary cooldown (#1218 item 5). A secondary (abuse-limiter) pause
/// is usually opened by BACKGROUND churn; hard-blocking the user's own
/// `g s` / `g m` / reply behind it refuses user work — lazybox advises,
/// it does not forbid. One spaced request at a time still lets the
/// cooldown cool; scheduled work keeps waiting out the full window, and
/// PRIMARY exhaustion (the quota is actually gone) still blocks everyone.
const SECONDARY_INTERACTIVE_GAP: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
struct CircuitState {
    reason: String,
    retry_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
struct Admission {
    forecast: u32,
    scheduled: bool,
}

pub struct RateBudget {
    capacity: u32,
    available: f64,
    refill_per_sec: f64,
    last_refill: Instant,
    background_share: f64,
    resources: HashMap<String, ResourceState>,
    background_credit: HashMap<String, f64>,
    last_credit_at: Option<Instant>,
    tick_allowance: HashMap<String, u32>,
    tick_scheduled: HashMap<String, u32>,
    tick_interval: Duration,
    operations: HashMap<String, OperationState>,
    pending: HashMap<(String, String), VecDeque<Admission>>,
    local_completed: HashMap<String, u64>,
    tick: Accounting,
    total: Accounting,
    request_latencies_ms: VecDeque<u64>,
    secondary_circuit: Option<CircuitState>,
    /// Earliest instant the next Interactive request may pass through an
    /// open secondary cooldown ([`SECONDARY_INTERACTIVE_GAP`] pacing).
    secondary_interactive_next: Option<Instant>,
    primary_circuits: HashMap<String, CircuitState>,
    secondary_failures: u32,
    min_request_gap: Duration,
    next_request_slot: Option<Instant>,
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
            background_share: DEFAULT_BACKGROUND_SHARE,
            resources: HashMap::new(),
            background_credit: HashMap::new(),
            last_credit_at: None,
            tick_allowance: HashMap::new(),
            tick_scheduled: HashMap::new(),
            tick_interval: Duration::from_secs(60),
            operations: HashMap::new(),
            pending: HashMap::new(),
            local_completed: HashMap::new(),
            tick: Accounting::default(),
            total: Accounting::default(),
            request_latencies_ms: VecDeque::new(),
            secondary_circuit: None,
            secondary_interactive_next: None,
            primary_circuits: HashMap::new(),
            secondary_failures: 0,
            min_request_gap: DEFAULT_MIN_REQUEST_GAP,
            next_request_slot: None,
            #[cfg(test)]
            force_fail: None,
        }
    }

    pub fn default_for_lazybox() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_REFILL_PER_MIN)
    }

    pub fn set_background_share(&mut self, share: f64) {
        self.background_share = if share.is_finite() {
            share.clamp(0.05, 0.90)
        } else {
            DEFAULT_BACKGROUND_SHARE
        };
    }

    /// Reserve the next paced request slot and return how long the
    /// caller must sleep before firing. Serializes request *starts* to
    /// at least [`Self::request_gap`] apart so a burst (a sweep firing
    /// its whole allowance, or retries stacking) can't trip GitHub's
    /// secondary limit while primary budget is still healthy (#745).
    ///
    /// An idle gap never banks burst credit: a slot in the past
    /// collapses to `now`, so the first request after a quiet period
    /// fires immediately.
    pub(crate) fn reserve_request_slot(&mut self, now: Instant) -> Duration {
        let gap = self.request_gap();
        let start = self.next_request_slot.map_or(now, |slot| slot.max(now));
        self.next_request_slot = Some(start + gap);
        start.checked_duration_since(now).unwrap_or_default()
    }

    /// Current inter-request spacing. Widens beyond the baseline while a
    /// secondary limit is recent (GitHub is signalling the token is hot)
    /// and while external `gh`/agent traffic on the shared token is
    /// heavy, then relaxes back as those pressures subside.
    fn request_gap(&self) -> Duration {
        let secondary = 1u32 << self.secondary_failures.min(4);
        let external_per_sec: f64 = self
            .resources
            .values()
            .map(|state| state.external_burn_per_sec)
            .sum();
        let contention =
            1.0 + (external_per_sec / EXTERNAL_CONTENTION_REF).clamp(0.0, MAX_CONTENTION_MULT);
        self.min_request_gap
            .mul_f64(f64::from(secondary) * contention)
            .min(MAX_REQUEST_GAP)
    }

    fn refill(&mut self) {
        self.refill_at(Instant::now());
    }

    fn refill_at(&mut self, now: Instant) {
        let elapsed = now
            .checked_duration_since(self.last_refill)
            .unwrap_or_default()
            .as_secs_f64();
        self.available = (self.available + elapsed * self.refill_per_sec).min(self.capacity as f64);
        self.last_refill = now;
    }

    pub fn forecast(&self, class: &str, default: u32) -> u32 {
        self.operations
            .get(class)
            .map_or(default.max(1), |operation| operation.forecast.max(1))
    }

    pub fn unit_forecast(&self, class: &str, default: u32) -> u32 {
        self.operations
            .get(class)
            .map_or(default.max(1), |operation| {
                operation.unit_forecast.max(operation.forecast).max(1)
            })
    }

    pub fn note_expected_pages(&mut self, class: &str, pages: u32) {
        let operation = self
            .operations
            .entry(class.to_string())
            .or_insert(OperationState {
                forecast: 1,
                unit_forecast: 1,
                last_actual: None,
                material_forecast_errors: 0,
            });
        operation.unit_forecast = operation
            .unit_forecast
            .max(operation.forecast.saturating_mul(pages.max(1)));
    }

    pub fn begin_background_tick(
        &mut self,
        interval: Duration,
        wall_now: DateTime<Utc>,
        mono_now: Instant,
    ) -> BackgroundPlan {
        self.refill_at(mono_now);
        let interval = interval.max(Duration::from_secs(1));
        let elapsed_intervals = self.last_credit_at.map_or(1.0, |last| {
            mono_now
                .checked_duration_since(last)
                .unwrap_or_default()
                .as_secs_f64()
                / interval.as_secs_f64()
        });
        self.last_credit_at = Some(mono_now);

        for (resource, spent) in &self.tick_scheduled {
            let credit = self.background_credit.entry(resource.clone()).or_default();
            *credit = (*credit - f64::from(*spent)).max(0.0);
        }
        self.tick = Accounting::default();
        self.tick_allowance.clear();
        self.tick_scheduled.clear();
        self.tick_interval = interval;
        self.expire_circuits(wall_now);

        for (resource, state) in &self.resources {
            let allowance = if state.reset_at <= wall_now {
                self.background_credit.remove(resource);
                expired_window_allowance()
            } else {
                let grant = sustainable_allowance_exact(
                    state,
                    self.background_share,
                    self.tick_interval,
                    wall_now,
                ) * elapsed_intervals;
                let cap = f64::from(background_headroom(state, self.background_share, wall_now));
                let credit = self.background_credit.entry(resource.clone()).or_default();
                *credit = (*credit + grant).min(cap);
                // Guaranteed minimum (#782): a high external burn on the
                // shared token makes `background_headroom` (and thus the
                // sustainable rate above) collapse toward zero, which would
                // leave a tick allowance too small to admit even one
                // complete sweep — sync would make no progress at all for as
                // long as the contention lasts, despite a healthy primary
                // budget. When external burn is present, hold the credit at a
                // floor so sync only slows, never stalls. The floor is
                // bounded by the room genuinely available above the protected
                // reserve, so a truly scarce budget still degrades to slow
                // accumulation rather than a false floor; the hard
                // `RemoteLow` / `ReserveProtected` guards in `admit` remain
                // the real reserve protection.
                if state.external_burn_per_sec > 0.0 {
                    let reserve = reserve_for(state.limit, self.background_share);
                    let capacity = state.remaining.saturating_sub(reserve);
                    let floor = f64::from(MIN_BACKGROUND_TICK_ALLOWANCE.min(capacity));
                    *credit = credit.max(floor);
                }
                credit.floor() as u32
            };
            self.tick_allowance.insert(resource.clone(), allowance);
        }

        let next_eligible_at = self
            .secondary_circuit
            .as_ref()
            .map(|circuit| circuit.retry_at);
        // A resource absent from `tick_allowance` was never observed — the
        // loop above inserts every resource in `self.resources`, so a miss
        // means an *unlearned* window, not a throttled one. Fall back to a full
        // sweep unit, exactly as an expired window does (see
        // `expired_window_allowance`): a lone `1` here reports a single-point
        // plan, and `admit`'s tick gate then refuses any multi-point scheduled
        // batch, so no request ever learns the window — the same
        // self-reinforcing stall the expired path fixes, reached via a
        // never-bootstrapped cold window instead.
        let graphql_points = self
            .tick_allowance
            .get("graphql")
            .copied()
            .unwrap_or(MIN_BACKGROUND_TICK_ALLOWANCE);
        let rest_core_points = self
            .tick_allowance
            .get("core")
            .copied()
            .unwrap_or(MIN_BACKGROUND_TICK_ALLOWANCE);
        let graphql_budget_current = self
            .resources
            .get("graphql")
            .is_some_and(|state| state.reset_at > wall_now);
        let pressure = next_eligible_at.is_some()
            || self.resources.values().any(|state| {
                sustainable_allowance_exact(
                    state,
                    self.background_share,
                    self.tick_interval,
                    wall_now,
                ) <= 1.0
            });
        BackgroundPlan {
            graphql_points,
            rest_core_points,
            graphql_budget_current,
            pressure,
            next_eligible_at,
            tick_interval: self.tick_interval,
        }
    }

    /// Compatibility helper for callers that only need the local/global
    /// admission result and do not participate in a background plan.
    pub fn try_acquire(&mut self) -> Result<(), AcquireError> {
        self.try_acquire_at(Instant::now())
    }

    fn try_acquire_at(&mut self, now: Instant) -> Result<(), AcquireError> {
        self.admit_at(
            ApiResource::Graphql,
            "legacy",
            RequestPriority::Interactive,
            1,
            Utc::now(),
            now,
        )
        .map(|_| ())
    }

    pub fn admit(
        &mut self,
        resource: ApiResource,
        class: &str,
        priority: RequestPriority,
        default_forecast: u32,
    ) -> Result<u32, AcquireError> {
        self.admit_at(
            resource,
            class,
            priority,
            default_forecast,
            Utc::now(),
            Instant::now(),
        )
    }

    fn admit_at(
        &mut self,
        resource: ApiResource,
        class: &str,
        priority: RequestPriority,
        default_forecast: u32,
        wall_now: DateTime<Utc>,
        mono_now: Instant,
    ) -> Result<u32, AcquireError> {
        #[cfg(test)]
        if let Some(forced) = self.force_fail.take() {
            return Err(forced);
        }

        self.expire_circuits(wall_now);
        if let Some(circuit) = &self.secondary_circuit {
            // #1218 item 5: user-initiated requests pass through a
            // secondary cooldown, spaced SECONDARY_INTERACTIVE_GAP apart;
            // everything scheduled waits out the window. Primary
            // exhaustion below still blocks unconditionally.
            let interactive_may_pass = priority == RequestPriority::Interactive
                && self
                    .secondary_interactive_next
                    .is_none_or(|next| mono_now >= next);
            if interactive_may_pass {
                self.secondary_interactive_next = Some(mono_now + SECONDARY_INTERACTIVE_GAP);
            } else {
                return Err(AcquireError::CircuitOpen {
                    reason: circuit.reason.clone(),
                    retry_at: circuit.retry_at,
                });
            }
        }
        if let Some(circuit) = self.primary_circuits.get(resource.key()) {
            return Err(AcquireError::CircuitOpen {
                reason: circuit.reason.clone(),
                retry_at: circuit.retry_at,
            });
        }

        let forecast = self.forecast(class, default_forecast);
        if let Some(state) = self.resources.get(resource.key())
            && state.reset_at > wall_now
        {
            let emergency_floor = LOW_THRESHOLD.min(state.limit.div_ceil(50).max(1));
            if state.remaining <= emergency_floor || state.remaining < forecast {
                return Err(AcquireError::RemoteLow {
                    remaining: state.remaining,
                    reset_at: state.reset_at,
                });
            }
            if priority.is_scheduled() {
                let reserve = reserve_for(state.limit, self.background_share);
                if state.remaining.saturating_sub(forecast) < reserve {
                    return Err(AcquireError::ReserveProtected {
                        resource: resource.key().to_string(),
                        remaining: state.remaining,
                        reserve,
                        reset_at: state.reset_at,
                    });
                }
            }
        }
        if priority.is_scheduled() {
            // No tick allowance means this resource's window was never observed
            // (an expired window is still present in `self.resources` and gets
            // a real entry above). Grant a full sweep unit so one scheduled
            // batch can go out and learn the window, matching the expired-window
            // backstop; `unwrap_or(1)` would refuse any multi-point batch and
            // deadlock a cold, never-bootstrapped window (see `expired_window_allowance`).
            let allowance = self
                .tick_allowance
                .get(resource.key())
                .copied()
                .unwrap_or(MIN_BACKGROUND_TICK_ALLOWANCE);
            let spent = self
                .tick_scheduled
                .get(resource.key())
                .copied()
                .unwrap_or(0);
            if spent.saturating_add(forecast) > allowance {
                return Err(AcquireError::TickAllowanceExhausted {
                    resource: resource.key().to_string(),
                    allowance,
                    spent,
                    wait_secs: self.tick_interval.as_secs().max(1),
                });
            }
        }

        self.refill_at(mono_now);
        // The local token bucket is SELF-imposed pacing, so it never
        // refuses a user-pressed action: an interactive request draws
        // into deficit (available goes negative) and the refill repays
        // it by delaying the next scheduled admits — the exact priority
        // inversion we want. Only scheduled traffic waits here (#1249;
        // mirrors the secondary-circuit interactive pass above).
        if self.available < 1.0 && priority != RequestPriority::Interactive {
            let needed = 1.0 - self.available;
            let wait_secs = if self.refill_per_sec > 0.0 {
                (needed / self.refill_per_sec).ceil() as u64
            } else {
                60
            };
            return Err(AcquireError::LocalBudgetExhausted {
                wait_secs: wait_secs.max(1),
            });
        }
        self.available -= 1.0;

        if priority.is_scheduled() {
            *self
                .tick_scheduled
                .entry(resource.key().to_string())
                .or_default() += forecast;
            self.tick.forecast_points += u64::from(forecast);
            self.total.forecast_points += u64::from(forecast);
        }
        self.operations
            .entry(class.to_string())
            .or_insert(OperationState {
                forecast,
                unit_forecast: forecast,
                last_actual: None,
                material_forecast_errors: 0,
            });
        self.pending
            .entry((resource.key().to_string(), class.to_string()))
            .or_default()
            .push_back(Admission {
                forecast,
                scheduled: priority.is_scheduled(),
            });
        Ok(forecast)
    }

    /// Compatibility observation for older callers and tests.
    pub fn observe(&mut self, remote: RemoteRateLimit) {
        let used = remote.limit.saturating_sub(remote.remaining);
        self.observe_primary(
            "graphql",
            remote.limit,
            remote.remaining,
            used,
            remote.reset_at,
            remote.observed_at,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn observe_graphql_response(
        &mut self,
        class: &str,
        remote: RemoteRateLimit,
        used: u32,
        actual_cost: u32,
        status: u16,
        bytes: usize,
        duration: Duration,
    ) {
        self.record_response(
            class,
            ApiResource::Graphql,
            Some(actual_cost),
            status,
            false,
            bytes,
            duration,
        );
        self.observe_primary(
            "graphql",
            remote.limit,
            remote.remaining,
            used,
            remote.reset_at,
            remote.observed_at,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn observe_rest_response(
        &mut self,
        resource: &str,
        class: &str,
        limit: u32,
        remaining: u32,
        used: u32,
        reset_at: DateTime<Utc>,
        status: u16,
        conditional_hit: bool,
        bytes: usize,
        duration: Duration,
    ) {
        let actual = u32::from(!conditional_hit);
        self.record_response(
            class,
            ApiResource::rest(resource),
            Some(actual),
            status,
            conditional_hit,
            bytes,
            duration,
        );
        self.observe_primary(resource, limit, remaining, used, reset_at, Instant::now());
    }

    #[allow(clippy::too_many_arguments)]
    fn record_response(
        &mut self,
        class: &str,
        resource: ApiResource,
        reported_actual: Option<u32>,
        status: u16,
        conditional_hit: bool,
        bytes: usize,
        duration: Duration,
    ) {
        let admission = self
            .pending
            .get_mut(&(resource.key().to_string(), class.to_string()))
            .and_then(VecDeque::pop_front);
        let forecast = admission.map_or_else(|| self.forecast(class, 1), |entry| entry.forecast);
        let actual = reported_actual.unwrap_or(forecast);
        *self
            .local_completed
            .entry(resource.key().to_string())
            .or_default() += u64::from(actual);
        if let Some(admission) = admission
            && admission.scheduled
        {
            let scheduled = self
                .tick_scheduled
                .entry(resource.key().to_string())
                .or_default();
            *scheduled = scheduled
                .saturating_sub(admission.forecast)
                .saturating_add(actual);
        }
        let error = i64::from(actual) - i64::from(forecast);
        let material =
            actual.abs_diff(forecast) >= 2 || (forecast > 0 && actual > forecast.saturating_mul(2));
        let operation = self
            .operations
            .entry(class.to_string())
            .or_insert(OperationState {
                forecast: forecast.max(actual).max(1),
                unit_forecast: forecast.max(actual).max(1),
                last_actual: None,
                material_forecast_errors: 0,
            });
        operation.last_actual = Some(actual);
        operation.forecast = operation.forecast.max(actual).max(1);
        operation.unit_forecast = operation.unit_forecast.max(operation.forecast);
        if material {
            operation.material_forecast_errors += 1;
            tracing::warn!(
                target: "gh_governor",
                operation = class,
                forecast,
                actual,
                error,
                "material GitHub cost forecast error"
            );
        }

        for accounting in [&mut self.tick, &mut self.total] {
            accounting.requests += 1;
            match resource {
                ApiResource::Graphql => accounting.graphql_points += u64::from(actual),
                ApiResource::Rest(_) => accounting.rest_points += u64::from(actual),
            }
            accounting.bytes += bytes as u64;
            accounting.duration_ms += duration.as_millis() as u64;
            accounting.conditional_hits += u64::from(conditional_hit);
            accounting.forecast_error_points += error;
        }
        self.request_latencies_ms
            .push_back(duration.as_millis() as u64);
        if self.request_latencies_ms.len() > LATENCY_SAMPLE_CAPACITY {
            self.request_latencies_ms.pop_front();
        }
        if (200..=399).contains(&status) {
            // Decay the secondary-abuse backoff one step per clean response
            // instead of snapping it to zero. A hard reset collapsed the
            // widened inter-request gap the instant a single request slipped
            // through, so the very next burst immediately re-tripped GitHub's
            // secondary (abuse) limit — the token never actually received the
            // sustained slow-down the limiter demands, and sync churned in a
            // trip → reset → re-trip loop (#1218). Decaying holds the gap wide
            // across the recovery and only fully relaxes after several
            // consecutive clean responses.
            self.secondary_failures = self.secondary_failures.saturating_sub(1);
        }

        tracing::info!(
            target: "gh_governor",
            operation = class,
            resource = resource.key(),
            status,
            conditional_hit,
            forecast,
            actual,
            bytes,
            duration_ms = duration.as_millis() as u64,
            "GitHub request accounted"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_primary(
        &mut self,
        resource: &str,
        limit: u32,
        remaining: u32,
        used: u32,
        reset_at: DateTime<Utc>,
        observed_at: Instant,
    ) {
        let local_completed = self.local_completed.get(resource).copied().unwrap_or(0);
        let has_pending = self
            .pending
            .iter()
            .any(|((pending_resource, _), admissions)| {
                pending_resource == resource && !admissions.is_empty()
            });
        match self.resources.get_mut(resource) {
            Some(previous) if reset_at < previous.reset_at => {
                return;
            }
            Some(previous) if reset_at == previous.reset_at => {
                previous.limit = previous.limit.max(limit);
                previous.remaining = previous.remaining.min(remaining);
                previous.used = previous.used.max(used);
                previous.observed_at = previous.observed_at.max(observed_at);
                if !has_pending {
                    if previous.external_baseline_pending {
                        // First live reading after a restore: re-anchor to
                        // it and defer burn measurement to the next
                        // observation. The persisted baseline predates the
                        // restart, so a sample here would misattribute
                        // downtime usage as an instantaneous spike.
                        previous.external_baseline_pending = false;
                    } else {
                        let elapsed = observed_at
                            .checked_duration_since(previous.external_baseline_at)
                            .unwrap_or_default()
                            .as_secs_f64();
                        if elapsed > 0.0 {
                            let total_delta = previous
                                .used
                                .saturating_sub(previous.external_baseline_used)
                                .max(
                                    previous
                                        .external_baseline_remaining
                                        .saturating_sub(previous.remaining),
                                );
                            let local_delta = local_completed
                                .saturating_sub(previous.external_baseline_local_completed);
                            let external_delta = total_delta
                                .saturating_sub(local_delta.min(u64::from(u32::MAX)) as u32);
                            let sample = f64::from(external_delta) / elapsed;
                            previous.external_burn_per_sec =
                                if previous.external_burn_per_sec == 0.0 {
                                    sample
                                } else {
                                    previous.external_burn_per_sec * 0.75 + sample * 0.25
                                };
                        }
                    }
                    previous.external_baseline_used = previous.used;
                    previous.external_baseline_remaining = previous.remaining;
                    previous.external_baseline_at = observed_at;
                    previous.external_baseline_local_completed = local_completed;
                }
            }
            _ => {
                self.resources.insert(
                    resource.to_string(),
                    ResourceState {
                        remaining,
                        limit,
                        used,
                        reset_at,
                        observed_at,
                        external_burn_per_sec: 0.0,
                        external_baseline_used: used,
                        external_baseline_remaining: remaining,
                        external_baseline_at: observed_at,
                        external_baseline_local_completed: local_completed,
                        external_baseline_pending: false,
                    },
                );
            }
        }
        if remaining == 0 && reset_at > Utc::now() {
            self.open_primary_circuit(
                resource,
                format!("{resource} primary budget exhausted"),
                reset_at + chrono::Duration::seconds(1),
            );
        }
    }

    pub fn note_dedup(&mut self, total_items: usize, unique_items: usize) {
        let duplicates = total_items.saturating_sub(unique_items) as u64;
        self.tick.duplicates += duplicates;
        self.total.duplicates += duplicates;
    }

    pub fn observe_primary_limit(
        &mut self,
        resource: &str,
        reset_at: DateTime<Utc>,
        reason: impl Into<String>,
    ) {
        self.open_primary_circuit(resource, reason.into(), reset_at);
        if let Some(state) = self.resources.get_mut(resource) {
            state.remaining = 0;
            state.reset_at = reset_at;
        }
    }

    pub fn observe_failed_response(
        &mut self,
        class: &str,
        resource: ApiResource,
        status: u16,
        bytes: usize,
        duration: Duration,
    ) {
        self.record_response(class, resource, None, status, false, bytes, duration);
    }

    pub(crate) fn observe_unreported_response(
        &mut self,
        class: &str,
        resource: ApiResource,
        status: u16,
        duration: Duration,
    ) {
        self.record_response(class, resource, None, status, false, 0, duration);
    }

    pub fn note_items_changed(&mut self, items: usize) {
        self.tick.items += items as u64;
        self.total.items += items as u64;
    }

    pub fn observe_secondary_limit(
        &mut self,
        retry_after: Option<Duration>,
        wall_now: DateTime<Utc>,
    ) -> DateTime<Utc> {
        self.secondary_failures = self.secondary_failures.saturating_add(1);
        let exponent = self.secondary_failures.saturating_sub(1).min(4);
        let computed = DEFAULT_SECONDARY_PAUSE
            .saturating_mul(1u32 << exponent)
            .min(MAX_SECONDARY_PAUSE);
        let base = retry_after.unwrap_or(computed);
        if retry_after.is_some() {
            let retry_at = wall_now
                + chrono::Duration::from_std(base)
                    .unwrap_or_else(|_| chrono::Duration::minutes(15));
            self.open_secondary_circuit("secondary rate limit".to_string(), retry_at);
            return retry_at;
        }
        let jitter_bound = (base.as_secs() / 10).max(1);
        let jitter = self.last_refill.elapsed().subsec_nanos() as u64 % (jitter_bound + 1);
        let delay = base.saturating_add(Duration::from_secs(jitter));
        let retry_at = wall_now
            + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::minutes(15));
        self.open_secondary_circuit("secondary rate limit".to_string(), retry_at);
        retry_at
    }

    fn open_secondary_circuit(&mut self, reason: String, retry_at: DateTime<Utc>) {
        let replace = self
            .secondary_circuit
            .as_ref()
            .is_none_or(|current| retry_at > current.retry_at);
        if replace {
            self.secondary_circuit = Some(CircuitState { reason, retry_at });
        }
    }

    fn open_primary_circuit(&mut self, resource: &str, reason: String, retry_at: DateTime<Utc>) {
        let replace = self
            .primary_circuits
            .get(resource)
            .is_none_or(|current| retry_at > current.retry_at);
        if replace {
            self.primary_circuits
                .insert(resource.to_string(), CircuitState { reason, retry_at });
        }
    }

    fn expire_circuits(&mut self, now: DateTime<Utc>) {
        if self
            .secondary_circuit
            .as_ref()
            .is_some_and(|circuit| circuit.retry_at <= now)
        {
            self.secondary_circuit = None;
        }
        self.primary_circuits
            .retain(|_, circuit| circuit.retry_at > now);
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut clone = self.clone_for_snapshot();
        clone.refill();
        let wall_now = Utc::now();
        let mut resources = self
            .resources
            .iter()
            .map(|(resource, state)| ResourceSnapshot {
                resource: resource.clone(),
                remaining: state.remaining,
                limit: state.limit,
                used: state.used,
                reset_at: state.reset_at,
                blocked_reason: self
                    .primary_circuits
                    .get(resource)
                    .map(|circuit| circuit.reason.clone()),
                eligible_at: self
                    .primary_circuits
                    .get(resource)
                    .map(|circuit| circuit.retry_at),
                reserve: reserve_for(state.limit, self.background_share),
                allowance: self
                    .tick_allowance
                    .get(resource)
                    .copied()
                    .unwrap_or_else(|| {
                        sustainable_allowance_exact(
                            state,
                            self.background_share,
                            self.tick_interval,
                            wall_now,
                        )
                        .floor() as u32
                    }),
                scheduled: self.tick_scheduled.get(resource).copied().unwrap_or(0),
                external_burn_per_hour: state.external_burn_per_sec * 3600.0,
            })
            .collect::<Vec<_>>();
        resources.sort_by(|a, b| a.resource.cmp(&b.resource));

        let mut operations = self
            .operations
            .iter()
            .map(|(class, state)| OperationSnapshot {
                class: class.clone(),
                forecast: state.forecast,
                unit_forecast: state.unit_forecast,
                last_actual: state.last_actual,
                material_forecast_errors: state.material_forecast_errors,
            })
            .collect::<Vec<_>>();
        operations.sort_by(|a, b| a.class.cmp(&b.class));

        let mut latencies = self
            .request_latencies_ms
            .iter()
            .copied()
            .collect::<Vec<_>>();
        latencies.sort_unstable();
        let remote = self.resources.get("graphql").map(|state| RemoteRateLimit {
            remaining: state.remaining,
            limit: state.limit,
            reset_at: state.reset_at,
            observed_at: state.observed_at,
        });
        Snapshot {
            local_available: clone.available,
            local_capacity: self.capacity,
            remote,
            background_share: self.background_share,
            resources,
            tick: self.tick.snapshot(),
            total: self.total.snapshot(),
            request_p50_ms: percentile(&latencies, 50),
            request_p95_ms: percentile(&latencies, 95),
            request_p99_ms: percentile(&latencies, 99),
            circuit_reason: self
                .secondary_circuit
                .as_ref()
                .map(|circuit| circuit.reason.clone()),
            retry_at: self
                .secondary_circuit
                .as_ref()
                .map(|circuit| circuit.retry_at),
            operations,
        }
    }

    /// Capture the wall-clock-anchored state for durable storage. See
    /// [`PersistedRateState`].
    pub fn persisted_state(&self) -> PersistedRateState {
        PersistedRateState {
            resources: self
                .resources
                .iter()
                .map(|(resource, state)| {
                    (
                        resource.clone(),
                        PersistedResource {
                            remaining: state.remaining,
                            limit: state.limit,
                            used: state.used,
                            reset_at: state.reset_at,
                            external_burn_per_sec: state.external_burn_per_sec,
                        },
                    )
                })
                .collect(),
            secondary_circuit: self
                .secondary_circuit
                .as_ref()
                .map(|circuit| PersistedCooldown {
                    reason: circuit.reason.clone(),
                    retry_at: circuit.retry_at,
                }),
            primary_circuits: self
                .primary_circuits
                .iter()
                .map(|(resource, circuit)| {
                    (
                        resource.clone(),
                        PersistedCooldown {
                            reason: circuit.reason.clone(),
                            retry_at: circuit.retry_at,
                        },
                    )
                })
                .collect(),
            secondary_failures: self.secondary_failures,
        }
    }

    /// Reload durable state into a freshly-constructed budget at startup.
    /// Monotonic anchors (`observed_at`, external-burn baselines) are
    /// re-based to now; the reset/retry timestamps are wall-clock and
    /// carry over verbatim. An already-expired reset or cooldown is left
    /// in place — the normal expiry paths (`begin_background_tick`,
    /// `expire_circuits`) drop it on the next tick, so a stale entry only
    /// ever fails safe.
    pub fn restore(&mut self, state: PersistedRateState) {
        self.restore_at(state, Utc::now(), Instant::now());
    }

    fn restore_at(
        &mut self,
        state: PersistedRateState,
        wall_now: DateTime<Utc>,
        mono_now: Instant,
    ) {
        for (resource, persisted) in state.resources {
            self.resources.insert(
                resource,
                ResourceState {
                    remaining: persisted.remaining,
                    limit: persisted.limit,
                    used: persisted.used,
                    reset_at: persisted.reset_at,
                    observed_at: mono_now,
                    external_burn_per_sec: persisted.external_burn_per_sec,
                    external_baseline_used: persisted.used,
                    external_baseline_remaining: persisted.remaining,
                    external_baseline_at: mono_now,
                    external_baseline_local_completed: 0,
                    external_baseline_pending: true,
                },
            );
        }
        let secondary_active = state
            .secondary_circuit
            .as_ref()
            .is_some_and(|circuit| circuit.retry_at > wall_now);
        if let Some(circuit) = state.secondary_circuit {
            self.secondary_circuit = Some(CircuitState {
                reason: circuit.reason,
                retry_at: circuit.retry_at,
            });
        }
        for (resource, circuit) in state.primary_circuits {
            self.primary_circuits.insert(
                resource,
                CircuitState {
                    reason: circuit.reason,
                    retry_at: circuit.retry_at,
                },
            );
        }
        // The backoff level only means something while the cooldown it
        // produced is still in effect. If that cooldown already elapsed
        // (e.g. during downtime), the throttle episode is over — start
        // fresh so a single later throttle doesn't inherit a stale, inflated
        // backoff. A still-active cooldown keeps its level so repeated
        // throttles keep escalating across the restart.
        self.secondary_failures = if secondary_active {
            state.secondary_failures
        } else {
            0
        };
    }

    fn clone_for_snapshot(&self) -> Self {
        Self {
            capacity: self.capacity,
            available: self.available,
            refill_per_sec: self.refill_per_sec,
            last_refill: self.last_refill,
            background_share: self.background_share,
            resources: self.resources.clone(),
            background_credit: self.background_credit.clone(),
            last_credit_at: self.last_credit_at,
            tick_allowance: self.tick_allowance.clone(),
            tick_scheduled: self.tick_scheduled.clone(),
            tick_interval: self.tick_interval,
            operations: self.operations.clone(),
            pending: self.pending.clone(),
            local_completed: self.local_completed.clone(),
            tick: self.tick.clone(),
            total: self.total.clone(),
            request_latencies_ms: self.request_latencies_ms.clone(),
            secondary_circuit: self.secondary_circuit.clone(),
            secondary_interactive_next: self.secondary_interactive_next,
            primary_circuits: self.primary_circuits.clone(),
            secondary_failures: self.secondary_failures,
            min_request_gap: self.min_request_gap,
            next_request_slot: self.next_request_slot,
            #[cfg(test)]
            force_fail: None,
        }
    }
}

fn reserve_for(limit: u32, background_share: f64) -> u32 {
    (f64::from(limit) * (1.0 - background_share)).ceil() as u32
}

fn background_headroom(state: &ResourceState, background_share: f64, now: DateTime<Utc>) -> u32 {
    let until_reset = state
        .reset_at
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_default();
    let projected_external =
        (state.external_burn_per_sec * until_reset.as_secs_f64()).ceil() as u32;
    let reserve = reserve_for(state.limit, background_share);
    state
        .remaining
        .saturating_sub(reserve)
        .saturating_sub(projected_external)
}

fn sustainable_allowance_exact(
    state: &ResourceState,
    background_share: f64,
    interval: Duration,
    now: DateTime<Utc>,
) -> f64 {
    let until_reset = state
        .reset_at
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_default();
    let headroom = background_headroom(state, background_share, now);
    if headroom == 0 {
        return 0.0;
    }
    let reset_secs = until_reset.as_secs_f64().max(1.0);
    (f64::from(headroom) * interval.as_secs_f64() / reset_secs).min(f64::from(headroom))
}

/// Allowance for a tick whose rate window looks expired (`reset_at <= now`)
/// — we haven't observed the fresh window yet, so `remaining` is stale and
/// the sustainable-rate math can't be trusted.
///
/// This must be enough to admit **one complete background sweep unit**, not a
/// single point. A hot-target batch is one GraphQL query but costs several
/// points; granting only `1` here refused the batch, so no request went out,
/// so the window was never re-learned — sync stalled for the entire window
/// even with thousands of points free (observed as a ~300s "sync" with 4796
/// GraphQL points available). Granting a sweep unit lets exactly one batch
/// through to refresh the window.
///
/// On an expired window the `RemoteLow` / `ReserveProtected` guards in
/// [`RateBudget::admit`] are **inert** — they are gated on `reset_at > now` —
/// so this allowance is the *sole* gate, and the sweep unit is spent
/// unguarded. That is deliberate, and it is why (unlike the external-burn
/// floor in [`RateBudget::begin_background_tick`]) the grant is **not** clamped
/// by `state.remaining`: an expired window's `remaining` is the last value of
/// the spent, now-elapsed window — characteristically low or zero — so
/// clamping by it would refuse the batch and re-deadlock the exact case this
/// fixes. The overspend is bounded and self-correcting instead: at most one
/// sweep unit is spent unguarded per tick, and the batch's response headers
/// re-learn the window and re-arm the admit guards immediately (even mid-tick,
/// for the next admission). The residual exposure is a local clock running
/// ahead of GitHub's, which classifies a still-live window as expired for the
/// final skew-seconds of each window; that spends up to one sweep unit against
/// the real budget before the next observed header clamps it back.
fn expired_window_allowance() -> u32 {
    MIN_BACKGROUND_TICK_ALLOWANCE
}

fn percentile(sorted: &[u64], percentile: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * percentile).div_ceil(100);
    sorted.get(rank.saturating_sub(1)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(
        limit: u32,
        remaining: u32,
        now: Instant,
        reset_at: DateTime<Utc>,
    ) -> RemoteRateLimit {
        RemoteRateLimit {
            remaining,
            limit,
            reset_at,
            observed_at: now,
        }
    }

    #[test]
    fn fresh_bucket_allows_one_acquire() {
        let now = Instant::now();
        let mut budget = RateBudget::new(2, 60.0);
        assert!(budget.try_acquire_at(now).is_ok());
        assert!(budget.try_acquire_at(now).is_ok());
        // The local bucket paces SCHEDULED traffic; an empty bucket
        // refuses the sweep, not the user (#1249).
        assert!(matches!(
            budget.admit_at(
                ApiResource::Graphql,
                "merged-sweep",
                RequestPriority::Recent,
                1,
                Utc::now(),
                now,
            ),
            Err(AcquireError::LocalBudgetExhausted { .. })
        ));
    }

    #[test]
    fn interactive_draws_into_deficit_instead_of_being_refused() {
        // #1249: the local bucket is self-imposed pacing — a user-pressed
        // merge/reply is admitted even when empty, going into deficit that
        // the refill repays by delaying the next scheduled admits.
        let now = Instant::now();
        let mut budget = RateBudget::new(1, 0.001);
        assert!(budget.try_acquire_at(now).is_ok());
        // Bucket empty: interactive still passes, twice.
        assert!(budget.try_acquire_at(now).is_ok());
        assert!(budget.try_acquire_at(now).is_ok());
        // The deficit is real: scheduled traffic now waits it out.
        let err = budget
            .admit_at(
                ApiResource::Graphql,
                "merged-sweep",
                RequestPriority::Recent,
                1,
                Utc::now(),
                now,
            )
            .expect_err("scheduled traffic repays the deficit");
        assert!(matches!(err, AcquireError::LocalBudgetExhausted { .. }));
    }

    #[test]
    fn low_remote_blocks_even_with_local_tokens() {
        let mut budget = RateBudget::new(100, 60.0);
        budget.observe(remote(
            5000,
            5,
            Instant::now(),
            Utc::now() + chrono::Duration::seconds(60),
        ));
        assert!(matches!(
            budget.try_acquire(),
            Err(AcquireError::RemoteLow { remaining: 5, .. })
        ));
    }

    #[test]
    fn expired_remote_low_doesnt_block() {
        let mut budget = RateBudget::new(100, 60.0);
        budget.observe(remote(
            5000,
            0,
            Instant::now(),
            Utc::now() - chrono::Duration::seconds(1),
        ));
        assert!(budget.try_acquire().is_ok());
    }

    #[test]
    fn refill_is_proportional_and_clamped() {
        let now = Instant::now();
        let mut budget = RateBudget::new(30, 30.0);
        // Drain exactly the capacity — an is_ok() drain loop no longer
        // terminates, since interactive admits draw into deficit (#1249).
        for _ in 0..30 {
            budget.try_acquire_at(now).expect("capacity drain");
        }
        budget.refill_at(now + Duration::from_secs(10));
        assert!((budget.available - 5.0).abs() < 1e-6);
        budget.refill_at(now + Duration::from_secs(3600));
        assert!((budget.available - 30.0).abs() < 1e-6);
    }

    #[test]
    fn starting_a_background_tick_preserves_idle_local_refill() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(1, 1.0);
        budget
            .try_acquire_at(mono_now)
            .expect("initial local token");

        let next = mono_now + Duration::from_secs(60);
        budget.begin_background_tick(
            Duration::from_secs(60),
            wall_now + chrono::Duration::seconds(60),
            next,
        );

        budget
            .admit_at(
                ApiResource::Graphql,
                "budget-bootstrap",
                RequestPriority::Recent,
                1,
                wall_now + chrono::Duration::seconds(60),
                next,
            )
            .expect("one minute of idle time must refill the local token");
    }

    #[test]
    fn background_allowance_protects_forty_five_percent_reserve() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe(remote(
            5000,
            5000,
            mono_now,
            wall_now + chrono::Duration::hours(1),
        ));
        let plan = budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        assert_eq!(plan.graphql_points, 45);
        for index in 0..plan.graphql_points {
            assert!(
                budget
                    .admit_at(
                        ApiResource::Graphql,
                        &format!("query-{index}"),
                        RequestPriority::Recent,
                        1,
                        wall_now,
                        mono_now,
                    )
                    .is_ok()
            );
        }
        assert!(matches!(
            budget.admit_at(
                ApiResource::Graphql,
                "one-too-many",
                RequestPriority::Recent,
                1,
                wall_now,
                mono_now,
            ),
            Err(AcquireError::TickAllowanceExhausted { .. })
        ));
        assert_eq!(reserve_for(5000, DEFAULT_BACKGROUND_SHARE), 2250);
    }

    #[test]
    fn rest_and_graphql_allowances_are_independent() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe(remote(
            5000,
            3000,
            mono_now,
            wall_now + chrono::Duration::hours(1),
        ));
        budget.observe_primary(
            "search",
            30,
            3,
            27,
            wall_now + chrono::Duration::hours(1),
            mono_now,
        );
        let plan = budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        assert!(plan.graphql_points > 1);
        assert_eq!(
            budget
                .admit_at(
                    ApiResource::rest("search"),
                    "search-rest",
                    RequestPriority::Recent,
                    1,
                    wall_now,
                    mono_now,
                )
                .unwrap_err(),
            AcquireError::ReserveProtected {
                resource: "search".to_string(),
                remaining: 3,
                reserve: 14,
                reset_at: wall_now + chrono::Duration::hours(1),
            }
        );
        assert!(
            budget
                .admit_at(
                    ApiResource::Graphql,
                    "graphql-query",
                    RequestPriority::Recent,
                    1,
                    wall_now,
                    mono_now,
                )
                .is_ok()
        );
    }

    #[test]
    fn primary_exhaustion_only_blocks_its_resource() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset_at = wall_now + chrono::Duration::minutes(30);
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary("graphql", 5000, 4000, 1000, reset_at, mono_now);
        budget.observe_primary("core", 5000, 10, 4990, reset_at, mono_now);
        budget.observe_primary_limit("core", reset_at, "core primary exhausted");

        assert!(matches!(
            budget.admit_at(
                ApiResource::rest("core"),
                "core read",
                RequestPriority::Interactive,
                1,
                wall_now,
                mono_now,
            ),
            Err(AcquireError::CircuitOpen { .. })
        ));
        budget
            .admit_at(
                ApiResource::Graphql,
                "interactive query",
                RequestPriority::Interactive,
                1,
                wall_now,
                mono_now,
            )
            .expect("an exhausted REST resource must not block GraphQL");
        let compact = budget.snapshot().compact();
        assert!(compact.contains(&format!("reset={}", reset_at.to_rfc3339())));
        assert!(compact.contains("eligible="));
        assert!(compact.contains("blocked=core primary exhausted"));
    }

    #[test]
    fn sudden_external_consumption_shrinks_next_allowance() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset = wall_now + chrono::Duration::hours(1);
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary("graphql", 5000, 5000, 0, reset, mono_now);
        let before = budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        budget.observe_primary(
            "graphql",
            5000,
            4400,
            600,
            reset,
            mono_now + Duration::from_secs(60),
        );
        let after = budget.begin_background_tick(
            Duration::from_secs(60),
            wall_now + chrono::Duration::seconds(60),
            mono_now + Duration::from_secs(60),
        );
        assert!(after.graphql_points < before.graphql_points);
    }

    #[test]
    fn external_contention_never_starves_the_daemon_to_zero() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset = wall_now + chrono::Duration::hours(1);
        let mut budget = RateBudget::new(100, 6000.0);
        // Healthy graphql budget: far above the 45% reserve (2250).
        budget.observe_primary("graphql", 5000, 5000, 0, reset, mono_now);
        budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        // A burst of heavy EXTERNAL usage on the shared token (interactive
        // `gh` / spawned agents) — the daemon itself completed nothing, so
        // all 1000 points are attributed to external burn.
        budget.observe_primary(
            "graphql",
            5000,
            4000,
            1000,
            reset,
            mono_now + Duration::from_secs(60),
        );
        let plan = budget.begin_background_tick(
            Duration::from_secs(60),
            wall_now + chrono::Duration::seconds(60),
            mono_now + Duration::from_secs(60),
        );
        // The projected external burn would zero the sustainable rate, but
        // the guaranteed minimum keeps the daemon admitting a complete sweep
        // unit — sync slows under contention, it does not stall (#782).
        assert!(
            plan.graphql_points >= MIN_BACKGROUND_TICK_ALLOWANCE,
            "governor starved the daemon to {} points under external contention",
            plan.graphql_points
        );
        // And the floored allowance is genuinely spendable.
        for index in 0..plan.graphql_points {
            budget
                .admit_at(
                    ApiResource::Graphql,
                    &format!("contended-{index}"),
                    RequestPriority::Recent,
                    1,
                    wall_now + chrono::Duration::seconds(60),
                    mono_now + Duration::from_secs(60),
                )
                .expect("the floored allowance must actually admit requests");
        }
    }

    #[test]
    fn self_imposed_governor_blocks_are_distinguished_from_github_limits() {
        let self_imposed = [
            AcquireError::LocalBudgetExhausted { wait_secs: 5 },
            AcquireError::TickAllowanceExhausted {
                resource: "graphql".into(),
                allowance: 3,
                spent: 0,
                wait_secs: 15,
            },
            AcquireError::ReserveProtected {
                resource: "graphql".into(),
                remaining: 3000,
                reserve: 2250,
                reset_at: Utc::now(),
            },
        ];
        for error in &self_imposed {
            assert!(error.is_self_imposed(), "{error} must be self-imposed");
        }
        let github_imposed = [
            AcquireError::RemoteLow {
                remaining: 5,
                reset_at: Utc::now(),
            },
            AcquireError::CircuitOpen {
                reason: "secondary rate limit".into(),
                retry_at: Utc::now(),
            },
        ];
        for error in &github_imposed {
            assert!(!error.is_self_imposed(), "{error} is GitHub-imposed");
        }
    }

    #[test]
    fn new_primary_window_drops_stale_external_burn() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let first_reset = wall_now + chrono::Duration::minutes(1);
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary("graphql", 5000, 5000, 0, first_reset, mono_now);
        budget.observe_primary(
            "graphql",
            5000,
            3000,
            2000,
            first_reset,
            mono_now + Duration::from_secs(30),
        );
        assert!(
            budget
                .resources
                .get("graphql")
                .expect("graphql resource")
                .external_burn_per_sec
                > 0.0
        );

        let second_reset = wall_now + chrono::Duration::hours(1);
        budget.observe_primary(
            "graphql",
            5000,
            5000,
            0,
            second_reset,
            mono_now + Duration::from_secs(60),
        );
        assert_eq!(
            budget
                .resources
                .get("graphql")
                .expect("graphql resource")
                .external_burn_per_sec,
            0.0
        );
    }

    #[test]
    fn concurrent_local_responses_do_not_look_like_external_burn_or_regress_state() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset_at = wall_now + chrono::Duration::hours(1);
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary("graphql", 5000, 5000, 0, reset_at, mono_now);
        budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        for _ in 0..2 {
            budget
                .admit_at(
                    ApiResource::Graphql,
                    "parallel-query",
                    RequestPriority::Recent,
                    1,
                    wall_now,
                    mono_now,
                )
                .expect("parallel admission");
        }

        budget.observe_graphql_response(
            "parallel-query",
            remote(5000, 4998, mono_now + Duration::from_secs(1), reset_at),
            2,
            1,
            200,
            10,
            Duration::from_millis(5),
        );
        budget.observe_graphql_response(
            "parallel-query",
            remote(5000, 4999, mono_now + Duration::from_secs(2), reset_at),
            1,
            1,
            200,
            10,
            Duration::from_millis(6),
        );

        let graphql = budget
            .snapshot()
            .resources
            .into_iter()
            .find(|resource| resource.resource == "graphql")
            .expect("graphql resource");
        assert_eq!(graphql.used, 2);
        assert_eq!(graphql.remaining, 4998);
        assert_eq!(graphql.external_burn_per_hour, 0.0);
    }

    #[test]
    fn unused_background_allowance_accumulates_for_a_bounded_unit() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset_at = wall_now + chrono::Duration::hours(1);
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary("graphql", 5000, 2310, 2690, reset_at, mono_now);

        let first = budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        assert_eq!(first.graphql_points, 1);
        let second = budget.begin_background_tick(
            Duration::from_secs(60),
            wall_now + chrono::Duration::seconds(60),
            mono_now + Duration::from_secs(60),
        );
        assert_eq!(second.graphql_points, 2);
        budget
            .admit_at(
                ApiResource::Graphql,
                "two-page-query",
                RequestPriority::Recent,
                2,
                wall_now + chrono::Duration::seconds(60),
                mono_now + Duration::from_secs(60),
            )
            .expect("unused credit must accumulate until the complete unit fits");
    }

    #[test]
    fn expired_primary_window_allows_a_complete_probe_unit() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary(
            "graphql",
            5000,
            0,
            5000,
            wall_now - chrono::Duration::seconds(1),
            mono_now,
        );

        let plan = budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        // A complete sweep unit, not a single point — enough for a hot-target
        // batch (one query, several points) to go through and re-learn the
        // window. The old `1` deadlocked sync (see fn docs).
        assert_eq!(plan.graphql_points, MIN_BACKGROUND_TICK_ALLOWANCE);
        assert!(!plan.graphql_budget_current);
        budget
            .admit_at(
                ApiResource::Graphql,
                "budget-bootstrap",
                RequestPriority::Recent,
                1,
                wall_now,
                mono_now,
            )
            .expect("expired observation must allow one reset probe");
    }

    /// #1090-adjacent regression: an expired GraphQL window must admit a
    /// multi-point hot-target batch, not just a 1-point probe. The old
    /// `expired_window_probe_allowance == 1` refused the batch, so no query
    /// re-learned the window and sync stalled for the whole window even with
    /// the primary budget healthy (a ~300s "sync" with 4796 points free).
    #[test]
    fn expired_window_admits_a_multi_point_hot_batch() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        // Healthy primary budget, but the observed window has expired with its
        // last-seen `remaining` spent down to 0 — the characteristic shape at
        // window end. The expired grant must NOT be clamped by that stale
        // `remaining`; clamping (as the non-expired external-burn floor does)
        // would yield 0 here and re-deadlock the exact case this fixes.
        budget.observe_primary(
            "graphql",
            5000,
            0,
            5000,
            wall_now - chrono::Duration::seconds(1),
            mono_now,
        );
        let plan = budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        assert!(
            plan.graphql_points >= 5,
            "an expired window must admit a full sweep unit, got {}",
            plan.graphql_points,
        );
        // A batch costing several points is admitted (not deadlocked).
        budget
            .admit_at(
                ApiResource::Graphql,
                "hot-target-batch",
                RequestPriority::Recent,
                5,
                wall_now,
                mono_now,
            )
            .expect("expired window must admit a multi-point batch to re-learn itself");
    }

    /// Sibling of `expired_window_admits_a_multi_point_hot_batch` for the
    /// *never-observed* window: a cold budget that was never bootstrapped (the
    /// startup probe failed, or a non-full-sweep tick issues a scheduled batch
    /// before any bootstrap) has no rate state at all. The tick-allowance
    /// fallback must still grant a full sweep unit so one scheduled batch can go
    /// out and learn the window; the old `unwrap_or(1)` refused any multi-point
    /// batch → nothing learned the window → the same self-reinforcing stall as
    /// the expired case, reached from a cold start.
    #[test]
    fn unlearned_window_admits_a_multi_point_scheduled_batch() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        // No observe_primary: the GraphQL window has never been observed, so
        // `self.resources` is empty and `tick_allowance` gets no graphql entry.
        let plan = budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        assert!(
            plan.graphql_points >= 5,
            "an unlearned window must plan a full sweep unit, got {}",
            plan.graphql_points,
        );
        // The reserve guards are inert with no observed state (just like an
        // expired window), so the sweep unit is the sole gate — and it must
        // admit a multi-point scheduled batch rather than deadlock at 1.
        budget
            .admit_at(
                ApiResource::Graphql,
                "hot-target-batch",
                RequestPriority::Recent,
                5,
                wall_now,
                mono_now,
            )
            .expect("unlearned window must admit a multi-point batch to learn itself");
    }

    #[test]
    fn actual_cost_reconciles_scheduled_tick_spend() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary(
            "graphql",
            5000,
            5000,
            0,
            wall_now + chrono::Duration::hours(1),
            mono_now,
        );
        budget.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);
        budget
            .admit_at(
                ApiResource::Graphql,
                "under-forecast",
                RequestPriority::Recent,
                1,
                wall_now,
                mono_now,
            )
            .expect("admission");
        budget.observe_graphql_response(
            "under-forecast",
            remote(
                5000,
                4996,
                mono_now + Duration::from_secs(1),
                wall_now + chrono::Duration::hours(1),
            ),
            4,
            4,
            200,
            100,
            Duration::from_millis(5),
        );

        assert_eq!(
            budget
                .snapshot()
                .resources
                .iter()
                .find(|resource| resource.resource == "graphql")
                .expect("graphql snapshot")
                .scheduled,
            4
        );
    }

    #[test]
    fn secondary_limit_opens_one_global_circuit_with_backoff() {
        let wall_now = Utc::now();
        let mut budget = RateBudget::new(100, 6000.0);
        let retry_at = budget.observe_secondary_limit(None, wall_now);
        assert!(retry_at >= wall_now + chrono::Duration::seconds(60));
        // Scheduled work waits out the window on every resource —
        // interactive pass-through (#1218 item 5) has its own test.
        for resource in [ApiResource::Graphql, ApiResource::rest("core")] {
            assert!(matches!(
                budget.admit(resource, "blocked", RequestPriority::Focused, 1),
                Err(AcquireError::CircuitOpen { .. })
            ));
        }
        let second = budget.observe_secondary_limit(None, wall_now);
        assert!(second >= wall_now + chrono::Duration::seconds(120));

        // A single clean response DECAYS the escalation one step, it does not
        // clear it (#1218): a limiter that's clearly still hot keeps backing
        // off harder rather than snapping to the 60s base and re-tripping.
        // failures: 2 → (decay) 1 → (this hit) 2 ⇒ still the 120s tier.
        budget.record_response(
            "recovered",
            ApiResource::Graphql,
            Some(1),
            200,
            false,
            0,
            Duration::ZERO,
        );
        let still_escalated = budget.observe_secondary_limit(None, wall_now);
        assert!(still_escalated >= wall_now + chrono::Duration::seconds(120));

        // Enough consecutive clean responses DO relax it back to the base:
        // decay failures 2 → 0, and the next hit is the 60s tier again.
        for _ in 0..2 {
            budget.record_response(
                "recovered",
                ApiResource::Graphql,
                Some(1),
                200,
                false,
                0,
                Duration::ZERO,
            );
        }
        let relaxed = budget.observe_secondary_limit(None, wall_now);
        assert!(relaxed < wall_now + chrono::Duration::seconds(120));
    }

    /// #1218 item 5: a secondary cooldown (usually opened by background
    /// churn) must not refuse the user's own actions — Interactive
    /// requests pass, paced one per SECONDARY_INTERACTIVE_GAP, while
    /// scheduled priorities keep waiting out the window.
    #[test]
    fn secondary_cooldown_lets_spaced_interactive_requests_through() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_secondary_limit(None, wall_now);

        assert!(
            budget
                .admit_at(
                    ApiResource::Graphql,
                    "g-sync",
                    RequestPriority::Interactive,
                    1,
                    wall_now,
                    mono_now
                )
                .is_ok(),
            "the first interactive request passes the open cooldown",
        );
        assert!(
            matches!(
                budget.admit_at(
                    ApiResource::Graphql,
                    "g-sync",
                    RequestPriority::Interactive,
                    1,
                    wall_now,
                    mono_now + Duration::from_secs(1)
                ),
                Err(AcquireError::CircuitOpen { .. })
            ),
            "a second interactive request inside the gap is paced",
        );
        assert!(
            budget
                .admit_at(
                    ApiResource::Graphql,
                    "g-sync",
                    RequestPriority::Interactive,
                    1,
                    wall_now,
                    mono_now + SECONDARY_INTERACTIVE_GAP
                )
                .is_ok(),
            "the gap elapsing readmits interactive work",
        );
        for scheduled in [RequestPriority::Focused, RequestPriority::Recent] {
            assert!(
                matches!(
                    budget.admit_at(
                        ApiResource::Graphql,
                        "bg",
                        scheduled,
                        1,
                        wall_now,
                        mono_now + SECONDARY_INTERACTIVE_GAP * 2
                    ),
                    Err(AcquireError::CircuitOpen { .. })
                ),
                "scheduled work still waits out the cooldown",
            );
        }
    }

    #[test]
    fn actual_cost_raises_conservative_forecast_and_is_observable() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_graphql_response(
            "InboxSearch",
            remote(5000, 4995, mono_now, wall_now + chrono::Duration::hours(1)),
            5,
            5,
            200,
            1000,
            Duration::from_millis(20),
        );
        assert_eq!(budget.forecast("InboxSearch", 1), 5);
        let snapshot = budget.snapshot();
        let operation = snapshot
            .operations
            .iter()
            .find(|operation| operation.class == "InboxSearch")
            .expect("operation");
        assert_eq!(operation.last_actual, Some(5));
        assert_eq!(operation.material_forecast_errors, 1);
        assert_eq!(snapshot.request_p99_ms, Some(20));
    }

    #[test]
    fn complete_unit_forecast_includes_every_reported_page() {
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_unreported_response(
            "paginated-search",
            ApiResource::Graphql,
            200,
            Duration::ZERO,
        );
        budget.note_expected_pages("paginated-search", 4);

        assert_eq!(budget.forecast("paginated-search", 1), 1);
        assert_eq!(budget.unit_forecast("paginated-search", 1), 4);
        let operation = budget
            .snapshot()
            .operations
            .into_iter()
            .find(|operation| operation.class == "paginated-search")
            .expect("operation");
        assert_eq!(operation.unit_forecast, 4);
    }

    #[test]
    fn changed_items_are_recorded_at_the_commit_boundary() {
        let mut budget = RateBudget::new(100, 6000.0);
        budget.note_items_changed(3);
        assert_eq!(budget.snapshot().tick.items, 3);
        assert_eq!(budget.snapshot().total.items, 3);
    }

    #[test]
    fn persisted_state_round_trips_through_json_and_restore() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset_at = wall_now + chrono::Duration::hours(1);
        let mut budget = RateBudget::new(100, 6000.0);
        // A primary window with observed external burn, plus a live
        // secondary cooldown with an escalated backoff level.
        budget.observe_primary("graphql", 5000, 5000, 0, reset_at, mono_now);
        budget.observe_primary(
            "graphql",
            5000,
            4400,
            600,
            reset_at,
            mono_now + Duration::from_secs(60),
        );
        budget.observe_secondary_limit(None, wall_now);
        budget.observe_secondary_limit(None, wall_now);

        let state = budget.persisted_state();
        let payload = serde_json::to_string(&state).expect("serialize");
        let decoded: PersistedRateState = serde_json::from_str(&payload).expect("deserialize");
        assert_eq!(decoded, state);

        let mut restored = RateBudget::new(100, 6000.0);
        restored.restore_at(decoded, wall_now, mono_now + Duration::from_secs(120));

        let graphql = restored
            .snapshot()
            .resources
            .into_iter()
            .find(|resource| resource.resource == "graphql")
            .expect("graphql resource restored");
        assert_eq!(graphql.remaining, 4400);
        assert_eq!(graphql.limit, 5000);
        // Contention awareness (ext/h) survives the restart.
        assert!(graphql.external_burn_per_hour > 0.0);
        assert_eq!(restored.secondary_failures, 2);
    }

    #[test]
    fn restored_secondary_cooldown_blocks_until_it_expires() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let mut source = RateBudget::new(100, 6000.0);
        let retry_at = source.observe_secondary_limit(None, wall_now);
        assert!(retry_at > wall_now);

        let mut restored = RateBudget::new(100, 6000.0);
        restored.restore_at(source.persisted_state(), wall_now, mono_now);
        // A fresh daemon's SCHEDULED work must NOT fire until the
        // persisted cooldown expires (interactive requests pace through
        // per #1218 item 5 — covered by its own test).
        assert!(matches!(
            restored.admit_at(
                ApiResource::Graphql,
                "post-restart",
                RequestPriority::Focused,
                1,
                wall_now,
                mono_now,
            ),
            Err(AcquireError::CircuitOpen { .. })
        ));
        // Once the window passes, admission resumes.
        assert!(
            restored
                .admit_at(
                    ApiResource::Graphql,
                    "after-cooldown",
                    RequestPriority::Interactive,
                    1,
                    retry_at + chrono::Duration::seconds(1),
                    mono_now,
                )
                .is_ok()
        );
    }

    #[test]
    fn restored_backoff_level_escalates_while_the_cooldown_is_live() {
        let wall_now = Utc::now();
        let mut source = RateBudget::new(100, 6000.0);
        source.observe_secondary_limit(None, wall_now);
        source.observe_secondary_limit(None, wall_now);

        let mut restored = RateBudget::new(100, 6000.0);
        // The persisted cooldown is still in effect at restart, so the
        // backoff level carries over: the next throttle must escalate past
        // a first-ever throttle (60s) instead of resetting.
        restored.restore_at(source.persisted_state(), wall_now, Instant::now());
        let escalated = restored.observe_secondary_limit(None, wall_now);
        assert!(escalated >= wall_now + chrono::Duration::seconds(240));

        let mut fresh = RateBudget::new(100, 6000.0);
        let first = fresh.observe_secondary_limit(None, wall_now);
        assert!(first < wall_now + chrono::Duration::seconds(120));
    }

    #[test]
    fn restored_backoff_resets_when_cooldown_already_elapsed() {
        let throttle_wall = Utc::now();
        let mut source = RateBudget::new(100, 6000.0);
        source.observe_secondary_limit(None, throttle_wall);
        source.observe_secondary_limit(None, throttle_wall);

        // Restart long after the persisted ~120s cooldown fully elapsed.
        let restart_wall = throttle_wall + chrono::Duration::hours(1);
        let mut restored = RateBudget::new(100, 6000.0);
        restored.restore_at(source.persisted_state(), restart_wall, Instant::now());

        // The concluded throttle episode must not inflate the next backoff:
        // the first throttle after restart backs off at the base level
        // (~60s), not the 240s it would inherit from a preserved level.
        let first = restored.observe_secondary_limit(None, restart_wall);
        assert!(first < restart_wall + chrono::Duration::seconds(120));
    }

    #[test]
    fn restore_defers_external_burn_sample_to_the_second_live_observation() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset_at = wall_now + chrono::Duration::hours(1);
        let persisted = PersistedRateState {
            resources: BTreeMap::from([(
                "graphql".to_string(),
                PersistedResource {
                    remaining: 5000,
                    limit: 5000,
                    used: 0,
                    reset_at,
                    external_burn_per_sec: 0.0,
                },
            )]),
            ..Default::default()
        };
        let mut budget = RateBudget::new(100, 6000.0);
        budget.restore_at(persisted, wall_now, mono_now);

        // First live reading two seconds after restart: 500 points were
        // consumed externally while the daemon was down. Diffing against the
        // persisted baseline would report ~250 points/sec; instead this
        // reading must only RE-ANCHOR, leaving burn at zero.
        budget.observe_primary(
            "graphql",
            5000,
            4500,
            500,
            reset_at,
            mono_now + Duration::from_secs(2),
        );
        let after_first = budget
            .snapshot()
            .resources
            .into_iter()
            .find(|resource| resource.resource == "graphql")
            .expect("graphql");
        assert_eq!(
            after_first.external_burn_per_hour, 0.0,
            "the first post-restart reading must not manufacture a burn spike"
        );

        // A second reading 60s later shows 60 more external points — a
        // genuine sample from the re-anchored baseline: ~1 point/sec.
        budget.observe_primary(
            "graphql",
            5000,
            4440,
            560,
            reset_at,
            mono_now + Duration::from_secs(62),
        );
        let after_second = budget
            .snapshot()
            .resources
            .into_iter()
            .find(|resource| resource.resource == "graphql")
            .expect("graphql");
        assert!(
            (after_second.external_burn_per_hour - 3600.0).abs() < 1.0,
            "second reading measures real burn (~3600/h), not a spike: {}/h",
            after_second.external_burn_per_hour
        );
    }

    #[test]
    fn restored_external_burn_shrinks_the_next_allowance() {
        let wall_now = Utc::now();
        let mono_now = Instant::now();
        let reset_at = wall_now + chrono::Duration::hours(1);
        let persisted = PersistedRateState {
            resources: BTreeMap::from([(
                "graphql".to_string(),
                PersistedResource {
                    remaining: 5000,
                    limit: 5000,
                    used: 0,
                    reset_at,
                    external_burn_per_sec: 1.0,
                },
            )]),
            ..Default::default()
        };
        let mut with_burn = RateBudget::new(100, 6000.0);
        with_burn.restore_at(persisted, wall_now, mono_now);
        let contended =
            with_burn.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);

        let mut without_burn = RateBudget::new(100, 6000.0);
        without_burn.observe_primary("graphql", 5000, 5000, 0, reset_at, mono_now);
        let idle = without_burn.begin_background_tick(Duration::from_secs(60), wall_now, mono_now);

        assert!(contended.graphql_points < idle.graphql_points);
    }

    #[test]
    fn request_starts_are_spaced_by_the_baseline_gap() {
        let now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        assert_eq!(budget.reserve_request_slot(now), Duration::ZERO);
        assert_eq!(budget.reserve_request_slot(now), DEFAULT_MIN_REQUEST_GAP);
        assert_eq!(
            budget.reserve_request_slot(now),
            DEFAULT_MIN_REQUEST_GAP * 2
        );
    }

    #[test]
    fn idle_period_does_not_bank_burst_credit() {
        let now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        assert_eq!(budget.reserve_request_slot(now), Duration::ZERO);
        let later = now + DEFAULT_MIN_REQUEST_GAP * 10;
        assert_eq!(budget.reserve_request_slot(later), Duration::ZERO);
    }

    #[test]
    fn secondary_failures_widen_the_gap() {
        let now = Instant::now();
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_secondary_limit(None, Utc::now());
        assert_eq!(budget.reserve_request_slot(now), Duration::ZERO);
        assert_eq!(
            budget.reserve_request_slot(now),
            DEFAULT_MIN_REQUEST_GAP * 2
        );
    }

    #[test]
    fn external_contention_widens_the_gap() {
        let now = Instant::now();
        let mono = Instant::now();
        let reset = Utc::now() + chrono::Duration::hours(1);
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary("graphql", 5000, 5000, 0, reset, mono);
        // 3600 external points over 3600s ⇒ 1 req/s on the shared token.
        budget.observe_primary(
            "graphql",
            5000,
            1400,
            3600,
            reset,
            mono + Duration::from_secs(3600),
        );
        assert_eq!(budget.reserve_request_slot(now), Duration::ZERO);
        assert_eq!(
            budget.reserve_request_slot(now),
            DEFAULT_MIN_REQUEST_GAP * 2
        );
    }

    #[test]
    fn the_gap_is_clamped_to_a_ceiling() {
        let now = Instant::now();
        let mono = Instant::now();
        let reset = Utc::now() + chrono::Duration::hours(1);
        let mut budget = RateBudget::new(100, 6000.0);
        budget.observe_primary("graphql", 100_000, 100_000, 0, reset, mono);
        // 4 req/s external ⇒ 5× contention multiplier.
        budget.observe_primary(
            "graphql",
            100_000,
            96_400,
            3600,
            reset,
            mono + Duration::from_secs(900),
        );
        for _ in 0..8 {
            budget.observe_secondary_limit(None, Utc::now());
        }
        assert_eq!(budget.reserve_request_slot(now), Duration::ZERO);
        assert_eq!(budget.reserve_request_slot(now), MAX_REQUEST_GAP);
    }

    #[test]
    fn a_clean_response_decays_the_secondary_backoff_instead_of_clearing_it() {
        let mut budget = RateBudget::new(100, 6000.0);
        // Trip GitHub's secondary (abuse) limit a few times → wide gap.
        for _ in 0..3 {
            budget.observe_secondary_limit(None, Utc::now());
        }
        assert_eq!(budget.secondary_failures, 3);

        // A single clean 2xx must DECAY the backoff by one step, NOT reset it
        // to zero — otherwise the widened inter-request gap collapses the
        // instant one request slips through and the next burst immediately
        // re-trips the limiter (#1218).
        budget.record_response(
            "probe",
            ApiResource::Graphql,
            Some(1),
            200,
            false,
            0,
            Duration::from_millis(1),
        );
        assert_eq!(
            budget.secondary_failures, 2,
            "one clean response decays by one, not to zero"
        );

        // The gap is still widened (2× the baseline), so the recovery stays
        // paced rather than snapping back to a burst.
        let now = Instant::now();
        assert_eq!(budget.reserve_request_slot(now), Duration::ZERO);
        assert!(budget.reserve_request_slot(now) > DEFAULT_MIN_REQUEST_GAP);
    }
}
