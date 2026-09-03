//! Durable, owner-qualified GitHub fleet claims.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use lazybox_core::{QualifiedWorkingClaim, SessionId, Task, TaskId, Workspace, WorkspaceKey};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::ServerConfig;

const CLAIM_KEY_PREFIX: &str = "terminal-working-claim:";
const MUTATION_TIMEOUT: Duration = Duration::from_secs(20);
/// Transient sync failures (offline, GitHub down, timeouts) re-occur on every
/// 15-minute heartbeat tick; surface at most one retryable notice per
/// workspace/action per hour so an offline laptop is not error spam.
const TRANSIENT_ERROR_DEBOUNCE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum ClaimHolder {
    Pty { backend_key: String },
    Structured { key: String },
}

impl ClaimHolder {
    fn id(&self) -> &str {
        match self {
            Self::Pty { backend_key } => backend_key,
            Self::Structured { key } => key,
        }
    }

    fn storage_key(&self) -> String {
        format!("{CLAIM_KEY_PREFIX}{}", self.id())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkingClaimRecord {
    holder: ClaimHolder,
    workspace_key: WorkspaceKey,
    session_id: Option<SessionId>,
    claim_session: Uuid,
    owner_id: String,
    target: WorkingClaimTarget,
    label: String,
    expires_at: DateTime<Utc>,
    applied: bool,
}

impl WorkingClaimRecord {
    fn new(
        holder: ClaimHolder,
        workspace_key: WorkspaceKey,
        session_id: Option<SessionId>,
        owner_id: String,
        target: WorkingClaimTarget,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        let claim_session = Uuid::new_v4();
        let expires_at = now + ChronoDuration::seconds(lazybox_core::WORKING_CLAIM_TTL_SECS);
        let label =
            lazybox_core::qualified_working_claim_label(&owner_id, claim_session, expires_at)?;
        Some(Self {
            holder,
            workspace_key,
            session_id,
            claim_session,
            owner_id,
            target,
            label,
            expires_at,
            applied: false,
        })
    }

    fn parsed_label(&self) -> Option<QualifiedWorkingClaim> {
        QualifiedWorkingClaim::parse(&self.label)
    }

    fn needs_heartbeat(&self, now: DateTime<Utc>) -> bool {
        !self.applied
            || self.expires_at
                <= now
                    + ChronoDuration::seconds(
                        lazybox_core::WORKING_CLAIM_TTL_SECS
                            - lazybox_core::WORKING_CLAIM_HEARTBEAT_SECS,
                    )
    }

    fn prepare_heartbeat(&mut self, now: DateTime<Utc>) -> bool {
        if !self.needs_heartbeat(now) {
            return false;
        }
        let refresh_at = now
            + ChronoDuration::seconds(
                lazybox_core::WORKING_CLAIM_TTL_SECS - lazybox_core::WORKING_CLAIM_HEARTBEAT_SECS,
            );
        if !self.applied && self.expires_at > refresh_at {
            return true;
        }
        self.expires_at = now + ChronoDuration::seconds(lazybox_core::WORKING_CLAIM_TTL_SECS);
        let Some(label) = lazybox_core::qualified_working_claim_label(
            &self.owner_id,
            self.claim_session,
            self.expires_at,
        ) else {
            return false;
        };
        self.label = label;
        self.applied = false;
        true
    }
}

/// Immutable target retained across issue-to-PR promotion and workspace
/// removal so teardown never guesses from the current headline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkingClaimTarget {
    pub(crate) id: TaskId,
    pub(crate) repo: String,
}

impl WorkingClaimTarget {
    fn from_task(task: &Task) -> Option<Self> {
        Some(Self {
            id: task.id.clone(),
            repo: task.repo.clone()?,
        })
    }
}

pub(crate) fn structured_holder_key() -> String {
    format!("run-{}", Uuid::new_v4().simple())
}

pub(crate) async fn acquire_pty(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    backend_key: &str,
    session_id: Option<SessionId>,
) {
    acquire(
        config,
        workspace_key,
        ClaimHolder::Pty {
            backend_key: backend_key.to_string(),
        },
        session_id,
    )
    .await;
}

pub(crate) async fn acquire_structured(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    holder_key: &str,
    session_id: Option<SessionId>,
) {
    acquire(
        config,
        workspace_key,
        ClaimHolder::Structured {
            key: holder_key.to_string(),
        },
        session_id,
    )
    .await;
}

async fn acquire(
    config: &ServerConfig,
    workspace_key: WorkspaceKey,
    holder: ClaimHolder,
    session_id: Option<SessionId>,
) {
    if !config.working_claims_enabled {
        return;
    }
    let key = holder.storage_key();
    let holder_guard = lock_holder(config, &key).await;
    let mut record = match load_record(config, &key) {
        Ok(Some(record)) => record,
        Ok(None) => {
            let Some(workspace) = load_workspace(config, &workspace_key) else {
                if workspace_key
                    .as_str()
                    .starts_with(&format!("{}-", lazybox_core::GITHUB_SOURCE))
                {
                    emit_error(
                        config,
                        &workspace_key,
                        "apply",
                        "workspace no longer exists",
                    );
                }
                return;
            };
            let Some(task) = workspace
                .primary_task()
                .filter(|task| task.id.source == lazybox_core::GITHUB_SOURCE)
            else {
                return;
            };
            let Some(target) = WorkingClaimTarget::from_task(task) else {
                emit_error(config, &workspace_key, "apply", "task has no repository");
                return;
            };
            let Some(record) = WorkingClaimRecord::new(
                holder,
                workspace_key.clone(),
                session_id,
                config.working_claim_owner_id.clone(),
                target,
                Utc::now(),
            ) else {
                emit_error(config, &workspace_key, "apply", "box identity is malformed");
                return;
            };
            if let Err(error) = persist_record(config, &record) {
                emit_error(config, &record.workspace_key, "persist", &error);
                return;
            }
            record
        }
        Err(error) => {
            emit_error(config, &workspace_key, "recover", &error);
            return;
        }
    };
    let claim_config = config.clone();
    tokio::spawn(async move {
        let _holder_guard = holder_guard;
        heartbeat_record(&claim_config, &mut record, Utc::now()).await;
    });
}

pub(crate) async fn release_pty(config: &ServerConfig, backend_key: &str) {
    release(
        config,
        &ClaimHolder::Pty {
            backend_key: backend_key.to_string(),
        },
    )
    .await;
}

pub(crate) async fn release_structured(config: &ServerConfig, holder_key: &str) {
    release(
        config,
        &ClaimHolder::Structured {
            key: holder_key.to_string(),
        },
    )
    .await;
}

async fn release(config: &ServerConfig, holder: &ClaimHolder) {
    if !config.working_claims_enabled {
        return;
    }
    let key = holder.storage_key();
    let _holder_guard = lock_holder(config, &key).await;
    let record = match load_record(config, &key) {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%key, %error, "working claim provenance could not be recovered");
            return;
        }
    };
    if record.owner_id != config.working_claim_owner_id {
        tracing::warn!(
            workspace = %record.workspace_key,
            owner = %record.owner_id,
            "preserving working claim provenance owned by a different box identity"
        );
        return;
    }
    if sync_remote(config, &record, None).await
        && let Err(error) = config.store.delete_kv(&key)
    {
        emit_error(config, &record.workspace_key, "forget", &error.to_string());
    }
}

async fn heartbeat_record(
    config: &ServerConfig,
    record: &mut WorkingClaimRecord,
    now: DateTime<Utc>,
) {
    if record.owner_id != config.working_claim_owner_id {
        tracing::warn!(
            workspace = %record.workspace_key,
            owner = %record.owner_id,
            "preserving working claim provenance owned by a different box identity"
        );
        return;
    }
    if !record.prepare_heartbeat(now) {
        return;
    }
    if let Err(error) = persist_record(config, record) {
        emit_error(config, &record.workspace_key, "persist", &error);
        return;
    }
    if sync_remote(config, record, Some(&record.label)).await {
        record.applied = true;
        if let Err(error) = persist_record(config, record) {
            emit_error(config, &record.workspace_key, "persist", &error);
        }
    }
}

async fn sync_remote(
    config: &ServerConfig,
    record: &WorkingClaimRecord,
    desired_label: Option<&str>,
) -> bool {
    let Some(identity) = record.parsed_label() else {
        emit_error(
            config,
            &record.workspace_key,
            "synchronize",
            "persisted qualified label is malformed",
        );
        return false;
    };
    let client = match crate::polling::resolve_gh_client_result(config).await {
        Ok(client) => client,
        Err(error) => {
            emit_transient_error(config, &record.workspace_key, "synchronize", &error);
            return false;
        }
    };
    let mutation = client.sync_working_claim_target(
        &record.target.id,
        &record.target.repo,
        desired_label,
        &identity.device,
        &identity.session,
    );
    match tokio::time::timeout(MUTATION_TIMEOUT, mutation).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            emit_transient_error(
                config,
                &record.workspace_key,
                "synchronize",
                &error.to_string(),
            );
            return false;
        }
        Err(_) => {
            // Not necessarily GitHub: this 20s cap (MUTATION_TIMEOUT) is
            // shorter than the client's own 30s PERMIT_WAIT_TIMEOUT, so a
            // request throttled behind lazybox's rate-budget pacing is cut
            // off here before its self-throttle error can surface. Don't
            // blame GitHub for what may be our own backoff (#1218).
            emit_transient_error(
                config,
                &record.workspace_key,
                "synchronize",
                "claim sync timed out after 20s (GitHub slow, or throttled behind lazybox's rate budget)",
            );
            return false;
        }
    }

    let target = record.target.id.clone();
    let desired = desired_label.map(str::to_string);
    let identity_for_projection = identity.clone();
    let outcome =
        crate::polling::apply_and_commit(config, &record.workspace_key, move |workspace| {
            project_identity(
                workspace,
                &target,
                &identity_for_projection,
                desired.as_deref(),
            );
        })
        .await;
    config.poll.wake(true);
    if outcome.is_applied() {
        tracing::info!(
            workspace = %record.workspace_key,
            claimed = desired_label.is_some(),
            device = %identity.device,
            session = %identity.session,
            "synchronized owner-qualified working claim"
        );
    }
    true
}

fn project_identity(
    workspace: &mut Workspace,
    target: &TaskId,
    identity: &QualifiedWorkingClaim,
    desired_label: Option<&str>,
) {
    let Some(task) = workspace.task_by_id_mut(target) else {
        return;
    };
    task.labels.retain(|label| {
        QualifiedWorkingClaim::parse(&label.name).is_none_or(|claim| !claim.same_owner(identity))
    });
    if let Some(label) = desired_label {
        task.labels
            .push(lazybox_core::Label::with_color(label, "fbca04"));
    }
}

pub(crate) fn spawn(config: ServerConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        maintain_once(&config, Utc::now()).await;
        let mut interval = tokio::time::interval(Duration::from_secs(
            lazybox_core::WORKING_CLAIM_HEARTBEAT_SECS as u64,
        ));
        interval.tick().await;
        loop {
            interval.tick().await;
            maintain_once(&config, Utc::now()).await;
        }
    })
}

async fn maintain_once(config: &ServerConfig, now: DateTime<Utc>) {
    if !config.working_claims_enabled {
        return;
    }
    let records = match list_records(config) {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "working claim maintenance could not enumerate provenance");
            return;
        }
    };
    let backend_keys = config.backend.list().await;
    let live_structured = config
        .agent_runs
        .lock()
        .await
        .values()
        .filter_map(|run| run.working_claim_holder.clone())
        .collect::<HashSet<_>>();
    for record in records {
        if record.owner_id != config.working_claim_owner_id {
            // Foreign provenance (box identity changed under this state dir):
            // never mutate the other identity's upstream label, but once the
            // lease is a full TTL past its expiry the local record is inert —
            // upstream cleanup happens through `cleanup_expired`'s
            // exact-label removal — so prune it instead of keeping it
            // forever.
            if record.expires_at + ChronoDuration::seconds(lazybox_core::WORKING_CLAIM_TTL_SECS)
                <= now
            {
                let key = record.holder.storage_key();
                let _holder_guard = lock_holder(config, &key).await;
                if let Err(error) = config.store.delete_kv(&key) {
                    tracing::warn!(%key, %error, "could not prune foreign working claim provenance");
                }
            }
            continue;
        }
        let live = match &record.holder {
            ClaimHolder::Pty { backend_key } => match &backend_keys {
                Ok(keys) => keys.contains(backend_key),
                Err(_) => continue,
            },
            ClaimHolder::Structured { key } => live_structured.contains(key),
        };
        if live {
            heartbeat_holder(config, &record.holder, now).await;
        } else {
            release(config, &record.holder).await;
        }
    }
    cleanup_expired(config, now).await;
    prune_idle_locks(config);
}

/// Drop per-holder lock entries that are neither currently held nor backed by
/// a durable record. Without this the lock map grows one entry per agent ever
/// spawned in this process. A held lock keeps an `Arc` clone alive, so
/// `strong_count > 1` protects in-flight acquire/heartbeat/release cycles.
fn prune_idle_locks(config: &ServerConfig) {
    // Two passes so no SQLite query ever runs inside the lock-map
    // guard (#1237): snapshot the idle candidates, query the store
    // unguarded, then retain against the verdicts. A lock acquired
    // between the passes bumps its strong count and survives the
    // retain regardless of the (stale) store verdict.
    let idle_keys: Vec<String> = {
        let locks = config.working_claim_locks.lock();
        locks
            .iter()
            .filter(|(_, lock)| std::sync::Arc::strong_count(lock) == 1)
            .map(|(key, _)| key.clone())
            .collect()
    };
    if idle_keys.is_empty() {
        return;
    }
    let mut removable: std::collections::HashSet<String> = std::collections::HashSet::new();
    for key in idle_keys {
        if matches!(config.store.get_kv(&key), Ok(None)) {
            removable.insert(key);
        }
    }
    let mut locks = config.working_claim_locks.lock();
    locks.retain(|key, lock| std::sync::Arc::strong_count(lock) > 1 || !removable.contains(key));
}

async fn heartbeat_holder(config: &ServerConfig, holder: &ClaimHolder, now: DateTime<Utc>) {
    let key = holder.storage_key();
    let _holder_guard = lock_holder(config, &key).await;
    let mut record = match load_record(config, &key) {
        Ok(Some(record)) => record,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%key, %error, "working claim heartbeat could not recover provenance");
            return;
        }
    };
    heartbeat_record(config, &mut record, now).await;
}

async fn cleanup_expired(config: &ServerConfig, now: DateTime<Utc>) {
    let records = match crate::store_blocking(&config.store, |store| store.list_workspaces()).await
    {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "working claim TTL cleanup could not list workspaces");
            return;
        }
    };
    for stored in records {
        let Some(json) = stored.workspace_json else {
            continue;
        };
        let Ok(workspace) = Workspace::decode_persisted(&json) else {
            continue;
        };
        let tasks = workspace
            .pr
            .iter()
            .chain(workspace.gh_issues.iter())
            .collect::<Vec<_>>();
        for task in tasks {
            let expired = task.expired_working_claim_labels(now);
            if expired.is_empty() {
                continue;
            }
            let Some(repo) = task.repo.as_deref() else {
                continue;
            };
            let client = match crate::polling::resolve_gh_client_result(config).await {
                Ok(client) => client,
                Err(error) => {
                    emit_transient_error(config, &workspace.key, "expire", &error);
                    return;
                }
            };
            let result = tokio::time::timeout(
                MUTATION_TIMEOUT,
                client.remove_working_claim_labels_target(&task.id, repo, &expired),
            )
            .await;
            if !matches!(result, Ok(Ok(()))) {
                let reason = match result {
                    Ok(Err(error)) => error.to_string(),
                    // See note in sync_remote: a 20s timeout here can be our
                    // own rate-budget pacing, not GitHub (#1218).
                    Err(_) => {
                        "claim expiry timed out after 20s (GitHub slow, or throttled behind lazybox's rate budget)".into()
                    }
                    Ok(Ok(())) => unreachable!(),
                };
                emit_transient_error(config, &workspace.key, "expire", &reason);
                continue;
            }
            let target = task.id.clone();
            let expired_for_projection = expired.clone();
            let _ = crate::polling::apply_and_commit(config, &workspace.key, move |fresh| {
                if let Some(task) = fresh.task_by_id_mut(&target) {
                    task.labels
                        .retain(|label| !expired_for_projection.contains(&label.name));
                }
            })
            .await;
            config.poll.wake(true);
        }
    }
}

fn load_workspace(config: &ServerConfig, key: &WorkspaceKey) -> Option<Workspace> {
    let record = config.store.get_workspace(key).ok()??;
    Workspace::decode_persisted(record.workspace_json.as_deref()?).ok()
}

fn persist_record(config: &ServerConfig, record: &WorkingClaimRecord) -> Result<(), String> {
    let json = serde_json::to_string(record).map_err(|error| error.to_string())?;
    config
        .store
        .set_kv(&record.holder.storage_key(), &json)
        .map_err(|error| error.to_string())
}

fn load_record(config: &ServerConfig, key: &str) -> Result<Option<WorkingClaimRecord>, String> {
    config
        .store
        .get_kv(key)
        .map_err(|error| error.to_string())?
        .map(|json| serde_json::from_str(&json).map_err(|error| error.to_string()))
        .transpose()
}

fn list_records(config: &ServerConfig) -> Result<Vec<WorkingClaimRecord>, String> {
    config
        .store
        .list_kv_prefix(CLAIM_KEY_PREFIX)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(_, json)| serde_json::from_str(&json).map_err(|error| error.to_string()))
        .collect()
}

/// A genuinely permanent failure — bad config, missing workspace, malformed
/// identity. Won't fix itself by waiting, so it surfaces every time.
fn emit_error(config: &ServerConfig, workspace: &WorkspaceKey, action: &str, reason: &str) {
    let message = format!(
        "agent coordination failed: could not {action} working claim on {workspace}: {reason}"
    );
    tracing::warn!(workspace = %workspace, %action, %reason, "working claim synchronization failed");
    let _ = config
        .bus
        .send(lazybox_ipc::Event::provider_error_permanent(
            "claim", message,
        ));
}

/// A transient failure — offline network, GitHub outage, request timeout. The
/// heartbeat retries on its own every 15 minutes, so this is retryable and
/// debounced to one notice per workspace/action per hour.
fn emit_transient_error(
    config: &ServerConfig,
    workspace: &WorkspaceKey,
    action: &str,
    reason: &str,
) {
    tracing::warn!(workspace = %workspace, %action, %reason, "working claim synchronization failed (will retry)");
    let debounce_key = format!("{workspace}:{action}");
    {
        let mut reports = config.working_claim_error_reports.lock();
        let now = std::time::Instant::now();
        if let Some(last) = reports.get(&debounce_key)
            && now.duration_since(*last) < TRANSIENT_ERROR_DEBOUNCE
        {
            return;
        }
        reports.insert(debounce_key, now);
    }
    let message = format!(
        "agent coordination degraded: could not {action} working claim on {workspace}: {reason} (retrying automatically)"
    );
    let _ = config
        .bus
        .send(lazybox_ipc::Event::provider_error_retryable(
            "claim", message,
        ));
}

async fn lock_holder(config: &ServerConfig, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = config
        .working_claim_locks
        .lock()
        .entry(key.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    lock.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn target() -> WorkingClaimTarget {
        WorkingClaimTarget {
            id: TaskId {
                source: lazybox_core::GITHUB_SOURCE.into(),
                key: "owner/repo#42".into(),
            },
            repo: "owner/repo".into(),
        }
    }

    fn record(now: DateTime<Utc>) -> WorkingClaimRecord {
        WorkingClaimRecord::new(
            ClaimHolder::Pty {
                backend_key: "pty-1".into(),
            },
            WorkspaceKey::new("github-owner-repo-42"),
            None,
            "0123456789abcdef0123456789abcdef".into(),
            target(),
            now,
        )
        .unwrap()
    }

    #[test]
    fn durable_intent_replays_the_same_label_after_a_crash() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let pending = record(now);
        let json = serde_json::to_string(&pending).unwrap();
        let recovered: WorkingClaimRecord = serde_json::from_str(&json).unwrap();

        assert!(!recovered.applied);
        assert_eq!(recovered.label, pending.label);
        assert!(recovered.needs_heartbeat(now));
    }

    #[test]
    fn stale_pending_intent_renews_the_same_owner_after_a_long_crash() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let mut pending = record(now);
        let previous = pending.parsed_label().unwrap();

        assert!(pending.prepare_heartbeat(now + ChronoDuration::hours(2)));

        let renewed = pending.parsed_label().unwrap();
        assert!(previous.same_owner(&renewed));
        assert_eq!(
            pending.expires_at,
            now + ChronoDuration::hours(3),
            "a recovered pending mutation must not republish an expired label"
        );
    }

    #[test]
    fn provenance_survives_a_server_config_restart_in_the_real_store_boundary() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let first = crate::ServerConfig::in_memory();
        let claim = record(now);
        let key = claim.holder.storage_key();
        persist_record(&first, &claim).unwrap();

        let restarted = crate::ServerConfig::with_store(first.store.clone());
        assert_eq!(load_record(&restarted, &key).unwrap(), Some(claim));
    }

    #[tokio::test]
    async fn a_new_box_identity_never_releases_the_previous_devices_provenance() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let mut config = crate::ServerConfig::in_memory();
        config.working_claims_enabled = true;
        let claim = record(now);
        let key = claim.holder.storage_key();
        persist_record(&config, &claim).unwrap();

        release_pty(&config, "pty-1").await;

        assert_eq!(load_record(&config, &key).unwrap(), Some(claim));
    }

    #[tokio::test]
    async fn missing_github_workspace_emits_a_visible_claim_error() {
        let mut config = crate::ServerConfig::in_memory();
        config.working_claims_enabled = true;
        let mut events = config.bus.subscribe();

        acquire_pty(
            &config,
            WorkspaceKey::new("github-owner-repo-42"),
            "pty-1",
            None,
        )
        .await;

        match events.recv().await.unwrap() {
            lazybox_ipc::Event::ProviderError {
                source,
                kind,
                message,
                ..
            } => {
                assert_eq!(source, "claim");
                assert_eq!(kind, "permanent");
                assert!(message.contains("workspace no longer exists"), "{message}");
            }
            event => panic!("expected a visible claim error, got {event:?}"),
        }
    }

    #[test]
    fn heartbeat_waits_fifteen_minutes_then_extends_the_one_hour_ttl() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let mut claim = record(now);
        claim.applied = true;
        assert!(!claim.prepare_heartbeat(now + ChronoDuration::minutes(14)));

        let previous = claim.label.clone();
        let beat = now + ChronoDuration::minutes(15);
        assert!(claim.prepare_heartbeat(beat));
        assert_ne!(claim.label, previous);
        assert_eq!(claim.expires_at, beat + ChronoDuration::hours(1));
        assert!(!claim.applied);
    }

    #[test]
    fn distinct_devices_and_sessions_never_share_an_upstream_identity() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let first = record(now);
        let mut second = record(now);
        second.owner_id = "fedcba9876543210fedcba9876543210".into();
        second.claim_session = Uuid::from_u128(2);
        second.applied = true;
        assert!(second.prepare_heartbeat(now + ChronoDuration::minutes(15)));

        let first = first.parsed_label().unwrap();
        let second = second.parsed_label().unwrap();
        assert!(!first.same_owner(&second));
    }

    // ── Crash-journey coverage: maintenance against a canned GitHub ──
    //
    // `maintain_once` is the crash story — dead-holder release, live-holder
    // heartbeat, and foreign-expired cleanup all run through it. These tests
    // drive it end-to-end with the mock session backend, the memory store,
    // and a recording HTTP server standing in for api.github.com.

    /// Serve `responses[i]` (JSON, 200 OK) for the i-th request and record
    /// each raw request. `Connection: close` forces one connection per
    /// request so ordering is deterministic.
    async fn spawn_recording_gh_server(
        responses: Vec<&'static str>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    continue;
                };
                let body = responses[served.min(responses.len() - 1)];
                served += 1;
                let requests = requests.clone();
                let mut buf = [0u8; 8192];
                let read = sock.read(&mut buf).await.unwrap_or(0);
                requests
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..read]).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    fn label_json(name: &str) -> String {
        format!(
            r#"{{"id":1,"node_id":"LA_1","url":"https://api.github.test/repos/owner/repo/labels/x","name":"{name}","description":null,"color":"fbca04","default":false}}"#
        )
    }

    /// A full GitHub PR task for seeding stored workspaces.
    fn github_task(key: &str) -> Task {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        Task {
            author: String::new(),
            id: TaskId {
                source: lazybox_core::GITHUB_SOURCE.into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{path}/pull/{num}"),
            repo: Some(path.to_string()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    /// A claim record owned by *this* config's box identity, so release and
    /// heartbeat treat it as ours.
    fn owned_record(
        config: &crate::ServerConfig,
        backend_key: &str,
        now: DateTime<Utc>,
    ) -> WorkingClaimRecord {
        WorkingClaimRecord::new(
            ClaimHolder::Pty {
                backend_key: backend_key.into(),
            },
            WorkspaceKey::new("github-owner-repo-42"),
            None,
            config.working_claim_owner_id.clone(),
            target(),
            now,
        )
        .unwrap()
    }

    async fn config_with_recorded_github(
        responses: Vec<&'static str>,
    ) -> (
        crate::ServerConfig,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let mut config = crate::ServerConfig::in_memory();
        config.working_claims_enabled = true;
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_uri = spawn_recording_gh_server(responses, requests.clone()).await;
        config.poll.cache_gh_client(
            lazybox_gh::GhClient::stub_with_base_uri_for_tests(&base_uri).unwrap(),
        );
        (config, requests)
    }

    #[tokio::test]
    async fn maintenance_releases_a_dead_holders_claim_and_its_label_definition() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        // Leak the responses vec builder into 'static via Box::leak-free
        // approach: build after the record exists so the list response can
        // carry the exact persisted label.
        let mut config = crate::ServerConfig::in_memory();
        config.working_claims_enabled = true;
        let claim = owned_record(&config, "pty-dead", now);
        let key = claim.holder.storage_key();
        persist_record(&config, &claim).unwrap();

        let attached: &'static str =
            Box::leak(format!("[{}]", label_json(&claim.label)).into_boxed_str());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_uri = spawn_recording_gh_server(vec![attached, "{}"], requests.clone()).await;
        config.poll.cache_gh_client(
            lazybox_gh::GhClient::stub_with_base_uri_for_tests(&base_uri).unwrap(),
        );

        // No live PTY backend session and no structured run owns the holder,
        // so maintenance must release: clear the upstream label *definition*
        // and forget the durable intent.
        maintain_once(&config, now).await;

        assert_eq!(
            load_record(&config, &key).unwrap(),
            None,
            "a dead holder's durable claim must be forgotten after release"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "one list plus one delete: {requests:?}");
        assert!(requests[0].starts_with("GET "), "{}", requests[0]);
        // Repo-level definition delete, not an issue-scoped detach — the
        // detach-only variant leaks one dead label per agent spawn into the
        // repo's label picker.
        assert!(requests[1].starts_with("DELETE "), "{}", requests[1]);
        assert!(!requests[1].contains("/issues/"), "{}", requests[1]);
    }

    #[tokio::test]
    async fn maintenance_heartbeats_a_live_holder_and_extends_the_lease() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let mut config = crate::ServerConfig::in_memory();
        config.working_claims_enabled = true;
        // A real (mock-backend) session makes the holder live.
        let backend_key = config
            .backend
            .spawn(&["sh".into()], None, &[], "claim-test")
            .await
            .unwrap();
        let mut claim = owned_record(&config, &backend_key, now);
        claim.applied = true;
        let key = claim.holder.storage_key();
        let old_label = claim.label.clone();
        persist_record(&config, &claim).unwrap();

        let attached: &'static str =
            Box::leak(format!("[{}]", label_json(&old_label)).into_boxed_str());
        let renamed: &'static str = Box::leak(label_json("renamed").into_boxed_str());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // list → rename (update_label) → attach (add_labels) → delete stale.
        let base_uri =
            spawn_recording_gh_server(vec![attached, renamed, "[]", "{}"], requests.clone()).await;
        config.poll.cache_gh_client(
            lazybox_gh::GhClient::stub_with_base_uri_for_tests(&base_uri).unwrap(),
        );

        let beat = now + ChronoDuration::minutes(20);
        maintain_once(&config, beat).await;

        let renewed = load_record(&config, &key)
            .unwrap()
            .expect("a live holder's claim must survive maintenance");
        assert!(renewed.applied, "the renewed lease must be marked applied");
        assert_eq!(
            renewed.expires_at,
            beat + ChronoDuration::seconds(lazybox_core::WORKING_CLAIM_TTL_SECS),
            "the heartbeat must extend the lease from the beat time"
        );
        assert_ne!(
            renewed.label, old_label,
            "the label must carry the new expiry"
        );
        assert!(
            claim
                .parsed_label()
                .unwrap()
                .same_owner(&renewed.parsed_label().unwrap()),
            "a heartbeat must never change the claim's owner identity"
        );
    }

    #[tokio::test]
    async fn maintenance_removes_a_foreign_expired_label_but_not_live_or_legacy_ones() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let expired = lazybox_core::qualified_working_claim_label(
            "fedcba9876543210fedcba9876543210",
            Uuid::from_u128(7),
            now - ChronoDuration::hours(2),
        )
        .unwrap();
        let live = lazybox_core::qualified_working_claim_label(
            "abcdefabcdefabcdefabcdefabcdefab",
            Uuid::from_u128(8),
            now + ChronoDuration::hours(1),
        )
        .unwrap();
        let mut task = github_task("owner/repo#42");
        task.labels = vec![
            lazybox_core::Label::new(expired.clone()),
            lazybox_core::Label::new(live.clone()),
            lazybox_core::Label::new(lazybox_core::WORKING_LABEL_NAME),
        ];
        let workspace = Workspace::from_task(task, now);
        let workspace_key = workspace.key.clone();

        let (config, requests) = config_with_recorded_github(vec!["{}"]).await;
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: workspace_key.as_str().to_string(),
                created_at: now,
                workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
            })
            .unwrap();

        maintain_once(&config, now).await;

        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "exactly the expired label is cleaned: {requests:?}"
        );
        assert!(requests[0].starts_with("DELETE "), "{}", requests[0]);
        assert!(!requests[0].contains("/issues/"), "{}", requests[0]);
        assert!(
            requests[0].contains("fedcba9876543210fedc"),
            "{}",
            requests[0]
        );
        drop(requests);

        let stored = config
            .store
            .get_workspace(&workspace_key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap();
        let projected = Workspace::decode_persisted(&stored).unwrap();
        let names: Vec<_> = projected
            .primary_task()
            .unwrap()
            .labels
            .iter()
            .map(|label| label.name.clone())
            .collect();
        assert!(!names.contains(&expired), "expired label projected away");
        assert!(names.contains(&live), "a live foreign owner is untouched");
        assert!(
            names.contains(&lazybox_core::WORKING_LABEL_NAME.to_string()),
            "the legacy label is preserved, never adopted or expired"
        );
    }

    #[tokio::test]
    async fn maintenance_prunes_foreign_provenance_only_after_a_full_ttl_grace() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let (config, requests) = config_with_recorded_github(vec!["{}"]).await;
        let mut foreign = owned_record(&config, "pty-foreign", now);
        foreign.owner_id = "fedcba9876543210fedcba9876543210".into();
        let key = foreign.holder.storage_key();
        persist_record(&config, &foreign).unwrap();

        // Within expiry + one TTL of grace: preserved.
        maintain_once(&config, now + ChronoDuration::hours(1)).await;
        assert!(
            load_record(&config, &key).unwrap().is_some(),
            "foreign provenance is preserved while its lease could still matter"
        );

        // A full TTL past expiry: inert, pruned locally without any upstream
        // mutation against the other identity's label.
        maintain_once(&config, now + ChronoDuration::hours(2)).await;
        assert_eq!(load_record(&config, &key).unwrap(), None);
        assert!(
            requests.lock().unwrap().is_empty(),
            "pruning foreign provenance must never touch GitHub"
        );
    }

    #[tokio::test]
    async fn idle_holder_locks_are_pruned_with_their_records() {
        let config = crate::ServerConfig::in_memory();
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let recorded = owned_record(&config, "pty-live", now);
        persist_record(&config, &recorded).unwrap();
        let recorded_key = recorded.holder.storage_key();

        let _held_guard = lock_holder(&config, "terminal-working-claim:held").await;
        let _ = lock_holder(&config, &recorded_key).await;
        let _ = lock_holder(&config, "terminal-working-claim:stale").await;

        prune_idle_locks(&config);

        let locks = config.working_claim_locks.lock();
        assert!(
            locks.contains_key("terminal-working-claim:held"),
            "a held lock must survive pruning even without a record"
        );
        assert!(
            locks.contains_key(&recorded_key),
            "a lock backed by a durable record must survive pruning"
        );
        assert!(
            !locks.contains_key("terminal-working-claim:stale"),
            "an idle, record-less lock must be pruned"
        );
    }

    #[tokio::test]
    async fn transient_sync_failures_are_retryable_and_debounced() {
        let config = crate::ServerConfig::in_memory();
        let mut events = config.bus.subscribe();
        let workspace = WorkspaceKey::new("github-owner-repo-42");

        emit_transient_error(&config, &workspace, "synchronize", "offline");
        emit_transient_error(&config, &workspace, "synchronize", "offline");

        match events.try_recv().unwrap() {
            lazybox_ipc::Event::ProviderError { kind, message, .. } => {
                assert_eq!(
                    kind, "retryable",
                    "heartbeat failures self-heal — never permanent"
                );
                assert!(message.contains("retrying automatically"), "{message}");
            }
            event => panic!("expected a retryable claim notice, got {event:?}"),
        }
        assert!(
            events.try_recv().is_err(),
            "the second failure within the debounce window must stay quiet"
        );
    }
}
