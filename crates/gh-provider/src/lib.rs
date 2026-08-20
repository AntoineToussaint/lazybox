//! # lazybox-gh
//!
//! GitHub event provider for lazybox. Uses a single GraphQL query per poll
//! cycle to fetch all PRs with comments, threads, and review status.

mod client;
mod graphql;
pub mod mentions;
mod notifications;
pub mod oauth;
pub mod rate_budget;

pub use client::{
    BackgroundSweepForecast, GhClient, HotFetch, SelectedFetchOutcome, credential_fingerprint,
};
pub use graphql::PrDetails;
pub use mentions::{LazyboxMention, MentionSource, parse_label_directive, scan_issue};
pub use notifications::{
    NotificationEntry, NotificationTarget, NotificationTargetKind, NotificationsPoll,
    NotificationsSnapshot, SyncCursors,
};
pub use rate_budget::{
    AcquireError, ApiResource, BackgroundPlan, PersistedRateState, RateBudget, RemoteRateLimit,
    RequestPriority, Snapshot as RateSnapshot,
};

use lazybox_auth::{CommandProvider, CredentialChain, EnvProvider};
use lazybox_core::{ProviderError, Scope, ScopeSource};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Workspace-key prefix and credential scope this provider owns.
/// Workspaces from GitHub are keyed `"github-<owner>-<repo>-<n>"`;
/// `build_provider_for_workspace` routes on `split_once('-').0`.
/// Using a single constant keeps the prefix authoritative — both the
/// router AND the credential resolve scope read it from here. The value
/// comes from `lazybox_core` so config's snippet scoping and the UI can't
/// drift from it.
pub const SOURCE: &str = lazybox_core::GITHUB_SOURCE;

/// Credential chain GitHub uses. Tried in order:
/// `LAZYBOX_GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `gh auth token`,
/// then a token stored by the native OAuth device flow
/// (`lazybox auth login github`). The lazybox-specific variable is a
/// credential override: spawned agents and interactive `gh` do not
/// automatically read it, but same-user tokens still share GitHub's
/// per-user API quota.
///
/// The stored OAuth token is the **last** resort, below `gh auth token`.
/// It is a manually-persisted credential that never self-heals: unlike an
/// env var or `gh` (which re-read live state each resolve), a stored token
/// that GitHub has invalidated server-side — a password reset, a revoked
/// authorization — is not detectable by `is_expired()` and keeps resolving
/// until the user runs `lazybox auth logout`. Placed ahead of `gh` it would
/// shadow a perfectly good `gh` credential with that dead token and 401 with
/// no fallthrough; placed last, it only activates when nothing better
/// resolves — which is exactly the `gh`-not-installed case it exists for.
/// The polling poller, mutation router, setup wizard's scope source, and
/// fetch-PR-details handler all build clients from this chain.
pub fn credential_chain() -> CredentialChain {
    CredentialChain::new()
        .with(EnvProvider::new("LAZYBOX_GITHUB_TOKEN"))
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &["auth", "token"]))
        .with(oauth::OAuthTokenProvider)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazybox_credential_override_precedes_standard_gh_token() {
        const CHILD: &str = "LAZYBOX_CREDENTIAL_CHAIN_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .args([
                "--exact",
                "tests::lazybox_credential_override_precedes_standard_gh_token",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("LAZYBOX_GITHUB_TOKEN", "lazybox-test-token")
            .env("GH_TOKEN", "shared-test-token")
            .status()
            .expect("spawn isolated credential-chain test");
            assert!(status.success(), "isolated credential-chain test failed");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let credential = runtime
            .block_on(credential_chain().resolve(SOURCE))
            .expect("lazybox credential override resolves");
        assert_eq!(credential.token(), "lazybox-test-token");
        assert_eq!(credential.source, "env:LAZYBOX_GITHUB_TOKEN");
    }

    /// The stored OAuth token is a last resort: it must sit *behind*
    /// `gh auth token` in the chain. A dead-but-unexpired stored token
    /// (GitHub-side revoke / password reset — invisible to `is_expired()`)
    /// placed ahead of `gh` would shadow a working `gh` credential and 401
    /// forever with no fallthrough. This pins the order that prevents it.
    #[test]
    fn stored_oauth_token_is_the_last_resort_behind_gh() {
        let chain = credential_chain();
        let names = chain.provider_names();
        let gh = names
            .iter()
            .position(|n| *n == "command")
            .expect("chain includes the `gh auth token` command provider");
        let oauth = names
            .iter()
            .position(|n| *n == "github-oauth")
            .expect("chain includes the OAuth provider");
        assert!(
            gh < oauth,
            "OAuth token must be tried after `gh auth token`, got {names:?}"
        );
        assert_eq!(
            names.last().copied(),
            Some("github-oauth"),
            "OAuth token must be the final fallback, got {names:?}"
        );
    }
}
