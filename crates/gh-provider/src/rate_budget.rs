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
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Emergency backstop. Proactive background admission normally stops
/// substantially earlier at the configured reserve.
pub const LOW_THRESHOLD: u32 = 100;

pub const DEFAULT_CAPACITY: u32 = 30;
pub const DEFAULT_REFILL_PER_MIN: f64 = 30.0;
pub const DEFAULT_BACKGROUND_SHARE: f64 = 0.55;
pub const DEFAULT_SECONDARY_PAUSE: Duration = Duration::from_secs(60);
const MAX_SECONDARY_PAUSE: Duration = Duration::from_secs(15 * 60);
const LATENCY_SAMPLE_CAPACITY: usize = 1024;

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
}

#[derive(Debug, Clone)]
struct OperationState {
    forecast: u32,
    unit_forecast: u32,
    last_actual: Option<u32>,
    material_forecast_errors: u64,
}

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
    primary_circuits: HashMap<String, CircuitState>,
    secondary_failures: u32,
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
            primary_circuits: HashMap::new(),
            secondary_failures: 0,
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
                expired_window_probe_allowance(resource)
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
                credit.floor() as u32
            };
            self.tick_allowance.insert(resource.clone(), allowance);
        }

        let next_eligible_at = self
            .secondary_circuit
            .as_ref()
            .map(|circuit| circuit.retry_at);
        let graphql_points = self.tick_allowance.get("graphql").copied().unwrap_or(1);
        let rest_core_points = self.tick_allowance.get("core").copied().unwrap_or(1);
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
            return Err(AcquireError::CircuitOpen {
                reason: circuit.reason.clone(),
                retry_at: circuit.retry_at,
            });
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
            let allowance = self
                .tick_allowance
                .get(resource.key())
                .copied()
                .unwrap_or(1);
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
        if self.available < 1.0 {
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
            self.secondary_failures = 0;
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
                        let external_delta =
                            total_delta.saturating_sub(local_delta.min(u64::from(u32::MAX)) as u32);
                        let sample = f64::from(external_delta) / elapsed;
                        previous.external_burn_per_sec = if previous.external_burn_per_sec == 0.0 {
                            sample
                        } else {
                            previous.external_burn_per_sec * 0.75 + sample * 0.25
                        };
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
            primary_circuits: self.primary_circuits.clone(),
            secondary_failures: self.secondary_failures,
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

fn expired_window_probe_allowance(_resource: &str) -> u32 {
    1
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
        assert!(matches!(
            budget.try_acquire_at(now),
            Err(AcquireError::LocalBudgetExhausted { .. })
        ));
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
        while budget.try_acquire_at(now).is_ok() {}
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
        assert_eq!(plan.graphql_points, 1);
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
        for resource in [ApiResource::Graphql, ApiResource::rest("core")] {
            assert!(matches!(
                budget.admit(resource, "blocked", RequestPriority::Interactive, 1),
                Err(AcquireError::CircuitOpen { .. })
            ));
        }
        let second = budget.observe_secondary_limit(None, wall_now);
        assert!(second >= wall_now + chrono::Duration::seconds(120));

        budget.record_response(
            "recovered",
            ApiResource::Graphql,
            Some(1),
            200,
            false,
            0,
            Duration::ZERO,
        );
        let recovered = budget.observe_secondary_limit(None, wall_now);
        assert!(recovered < wall_now + chrono::Duration::seconds(120));
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
}
