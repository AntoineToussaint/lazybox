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
    BackgroundSweepForecast, GhClient, HotFetch, RepoSweepOutcome, RepoSweepSpec,
    SelectedFetchOutcome, credential_fingerprint,
};
pub use graphql::{
    PrDetails, repo_sweep_issue_query, repo_sweep_pr_query, roster_member_qualifier,
};
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
///
/// `host` must be the same value passed to the matching
/// `GhClient::from_credential_with_host` — without `--hostname`, `gh auth
/// token` resolves whatever `gh` considers its default host (`github.com`),
/// which silently returns the wrong token when the user is also logged into
/// an unrelated `github.com` account alongside a configured GitHub
/// Enterprise host.
pub fn credential_chain(host: Option<&str>) -> CredentialChain {
    let mut gh_auth_token_args = vec!["auth", "token"];
    if let Some(host) = host {
        gh_auth_token_args.extend(["--hostname", host]);
    }
    CredentialChain::new()
        .with(EnvProvider::new("LAZYBOX_GITHUB_TOKEN"))
        .with(EnvProvider::new("GH_TOKEN"))
        .with(EnvProvider::new("GITHUB_TOKEN"))
        .with(CommandProvider::new("gh", &gh_auth_token_args))
        .with(oauth::OAuthTokenProvider)
}

/// Scope key to pass to `credential_chain(host).resolve(..)`, in place of
/// the bare [`SOURCE`] constant.
///
/// `CredentialChain::resolve` caches its result process-globally, keyed
/// only by this scope string, for 5 minutes — no provider in the chain
/// reads it, it exists purely as a cache key. `SOURCE` alone would let two
/// different hosts collide on the same cache entry: whichever host
/// resolved first would silently serve its token to the other host's
/// requests (e.g. `build_guard`'s public-github.com check and a
/// configured Enterprise host) until the cache expired. Folding the host
/// into the key keeps each host's resolution independent.
pub fn credential_scope(host: Option<&str>) -> String {
    match host {
        Some(host) => format!("{SOURCE}:{host}"),
        None => SOURCE.to_string(),
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
            .block_on(credential_chain(None).resolve(SOURCE))
            .expect("lazybox credential override resolves");
        assert_eq!(credential.token(), "lazybox-test-token");
        assert_eq!(credential.source, "env:LAZYBOX_GITHUB_TOKEN");
    }

    /// A configured host must reach `gh auth token` as `--hostname
    /// <host>` — otherwise `gh` resolves whatever it considers its
    /// default host (`github.com`), which silently returns the wrong
    /// token for a user also logged into an unrelated `github.com`
    /// account alongside a configured GitHub Enterprise host. Also
    /// exercises `credential_scope`: resolving two different hosts back
    /// to back must NOT let `CredentialChain`'s process-global,
    /// scope-keyed cache serve one host's token to the other (it did,
    /// silently, before `credential_scope` folded the host into the
    /// cache key). Isolated in a child process (like the override test
    /// above) both because `CommandProvider`/`CredentialChain` cache
    /// process-globally and because it swaps `PATH` to point `gh` at a
    /// fake script that just echoes its argv back as the "token".
    #[test]
    fn credential_chain_passes_configured_host_to_gh_auth_token() {
        const CHILD: &str = "LAZYBOX_CREDENTIAL_CHAIN_HOSTNAME_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = std::env::temp_dir()
                .join(format!("lazybox-gh-hostname-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create fake-gh dir");
            let fake_gh = dir.join("gh");
            std::fs::write(&fake_gh, "#!/bin/sh\necho \"$@\"\n").expect("write fake gh");
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod fake gh");
            }
            let path = format!(
                "{}:{}",
                dir.display(),
                std::env::var("PATH").unwrap_or_default()
            );

            let status = std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .args([
                "--exact",
                "tests::credential_chain_passes_configured_host_to_gh_auth_token",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PATH", path)
            .env_remove("LAZYBOX_GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .status()
            .expect("spawn isolated hostname test");
            let _ = std::fs::remove_dir_all(&dir);
            assert!(status.success(), "isolated hostname test failed");
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let host = Some("ghe.example.com");
        let credential = runtime
            .block_on(credential_chain(host).resolve(&credential_scope(host)))
            .expect("fake gh resolves a credential");
        assert_eq!(credential.token(), "auth token --hostname ghe.example.com");

        // No host configured: must NOT pass `--hostname` at all, so `gh`
        // keeps resolving its own default host as before — and must NOT
        // reuse the other host's cached chain resolution above.
        let credential = runtime
            .block_on(credential_chain(None).resolve(&credential_scope(None)))
            .expect("fake gh resolves a credential");
        assert_eq!(credential.token(), "auth token");
    }

    /// The stored OAuth token is a last resort: it must sit *behind*
    /// `gh auth token` in the chain. A dead-but-unexpired stored token
    /// (GitHub-side revoke / password reset — invisible to `is_expired()`)
    /// placed ahead of `gh` would shadow a working `gh` credential and 401
    /// forever with no fallthrough. This pins the order that prevents it.
    #[test]
    fn stored_oauth_token_is_the_last_resort_behind_gh() {
        let chain = credential_chain(None);
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
