//! # lazybox-gh
//!
//! GitHub event provider for lazybox. Uses a single GraphQL query per poll
//! cycle to fetch all PRs with comments, threads, and review status.

mod client;
mod graphql;
pub mod mentions;
mod notifications;
pub mod rate_budget;

pub use client::{GhClient, SelectedFetchCoverage, SelectedFetchOutcome, credential_fingerprint};
pub use graphql::PrDetails;
pub use mentions::{LazyboxMention, MentionSource, parse_label_directive, scan_issue};
pub use notifications::{
    NotificationEntry, NotificationTarget, NotificationTargetKind, NotificationsPoll,
    NotificationsSnapshot,
};
pub use rate_budget::{AcquireError, RateBudget, RemoteRateLimit, Snapshot as RateSnapshot};

use lazybox_auth::{CommandProvider, CredentialChain, EnvProvider};
use lazybox_core::{ProviderError, Scope, ScopeSource};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Workspace-key prefix and credential scope this provider owns.
/// Workspaces from GitHub are keyed `"github-<owner>-<repo>-<n>"`;
/// `build_provider_for_workspace` routes on `split_once('-').0`.
/// Using a single constant keeps the prefix authoritative — both the
/// router AND the credential resolve scope read it from here.
pub const SOURCE: &str = "github";

/// Credential chain GitHub uses. Tried in order:
/// `LAZYBOX_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `gh auth
/// token`. The lazybox-specific variable gives the daemon a token
/// that spawned agents and interactive `gh` do not automatically
/// consume. The polling poller, mutation router, setup wizard's scope
/// source, and fetch-PR-details handler all build clients from this
/// chain.
pub fn credential_chain() -> CredentialChain {
    CredentialChain::new()
        .with(EnvProvider::new("LAZYBOX_GITHUB_TOKEN"))
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dedicated_daemon_token_precedes_standard_gh_token() {
        let dedicated_before = std::env::var_os("LAZYBOX_GITHUB_TOKEN");
        let gh_before = std::env::var_os("GH_TOKEN");
        unsafe {
            std::env::set_var("LAZYBOX_GITHUB_TOKEN", "dedicated-test-token");
            std::env::set_var("GH_TOKEN", "shared-test-token");
        }

        let result = credential_chain().resolve(SOURCE).await;

        unsafe {
            match dedicated_before {
                Some(value) => std::env::set_var("LAZYBOX_GITHUB_TOKEN", value),
                None => std::env::remove_var("LAZYBOX_GITHUB_TOKEN"),
            }
            match gh_before {
                Some(value) => std::env::set_var("GH_TOKEN", value),
                None => std::env::remove_var("GH_TOKEN"),
            }
        }
        let credential = result.expect("dedicated token resolves");
        assert_eq!(credential.token(), "dedicated-test-token");
        assert_eq!(credential.source, "env:LAZYBOX_GITHUB_TOKEN");
    }
}

/// `ScopeSource` adapter over [`GhClient`]. Lets the setup screen
/// render its picker against any provider via `dyn ScopeSource`
/// without leaking GitHub-specific types.
///
/// Constructed by the daemon at setup time from an authenticated
/// `GhClient`; tests use `lazybox_core::MockScopeSource` instead so
/// no real token is needed.
pub struct GhScopes {
    client: Arc<GhClient>,
}

impl GhScopes {
    pub fn new(client: Arc<GhClient>) -> Self {
        Self { client }
    }
}

impl ScopeSource for GhScopes {
    fn provider_id(&self) -> &str {
        "github"
    }

    fn list_scopes<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Scope>, ProviderError>> + Send + 'a>> {
        Box::pin(async move { self.client.list_scopes().await.map_err(Into::into) })
    }

    fn list_children<'a>(
        &'a self,
        parent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Scope>, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            self.client
                .list_repos_in_org(parent_id)
                .await
                .map_err(Into::into)
        })
    }
}
