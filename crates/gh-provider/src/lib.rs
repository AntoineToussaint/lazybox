//! # pilot-gh
//!
//! GitHub event provider for pilot. Uses a single GraphQL query per poll
//! cycle to fetch all PRs with comments, threads, and review status.

mod client;
mod graphql;
mod poller;
pub mod rate_budget;

pub use client::GhClient;
pub use graphql::PrDetails;
pub use poller::GhPoller;
pub use rate_budget::{AcquireError, RateBudget, RemoteRateLimit, Snapshot as RateSnapshot};

use pilot_auth::{CommandProvider, CredentialChain, EnvProvider};
use pilot_core::{ProviderError, Scope, ScopeSource};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Workspace-key prefix and credential scope this provider owns.
/// Workspaces from GitHub are keyed `"github-<owner>-<repo>-<n>"`;
/// `build_provider_for_workspace` routes on `split_once('-').0`.
/// Using a single constant keeps the prefix authoritative — both the
/// router AND the credential resolve scope read it from here.
pub const SOURCE: &str = "github";

/// Credential chain GitHub uses. Tried in order: `GH_TOKEN` env,
/// `GITHUB_TOKEN` env, `gh auth token`. The polling poller, the
/// mutation router, the setup wizard's scope source, and the
/// fetch-PR-details handler all build clients from this chain — keep
/// it in one place so a future addition (Keychain, Vault) lands
/// everywhere at once.
pub fn credential_chain() -> CredentialChain {
    CredentialChain::new()
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]))
}

/// `ScopeSource` adapter over [`GhClient`]. Lets the setup screen
/// render its picker against any provider via `dyn ScopeSource`
/// without leaking GitHub-specific types.
///
/// Constructed by the daemon at setup time from an authenticated
/// `GhClient`; tests use `pilot_core::MockScopeSource` instead so
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
