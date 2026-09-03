use std::fmt;

/// A resolved credential (token, API key, etc.).
#[derive(Clone)]
pub struct Credential {
    /// The secret value.
    token: String,
    /// Human-readable label for where this credential came from.
    /// e.g. "GH_TOKEN env", "gh auth token", "vault:secret/github"
    pub source: String,
}

impl Credential {
    pub fn new(token: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            source: source.into(),
        }
    }

    /// Access the secret token value.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Consume and return the token string.
    pub fn into_token(self) -> String {
        self.token
    }
}

// Never print the actual token.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("source", &self.source)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Credential({})", self.source)
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential not found: {0}")]
    NotFound(String),
    #[error("credential provider failed: {0}")]
    Provider(String),
    #[error("all providers exhausted")]
    Exhausted,
}

/// Trait for resolving credentials. Implement this for custom sources
/// (Vault, Keychain, OAuth token refresh, etc.).
///
/// Providers should be cheap to clone and safe to share across tasks.
pub trait CredentialProvider: Send + Sync {
    /// Human-readable name for this provider (for logging/errors).
    fn name(&self) -> &str;

    /// Attempt to resolve a credential for the given scope.
    ///
    /// `scope` is a free-form string that identifies what the credential is for.
    /// e.g. "github", "linear", "vault:secret/path". Providers that don't use
    /// scope can ignore it.
    fn resolve(
        &self,
        scope: &str,
    ) -> impl std::future::Future<Output = Result<Credential, CredentialError>> + Send;
}
