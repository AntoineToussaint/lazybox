use crate::{Credential, CredentialError, CredentialProvider};

/// Resolves a credential from an environment variable.
///
/// ```rust,no_run
/// use lazybox_auth::EnvProvider;
/// let provider = EnvProvider::new("GH_TOKEN");
/// ```
pub struct EnvProvider {
    var_name: String,
}

impl EnvProvider {
    pub fn new(var_name: impl Into<String>) -> Self {
        Self {
            var_name: var_name.into(),
        }
    }
}

impl CredentialProvider for EnvProvider {
    fn name(&self) -> &str {
        "env"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
        match std::env::var(&self.var_name) {
            Ok(val) if !val.is_empty() => {
                Ok(Credential::new(val, format!("env:{}", self.var_name)))
            }
            _ => Err(CredentialError::NotFound(format!(
                "${} not set",
                self.var_name
            ))),
        }
    }
}
