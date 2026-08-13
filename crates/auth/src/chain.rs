use crate::{Credential, CredentialError, CredentialProvider};
use tracing::{debug, trace};

/// Tries multiple credential providers in order, returning the first success.
/// Modeled after AWS SDK's credential chain.
///
/// ```rust,no_run
/// use lazybox_auth::*;
///
/// let chain = CredentialChain::new()
///     .with(EnvProvider::new("GH_TOKEN"))
///     .with(EnvProvider::new("GITHUB_TOKEN"))
///     .with(CommandProvider::new("gh", &["auth", "token"]));
///
/// // In async context:
/// // let cred = chain.resolve("github").await?;
/// ```
pub struct CredentialChain {
    providers: Vec<Box<dyn CredentialProviderBoxed>>,
}

/// Object-safe version of CredentialProvider for dynamic dispatch.
trait CredentialProviderBoxed: Send + Sync {
    fn name(&self) -> &str;
    fn resolve_boxed<'a>(
        &'a self,
        scope: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Credential, CredentialError>> + Send + 'a>,
    >;
}

impl<T: CredentialProvider> CredentialProviderBoxed for T {
    fn name(&self) -> &str {
        CredentialProvider::name(self)
    }

    fn resolve_boxed<'a>(
        &'a self,
        scope: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Credential, CredentialError>> + Send + 'a>,
    > {
        Box::pin(CredentialProvider::resolve(self, scope))
    }
}

impl CredentialChain {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Add a provider to the end of the chain.
    pub fn with<P: CredentialProvider + 'static>(mut self, provider: P) -> Self {
        self.providers.push(Box::new(provider));
        self
    }

    /// Try each provider in order. Returns the first successful credential.
    ///
    /// When every provider declines, the chain reports *why*: a
    /// [`CredentialError::NotFound`] is mere absence (nothing configured) and
    /// must not mask a real failure, but any other error means a provider
    /// ran and failed — a locked keyring, a network blip, an expired
    /// token — so the most recent such failure is surfaced instead of a bare
    /// [`CredentialError::Exhausted`]. Collapsing every cause into `Exhausted`
    /// made a 2-second `gh auth token` hiccup indistinguishable from an
    /// unconfigured user, turning a transient error into a permanent-looking
    /// auth failure. Only a chain where *no* provider even ran (all absent)
    /// stays `Exhausted` — where "absent" covers both `NotFound` and a
    /// provider echoing this chain's own `Exhausted` sentinel.
    pub async fn resolve(&self, scope: &str) -> Result<Credential, CredentialError> {
        let mut last_failure: Option<CredentialError> = None;
        for provider in &self.providers {
            trace!(
                provider = provider.name(),
                scope, "trying credential provider"
            );
            match provider.resolve_boxed(scope).await {
                Ok(cred) => {
                    debug!(
                        provider = provider.name(),
                        source = %cred.source,
                        scope,
                        "credential resolved"
                    );
                    return Ok(cred);
                }
                Err(e) => {
                    trace!(provider = provider.name(), error = %e, "provider skipped");
                    // `NotFound` is mere absence; `Exhausted` is this chain's
                    // own sentinel (a provider returning it — e.g. a nested
                    // chain — is signalling absence too). Neither is a real
                    // failure worth surfacing, so neither may mask one.
                    if !matches!(e, CredentialError::NotFound(_) | CredentialError::Exhausted) {
                        last_failure = Some(e);
                    }
                    continue;
                }
            }
        }
        Err(last_failure.unwrap_or(CredentialError::Exhausted))
    }

    /// Number of providers in the chain.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Provider names in resolution order. Lets callers assert the order a
    /// chain resolves in — e.g. that a last-resort provider stays behind the
    /// providers meant to take precedence — without running any resolution.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl Default for CredentialChain {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that always fails with a preset error — lets the chain
    /// tests assert which cause survives to the exhausted result.
    struct Failing {
        name: &'static str,
        err: fn() -> CredentialError,
    }

    impl CredentialProvider for Failing {
        fn name(&self) -> &str {
            self.name
        }
        async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
            Err((self.err)())
        }
    }

    fn not_found() -> CredentialError {
        CredentialError::NotFound("nothing here".into())
    }
    fn provider_err() -> CredentialError {
        CredentialError::Provider("gh auth token: keyring locked".into())
    }
    fn exhausted() -> CredentialError {
        CredentialError::Exhausted
    }

    #[tokio::test]
    async fn all_absent_reports_exhausted() {
        let chain = CredentialChain::new()
            .with(Failing {
                name: "env",
                err: not_found,
            })
            .with(Failing {
                name: "cmd",
                err: not_found,
            });
        assert!(matches!(
            chain.resolve("github").await,
            Err(CredentialError::Exhausted)
        ));
    }

    #[tokio::test]
    async fn a_real_provider_failure_is_surfaced_not_masked() {
        // A transient `gh auth token` failure must not read as "nothing
        // configured" (Exhausted) — the specific cause survives.
        let chain = CredentialChain::new()
            .with(Failing {
                name: "env",
                err: not_found,
            })
            .with(Failing {
                name: "cmd",
                err: provider_err,
            });
        match chain.resolve("github").await {
            Err(CredentialError::Provider(msg)) => assert!(msg.contains("keyring locked")),
            other => panic!("expected the provider failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_later_absence_does_not_overwrite_an_earlier_failure() {
        let chain = CredentialChain::new()
            .with(Failing {
                name: "cmd",
                err: provider_err,
            })
            .with(Failing {
                name: "env",
                err: not_found,
            });
        assert!(matches!(
            chain.resolve("github").await,
            Err(CredentialError::Provider(_))
        ));
    }

    #[tokio::test]
    async fn an_exhausted_sentinel_does_not_overwrite_an_earlier_failure() {
        // A provider echoing this chain's own `Exhausted` sentinel (e.g. a
        // nested chain that found nothing) signals absence, not a failure —
        // it must not mask the real `Provider` error that preceded it.
        let chain = CredentialChain::new()
            .with(Failing {
                name: "cmd",
                err: provider_err,
            })
            .with(Failing {
                name: "nested",
                err: exhausted,
            });
        match chain.resolve("github").await {
            Err(CredentialError::Provider(msg)) => assert!(msg.contains("keyring locked")),
            other => panic!("expected the provider failure, got {other:?}"),
        }
    }
}
