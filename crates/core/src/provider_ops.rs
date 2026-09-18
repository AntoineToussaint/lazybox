//! Shared provider operation state machine (#1736).
//!
//! Every source lazybox writes to — GitHub, Linear — has the same three
//! dimensions tangled together in the naive shape "call the API, then
//! hope the next poll agrees":
//!
//! - **observed remote facts** — what the last provider response said;
//! - **desired user intent** — what the user asked for and we have not
//!   yet seen confirmed upstream;
//! - **operation lifecycle** — accepted, sent, acknowledged, uncertain,
//!   waiting out a retry.
//!
//! Collapsing them produces the two failures this module exists to
//! prevent. A poll reply that left the provider *before* our write
//! landed carries the pre-write value, so applying it verbatim silently
//! undoes the write. And an error from a write the user has already
//! superseded (or cancelled) flashes a failure and restores fields
//! nobody is waiting on.
//!
//! ## Shape
//!
//! [`ProviderOps`] is a per-workspace ledger of in-flight intent. It is
//! pure and IO-free: [`ProviderOps::apply`] is `(state, event) ->
//! effects`, and the daemon coordinator persists the transition *before*
//! it runs the effects, so a restart resumes from a durable position.
//!
//! The ledger never writes into [`Task`]. A task always holds the last
//! observation; the ledger holds the desired values, and the projection
//! the UI renders is `observed ⊕ pending` ([`ProviderOps::overlay`]).
//! That is what makes rejection free: dropping the operation removes the
//! overlay, and the true remote value is already underneath it. There is
//! no rollback stash to get out of step.
//!
//! ## Freshness, not arrival order
//!
//! Neither provider exposes a universal monotonic revision, and a
//! response arriving later is not evidence it was produced later. The
//! one sound proof available is the entity's own revision stamp: a
//! successful write bumps the issue/PR `updated_at`, so an observation
//! whose `updated_at` is at or after the moment we were acknowledged has
//! necessarily seen our write. Until such an observation arrives the
//! claimed fields stay overlaid; once it does, the operation settles and
//! whatever the provider reports wins — including a value that
//! contradicts the write, which is how a legitimate external edit
//! (a Linear issue reopened by a teammate) is accepted rather than
//! fought.
//!
//! Provider domain rules stay in the provider adapters. This module owns
//! identity, generations, conflict, freshness and lifecycle; it does not
//! know what a merge queue or a workflow state *means*.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Label, Task, TaskId, TaskState};

/// How many times a transient failure is replayed before the operation
/// gives up and reports. Matches the daemon's own mutation budget.
const MAX_ATTEMPTS: u32 = 5;

/// How long an unconfirmed claim keeps painting the row before the
/// ledger stops waiting for it.
///
/// The revision proof has one hole: a write the provider accepts as a
/// no-op (assigning the person already assigned) need not bump the
/// entity's `updated_at`, so no future observation can ever be shown to
/// postdate it. Without a deadline that claim overlays forever and the
/// ledger never empties. Once this much time has passed the provider's
/// own value is the better answer than an intent nothing has confirmed.
pub const SETTLE_DEADLINE: chrono::TimeDelta = chrono::TimeDelta::minutes(10);

/// A fact on a provider entity that a write claims ownership of while it
/// is in flight. Deliberately closed and small: these are the fields
/// both GitHub and Linear actually mutate, and adding one is a
/// deliberate edit rather than a stringly-typed escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum MutationField {
    Assignees,
    Labels,
    Reviewers,
    State,
}

/// A write to the entity's workflow state.
///
/// `state` is the canonical collapsed state so core rules (and the UI)
/// can reason without knowing the provider. `label` is the provider's
/// own name for the destination — Linear's per-team workflow status,
/// resolved from live team metadata by the adapter. Core never
/// enumerates those names: a Linear team's states are its own
/// configuration, not a global Open/In Progress/Done enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct StateWrite {
    pub state: TaskState,
    #[serde(default)]
    pub label: Option<String>,
}

/// The values a single write asks the provider to hold. The set of
/// populated fields *is* the operation's claim: two operations conflict
/// exactly when their claims intersect, and independent edits (a label
/// change while an assignment is still in flight) never block on each
/// other.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct DesiredFields {
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub reviewers: Option<Vec<String>>,
    #[serde(default)]
    pub state: Option<StateWrite>,
}

impl DesiredFields {
    pub fn assignees(logins: Vec<String>) -> Self {
        Self {
            assignees: Some(logins),
            ..Self::default()
        }
    }

    pub fn labels(names: Vec<String>) -> Self {
        Self {
            labels: Some(names),
            ..Self::default()
        }
    }

    pub fn reviewers(logins: Vec<String>) -> Self {
        Self {
            reviewers: Some(logins),
            ..Self::default()
        }
    }

    pub fn state(state: TaskState, label: Option<String>) -> Self {
        Self {
            state: Some(StateWrite { state, label }),
            ..Self::default()
        }
    }

    /// The fields this write claims.
    pub fn claimed(&self) -> BTreeSet<MutationField> {
        let mut set = BTreeSet::new();
        if self.assignees.is_some() {
            set.insert(MutationField::Assignees);
        }
        if self.labels.is_some() {
            set.insert(MutationField::Labels);
        }
        if self.reviewers.is_some() {
            set.insert(MutationField::Reviewers);
        }
        if self.state.is_some() {
            set.insert(MutationField::State);
        }
        set
    }

    pub fn is_empty(&self) -> bool {
        self.claimed().is_empty()
    }

    /// `task`'s current values for exactly the fields `self` claims —
    /// the baseline a write falls back to if it never lands.
    pub fn observed_counterpart(&self, task: &Task) -> Self {
        Self {
            assignees: self.assignees.as_ref().map(|_| task.assignees.clone()),
            labels: self
                .labels
                .as_ref()
                .map(|_| task.labels.iter().map(|l| l.name.clone()).collect()),
            reviewers: self.reviewers.as_ref().map(|_| task.reviewers.clone()),
            state: self.state.as_ref().map(|_| StateWrite {
                state: task.state,
                label: task.state_label.clone(),
            }),
        }
    }

    /// Take each field `self` does not carry from `other`.
    fn fill_from(&mut self, other: &Self) {
        if self.assignees.is_none() {
            self.assignees = other.assignees.clone();
        }
        if self.labels.is_none() {
            self.labels = other.labels.clone();
        }
        if self.reviewers.is_none() {
            self.reviewers = other.reviewers.clone();
        }
        if self.state.is_none() {
            self.state = other.state.clone();
        }
    }

    /// Keep only the fields `claimed` names.
    fn restricted_to(&self, claimed: &BTreeSet<MutationField>) -> Self {
        Self {
            assignees: claimed
                .contains(&MutationField::Assignees)
                .then(|| self.assignees.clone())
                .flatten(),
            labels: claimed
                .contains(&MutationField::Labels)
                .then(|| self.labels.clone())
                .flatten(),
            reviewers: claimed
                .contains(&MutationField::Reviewers)
                .then(|| self.reviewers.clone())
                .flatten(),
            state: claimed
                .contains(&MutationField::State)
                .then(|| self.state.clone())
                .flatten(),
        }
    }
}

/// Identity of one write attempt, unique within a workspace ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct OperationId(pub u64);

impl std::fmt::Display for OperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "op{}", self.0)
    }
}

/// Where an operation sits between acceptance and settlement.
///
/// The distinction that carries the weight is **has anything reached the
/// provider**: `Accepted` and `RetryScheduled` have not, so cancelling
/// them is clean. Everything past that may have mutated remote state
/// whatever the local outcome says, so it is reconciled rather than
/// assumed failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum OpPhase {
    /// Persisted, not yet handed to the provider.
    Accepted,
    /// Handed to the provider; outcome not yet known.
    Sent,
    /// The provider acknowledged the write. The claim stays overlaid
    /// until an observation at or after `at` proves the read side agrees.
    Acked { at: DateTime<Utc> },
    /// The outcome is unknown — a timeout, a dropped connection, or a
    /// restart across the send. A timeout is not proof of failure, so
    /// this is reconciled by a targeted re-fetch and never replayed
    /// blind.
    Uncertain { since: DateTime<Utc> },
    /// A transient failure is waiting out its backoff.
    RetryScheduled { not_before: DateTime<Utc> },
}

impl OpPhase {
    /// Whether the request may already have mutated remote state.
    fn reached_provider(&self) -> bool {
        matches!(
            self,
            Self::Sent | Self::Acked { .. } | Self::Uncertain { .. }
        )
    }

    /// The instant from which an observation is new enough to settle
    /// this operation. An acknowledged write is proven visible only from
    /// its ack; for every other phase the write is at most as old as the
    /// request, so the request time is the honest bar.
    fn settles_from(&self, issued_at: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::Acked { at } => *at,
            _ => issued_at,
        }
    }
}

/// One accepted, not-yet-settled write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct PendingMutation {
    pub id: OperationId,
    /// The entity this write targets. A workspace can hold a PR and its
    /// issues; the claim belongs to one of them, not to the row.
    pub task: TaskId,
    pub desired: DesiredFields,
    /// What the provider last said the claimed fields held. Restored if
    /// the claim ends without ever landing, so a rejected write uncovers
    /// the truth immediately instead of leaving its value on the row
    /// until the next poll. Refreshed by every observation, so it always
    /// names the freshest thing the provider actually told us.
    #[serde(default)]
    pub previous: DesiredFields,
    /// The per-field generation this write was accepted at. A field
    /// whose ledger generation has moved past the value recorded here
    /// has a newer owner, so this operation's outcome is obsolete for it.
    pub claimed: BTreeMap<MutationField, u64>,
    pub issued_at: DateTime<Utc>,
    pub phase: OpPhase,
    pub attempts: u32,
}

impl PendingMutation {
    /// Whether an observation of this entity, carrying provider revision
    /// `revision` and read at `now`, ends this operation's claim.
    ///
    /// Three ways to be done. The observation provably postdates the
    /// write, so it has seen it. Or the operation had already given up on
    /// knowing ([`OpPhase::Uncertain`]) and a re-read is by construction
    /// the answer it was waiting for. Or the claim has simply outlived
    /// [`SETTLE_DEADLINE`] with nothing to confirm it.
    fn settled_by(&self, revision: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        matches!(self.phase, OpPhase::Uncertain { .. })
            || revision >= self.phase.settles_from(self.issued_at)
            || now - self.issued_at >= SETTLE_DEADLINE
    }
}

/// Why a write failed, as far as the local side can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Transport or rate-limit; the same request can be replayed.
    Transient,
    /// The provider rejected it — validation, permission, a missing or
    /// inaccessible entity. Replaying changes nothing.
    Rejected,
    /// No verdict reached us. The write may or may not have landed.
    Uncertain,
}

/// Something that happened to an operation, or to the entity it targets.
#[derive(Debug, Clone)]
pub enum OpEvent {
    /// A user command (or automation) asked for a write.
    Requested {
        task: TaskId,
        desired: DesiredFields,
        now: DateTime<Utc>,
    },
    /// The effect was handed to the provider.
    Sent { id: OperationId },
    /// The provider acknowledged the write.
    Acked { id: OperationId, now: DateTime<Utc> },
    Failed {
        id: OperationId,
        class: FailureClass,
        detail: String,
        now: DateTime<Utc>,
    },
    /// The user withdrew intent over these fields without replacing it.
    Cancelled {
        task: TaskId,
        fields: BTreeSet<MutationField>,
        now: DateTime<Utc>,
    },
}

/// Work the coordinator must do after a transition is persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpEffect {
    /// Issue the provider write for this operation.
    Send(OperationId),
    /// Outcome unknown — re-fetch the entity and let the observation
    /// decide, before any replay.
    Reconcile(OperationId),
    /// Replay after the backoff.
    Retry {
        id: OperationId,
        not_before: DateTime<Utc>,
    },
    /// Surface this failure to the user. Only ever emitted for an
    /// operation that is still the current owner of its fields.
    Report { id: OperationId, message: String },
    /// The operation left the ledger; any timer still holding it can stop.
    Settled(OperationId),
}

/// Per-workspace ledger of in-flight provider intent.
///
/// Persisted with the workspace, which is what makes restart recovery a
/// replay rather than a guess.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ProviderOps {
    #[serde(default)]
    next_id: u64,
    /// Current owner generation per field. Bumped by every request and
    /// every cancellation, so an in-flight operation can tell whether it
    /// still speaks for a field without the newer operation having to
    /// still exist.
    #[serde(default)]
    generations: BTreeMap<MutationField, u64>,
    #[serde(default)]
    pending: Vec<PendingMutation>,
}

impl ProviderOps {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn pending(&self) -> &[PendingMutation] {
        &self.pending
    }

    pub fn get(&self, id: OperationId) -> Option<&PendingMutation> {
        self.pending.iter().find(|op| op.id == id)
    }

    /// Whether `op` still owns every field it claimed. A superseded or
    /// cancelled operation loses the right to report, to retry, and to
    /// keep its overlay — but not the obligation to have its remote
    /// outcome reconciled.
    fn is_current(&self, op: &PendingMutation) -> bool {
        op.claimed
            .iter()
            .all(|(field, at)| self.generations.get(field).copied().unwrap_or(0) == *at)
    }

    fn remove(&mut self, id: OperationId) {
        self.pending.retain(|op| op.id != id);
    }

    fn phase_mut(&mut self, id: OperationId) -> Option<&mut PendingMutation> {
        self.pending.iter_mut().find(|op| op.id == id)
    }

    /// `(state, event) -> effects`, against the entity the event
    /// concerns. Pure: no IO, no clock of its own — every event carries
    /// the instant it happened at.
    ///
    /// `entity` is re-projected on the way out, so the ledger and the row
    /// it paints move together and can never disagree. A claim that ends
    /// without ever landing puts back what the provider last said, which
    /// is why a rejected write needs no separate rollback path.
    pub fn apply(&mut self, event: OpEvent, entity: &mut Task) -> Vec<OpEffect> {
        let effects = match event {
            OpEvent::Requested { task, desired, now } => {
                self.on_requested(task, desired, now, entity)
            }
            OpEvent::Sent { id } => {
                if let Some(op) = self.phase_mut(id) {
                    op.phase = OpPhase::Sent;
                }
                Vec::new()
            }
            OpEvent::Acked { id, now } => {
                if let Some(op) = self.phase_mut(id) {
                    op.phase = OpPhase::Acked { at: now };
                }
                Vec::new()
            }
            OpEvent::Failed {
                id,
                class,
                detail,
                now,
            } => self.on_failed(id, class, detail, now, entity),
            OpEvent::Cancelled { task, fields, now } => {
                self.on_cancelled(&task, &fields, now, entity)
            }
        };
        self.overlay(entity);
        effects
    }

    /// Re-adopt a persisted ledger after a restart.
    ///
    /// Task-free on purpose: recovery decides what to *re-issue*, and
    /// drops nothing, so nothing on the row changes. A workspace can hold
    /// several entities and this speaks for all of them at once.
    pub fn recover(&mut self, now: DateTime<Utc>) -> Vec<OpEffect> {
        let mut effects = Vec::new();
        for op in &mut self.pending {
            match op.phase {
                // Persisted before the effect ran, so nothing reached the
                // provider — the restart is the first attempt.
                OpPhase::Accepted => effects.push(OpEffect::Send(op.id)),
                // We were mid-flight or already unsure. Either way the
                // remote outcome is unknown across the restart, and
                // replaying a non-idempotent write on a guess is how you
                // get a second mutation.
                OpPhase::Sent | OpPhase::Uncertain { .. } => {
                    op.phase = OpPhase::Uncertain { since: now };
                    effects.push(OpEffect::Reconcile(op.id));
                }
                // Acknowledged: nothing to re-issue. It settles when an
                // observation catches up, exactly as before the restart.
                OpPhase::Acked { .. } => {}
                OpPhase::RetryScheduled { not_before } => effects.push(OpEffect::Retry {
                    id: op.id,
                    not_before: not_before.max(now),
                }),
            }
        }
        effects
    }

    /// Put back what the provider last said about `op`'s fields. Called
    /// for a claim that ends without ever being confirmed; the caller
    /// re-overlays afterwards, so intent that still stands wins again.
    fn restore(op: &PendingMutation, entity: &mut Task) {
        if op.task != entity.id {
            return;
        }
        if let Some(assignees) = &op.previous.assignees {
            entity.assignees = assignees.clone();
        }
        if let Some(names) = &op.previous.labels {
            entity.labels = names.iter().map(|n| Label::new(n.clone())).collect();
        }
        if let Some(reviewers) = &op.previous.reviewers {
            entity.reviewers = reviewers.clone();
        }
        if let Some(write) = &op.previous.state {
            entity.state = write.state;
            entity.state_label = write.label.clone();
        }
    }

    fn on_requested(
        &mut self,
        task: TaskId,
        desired: DesiredFields,
        now: DateTime<Utc>,
        entity: &mut Task,
    ) -> Vec<OpEffect> {
        let fields = desired.claimed();
        if fields.is_empty() {
            return Vec::new();
        }
        // The baseline is what the PROVIDER last said, which is not what
        // the entity shows while an earlier write of ours is painting it.
        // Inherit that write's baseline for the fields it already claims,
        // and read the rest off the entity.
        let mut previous = DesiredFields::default();
        for op in self.pending.iter().filter(|op| op.task == task) {
            previous.fill_from(&op.previous.restricted_to(&fields));
        }
        previous.fill_from(&desired.observed_counterpart(entity));
        let mut effects = Vec::new();
        let claimed = self.bump(&fields);

        // An operation that never reached the provider and speaks for a
        // field this request has just taken over is dead weight: nothing
        // remote depends on it, so drop it rather than let the user's
        // newer intent queue behind a request that is now wrong.
        let superseded: Vec<OperationId> = self
            .pending
            .iter()
            .filter(|op| {
                op.task == task
                    && !op.phase.reached_provider()
                    && !op.desired.claimed().is_disjoint(&fields)
            })
            .map(|op| op.id)
            .collect();
        for id in superseded {
            self.remove(id);
            effects.push(OpEffect::Settled(id));
        }

        let id = OperationId(self.next_id);
        self.next_id += 1;
        self.pending.push(PendingMutation {
            id,
            task,
            desired,
            previous,
            claimed,
            issued_at: now,
            phase: OpPhase::Accepted,
            attempts: 0,
        });
        effects.push(OpEffect::Send(id));
        effects
    }

    fn on_failed(
        &mut self,
        id: OperationId,
        class: FailureClass,
        detail: String,
        now: DateTime<Utc>,
        entity: &mut Task,
    ) -> Vec<OpEffect> {
        let Some(op) = self.get(id).cloned() else {
            return Vec::new();
        };
        // An uncertain outcome is uncertain whoever owns the fields now:
        // the write may have landed, so the entity must be re-read before
        // anything replays over it.
        if class == FailureClass::Uncertain {
            if let Some(op) = self.phase_mut(id) {
                op.phase = OpPhase::Uncertain { since: now };
            }
            return vec![OpEffect::Reconcile(id)];
        }
        // Superseded or cancelled: the failure is real but it belongs to
        // intent nobody holds any more. Drop it without touching state or
        // the user's screen — the diagnostic stays in the daemon log.
        if !self.is_current(&op) {
            Self::restore(&op, entity);
            self.remove(id);
            return vec![OpEffect::Settled(id)];
        }
        if class == FailureClass::Transient && op.attempts + 1 < MAX_ATTEMPTS {
            let attempts = op.attempts + 1;
            let not_before = now + chrono::Duration::seconds(backoff_secs(attempts));
            if let Some(op) = self.phase_mut(id) {
                op.attempts = attempts;
                op.phase = OpPhase::RetryScheduled { not_before };
            }
            return vec![OpEffect::Retry { id, not_before }];
        }
        Self::restore(&op, entity);
        self.remove(id);
        vec![
            OpEffect::Report {
                id,
                message: detail,
            },
            OpEffect::Settled(id),
        ]
    }

    fn on_cancelled(
        &mut self,
        task: &TaskId,
        fields: &BTreeSet<MutationField>,
        _now: DateTime<Utc>,
        entity: &mut Task,
    ) -> Vec<OpEffect> {
        if fields.is_empty() {
            return Vec::new();
        }
        self.bump(fields);
        let mut effects = Vec::new();
        let affected: Vec<PendingMutation> = self
            .pending
            .iter()
            .filter(|op| &op.task == task && !op.desired.claimed().is_disjoint(fields))
            .cloned()
            .collect();
        for op in affected {
            if op.phase.reached_provider() {
                // Already on the wire — cancellation cannot unsend it.
                // Find out what it actually did instead of reporting a
                // withdrawal we cannot honour.
                effects.push(OpEffect::Reconcile(op.id));
            } else {
                Self::restore(&op, entity);
                self.remove(op.id);
                effects.push(OpEffect::Settled(op.id));
            }
        }
        effects
    }

    /// Bump each field's generation and return the operation's snapshot
    /// of the new values.
    fn bump(&mut self, fields: &BTreeSet<MutationField>) -> BTreeMap<MutationField, u64> {
        fields
            .iter()
            .map(|field| {
                let counter = self.generations.entry(*field).or_insert(0);
                *counter += 1;
                (*field, *counter)
            })
            .collect()
    }

    /// Fold a fresh observation of `task` into the ledger, then overlay
    /// whatever intent it has not caught up with.
    ///
    /// Settlement is decided by the entity's own revision stamp, never by
    /// arrival order: an observation at or after the moment an operation
    /// was acknowledged has necessarily seen that write, so the operation
    /// is done and the provider's value — agreeing or not — becomes the
    /// truth. Anything older is a reply that left before our write and
    /// must not undo it.
    pub fn reconcile_observation(&mut self, task: &mut Task, now: DateTime<Utc>) {
        let revision = task.updated_at;
        self.pending
            .retain(|op| op.task != task.id || !op.settled_by(revision, now));
        // Whatever this read says is now the freshest thing the provider
        // has told us about these fields, so it becomes the baseline a
        // surviving claim falls back to. Without the refresh a later
        // rejection would put back a value two observations old.
        for op in self.pending.iter_mut().filter(|op| op.task == task.id) {
            op.previous = op.desired.observed_counterpart(task);
        }
        self.overlay(task);
    }

    /// Lay the still-pending desired values over an observed task, so the
    /// row shows what the user asked for while the provider catches up.
    /// Later operations win over earlier ones on the same field.
    pub fn overlay(&self, task: &mut Task) {
        for op in self.pending.iter().filter(|op| op.task == task.id) {
            if !self.is_current(op) {
                continue;
            }
            if let Some(assignees) = &op.desired.assignees {
                task.assignees = assignees.clone();
            }
            if let Some(names) = &op.desired.labels {
                task.labels = names
                    .iter()
                    .map(|name| {
                        task.labels
                            .iter()
                            .find(|existing| existing.name.eq_ignore_ascii_case(name))
                            .cloned()
                            .unwrap_or_else(|| Label::new(name.clone()))
                    })
                    .collect();
            }
            if let Some(reviewers) = &op.desired.reviewers {
                task.reviewers = reviewers.clone();
            }
            if let Some(write) = &op.desired.state {
                task.state = write.state;
                // The observed label names the observed state, which this
                // write has just superseded. A provider that has a name
                // for the destination supplies it; one that has none
                // (GitHub) leaves the row unlabelled rather than showing
                // the status the entity has moved off.
                task.state_label = write.label.clone();
            }
        }
    }
}

/// Backoff before replaying a transient failure, in seconds. Linear in
/// the attempt count — the caller's own rate-limit hint, when the
/// provider gives one, is a floor applied on top by the coordinator.
fn backoff_secs(attempt: u32) -> i64 {
    i64::from(attempt) * 15
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskRole, TaskState};

    fn task_id() -> TaskId {
        TaskId {
            source: "linear".into(),
            key: "ENG-1".into(),
        }
    }

    fn other_task_id() -> TaskId {
        TaskId {
            source: "linear".into(),
            key: "ENG-2".into(),
        }
    }

    fn task_at(updated_at: DateTime<Utc>) -> Task {
        Task {
            id: task_id(),
            title: "t".into(),
            body: None,
            state: TaskState::InProgress,
            role: TaskRole::Assignee,
            ci: Default::default(),
            review: Default::default(),
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: None,
            branch: None,
            base_branch: None,
            updated_at,
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            approval_policy: Default::default(),
            assignees: vec![],
            author: String::new(),
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Default::default(),
            is_behind_base: false,
            merge_blocked: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            closes_issues: vec![],
            linked_tasks: vec![],
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
            parent: None,
            kind: None,
            priority: None,
            state_label: Some("In Progress".into()),
        }
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    /// Lifecycle-only transitions against a scratch entity. Every
    /// transition re-projects the row it concerns; tests that care about
    /// the projection pass their own entity to
    /// [`ProviderOps::apply`] / [`ProviderOps::overlay`] instead.
    trait ApplyDetached {
        fn apply_detached(&mut self, event: OpEvent) -> Vec<OpEffect>;
    }

    impl ApplyDetached for ProviderOps {
        fn apply_detached(&mut self, event: OpEvent) -> Vec<OpEffect> {
            self.apply(event, &mut task_at(t(-100)))
        }
    }

    fn send_id(effects: &[OpEffect]) -> OperationId {
        effects
            .iter()
            .find_map(|e| match e {
                OpEffect::Send(id) => Some(*id),
                _ => None,
            })
            .expect("a request must produce a Send")
    }

    /// Accepting a request claims its fields and asks for exactly one
    /// send. Nothing is written into the task.
    #[test]
    fn request_claims_fields_and_asks_for_one_send() {
        let mut ops = ProviderOps::default();
        let effects = ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        });
        assert_eq!(effects.len(), 1);
        let id = send_id(&effects);
        let op = ops.get(id).unwrap();
        assert_eq!(op.phase, OpPhase::Accepted);
        assert_eq!(op.desired.claimed(), [MutationField::Assignees].into());
    }

    #[test]
    fn empty_request_is_ignored() {
        let mut ops = ProviderOps::default();
        assert!(
            ops.apply_detached(OpEvent::Requested {
                task: task_id(),
                desired: DesiredFields::default(),
                now: t(0),
            })
            .is_empty()
        );
        assert!(ops.is_empty());
    }

    /// The overlay is the whole point: an observation keeps its own
    /// values underneath, and the pending write shows on top.
    #[test]
    fn overlay_shows_desired_over_observed() {
        let mut ops = ProviderOps::default();
        ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        });
        let mut task = task_at(t(-10));
        task.assignees = vec!["bo".into()];
        ops.overlay(&mut task);
        assert_eq!(task.assignees, vec!["ana".to_string()]);
    }

    /// A rejected write needs no rollback stash: dropping the operation
    /// removes the overlay and the observed value is already underneath.
    #[test]
    fn rejection_drops_the_overlay_and_reports() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id });
        let effects = ops.apply_detached(OpEvent::Failed {
            id,
            class: FailureClass::Rejected,
            detail: "user `ana` not found".into(),
            now: t(1),
        });
        assert!(effects.contains(&OpEffect::Report {
            id,
            message: "user `ana` not found".into()
        }));
        let mut task = task_at(t(-10));
        task.assignees = vec!["bo".into()];
        ops.overlay(&mut task);
        assert_eq!(task.assignees, vec!["bo".to_string()], "overlay is gone");
    }

    /// Linear regression (#1736): mark Done, then a delayed poll reply
    /// that left Linear before the write must not undo it — and a later
    /// authoritative reopen must.
    #[test]
    fn stale_observation_cannot_undo_a_write_but_a_fresh_one_can() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::state(TaskState::Closed, Some("Done".into())),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id });
        ops.apply_detached(OpEvent::Acked { id, now: t(1) });

        // A poll produced at t(-5) — before the write — arrives now.
        let mut stale = task_at(t(-5));
        stale.state = TaskState::InProgress;
        stale.state_label = Some("In Progress".into());
        ops.reconcile_observation(&mut stale, t(6));
        assert_eq!(stale.state, TaskState::Closed, "stale poll must not reopen");
        assert_eq!(stale.state_label.as_deref(), Some("Done"));
        assert!(!ops.is_empty(), "the write is still unconfirmed");

        // A teammate reopens the issue at t(30); that observation is
        // newer than our ack, so it is the truth.
        let mut reopened = task_at(t(30));
        reopened.state = TaskState::InProgress;
        reopened.state_label = Some("In Progress".into());
        ops.reconcile_observation(&mut reopened, t(31));
        assert_eq!(reopened.state, TaskState::InProgress, "reopen is accepted");
        assert!(ops.is_empty(), "the operation settled on a fresh read");
    }

    /// An observation that agrees settles the operation just the same —
    /// the ledger must not keep overlaying a write the provider already
    /// holds.
    #[test]
    fn agreeing_observation_settles_the_operation() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Acked { id, now: t(1) });
        let mut observed = task_at(t(2));
        observed.assignees = vec!["ana".into()];
        ops.reconcile_observation(&mut observed, t(3));
        assert!(ops.is_empty());
    }

    /// An observation of a *different* entity settles nothing — a
    /// workspace holds a PR and its issues, and a claim belongs to one.
    #[test]
    fn observation_of_another_entity_leaves_the_claim_alone() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Acked { id, now: t(1) });
        let mut other = task_at(t(50));
        other.id = other_task_id();
        ops.reconcile_observation(&mut other, t(51));
        assert_eq!(ops.pending().len(), 1);
        assert!(other.assignees.is_empty(), "no overlay onto another entity");
    }

    /// Linear regression (#1736): a failure from a write the user has
    /// already replaced must not flash an error or restore old fields.
    #[test]
    fn obsolete_failure_neither_reports_nor_reverts() {
        let mut ops = ProviderOps::default();
        let first = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id: first });
        // The user picks someone else while the first write is in flight.
        let second = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["bo".into()]),
            now: t(1),
        }));
        let effects = ops.apply_detached(OpEvent::Failed {
            id: first,
            class: FailureClass::Rejected,
            detail: "boom".into(),
            now: t(2),
        });
        assert_eq!(effects, vec![OpEffect::Settled(first)], "no Report");

        let mut task = task_at(t(-10));
        ops.overlay(&mut task);
        assert_eq!(
            task.assignees,
            vec!["bo".to_string()],
            "the live intent survives the obsolete failure"
        );
        assert!(ops.get(second).is_some());
    }

    /// A newer request supersedes an unsent one outright — the user's
    /// latest intent must not queue behind a request that is now wrong.
    #[test]
    fn a_new_request_drops_an_unsent_one_on_the_same_field() {
        let mut ops = ProviderOps::default();
        let first = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        let effects = ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["chore".into()]),
            now: t(1),
        });
        assert!(effects.contains(&OpEffect::Settled(first)));
        assert_eq!(ops.pending().len(), 1);
    }

    /// A sent request is not dropped by a newer one — its remote outcome
    /// still has to be reconciled — but it loses the overlay.
    #[test]
    fn a_new_request_keeps_a_sent_one_for_reconciliation() {
        let mut ops = ProviderOps::default();
        let first = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id: first });
        ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["chore".into()]),
            now: t(1),
        });
        assert_eq!(ops.pending().len(), 2);
        let mut task = task_at(t(-10));
        ops.overlay(&mut task);
        assert_eq!(
            task.labels
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            vec!["chore"],
            "the superseded claim must not paint the row"
        );
    }

    /// Independent fields do not serialize against each other.
    #[test]
    fn unrelated_fields_do_not_block_each_other() {
        let mut ops = ProviderOps::default();
        let assign = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        }));
        let label = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(1),
        }));
        assert_eq!(ops.pending().len(), 2);
        // The label request must not have obsoleted the assignment.
        let effects = ops.apply_detached(OpEvent::Failed {
            id: assign,
            class: FailureClass::Rejected,
            detail: "nope".into(),
            now: t(2),
        });
        assert!(effects.iter().any(|e| matches!(e, OpEffect::Report { .. })));
        assert!(ops.get(label).is_some());
    }

    #[test]
    fn transient_failure_retries_then_reports() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        for attempt in 1..MAX_ATTEMPTS {
            let effects = ops.apply_detached(OpEvent::Failed {
                id,
                class: FailureClass::Transient,
                detail: "rate limited".into(),
                now: t(i64::from(attempt)),
            });
            assert!(
                matches!(effects.as_slice(), [OpEffect::Retry { .. }]),
                "attempt {attempt} must schedule a retry, got {effects:?}"
            );
        }
        let effects = ops.apply_detached(OpEvent::Failed {
            id,
            class: FailureClass::Transient,
            detail: "rate limited".into(),
            now: t(99),
        });
        assert!(effects.iter().any(|e| matches!(e, OpEffect::Report { .. })));
        assert!(ops.is_empty());
    }

    /// A timeout is an unknown outcome, not a failure: it must reconcile
    /// rather than report or replay.
    #[test]
    fn uncertain_outcome_reconciles_instead_of_replaying() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::state(TaskState::Closed, Some("Done".into())),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id });
        let effects = ops.apply_detached(OpEvent::Failed {
            id,
            class: FailureClass::Uncertain,
            detail: "timed out".into(),
            now: t(30),
        });
        assert_eq!(effects, vec![OpEffect::Reconcile(id)]);
        assert!(matches!(
            ops.get(id).unwrap().phase,
            OpPhase::Uncertain { .. }
        ));
    }

    /// An uncertain outcome on an operation nobody owns any more still
    /// reconciles — the write may have landed whatever the user has since
    /// decided.
    #[test]
    fn uncertain_outcome_reconciles_even_when_obsolete() {
        let mut ops = ProviderOps::default();
        let first = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id: first });
        ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["chore".into()]),
            now: t(1),
        });
        let effects = ops.apply_detached(OpEvent::Failed {
            id: first,
            class: FailureClass::Uncertain,
            detail: "timed out".into(),
            now: t(2),
        });
        assert_eq!(effects, vec![OpEffect::Reconcile(first)]);
    }

    /// Cancelling an unsent write is clean; cancelling a sent one cannot
    /// unsend it, so the outcome is reconciled instead of assumed.
    #[test]
    fn cancel_drops_unsent_work_and_reconciles_sent_work() {
        let mut ops = ProviderOps::default();
        let unsent = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        let effects = ops.apply_detached(OpEvent::Cancelled {
            task: task_id(),
            fields: [MutationField::Labels].into(),
            now: t(1),
        });
        assert_eq!(effects, vec![OpEffect::Settled(unsent)]);

        let sent = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["chore".into()]),
            now: t(2),
        }));
        ops.apply_detached(OpEvent::Sent { id: sent });
        let effects = ops.apply_detached(OpEvent::Cancelled {
            task: task_id(),
            fields: [MutationField::Labels].into(),
            now: t(3),
        });
        assert_eq!(effects, vec![OpEffect::Reconcile(sent)]);
        // Cancelled intent stops painting the row immediately.
        let mut task = task_at(t(-10));
        task.labels = vec![Label::new("bug")];
        ops.overlay(&mut task);
        assert_eq!(
            task.labels
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bug"]
        );
    }

    /// A failure arriving after cancellation must stay silent.
    #[test]
    fn failure_after_cancel_is_silent() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id });
        ops.apply_detached(OpEvent::Cancelled {
            task: task_id(),
            fields: [MutationField::Labels].into(),
            now: t(1),
        });
        let effects = ops.apply_detached(OpEvent::Failed {
            id,
            class: FailureClass::Rejected,
            detail: "boom".into(),
            now: t(2),
        });
        assert_eq!(effects, vec![OpEffect::Settled(id)]);
    }

    /// Restart: what never left the box is sent, what was in flight is
    /// reconciled before anything replays, and an acknowledged write just
    /// waits for the read side.
    #[test]
    fn restart_resends_unsent_and_reconciles_in_flight() {
        let mut ops = ProviderOps::default();
        let accepted = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        let sent = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(1),
        }));
        ops.apply_detached(OpEvent::Sent { id: sent });
        let acked = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::reviewers(vec!["cy".into()]),
            now: t(2),
        }));
        ops.apply_detached(OpEvent::Acked {
            id: acked,
            now: t(3),
        });

        let effects = ops.recover(t(100));
        assert!(effects.contains(&OpEffect::Send(accepted)));
        assert!(effects.contains(&OpEffect::Reconcile(sent)));
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                OpEffect::Send(id) | OpEffect::Reconcile(id) if *id == acked
            )),
            "an acknowledged write must not be re-issued"
        );
    }

    /// A retry that was still pending when the daemon stopped is re-armed
    /// rather than dropped or fired instantly in the past.
    #[test]
    fn restart_rearms_a_scheduled_retry() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Failed {
            id,
            class: FailureClass::Transient,
            detail: "rate limited".into(),
            now: t(1),
        });
        let effects = ops.recover(t(100));
        assert_eq!(
            effects,
            vec![OpEffect::Retry {
                id,
                not_before: t(100)
            }],
            "an overdue retry fires from now, not from a stale deadline"
        );
    }

    /// Duplicate completion notices are idempotent: the second Acked for
    /// an operation that already settled changes nothing.
    #[test]
    fn duplicate_completion_is_idempotent() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Acked { id, now: t(1) });
        let mut observed = task_at(t(2));
        observed.assignees = vec!["ana".into()];
        ops.reconcile_observation(&mut observed, t(3));
        assert!(ops.is_empty());
        assert!(
            ops.apply_detached(OpEvent::Acked { id, now: t(3) })
                .is_empty()
        );
        assert!(
            ops.apply_detached(OpEvent::Failed {
                id,
                class: FailureClass::Rejected,
                detail: "late".into(),
                now: t(4),
            })
            .is_empty(),
            "a verdict for a settled operation is inert"
        );
    }

    /// A write the provider accepted as a no-op need not bump the
    /// entity's revision, so no observation can ever be proven to
    /// postdate it. The deadline is what stops that claim painting the
    /// row — and the ledger growing — forever.
    #[test]
    fn an_unconfirmable_claim_settles_on_the_deadline() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::assignees(vec!["ana".into()]),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Acked { id, now: t(1) });

        // The entity's revision never moves past the ack.
        let mut observed = task_at(t(-10));
        ops.reconcile_observation(&mut observed, t(60));
        assert!(!ops.is_empty(), "still inside the deadline");
        assert_eq!(observed.assignees, vec!["ana".to_string()]);

        let mut later = task_at(t(-10));
        ops.reconcile_observation(&mut later, t(0) + SETTLE_DEADLINE);
        assert!(ops.is_empty(), "the claim stops waiting");
        assert!(later.assignees.is_empty(), "the provider's value wins");
    }

    /// An uncertain outcome is settled by the next read of the entity,
    /// whatever it says — that re-read is the answer the reconcile asked
    /// for, and a write whose outcome is unknown has no claim to keep
    /// painting the row.
    #[test]
    fn an_uncertain_claim_settles_on_the_next_read() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::state(TaskState::Closed, Some("Done".into())),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Sent { id });
        ops.apply_detached(OpEvent::Failed {
            id,
            class: FailureClass::Uncertain,
            detail: "timed out".into(),
            now: t(5),
        });
        // The re-fetch comes back with a revision older than the request
        // — it still answers the question the reconcile asked.
        let mut observed = task_at(t(-10));
        observed.state = TaskState::InProgress;
        ops.reconcile_observation(&mut observed, t(6));
        assert!(ops.is_empty());
        assert_eq!(observed.state, TaskState::InProgress);
    }

    /// A rejected write puts the provider's value back on the row at
    /// once. The daemon persists the projection, so leaving the rejected
    /// value there would show a lie until the next poll.
    #[test]
    fn a_rejected_write_restores_the_row_immediately() {
        let mut ops = ProviderOps::default();
        let mut row = task_at(t(-10));
        row.assignees = vec!["bo".into()];

        let id = send_id(&ops.apply(
            OpEvent::Requested {
                task: task_id(),
                desired: DesiredFields::assignees(vec!["ana".into()]),
                now: t(0),
            },
            &mut row,
        ));
        assert_eq!(
            row.assignees,
            vec!["ana".to_string()],
            "intent paints the row"
        );

        ops.apply(
            OpEvent::Failed {
                id,
                class: FailureClass::Rejected,
                detail: "user `ana` not found".into(),
                now: t(1),
            },
            &mut row,
        );
        assert_eq!(
            row.assignees,
            vec!["bo".to_string()],
            "the rejection uncovers what the provider actually holds"
        );
    }

    /// Stacked writes on one field: rejecting the FIRST must leave the
    /// second painting the row, and rejecting the second must fall back
    /// to the provider's value — not to the first write, which never
    /// landed either.
    #[test]
    fn stacked_writes_fall_back_past_each_other_to_the_provider() {
        let mut ops = ProviderOps::default();
        let mut row = task_at(t(-10));
        row.assignees = vec!["bo".into()];

        let first = send_id(&ops.apply(
            OpEvent::Requested {
                task: task_id(),
                desired: DesiredFields::assignees(vec!["ana".into()]),
                now: t(0),
            },
            &mut row,
        ));
        ops.apply(OpEvent::Sent { id: first }, &mut row);
        let second = send_id(&ops.apply(
            OpEvent::Requested {
                task: task_id(),
                desired: DesiredFields::assignees(vec!["cy".into()]),
                now: t(1),
            },
            &mut row,
        ));

        ops.apply(
            OpEvent::Failed {
                id: first,
                class: FailureClass::Rejected,
                detail: "nope".into(),
                now: t(2),
            },
            &mut row,
        );
        assert_eq!(
            row.assignees,
            vec!["cy".to_string()],
            "the live intent keeps the row"
        );

        ops.apply(
            OpEvent::Failed {
                id: second,
                class: FailureClass::Rejected,
                detail: "nope".into(),
                now: t(3),
            },
            &mut row,
        );
        assert_eq!(
            row.assignees,
            vec!["bo".to_string()],
            "falls back past a write that never landed, to the provider"
        );
    }

    /// The baseline follows the provider. After an observation the row
    /// falls back to what THAT read said, not to a value two
    /// observations old.
    #[test]
    fn an_observation_refreshes_the_fallback() {
        let mut ops = ProviderOps::default();
        let mut row = task_at(t(-10));
        row.assignees = vec!["bo".into()];
        let id = send_id(&ops.apply(
            OpEvent::Requested {
                task: task_id(),
                desired: DesiredFields::assignees(vec!["ana".into()]),
                now: t(0),
            },
            &mut row,
        ));
        ops.apply(OpEvent::Sent { id }, &mut row);

        // A poll older than the write shows a third party took it.
        let mut observed = task_at(t(-5));
        observed.assignees = vec!["dee".into()];
        ops.reconcile_observation(&mut observed, t(1));
        assert_eq!(
            observed.assignees,
            vec!["ana".to_string()],
            "the unconfirmed write still paints the row"
        );

        ops.apply(
            OpEvent::Failed {
                id,
                class: FailureClass::Rejected,
                detail: "nope".into(),
                now: t(2),
            },
            &mut observed,
        );
        assert_eq!(
            observed.assignees,
            vec!["dee".to_string()],
            "the fallback is the freshest thing the provider said"
        );
    }

    /// Cancelling an unsent write puts the row back without waiting for
    /// a poll.
    #[test]
    fn cancelling_an_unsent_write_restores_the_row() {
        let mut ops = ProviderOps::default();
        let mut row = task_at(t(-10));
        row.labels = vec![Label::new("bug")];
        ops.apply(
            OpEvent::Requested {
                task: task_id(),
                desired: DesiredFields::labels(vec!["chore".into()]),
                now: t(0),
            },
            &mut row,
        );
        assert_eq!(row.labels[0].name, "chore");

        ops.apply(
            OpEvent::Cancelled {
                task: task_id(),
                fields: [MutationField::Labels].into(),
                now: t(1),
            },
            &mut row,
        );
        assert_eq!(row.labels[0].name, "bug");
    }

    /// A label overlay keeps the color of a label the entity already
    /// carries, so applying an edit doesn't repaint the row grey.
    #[test]
    fn label_overlay_preserves_known_colors() {
        let mut ops = ProviderOps::default();
        ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::labels(vec!["bug".into(), "new".into()]),
            now: t(0),
        });
        let mut task = task_at(t(-10));
        task.labels = vec![Label {
            name: "bug".into(),
            color: "ff0000".into(),
        }];
        ops.overlay(&mut task);
        assert_eq!(task.labels[0].color, "ff0000");
        assert_eq!(task.labels[1].name, "new");
    }

    /// The ledger round-trips through JSON — it is persisted with the
    /// workspace, which is what makes restart recovery a replay.
    #[test]
    fn ledger_round_trips_through_json() {
        let mut ops = ProviderOps::default();
        let id = send_id(&ops.apply_detached(OpEvent::Requested {
            task: task_id(),
            desired: DesiredFields::state(TaskState::Closed, Some("Done".into())),
            now: t(0),
        }));
        ops.apply_detached(OpEvent::Acked { id, now: t(1) });
        let json = serde_json::to_string(&ops).unwrap();
        let back: ProviderOps = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ops);
    }
}
