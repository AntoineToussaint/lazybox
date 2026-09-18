//! Daemon coordinator for the shared provider state machine (#1736).
//!
//! [`lazybox_core::ProviderOps`] decides *what* should happen; this
//! module is the only thing that makes it happen. Every field mutation —
//! whichever client, command or automation asked for it — enters through
//! [`request`], so there is one owner of in-flight provider intent rather
//! than one per call site.
//!
//! The loop is deliberately boring:
//!
//! 1. feed the event to the reducer inside the workspace lock;
//! 2. **persist** the transition (the commit also broadcasts the row, so
//!    the UI shows the user's intent immediately — and authoritatively,
//!    with no client-side optimism to contradict the daemon later);
//! 3. run whatever effects the reducer asked for, feeding each outcome
//!    back in as the next event.
//!
//! Persisting before the effect is what makes a restart recoverable: an
//! operation on disk in `Accepted` provably never reached the provider,
//! and one in `Sent` provably may have. [`recover`] replays that
//! distinction at startup.
//!
//! No exactly-once claim is made or possible. A crash between the send
//! and its acknowledgement leaves an operation whose remote outcome is
//! unknown, and the answer is a targeted re-read, not a replay.

use std::collections::VecDeque;

use chrono::Utc;
use lazybox_core::{
    DesiredFields, FailureClass, OpEffect, OpEvent, OperationId, ProviderError, TaskId, Workspace,
    WorkspaceKey,
};
use lazybox_ipc::Event;

use super::handlers::{
    ProviderHandle, build_provider_for_workspace, github_target, workspace_source,
};
use super::{apply_and_commit, load_workspace};
use crate::ServerConfig;

/// What a caller should tell the user once a request has run its course.
///
/// [`RequestOutcome::Quiet`] is the interesting one: it covers an empty
/// request, a workspace with nothing to mutate, and — the case this
/// machinery exists for — a verdict that belongs to intent the user has
/// already replaced or withdrawn. Such a verdict must not reach the
/// screen, because the fields it speaks for are not the ones on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOutcome {
    Acked,
    Failed(String),
    Quiet,
}

/// Ask the provider to hold `desired` on the workspace's primary task.
///
/// `source` labels the operation for the humanized failure message
/// (`"assignees"`, `"labels"`, …). Surfacing the outcome is the caller's
/// job: each command has its own notice shape, and the coordinator is not
/// where that belongs.
pub async fn request(
    config: &ServerConfig,
    key: &WorkspaceKey,
    desired: DesiredFields,
    source: &'static str,
) -> RequestOutcome {
    if desired.is_empty() {
        return RequestOutcome::Quiet;
    }
    let Some(task) =
        load_workspace(config, key).and_then(|ws| ws.primary_task().map(|t| t.id.clone()))
    else {
        return RequestOutcome::Failed(format!("workspace {key} has no task to mutate"));
    };
    let desired = narrow_to_provider(key, desired);
    let effects = transition(
        config,
        key,
        &task,
        OpEvent::Requested {
            task: task.clone(),
            desired,
            now: Utc::now(),
        },
    )
    .await;
    drive(config, key, source, effects).await
}

/// Narrow requested values to what this workspace's provider can hold,
/// so the intent the ledger records — and paints on the row — is what the
/// write will actually do.
///
/// The rule lives in the provider crate; routing to it is all the daemon
/// does. Reading the prefix rather than building a client keeps this off
/// the credential path, so accepting a request stays local and instant.
fn narrow_to_provider(key: &WorkspaceKey, mut desired: DesiredFields) -> DesiredFields {
    if workspace_source(key) == lazybox_linear::SOURCE
        && let Some(logins) = desired.assignees.take()
    {
        desired.assignees = Some(lazybox_linear::narrow_assignees(&logins));
    }
    desired
}

/// Withdraw intent over `fields` without replacing it. An operation that
/// never left the daemon is dropped; one already on the wire cannot be
/// unsent, so its real outcome is read back instead.
pub async fn cancel(
    config: &ServerConfig,
    key: &WorkspaceKey,
    fields: std::collections::BTreeSet<lazybox_core::MutationField>,
    source: &'static str,
) {
    let Some(task) =
        load_workspace(config, key).and_then(|ws| ws.primary_task().map(|t| t.id.clone()))
    else {
        return;
    };
    let effects = transition(
        config,
        key,
        &task,
        OpEvent::Cancelled {
            task: task.clone(),
            fields,
            now: Utc::now(),
        },
    )
    .await;
    let _ = drive(config, key, source, effects).await;
}

/// Surface a request's outcome on the shared provider-error channel —
/// the default for commands with no notice shape of their own.
pub fn report(config: &ServerConfig, source: &'static str, outcome: RequestOutcome) {
    if let RequestOutcome::Failed(message) = outcome {
        let _ = config
            .bus
            .send(Event::provider_error_retryable(source, message));
    }
}

/// Re-adopt every workspace's persisted ledger at daemon startup.
///
/// An operation we had accepted but not sent is issued for the first
/// time; one that was in flight when the process died has an unknown
/// remote outcome and is reconciled by a re-read before anything else
/// touches it.
pub async fn recover(config: &ServerConfig) {
    let keys: Vec<WorkspaceKey> = config
        .store
        .list_workspaces()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| {
            let ws = Workspace::decode_persisted(record.workspace_json.as_deref()?).ok()?;
            (!ws.provider_ops.is_empty()).then_some(ws.key)
        })
        .collect();
    for key in keys {
        // Recovery re-issues work; it drops nothing, so no row changes and
        // the ledger's own phase edits are what this commit persists.
        let mut effects = Vec::new();
        apply_and_commit(config, &key, |ws| {
            effects = ws.provider_ops.recover(Utc::now());
        })
        .await;
        if !effects.is_empty() {
            tracing::info!(
                workspace = %key,
                effects = effects.len(),
                "provider-ops: resuming in-flight intent after restart"
            );
        }
        let _ = drive(config, &key, "provider", effects).await;
    }
}

/// Feed one event to the reducer under the workspace lock and persist the
/// result before the caller runs any effect.
///
/// `task` is the entity the event concerns: the reducer re-projects it in
/// the same step, so the row this commit persists **and broadcasts**
/// already carries the user's intent — or, for a claim that just ended
/// without landing, already has the provider's value back.
async fn transition(
    config: &ServerConfig,
    key: &WorkspaceKey,
    task: &TaskId,
    event: OpEvent,
) -> Vec<OpEffect> {
    let mut effects = Vec::new();
    let outcome = apply_and_commit(config, key, |ws| {
        let mut ops = std::mem::take(&mut ws.provider_ops);
        if let Some(entity) = ws.task_by_id_mut(task) {
            effects = ops.apply(event, entity);
        }
        ws.provider_ops = ops;
    })
    .await;
    // A transition we could not persist must not be acted on: the effect
    // would run against state no restart could reconstruct.
    if outcome.is_applied() {
        effects
    } else {
        tracing::warn!(
            workspace = %key,
            outcome = ?outcome,
            "provider-ops: dropping effects for an unpersisted transition"
        );
        Vec::new()
    }
}

/// Run the reducer's effects, feeding every outcome back through it, and
/// return what the caller should say.
async fn drive(
    config: &ServerConfig,
    key: &WorkspaceKey,
    source: &'static str,
    effects: Vec<OpEffect>,
) -> RequestOutcome {
    let mut outcome = RequestOutcome::Quiet;
    let mut queue: VecDeque<OpEffect> = effects.into();
    while let Some(effect) = queue.pop_front() {
        match effect {
            OpEffect::Send(id) => {
                let (acked, next) = send(config, key, source, id).await;
                if acked {
                    outcome = RequestOutcome::Acked;
                }
                queue.extend(next);
            }
            OpEffect::Reconcile(id) => reconcile(config, key, id).await,
            OpEffect::Retry { id, not_before } => {
                let wait = (not_before - Utc::now()).to_std().unwrap_or_default();
                tokio::time::sleep(wait).await;
                let (acked, next) = send(config, key, source, id).await;
                if acked {
                    outcome = RequestOutcome::Acked;
                }
                queue.extend(next);
            }
            // The reducer only emits this for an operation that still
            // owns its fields, so reaching here means the failure is the
            // user's to see.
            OpEffect::Report { message, .. } => outcome = RequestOutcome::Failed(message),
            OpEffect::Settled(id) => {
                tracing::debug!(workspace = %key, %id, "provider-ops: settled");
            }
        }
    }
    outcome
}

/// Issue one operation's write and fold its outcome back in.
async fn send(
    config: &ServerConfig,
    key: &WorkspaceKey,
    source: &'static str,
    id: OperationId,
) -> (bool, Vec<OpEffect>) {
    let Some(ws) = load_workspace(config, key) else {
        return (false, Vec::new());
    };
    // The operation may have settled while this effect waited out a
    // backoff — a fresh observation, a cancellation.
    let Some(op) = ws.provider_ops.get(id).cloned() else {
        return (false, Vec::new());
    };
    let provider = match build_provider_for_workspace(config, key).await {
        Ok(provider) => provider,
        Err(detail) => {
            let effects = transition(
                config,
                key,
                &op.task,
                OpEvent::Failed {
                    id,
                    class: FailureClass::Rejected,
                    detail,
                    now: Utc::now(),
                },
            )
            .await;
            return (false, effects);
        }
    };
    // Crossing the "may have mutated remote state" line is itself a
    // persisted transition, so a crash on the next line is recoverable as
    // "unknown" rather than mistaken for "never sent".
    transition(config, key, &op.task, OpEvent::Sent { id }).await;

    let event = match dispatch(&provider, &ws, &op.desired).await {
        Ok(()) => OpEvent::Acked {
            id,
            now: Utc::now(),
        },
        Err(error) => {
            tracing::warn!(workspace = %key, %id, "provider-ops: {source} write failed: {error:?}");
            OpEvent::Failed {
                id,
                class: classify(&error),
                detail: super::handlers::humanize_mutation_failure(source, &error),
                now: Utc::now(),
            }
        }
    };
    let acked = matches!(event, OpEvent::Acked { .. });
    let effects = transition(config, key, &op.task, event).await;
    if acked {
        // Pull the entity forward so the write is confirmed from the
        // provider within a round-trip instead of a poll interval.
        config.poll.wake(true);
    }
    (acked, effects)
}

/// Apply the desired fields through the provider.
///
/// Every write here is *absolute* — it sets a value rather than applying
/// a delta — which is what lets a transient failure be replayed without
/// reconciling first.
async fn dispatch(
    provider: &ProviderHandle,
    ws: &Workspace,
    desired: &DesiredFields,
) -> Result<(), ProviderError> {
    if let Some(logins) = &desired.assignees {
        provider.set_assignees(ws, logins).await?;
    }
    if let Some(names) = &desired.labels {
        provider.set_labels(ws, names).await?;
    }
    if let Some(logins) = &desired.reviewers {
        provider.request_reviewers(ws, logins).await?;
    }
    if desired.state.is_some() {
        // The destination is the provider's business: GitHub closes the
        // issue, Linear moves it to a cancelled workflow state resolved
        // from the issue's own team. Core carries the canonical state and
        // the provider's own label for it; it never enumerates either
        // provider's statuses.
        provider.close_issue(ws).await?;
    }
    Ok(())
}

/// How the local side should treat a failed write.
///
/// A rate-limited or transport failure can be replayed; anything else is
/// the provider saying no. There is no *uncertain* verdict here: a
/// completed request always came back one way or the other. Uncertainty
/// enters only where the answer genuinely never arrived — a restart
/// across the send, which [`recover`] raises.
fn classify(error: &ProviderError) -> FailureClass {
    if error.is_retryable() {
        FailureClass::Transient
    } else {
        FailureClass::Rejected
    }
}

/// Re-read the entity an operation targeted and ingest it, so the
/// observation — not a guess — decides what the write did.
async fn reconcile(config: &ServerConfig, key: &WorkspaceKey, id: OperationId) {
    let Some(ws) = load_workspace(config, key) else {
        return;
    };
    let Some(op) = ws.provider_ops.get(id).cloned() else {
        return;
    };
    match refetch(config, &ws, &op.task).await {
        Ok(Some(task)) => super::upsert(config, task).await,
        // The entity is gone (deleted, moved out of scope, or no longer
        // visible to this token). Nothing to reconcile against, and the
        // claim settles on its deadline rather than painting a row whose
        // subject no longer exists.
        Ok(None) => tracing::info!(
            workspace = %key, %id, task = %op.task,
            "provider-ops: reconcile found no entity"
        ),
        // A failed re-read leaves the operation pending on purpose: the
        // regular poll delivers an observation that settles it, and
        // settlement is capped by `SETTLE_DEADLINE` either way.
        Err(error) => tracing::warn!(
            workspace = %key, %id, "provider-ops: reconcile re-read failed: {error:?}"
        ),
    }
}

/// Targeted single-entity re-read. Deliberately not the `g s` sweep: the
/// question is what happened to *this* entity, and answering it should
/// cost one request.
async fn refetch(
    config: &ServerConfig,
    ws: &Workspace,
    id: &TaskId,
) -> Result<Option<lazybox_core::Task>, ProviderError> {
    let Some(task) = ws.task_by_id(id) else {
        return Ok(None);
    };
    match build_provider_for_workspace(config, &ws.key).await {
        Ok(ProviderHandle::Github(client)) => {
            let Some((owner, repo, number)) = github_target(task) else {
                return Ok(None);
            };
            if task.is_pr() {
                client
                    .fetch_single_pr_interactive(&owner, &repo, number)
                    .await
                    .map_err(Into::into)
            } else {
                client
                    .fetch_single_issue_interactive(&owner, &repo, number)
                    .await
                    .map_err(Into::into)
            }
        }
        Ok(ProviderHandle::Linear(client)) => {
            let Some(node_id) = task.node_id.as_deref() else {
                return Ok(None);
            };
            client.fetch_issue_by_id(node_id).await.map_err(Into::into)
        }
        Err(detail) => Err(ProviderError::permanent("provider", detail)),
    }
}
