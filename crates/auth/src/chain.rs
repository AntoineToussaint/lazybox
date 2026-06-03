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
    pub async fn resolve(&self, scope: &str) -> Result<Credential, CredentialError> {
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
                    continue;
                }
            }
        }
        Err(CredentialError::Exhausted)
    }

    /// Number of providers in the chain.
    pub fn len(&self) -> usize {
        self.providers.len()
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
