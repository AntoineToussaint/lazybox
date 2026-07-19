//! Workspace mutation helpers.
//!
//! Every mutation handler in `handlers.rs` open-coded the same shape:
//! load a workspace, do something (often network IO that takes a
//! second or two), then persist + broadcast. The post-IO branch had
//! a subtle race — by the time the IO returned, the main poll loop
//! might have written a fresher copy with new CI / review / mergeable
//! state. Re-using the stale snapshot from the top of the function
//! clobbered the fresh data.
//!
//! `fetch_and_apply` captures the race-safe shape in one place:
//! load → run the IO with the initial snapshot → re-load just before
//! the transform → commit. Callers can't accidentally forget the
//! re-load step.
//!
//! `apply_and_commit` covers the IO-free case (clean_worktrees walks
//! the workspace list locally) so even that path uses the same
//! mutation primitive.

use lazybox_core::{Workspace, WorkspaceKey};

use super::{commit_upsert, load_workspace, report_commit_error};
use crate::ServerConfig;

/// Outcome of a workspace mutation. `Applied` means the workspace
/// was found, the closure ran, and the durable commit succeeded.
/// `Missing` means the workspace vanished mid-mutation — typically
/// because the user pressed `x x` while we were in the
/// middle of an IO call. `Failed` means the transformed state was not
/// committed or broadcast; a retryable store error is emitted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOutcome {
    Applied,
    Missing,
    Failed,
}

impl MutationOutcome {
    pub fn is_applied(self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// Load `key`, apply `transform`, commit. The whole sequence is
/// synchronous so there's no IO window where a concurrent write
/// could race. For mutations that need to run network IO before
/// touching the workspace, see [`fetch_and_apply`].
///
/// Returns [`MutationOutcome::Missing`] when the workspace isn't
/// in the store; the caller can surface a "not found" error.
pub fn apply_and_commit<F>(
    config: &ServerConfig,
    key: &WorkspaceKey,
    transform: F,
) -> MutationOutcome
where
    F: FnOnce(&mut Workspace),
{
    let Some(mut ws) = load_workspace(config, key) else {
        return MutationOutcome::Missing;
    };
    transform(&mut ws);
    match commit_upsert(config, key, ws) {
        Ok(_) => MutationOutcome::Applied,
        Err(error) => {
            report_commit_error(config, "apply workspace mutation", &error);
            MutationOutcome::Failed
        }
    }
}

/// Two-phase mutation: an async `fetch` that takes the initial
/// workspace snapshot for context, then a synchronous `transform`
/// that runs against a *freshly re-loaded* workspace right before
/// the commit. This locks in the race fix that
/// `handle_fetch_pr_details` had to discover the hard way (symptom:
/// PR row stuck on "CI RUN" long after GitHub flipped to SUCCESS,
/// because a 1-2s GraphQL fetch wrote back a stale snapshot over
/// the poll's fresher state).
///
/// `fetch` is allowed to fail — its error type bubbles unchanged.
/// `transform` runs only on `Ok`, only after a successful re-load.
///
/// Returns:
/// - `Ok(Applied)` — fetch + transform + commit ran end-to-end.
/// - `Ok(Missing)` — workspace was gone at either the initial load
///   or the re-load. No commit, no error from `fetch`.
/// - `Err(_)` — `fetch` returned an error. No commit, no transform.
pub async fn fetch_and_apply<T, E, Fut, F, G>(
    config: &ServerConfig,
    key: &WorkspaceKey,
    fetch: F,
    transform: G,
) -> Result<MutationOutcome, E>
where
    F: FnOnce(Workspace) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    G: FnOnce(&mut Workspace, T),
{
    let Some(initial) = load_workspace(config, key) else {
        return Ok(MutationOutcome::Missing);
    };
    let payload = fetch(initial).await?;
    let Some(mut fresh) = load_workspace(config, key) else {
        return Ok(MutationOutcome::Missing);
    };
    transform(&mut fresh, payload);
    Ok(match commit_upsert(config, key, fresh) {
        Ok(_) => MutationOutcome::Applied,
        Err(error) => {
            report_commit_error(config, "apply fetched workspace mutation", &error);
            MutationOutcome::Failed
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;
    use chrono::Utc;
    use lazybox_core::Workspace;
    use lazybox_store::WorkspaceRecord;

    fn seed_workspace(config: &ServerConfig, key: &str) -> WorkspaceKey {
        let wk = WorkspaceKey::new(key);
        let ws = Workspace::empty(wk.clone(), "main", Utc::now());
        let record = WorkspaceRecord {
            key: key.to_string(),
            created_at: ws.created_at,
            workspace_json: Some(serde_json::to_string(&ws).unwrap()),
        };
        config.store.save_workspace(&record).unwrap();
        wk
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_and_commit_returns_applied_when_workspace_exists() {
        let config = ServerConfig::in_memory();
        let key = seed_workspace(&config, "github:o/r#1");

        let outcome = apply_and_commit(&config, &key, |ws| {
            ws.snoozed_until = Some(Utc::now() + chrono::Duration::hours(1));
        });

        assert_eq!(outcome, MutationOutcome::Applied);
        let stored = load_workspace(&config, &key).unwrap();
        assert!(
            stored.snoozed_until.is_some(),
            "transform must be persisted"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_and_commit_returns_missing_when_workspace_absent() {
        let config = ServerConfig::in_memory();
        let key = WorkspaceKey::new("github:o/r#nope");
        let outcome = apply_and_commit(&config, &key, |_| panic!("transform should not run"));
        assert_eq!(outcome, MutationOutcome::Missing);
    }

    /// Regression: the second load is the race fix. We seed v1, the
    /// "fetch" step waits long enough for an out-of-band write to land
    /// v2, and then we assert the transform sees v2 — not v1.
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_and_apply_reloads_workspace_before_transform() {
        let config = ServerConfig::in_memory();
        let key = seed_workspace(&config, "github:o/r#1");

        let config_for_writer = config.clone();
        let key_for_writer = key.clone();

        let outcome: Result<MutationOutcome, std::convert::Infallible> = fetch_and_apply(
            &config,
            &key,
            |initial| async move {
                // While the "fetch" is running, simulate the poll loop
                // writing a fresher copy with a new field set.
                let mut fresher = initial.clone();
                fresher.snoozed_until = Some(Utc::now() + chrono::Duration::hours(2));
                commit_upsert(&config_for_writer, &key_for_writer, fresher).unwrap();
                Ok(42_i32)
            },
            |ws, payload| {
                // The transform must run against the fresher load, so
                // the snooze the writer set above is visible here.
                assert!(
                    ws.snoozed_until.is_some(),
                    "transform must see the post-fetch state, not the initial snapshot",
                );
                assert_eq!(payload, 42);
            },
        )
        .await;
        assert_eq!(outcome.unwrap(), MutationOutcome::Applied);
    }

    /// Workspace deleted during the fetch — transform must NOT run
    /// (otherwise an `x x` during a slow fetch would re-create
    /// the row from a stale snapshot).
    #[tokio::test(flavor = "current_thread")]
    async fn fetch_and_apply_skips_transform_when_workspace_deleted_during_fetch() {
        let config = ServerConfig::in_memory();
        let key = seed_workspace(&config, "github:o/r#1");
        let config_for_deleter = config.clone();
        let key_for_deleter = key.clone();

        let outcome: Result<MutationOutcome, std::convert::Infallible> = fetch_and_apply(
            &config,
            &key,
            |_initial| async move {
                let _ = config_for_deleter.store.delete_workspace(&key_for_deleter);
                Ok(())
            },
            |_ws, _| panic!("transform must not run when the workspace is gone"),
        )
        .await;
        assert_eq!(outcome.unwrap(), MutationOutcome::Missing);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_and_apply_bubbles_fetch_errors_unchanged() {
        let config = ServerConfig::in_memory();
        let key = seed_workspace(&config, "github:o/r#1");

        #[derive(Debug, PartialEq)]
        struct Boom(&'static str);

        let outcome: Result<MutationOutcome, Boom> = fetch_and_apply(
            &config,
            &key,
            |_initial| async { Err(Boom("network down")) },
            |_ws, _: ()| panic!("transform must not run on fetch error"),
        )
        .await;
        assert_eq!(outcome.unwrap_err(), Boom("network down"));
    }
}
